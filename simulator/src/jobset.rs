use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::PodTemplateSpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, ApiResource, DynamicObject, Patch, PatchParams, PostParams};
use kube::runtime::controller::Action;
use kube::runtime::watcher;
use kube::runtime::Controller;
use kube::{Client, ResourceExt};
use tracing::{info, warn};

fn jobset_api_resource() -> ApiResource {
    ApiResource {
        group: "jobset.x-k8s.io".into(),
        version: "v1alpha2".into(),
        api_version: "jobset.x-k8s.io/v1alpha2".into(),
        kind: "JobSet".into(),
        plural: "jobsets".into(),
    }
}

async fn reconcile_jobset(
    js: Arc<DynamicObject>,
    ctx: Arc<Client>,
) -> Result<Action, kube::Error> {
    let name = js.name_any();
    let ns = js
        .namespace()
        .unwrap_or_else(|| "default".to_string());

    let suspended = js
        .data
        .get("spec")
        .and_then(|s| s.get("suspend"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if suspended {
        return Ok(Action::await_change());
    }

    let terminal = js
        .data
        .get("status")
        .and_then(|s| s.get("terminalState"))
        .and_then(|v| v.as_str());

    if terminal.is_some() {
        return Ok(Action::await_change());
    }

    let replicated_jobs = match js
        .data
        .get("spec")
        .and_then(|s| s.get("replicatedJobs"))
        .and_then(|r| r.as_array())
    {
        Some(rj) => rj,
        None => return Ok(Action::await_change()),
    };

    let uid = js.metadata.uid.as_deref().unwrap_or("");
    let jobs_api: Api<Job> = Api::namespaced(ctx.as_ref().clone(), &ns);

    let mut total_jobs = 0u32;
    let mut succeeded_jobs = 0u32;
    let mut failed_jobs = 0u32;

    for rj in replicated_jobs {
        let rj_name = rj
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("worker");
        let replicas = rj
            .get("replicas")
            .and_then(|r| r.as_u64())
            .unwrap_or(1) as u32;

        let job_template: Option<JobSpec> = rj
            .get("template")
            .and_then(|t| t.get("spec"))
            .and_then(|s| serde_json::from_value(s.clone()).ok());

        let pod_template: Option<PodTemplateSpec> = rj
            .get("template")
            .and_then(|t| t.get("spec"))
            .and_then(|s| s.get("template"))
            .and_then(|t| serde_json::from_value(t.clone()).ok());

        for idx in 0..replicas {
            let job_name = format!("{name}-{rj_name}-{idx}");
            total_jobs += 1;

            match jobs_api.get_opt(&job_name).await? {
                Some(existing) => {
                    let status = existing.status.as_ref();
                    if status.and_then(|s| s.succeeded).unwrap_or(0) > 0 {
                        succeeded_jobs += 1;
                    } else if status.and_then(|s| s.failed).unwrap_or(0) > 0 {
                        failed_jobs += 1;
                    }
                }
                None => {
                    let spec = if let Some(ref jt) = job_template {
                        let mut spec = jt.clone();
                        if spec.template.spec.is_none() {
                            if let Some(ref pt) = pod_template {
                                spec.template = pt.clone();
                            }
                        }
                        spec
                    } else {
                        JobSpec {
                            template: pod_template.clone().unwrap_or_default(),
                            ..Default::default()
                        }
                    };

                    let job = Job {
                        metadata: ObjectMeta {
                            name: Some(job_name.clone()),
                            namespace: Some(ns.clone()),
                            labels: Some(
                                [
                                    ("jobset.x-k8s.io/jobset-name".into(), name.clone()),
                                    (
                                        "jobset.x-k8s.io/replicatedjob-name".into(),
                                        rj_name.to_string(),
                                    ),
                                    (
                                        "jobset.x-k8s.io/job-index".into(),
                                        idx.to_string(),
                                    ),
                                ]
                                .into(),
                            ),
                            owner_references: Some(vec![
                                k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                                    api_version: "jobset.x-k8s.io/v1alpha2".into(),
                                    kind: "JobSet".into(),
                                    name: name.clone(),
                                    uid: uid.to_string(),
                                    controller: Some(true),
                                    block_owner_deletion: Some(true),
                                },
                            ]),
                            ..Default::default()
                        },
                        spec: Some(spec),
                        ..Default::default()
                    };

                    match jobs_api.create(&PostParams::default(), &job).await {
                        Ok(_) => info!(ns, job_name, "created child job for jobset"),
                        Err(kube::Error::Api(e)) if e.code == 409 => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }
    }

    let ar = jobset_api_resource();
    let js_api: Api<DynamicObject> = Api::namespaced_with(ctx.as_ref().clone(), &ns, &ar);

    if succeeded_jobs + failed_jobs == total_jobs && total_jobs > 0 {
        let now = chrono::Utc::now().to_rfc3339();
        let (terminal_state, cond_type) = if failed_jobs > 0 {
            ("Failed", "Failed")
        } else {
            ("Completed", "Completed")
        };

        let status_patch = serde_json::json!({
            "status": {
                "terminalState": terminal_state,
                "conditions": [{
                    "type": cond_type,
                    "status": "True",
                    "lastTransitionTime": now,
                    "reason": terminal_state,
                    "message": format!("{succeeded_jobs}/{total_jobs} jobs succeeded")
                }]
            }
        });

        js_api
            .patch_status(&name, &PatchParams::apply("ocp-sim"), &Patch::Merge(&status_patch))
            .await?;

        info!(ns, name, terminal_state, "jobset reached terminal state");
        return Ok(Action::await_change());
    }

    if total_jobs > 0 {
        let status_patch = serde_json::json!({
            "status": {
                "conditions": [{
                    "type": "Running",
                    "status": "True",
                    "lastTransitionTime": chrono::Utc::now().to_rfc3339(),
                    "reason": "JobsRunning",
                    "message": format!("{succeeded_jobs}/{total_jobs} jobs completed")
                }]
            }
        });

        js_api
            .patch_status(&name, &PatchParams::apply("ocp-sim"), &Patch::Merge(&status_patch))
            .await?;
    }

    Ok(Action::requeue(std::time::Duration::from_secs(5)))
}

fn error_policy(
    _obj: Arc<DynamicObject>,
    _error: &kube::Error,
    _ctx: Arc<Client>,
) -> Action {
    Action::requeue(std::time::Duration::from_secs(10))
}

pub async fn run(client: Client) -> Result<(), kube::Error> {
    let ar = jobset_api_resource();
    let jobsets: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
    let ctx = Arc::new(client);

    info!("starting jobset controller");

    Controller::new_with(jobsets, watcher::Config::default(), ar)
        .run(reconcile_jobset, error_policy, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!("jobset reconcile error: {e:?}");
            }
        })
        .await;

    Ok(())
}
