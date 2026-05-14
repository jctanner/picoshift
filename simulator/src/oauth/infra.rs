use k8s_openapi::api::core::v1::{
    Endpoints, EndpointAddress, EndpointPort, EndpointSubset, Namespace, Service, ServicePort,
    ServiceSpec,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, ApiResource, DynamicObject, Patch, PatchParams, PostParams};
use kube::Client;
use rcgen::{CertificateParams, Issuer, KeyPair};
use rustls::ServerConfig;
use tracing::{info, warn};

use crate::{AuthMode, CaState};

use super::types::{DOMAIN, OAUTH_HOST, OAUTH_NS, OAUTH_PORT};

pub(crate) fn generate_tls_config(
    ca: &CaState,
) -> Result<ServerConfig, Box<dyn std::error::Error + Send + Sync>> {
    let cn = format!("{OAUTH_HOST}.{DOMAIN}");
    let ca_key = KeyPair::from_pem(&ca.ca_key_pem)?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let issuer = Issuer::from_params(&ca_params, &ca_key);

    let entra_host = format!("entra.{DOMAIN}");
    let mut params = CertificateParams::new(vec![cn.clone(), entra_host])?;
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        rcgen::DnValue::Utf8String(cn.clone()),
    );

    let key_pair = KeyPair::generate()?;
    let cert = params.signed_by(&key_pair, &issuer)?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    let certs = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())?
        .ok_or("no private key found")?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    Ok(config)
}

