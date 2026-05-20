use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::Arc;

use base64::Engine;
use bytes::Bytes;
use futures::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use k8s_openapi::api::admissionregistration::v1::{
    MutatingWebhookConfiguration, RuleWithOperations, WebhookClientConfig,
};
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::watcher;
use kube::runtime::Controller;
use kube::{Client, ResourceExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

use crate::CaState;

const WEBHOOK_PORT: u16 = 8443;
const UID_RANGE_ANNOTATION: &str = "openshift.io/sa.scc.uid-range";
const SUPPLEMENTAL_GROUPS_ANNOTATION: &str = "openshift.io/sa.scc.supplemental-groups";

const SKIP_NAMESPACES: &[&str] = &[
    "kube-system",
    "kube-public",
    "kube-node-lease",
    "ocp-sim",
    "local-path-storage",
];

fn uid_range_for_namespace(name: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    let h = hasher.finish();
    let block = h % 100_000;
    1_000_000_000 + block * 10_000
}


fn build_admission_response(uid: &str, patch: Option<Vec<serde_json::Value>>) -> serde_json::Value {
    let mut response = serde_json::json!({
        "apiVersion": "admission.k8s.io/v1",
        "kind": "AdmissionReview",
        "response": {
            "uid": uid,
            "allowed": true
        }
    });

    if let Some(ops) = patch {
        let patch_json = serde_json::to_string(&ops).unwrap();
        let patch_b64 = base64::engine::general_purpose::STANDARD.encode(patch_json.as_bytes());
        response["response"]["patchType"] = serde_json::json!("JSONPatch");
        response["response"]["patch"] = serde_json::json!(patch_b64);
    }

    response
}

fn compute_patch(pod: &serde_json::Value, namespace: &str) -> Option<Vec<serde_json::Value>> {
    let spec = pod.get("spec")?;
    let sc = spec.get("securityContext");
    let fs_group = sc.and_then(|s| s.get("fsGroup"));

    if fs_group.is_some() {
        return None;
    }

    let run_as_non_root_pod = sc
        .and_then(|s| s.get("runAsNonRoot"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let run_as_non_root_container = spec
        .get("containers")
        .and_then(|c| c.as_array())
        .map(|containers| {
            containers.iter().any(|c| {
                c.get("securityContext")
                    .and_then(|s| s.get("runAsNonRoot"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    if !run_as_non_root_pod && !run_as_non_root_container {
        return None;
    }

    let uid = uid_range_for_namespace(namespace) as i64;

    if sc.is_some() {
        Some(vec![serde_json::json!({
            "op": "add",
            "path": "/spec/securityContext/fsGroup",
            "value": uid
        })])
    } else {
        Some(vec![serde_json::json!({
            "op": "add",
            "path": "/spec/securityContext",
            "value": { "fsGroup": uid }
        })])
    }
}

async fn handle_request(
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let path = req.uri().path().to_string();

    if req.method() != Method::POST || path != "/mutate/pods" {
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("not found\n")))
            .unwrap());
    }

    let body = req.collect().await?.to_bytes();
    let review: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            warn!(%e, "failed to parse AdmissionReview");
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from("bad request\n")))
                .unwrap());
        }
    };

    let uid = review["request"]["uid"].as_str().unwrap_or("");
    let namespace = review["request"]["namespace"].as_str().unwrap_or("default");
    let pod = &review["request"]["object"];

    let patch = compute_patch(pod, namespace);

    if patch.is_some() {
        let pod_name = pod["metadata"]["name"]
            .as_str()
            .or_else(|| pod["metadata"]["generateName"].as_str())
            .unwrap_or("<unknown>");
        let fs = uid_range_for_namespace(namespace);
        info!(namespace, pod_name, fs, "injecting fsGroup into pod");
    }

    let response = build_admission_response(uid, patch);
    let body = serde_json::to_vec(&response).unwrap();

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

use crate::util::{get_node_ip, sign_tls_config};

async fn setup_webhook_with_ip(client: &Client, ca: &CaState, node_ip: &str) -> anyhow::Result<()> {
    let ca_bundle = base64::engine::general_purpose::STANDARD.encode(ca.ca_cert_pem.as_bytes());

    let webhook_url = format!("https://{node_ip}:{WEBHOOK_PORT}/mutate/pods");

    let mwc = MutatingWebhookConfiguration {
        metadata: ObjectMeta {
            name: Some("ocp-sim-scc".to_string()),
            ..Default::default()
        },
        webhooks: Some(vec![
            k8s_openapi::api::admissionregistration::v1::MutatingWebhook {
                name: "scc.ocp-sim.io".to_string(),
                client_config: WebhookClientConfig {
                    url: Some(webhook_url.clone()),
                    ca_bundle: Some(k8s_openapi::ByteString(
                        base64::engine::general_purpose::STANDARD
                            .decode(&ca_bundle)
                            .unwrap_or_default(),
                    )),
                    service: None,
                },
                rules: Some(vec![RuleWithOperations {
                    api_groups: Some(vec!["".to_string()]),
                    api_versions: Some(vec!["v1".to_string()]),
                    operations: Some(vec!["CREATE".to_string()]),
                    resources: Some(vec!["pods".to_string()]),
                    scope: Some("Namespaced".to_string()),
                }]),
                failure_policy: Some("Ignore".to_string()),
                side_effects: "None".to_string(),
                admission_review_versions: vec!["v1".to_string()],
                namespace_selector: Some(
                    k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
                        match_expressions: Some(vec![
                            k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelectorRequirement {
                                key: "kubernetes.io/metadata.name".to_string(),
                                operator: "NotIn".to_string(),
                                values: Some(
                                    SKIP_NAMESPACES
                                        .iter()
                                        .map(|s| s.to_string())
                                        .collect(),
                                ),
                            },
                        ]),
                        ..Default::default()
                    },
                ),
                reinvocation_policy: Some("Never".to_string()),
                timeout_seconds: Some(5),
                ..Default::default()
            },
        ]),
    };

    let api: Api<MutatingWebhookConfiguration> = Api::all(client.clone());
    api.patch(
        "ocp-sim-scc",
        &PatchParams::apply("ocp-sim"),
        &Patch::Apply(mwc),
    )
    .await?;

    info!(webhook_url, "registered MutatingWebhookConfiguration ocp-sim-scc");
    Ok(())
}

// --- Namespace UID range annotation controller ---

async fn reconcile_namespace(
    ns: Arc<Namespace>,
    ctx: Arc<Client>,
) -> Result<Action, kube::Error> {
    let name = ns.name_any();

    if SKIP_NAMESPACES.contains(&name.as_str()) {
        return Ok(Action::await_change());
    }

    let has_uid_range = ns
        .metadata
        .annotations
        .as_ref()
        .map(|a| a.contains_key(UID_RANGE_ANNOTATION))
        .unwrap_or(false);

    if has_uid_range {
        return Ok(Action::await_change());
    }

    let uid = uid_range_for_namespace(&name);
    let range = format!("{uid}/10000");

    let api: Api<Namespace> = Api::all((*ctx).clone());
    let patch = serde_json::json!({
        "metadata": {
            "annotations": {
                UID_RANGE_ANNOTATION: range,
                SUPPLEMENTAL_GROUPS_ANNOTATION: range
            }
        }
    });

    api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;

    info!(name, range, "annotated namespace with UID range");
    Ok(Action::await_change())
}

fn error_policy_ns(
    _obj: Arc<Namespace>,
    _error: &kube::Error,
    _ctx: Arc<Client>,
) -> Action {
    Action::requeue(std::time::Duration::from_secs(10))
}

pub async fn run(client: Client, ca: Arc<CaState>) -> anyhow::Result<()> {
    let node_ip = get_node_ip(&client).await?;

    let cn = "ocp-sim-webhook.ocp-sim.svc";
    let tls_config = sign_tls_config(&ca, &[cn, &node_ip])
        .map_err(|e| anyhow::anyhow!("failed to generate webhook TLS config: {e}"))?;
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));

    if let Err(e) = setup_webhook_with_ip(&client, &ca, &node_ip).await {
        warn!(%e, "failed to register MutatingWebhookConfiguration (will retry on next restart)");
    }

    let ns_client = client.clone();
    tokio::spawn(async move {
        let namespaces: Api<Namespace> = Api::all(ns_client.clone());
        let ctx = Arc::new(ns_client);

        info!("starting namespace UID-range annotation controller");

        Controller::new(namespaces, watcher::Config::default())
            .run(reconcile_namespace, error_policy_ns, ctx)
            .for_each(|res| async move {
                if let Err(e) = res {
                    warn!("namespace uid-range reconcile error: {e:?}");
                }
            })
            .await;
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], WEBHOOK_PORT));
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "pod-mutate webhook server listening (HTTPS)");

    loop {
        let (stream, _) = listener.accept().await?;
        let acceptor = acceptor.clone();

        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("webhook TLS handshake failed: {e}");
                    return;
                }
            };

            let service = service_fn(handle_request);

            if let Err(e) = http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(tls_stream), service)
                .await
            {
                if !e.to_string().contains("connection closed") {
                    warn!("webhook connection error: {e}");
                }
            }
        });
    }
}
