use futures::StreamExt;
use k8s_openapi::api::core::v1::{Node, Service};
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::watcher;
use kube::runtime::WatchStreamExt;
use kube::{Client, ResourceExt};
use tracing::{info, warn};

async fn get_node_ip(client: &Client) -> anyhow::Result<String> {
    let nodes: Api<Node> = Api::all(client.clone());
    let node_list = nodes.list(&Default::default()).await?;
    for node in node_list {
        if let Some(status) = node.status {
            if let Some(addresses) = status.addresses {
                for addr in addresses {
                    if addr.type_ == "InternalIP" {
                        return Ok(addr.address);
                    }
                }
            }
        }
    }
    anyhow::bail!("no node with InternalIP found")
}

fn needs_ip(svc: &Service) -> bool {
    let is_lb = svc
        .spec
        .as_ref()
        .and_then(|s| s.type_.as_deref())
        .map(|t| t == "LoadBalancer")
        .unwrap_or(false);
    if !is_lb {
        return false;
    }
    let ingress = svc
        .status
        .as_ref()
        .and_then(|s| s.load_balancer.as_ref())
        .and_then(|lb| lb.ingress.as_ref());
    match ingress {
        None => true,
        Some(list) => list.is_empty(),
    }
}

pub async fn run(client: Client) -> anyhow::Result<()> {
    let node_ip = get_node_ip(&client).await?;
    info!(node_ip, "loadbalancer controller started");

    let svcs: Api<Service> = Api::all(client.clone());
    let stream = watcher::watcher(svcs, watcher::Config::default())
        .default_backoff()
        .applied_objects();

    tokio::pin!(stream);

    while let Some(event) = stream.next().await {
        match event {
            Ok(svc) => {
                if !needs_ip(&svc) {
                    continue;
                }
                let ns = svc.namespace().unwrap_or_default();
                let name = svc.name_any();

                let status = serde_json::json!({
                    "status": {
                        "loadBalancer": {
                            "ingress": [{ "ip": node_ip }]
                        }
                    }
                });

                let api: Api<Service> = Api::namespaced(client.clone(), &ns);
                match api
                    .patch_status(&name, &PatchParams::apply("ocp-sim"), &Patch::Merge(&status))
                    .await
                {
                    Ok(_) => info!(ns, name, node_ip, "assigned LoadBalancer IP"),
                    Err(e) => warn!(ns, name, %e, "failed to patch LoadBalancer status"),
                }
            }
            Err(e) => {
                warn!(%e, "service watch error");
            }
        }
    }

    Ok(())
}
