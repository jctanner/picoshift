use std::sync::Arc;

use futures::StreamExt;
use kube::api::{Api, ApiResource, DynamicObject, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::watcher;
use kube::runtime::Controller;
use kube::{Client, ResourceExt};
use tracing::{info, warn};

const DOMAIN: &str = "apps.ocp-sim.test";

fn route_api_resource() -> ApiResource {
    ApiResource {
        group: "route.openshift.io".into(),
        version: "v1".into(),
        api_version: "route.openshift.io/v1".into(),
        kind: "Route".into(),
        plural: "routes".into(),
    }
}

fn is_admitted(route: &DynamicObject) -> bool {
    route
        .data
        .get("status")
        .and_then(|s| s.get("ingress"))
        .and_then(|i| i.as_array())
        .map(|arr| {
            arr.iter().any(|ingress| {
                ingress
                    .get("conditions")
                    .and_then(|c| c.as_array())
                    .map(|conds| {
                        conds.iter().any(|c| {
                            c.get("type").and_then(|t| t.as_str()) == Some("Admitted")
                                && c.get("status").and_then(|s| s.as_str()) == Some("True")
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn compute_host(route: &DynamicObject) -> String {
    if let Some(host) = route
        .data
        .get("spec")
        .and_then(|s| s.get("host"))
        .and_then(|h| h.as_str())
    {
        if !host.is_empty() {
            return host.to_string();
        }
    }

    let name = route.name_any();
    let ns = route.namespace().unwrap_or_default();
    format!("{name}-{ns}.{DOMAIN}")
}

async fn reconcile_route(
    route: Arc<DynamicObject>,
    ctx: Arc<Client>,
) -> Result<Action, kube::Error> {
    let ns = route.namespace().unwrap_or_default();
    let name = route.name_any();
    let host = compute_host(&route);

    if !is_admitted(&route) {
        let now = chrono::Utc::now().to_rfc3339();

        let ar = route_api_resource();
        let routes: Api<DynamicObject> = Api::namespaced_with(ctx.as_ref().clone(), &ns, &ar);

        let status_patch = serde_json::json!({
            "status": {
                "ingress": [{
                    "host": host,
                    "routerName": "default",
                    "routerCanonicalHostname": format!("router-default.{DOMAIN}"),
                    "conditions": [{
                        "type": "Admitted",
                        "status": "True",
                        "lastTransitionTime": now
                    }]
                }]
            }
        });

        routes
            .patch_status(
                &name,
                &PatchParams::apply("ocp-sim"),
                &Patch::Merge(&status_patch),
            )
            .await?;

        info!(ns, name, host, "admitted route");
    }

    Ok(Action::await_change())
}

fn error_policy(
    _obj: Arc<DynamicObject>,
    _error: &kube::Error,
    _ctx: Arc<Client>,
) -> Action {
    Action::requeue(std::time::Duration::from_secs(10))
}

pub async fn run(client: Client) -> Result<(), kube::Error> {
    let ar = route_api_resource();
    let routes: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
    let ctx = Arc::new(client);

    info!("starting route admission controller");

    Controller::new_with(routes, watcher::Config::default(), ar)
        .run(reconcile_route, error_policy, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!("route reconcile error: {e:?}");
            }
        })
        .await;

    Ok(())
}