pub(crate) async fn setup_infrastructure(
    client: &Client,
    auth_mode: &AuthMode,
) -> anyhow::Result<()> {
    let ns_api: Api<Namespace> = Api::all(client.clone());
    let ns = Namespace {
        metadata: ObjectMeta {
            name: Some(OAUTH_NS.to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    match ns_api.create(&PostParams::default(), &ns).await {
        Ok(_) => info!("created namespace {OAUTH_NS}"),
        Err(kube::Error::Api(e)) if e.code == 409 => {}
        Err(e) => return Err(e.into()),
    }

    let node_ip = get_node_ip(client).await?;
    info!(node_ip, "discovered node IP for OAuth service");

    if matches!(auth_mode, AuthMode::Byoidc) {
        let svc_api: Api<Service> = Api::namespaced(client.clone(), OAUTH_NS);
        match svc_api.delete(OAUTH_HOST, &Default::default()).await {
            Ok(_) => info!("deleted leftover service {OAUTH_HOST}"),
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => warn!(%e, "failed to delete leftover service {OAUTH_HOST}"),
        }
        let ep_api: Api<Endpoints> = Api::namespaced(client.clone(), OAUTH_NS);
        match ep_api.delete(OAUTH_HOST, &Default::default()).await {
            Ok(_) => info!("deleted leftover endpoints {OAUTH_HOST}"),
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => warn!(%e, "failed to delete leftover endpoints {OAUTH_HOST}"),
        }
        {
            let ar = ApiResource {
                group: "route.openshift.io".into(),
                version: "v1".into(),
                api_version: "route.openshift.io/v1".into(),
                kind: "Route".into(),
                plural: "routes".into(),
            };
            let routes: Api<DynamicObject> =
                Api::namespaced_with(client.clone(), OAUTH_NS, &ar);
            match routes.delete(OAUTH_HOST, &Default::default()).await {
                Ok(_) => info!("deleted leftover route {OAUTH_HOST}"),
                Err(kube::Error::Api(e)) if e.code == 404 => {}
                Err(e) => warn!(%e, "failed to delete leftover route {OAUTH_HOST}"),
            }
        }

        let svc_api: Api<Service> = Api::namespaced(client.clone(), OAUTH_NS);
        let svc = Service {
            metadata: ObjectMeta {
                name: Some("entra".to_string()),
                namespace: Some(OAUTH_NS.to_string()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                cluster_ip: Some("None".to_string()),
                ports: Some(vec![ServicePort {
                    name: Some("https".to_string()),
                    port: OAUTH_PORT as i32,
                    target_port: Some(IntOrString::Int(OAUTH_PORT as i32)),
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        match svc_api
            .patch("entra", &PatchParams::apply("ocp-sim"), &Patch::Apply(svc))
            .await
        {
            Ok(_) => info!("created/updated service entra in {OAUTH_NS}"),
            Err(e) => warn!(%e, "failed to create entra service"),
        }

        let ep_api: Api<Endpoints> = Api::namespaced(client.clone(), OAUTH_NS);
        let ep = Endpoints {
            metadata: ObjectMeta {
                name: Some("entra".to_string()),
                namespace: Some(OAUTH_NS.to_string()),
                ..Default::default()
            },
            subsets: Some(vec![EndpointSubset {
                addresses: Some(vec![EndpointAddress {
                    ip: node_ip.clone(),
                    ..Default::default()
                }]),
                ports: Some(vec![EndpointPort {
                    name: Some("https".to_string()),
                    port: OAUTH_PORT as i32,
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
        };
        match ep_api
            .patch("entra", &PatchParams::apply("ocp-sim"), &Patch::Apply(ep))
            .await
        {
            Ok(_) => info!("created/updated endpoints for entra"),
            Err(e) => warn!(%e, "failed to create entra endpoints"),
        }

        create_oauth_route(client, &format!("entra.{DOMAIN}"), "entra").await?;
    } else {
        let svc_api: Api<Service> = Api::namespaced(client.clone(), OAUTH_NS);
        let svc = Service {
            metadata: ObjectMeta {
                name: Some(OAUTH_HOST.to_string()),
                namespace: Some(OAUTH_NS.to_string()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                cluster_ip: Some("None".to_string()),
                ports: Some(vec![ServicePort {
                    name: Some("https".to_string()),
                    port: OAUTH_PORT as i32,
                    target_port: Some(IntOrString::Int(OAUTH_PORT as i32)),
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        match svc_api
            .patch(
                OAUTH_HOST,
                &PatchParams::apply("ocp-sim"),
                &Patch::Apply(svc),
            )
            .await
        {
            Ok(_) => info!("created/updated service {OAUTH_HOST} in {OAUTH_NS}"),
            Err(e) => warn!(%e, "failed to create oauth service"),
        }

        let ep_api: Api<Endpoints> = Api::namespaced(client.clone(), OAUTH_NS);
        let ep = Endpoints {
            metadata: ObjectMeta {
                name: Some(OAUTH_HOST.to_string()),
                namespace: Some(OAUTH_NS.to_string()),
                ..Default::default()
            },
            subsets: Some(vec![EndpointSubset {
                addresses: Some(vec![EndpointAddress {
                    ip: node_ip.clone(),
                    ..Default::default()
                }]),
                ports: Some(vec![EndpointPort {
                    name: Some("https".to_string()),
                    port: OAUTH_PORT as i32,
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
        };
        match ep_api
            .patch(
                OAUTH_HOST,
                &PatchParams::apply("ocp-sim"),
                &Patch::Apply(ep),
            )
            .await
        {
            Ok(_) => info!("created/updated endpoints for {OAUTH_HOST}"),
            Err(e) => warn!(%e, "failed to create oauth endpoints"),
        }

        create_oauth_route(client, &format!("{OAUTH_HOST}.{DOMAIN}"), OAUTH_HOST).await?;
    }

    patch_coredns(client, &node_ip, auth_mode).await?;

    let ep_api: Api<Endpoints> = Api::namespaced(client.clone(), "default");
    let ep = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Endpoints",
        "metadata": { "name": "kubernetes" },
        "subsets": [{
            "addresses": [{ "ip": node_ip }],
            "ports": [{ "name": "https", "port": 6443, "protocol": "TCP" }]
        }]
    });
    match ep_api
        .patch(
            "kubernetes",
            &PatchParams::apply("ocp-sim"),
            &Patch::Apply(serde_json::from_value::<Endpoints>(ep)?),
        )
        .await
    {
        Ok(_) => info!("patched kubernetes endpoints to ocp-shim port 6443"),
        Err(e) => warn!(%e, "failed to patch kubernetes endpoints"),
    }

    Ok(())
}

async fn get_node_ip(client: &Client) -> anyhow::Result<String> {
    use k8s_openapi::api::core::v1::Node;
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

async fn create_oauth_route(
    client: &Client,
    host: &str,
    svc_name: &str,
) -> anyhow::Result<()> {
    let ar = ApiResource {
        group: "route.openshift.io".into(),
        version: "v1".into(),
        api_version: "route.openshift.io/v1".into(),
        kind: "Route".into(),
        plural: "routes".into(),
    };
    let routes: Api<DynamicObject> = Api::namespaced_with(client.clone(), OAUTH_NS, &ar);

    let route_name = host.split('.').next().unwrap_or(host);
    let route = serde_json::json!({
        "apiVersion": "route.openshift.io/v1",
        "kind": "Route",
        "metadata": {
            "name": route_name,
            "namespace": OAUTH_NS
        },
        "spec": {
            "host": host,
            "to": {
                "kind": "Service",
                "name": svc_name
            },
            "port": {
                "targetPort": "https"
            },
            "tls": {
                "termination": "passthrough"
            }
        }
    });

    match routes
        .patch(
            route_name,
            &PatchParams::apply("ocp-sim"),
            &Patch::Apply(serde_json::from_value::<DynamicObject>(route)?),
        )
        .await
    {
        Ok(_) => info!(host, "created/updated oauth route"),
        Err(e) => warn!(%e, "failed to create oauth route"),
    }

    Ok(())
}

async fn patch_coredns(
    client: &Client,
    node_ip: &str,
    auth_mode: &AuthMode,
) -> anyhow::Result<()> {
    let cm_api: Api<k8s_openapi::api::core::v1::ConfigMap> =
        Api::namespaced(client.clone(), "kube-system");

    let cm = cm_api.get("coredns").await?;
    let corefile = cm
        .data
        .as_ref()
        .and_then(|d| d.get("Corefile"))
        .cloned()
        .unwrap_or_default();

    if corefile.contains("apps.ocp-sim.test") {
        info!("CoreDNS already patched for apps.ocp-sim.test");
        return Ok(());
    }

    let hosts_entries = if matches!(auth_mode, AuthMode::Byoidc) {
        format!("        {node_ip} entra.apps.ocp-sim.test")
    } else {
        format!("        {node_ip} oauth-openshift.apps.ocp-sim.test")
    };
    let hosts_block = format!(
        "\napps.ocp-sim.test:53 {{\n    hosts {{\n{hosts_entries}\n        fallthrough\n    }}\n}}\n"
    );
    let new_corefile = format!("{corefile}{hosts_block}");

    let patch = serde_json::json!({
        "data": {
            "Corefile": new_corefile
        }
    });
    cm_api
        .patch("coredns", &PatchParams::default(), &Patch::Merge(&patch))
        .await?;

    info!(node_ip, "patched CoreDNS for apps.ocp-sim.test");

    let pods: Api<k8s_openapi::api::core::v1::Pod> =
        Api::namespaced(client.clone(), "kube-system");
    let pod_list = pods
        .list(&kube::api::ListParams::default().labels("k8s-app=kube-dns"))
        .await?;
    for pod in pod_list {
        let name = pod.metadata.name.unwrap_or_default();
        match pods.delete(&name, &Default::default()).await {
            Ok(_) => info!(name, "restarted CoreDNS pod"),
            Err(e) => warn!(name, %e, "failed to restart CoreDNS pod"),
        }
    }

    Ok(())
}
