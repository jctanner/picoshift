use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::core::v1::Namespace;
use kube::api::{Api, DynamicObject, ObjectMeta, Patch, PatchParams, PostParams};
use kube::runtime::controller::Action;
use kube::runtime::watcher;
use kube::runtime::Controller;
use kube::{Client, ResourceExt};
use tracing::{info, warn};

use crate::util::project_ar;

async fn reconcile_namespace(
    ns: Arc<Namespace>,
    ctx: Arc<Client>,
) -> Result<Action, kube::Error> {
    let name = ns.name_any();

    let phase = ns
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .unwrap_or("Active");

    let labels = ns.metadata.labels.clone().unwrap_or_default();

    let ar = project_ar();
    let projects: Api<DynamicObject> = Api::all_with(ctx.as_ref().clone(), &ar);

    let project = serde_json::json!({
        "apiVersion": "project.openshift.io/v1",
        "kind": "Project",
        "metadata": {
            "name": name,
            "labels": labels,
        },
        "status": {
            "phase": phase,
        }
    });

    projects
        .patch(
            &name,
            &PatchParams::apply("ocp-sim-project"),
            &Patch::Apply(project),
        )
        .await?;

    info!(name, phase, "synced project from namespace");

    Ok(Action::await_change())
}

async fn reconcile_project(
    proj: Arc<DynamicObject>,
    ctx: Arc<Client>,
) -> Result<Action, kube::Error> {
    let name = proj.name_any();

    let ns_api: Api<Namespace> = Api::all(ctx.as_ref().clone());

    if ns_api.get_opt(&name).await?.is_none() {
        let ns = Namespace {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                labels: proj.metadata.labels.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        match ns_api.create(&PostParams::default(), &ns).await {
            Ok(_) => info!(name, "created namespace from project"),
            Err(kube::Error::Api(e)) if e.code == 409 => {}
            Err(e) => return Err(e),
        }
    }

    Ok(Action::await_change())
}

fn ns_error_policy(
    _obj: Arc<Namespace>,
    _error: &kube::Error,
    _ctx: Arc<Client>,
) -> Action {
    Action::requeue(std::time::Duration::from_secs(10))
}

fn proj_error_policy(
    _obj: Arc<DynamicObject>,
    _error: &kube::Error,
    _ctx: Arc<Client>,
) -> Action {
    Action::requeue(std::time::Duration::from_secs(10))
}

pub async fn run(client: Client) -> anyhow::Result<()> {
    let ctx = Arc::new(client.clone());

    info!("starting project controller (namespace ↔ project sync)");

    let ns_api: Api<Namespace> = Api::all(client.clone());
    let ns_ctrl = Controller::new(ns_api, watcher::Config::default())
        .run(reconcile_namespace, ns_error_policy, ctx.clone())
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!("project reconcile error (ns→proj): {e:?}");
            }
        });

    let ar = project_ar();
    let proj_api: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
    let proj_ctrl = Controller::new_with(proj_api, watcher::Config::default(), ar)
        .run(reconcile_project, proj_error_policy, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!("project reconcile error (proj→ns): {e:?}");
            }
        });

    tokio::select! {
        _ = ns_ctrl => {},
        _ = proj_ctrl => {},
    }

    Ok(())
}
