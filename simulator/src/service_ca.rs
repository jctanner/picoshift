use std::sync::Arc;
use std::collections::BTreeMap;

use base64::Engine;
use futures::StreamExt;
use k8s_openapi::api::admissionregistration::v1::{
    MutatingWebhookConfiguration, ValidatingWebhookConfiguration,
};
use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use k8s_openapi::ByteString;
use kube::api::{Api, ObjectMeta, Patch, PatchParams, PostParams};
use kube::runtime::controller::Action;
use kube::runtime::Controller;
use kube::runtime::watcher;
use kube::{Client, ResourceExt};
use rcgen::{CertificateParams, Issuer, KeyPair};
use tracing::{info, warn};

use crate::CaState;

const SERVING_CERT_ANNOTATION: &str = "service.beta.openshift.io/serving-cert-secret-name";
const INJECT_CABUNDLE_ANNOTATION: &str = "service.beta.openshift.io/inject-cabundle";

fn generate_serving_cert(
    ca: &CaState,
    cn: &str,
) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    let ca_key = KeyPair::from_pem(&ca.ca_key_pem)?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(
        rcgen::DnType::CommonName,
        rcgen::DnValue::Utf8String("ocp-sim-service-ca".into()),
    );
    let issuer = Issuer::from_params(&ca_params, &ca_key);

    let mut params = CertificateParams::new(vec![cn.to_string()])?;
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        rcgen::DnValue::Utf8String(cn.into()),
    );

    let key_pair = KeyPair::generate()?;
    let cert = params.signed_by(&key_pair, &issuer)?;

    Ok((cert.pem(), key_pair.serialize_pem()))
}

async fn reconcile_service(
    svc: Arc<Service>,
    ctx: Arc<(Client, Arc<CaState>)>,
) -> Result<Action, kube::Error> {
    let (client, ca) = ctx.as_ref();

    let annotations = svc.metadata.annotations.as_ref();
    let secret_name = match annotations.and_then(|a| a.get(SERVING_CERT_ANNOTATION)) {
        Some(name) => name.clone(),
        None => return Ok(Action::await_change()),
    };

    let ns = svc.namespace().unwrap_or_default();
    let svc_name = svc.name_any();

    let ca_fingerprint = ca_bundle_base64(ca);
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &ns);
    if let Some(existing) = secrets.get_opt(&secret_name).await? {
        let existing_fp = existing
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("ocp-sim/ca-fingerprint"));
        if existing_fp == Some(&ca_fingerprint) {
            return Ok(Action::await_change());
        }
        secrets.delete(&secret_name, &Default::default()).await?;
        info!(ns, svc_name, secret_name, "deleted stale TLS secret (CA changed)");
    }

    let cn = format!("{svc_name}.{ns}.svc");
    let (cert_pem, key_pem) = match generate_serving_cert(ca, &cn) {
        Ok(pair) => pair,
        Err(e) => {
            warn!(%e, ns, svc_name, "failed to generate serving cert");
            return Ok(Action::requeue(std::time::Duration::from_secs(30)));
        }
    };

    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(secret_name.clone()),
            namespace: Some(ns.clone()),
            annotations: Some(BTreeMap::from([
                ("ocp-sim/ca-fingerprint".to_string(), ca_fingerprint),
            ])),
            owner_references: Some(vec![OwnerReference {
                api_version: "v1".to_string(),
                kind: "Service".to_string(),
                name: svc_name.clone(),
                uid: svc.metadata.uid.clone().unwrap_or_default(),
                controller: Some(true),
                block_owner_deletion: Some(true),
            }]),
            ..Default::default()
        },
        type_: Some("kubernetes.io/tls".to_string()),
        data: Some(BTreeMap::from([
            ("tls.crt".to_string(), ByteString(cert_pem.into_bytes())),
            ("tls.key".to_string(), ByteString(key_pem.into_bytes())),
        ])),
        ..Default::default()
    };

    secrets.create(&PostParams::default(), &secret).await?;
    info!(ns, svc_name, secret_name, "created TLS secret");

    Ok(Action::await_change())
}

pub async fn run_service_controller(
    client: Client,
    ca: Arc<CaState>,
) -> Result<(), kube::Error> {
    let services: Api<Service> = Api::all(client.clone());
    let ctx = Arc::new((client, ca));

    info!("starting service-ca controller");

    Controller::new(services, watcher::Config::default())
        .run(reconcile_service, error_policy_svc, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!("service reconcile error: {e:?}");
            }
        })
        .await;

    Ok(())
}

fn error_policy_svc(
    _obj: Arc<Service>,
    _error: &kube::Error,
    _ctx: Arc<(Client, Arc<CaState>)>,
) -> Action {
    Action::requeue(std::time::Duration::from_secs(10))
}

async fn reconcile_configmap(
    cm: Arc<ConfigMap>,
    ctx: Arc<(Client, Arc<CaState>)>,
) -> Result<Action, kube::Error> {
    let (client, ca) = ctx.as_ref();

    let annotations = cm.metadata.annotations.as_ref();
    let should_inject = annotations
        .and_then(|a| a.get(INJECT_CABUNDLE_ANNOTATION))
        .map(|v| v == "true")
        .unwrap_or(false);

    if !should_inject {
        return Ok(Action::await_change());
    }

    let has_ca_key = cm
        .data
        .as_ref()
        .map(|d| d.contains_key("service-ca.crt"))
        .unwrap_or(false);

    if has_ca_key {
        return Ok(Action::await_change());
    }

    let ns = cm.namespace().unwrap_or_default();
    let cm_name = cm.name_any();

    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), &ns);
    let patch = serde_json::json!({
        "data": {
            "service-ca.crt": ca.ca_cert_pem
        }
    });

    cms.patch(
        &cm_name,
        &PatchParams::apply("ocp-sim"),
        &Patch::Merge(&patch),
    )
    .await?;

    info!(ns, cm_name, "injected service-ca.crt into ConfigMap");

    Ok(Action::await_change())
}

pub async fn run_configmap_controller(
    client: Client,
    ca: Arc<CaState>,
) -> Result<(), kube::Error> {
    let configmaps: Api<ConfigMap> = Api::all(client.clone());
    let ctx = Arc::new((client, ca));

    info!("starting configmap ca-inject controller");

    Controller::new(configmaps, watcher::Config::default())
        .run(reconcile_configmap, error_policy_cm, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!("configmap reconcile error: {e:?}");
            }
        })
        .await;

    Ok(())
}

fn error_policy_cm(
    _obj: Arc<ConfigMap>,
    _error: &kube::Error,
    _ctx: Arc<(Client, Arc<CaState>)>,
) -> Action {
    Action::requeue(std::time::Duration::from_secs(10))
}

// --- Webhook CA bundle injection ---

fn ca_bundle_base64(ca: &CaState) -> String {
    base64::engine::general_purpose::STANDARD.encode(ca.ca_cert_pem.as_bytes())
}

fn needs_cabundle_injection(annotations: Option<&BTreeMap<String, String>>) -> bool {
    annotations
        .and_then(|a| a.get(INJECT_CABUNDLE_ANNOTATION))
        .map(|v| v == "true")
        .unwrap_or(false)
}

async fn reconcile_mutating_webhook(
    mwc: Arc<MutatingWebhookConfiguration>,
    ctx: Arc<(Client, Arc<CaState>)>,
) -> Result<Action, kube::Error> {
    let (client, ca) = ctx.as_ref();

    if !needs_cabundle_injection(mwc.metadata.annotations.as_ref()) {
        return Ok(Action::await_change());
    }

    let expected_ca = ca.ca_cert_pem.as_bytes();
    let name = mwc.name_any();

    let existing_bundle = mwc
        .webhooks
        .as_ref()
        .and_then(|whs| whs.first())
        .and_then(|wh| wh.client_config.ca_bundle.as_ref());

    if let Some(b) = existing_bundle {
        if b.0 == expected_ca {
            return Ok(Action::await_change());
        }
    }

    let ca_b64 = ca_bundle_base64(ca);

    let json_patch: Vec<serde_json::Value> = mwc
        .webhooks
        .as_ref()
        .map(|whs| {
            whs.iter()
                .enumerate()
                .map(|(i, _wh)| {
                    serde_json::json!({
                        "op": "add",
                        "path": format!("/webhooks/{i}/clientConfig/caBundle"),
                        "value": ca_b64
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let api: Api<MutatingWebhookConfiguration> = Api::all(client.clone());
    api.patch(&name, &PatchParams::default(), &Patch::Json::<()>(serde_json::from_value(serde_json::Value::Array(json_patch)).unwrap()))
        .await?;

    info!(name, "injected caBundle into MutatingWebhookConfiguration");
    Ok(Action::await_change())
}

fn error_policy_mwc(
    _obj: Arc<MutatingWebhookConfiguration>,
    _error: &kube::Error,
    _ctx: Arc<(Client, Arc<CaState>)>,
) -> Action {
    Action::requeue(std::time::Duration::from_secs(10))
}

pub async fn run_mutating_webhook_controller(
    client: Client,
    ca: Arc<CaState>,
) -> Result<(), kube::Error> {
    let api: Api<MutatingWebhookConfiguration> = Api::all(client.clone());
    let ctx = Arc::new((client, ca));

    info!("starting mutating-webhook ca-inject controller");

    Controller::new(api, watcher::Config::default())
        .run(reconcile_mutating_webhook, error_policy_mwc, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!("mutating webhook reconcile error: {e:?}");
            }
        })
        .await;

    Ok(())
}

async fn reconcile_validating_webhook(
    vwc: Arc<ValidatingWebhookConfiguration>,
    ctx: Arc<(Client, Arc<CaState>)>,
) -> Result<Action, kube::Error> {
    let (client, ca) = ctx.as_ref();

    if !needs_cabundle_injection(vwc.metadata.annotations.as_ref()) {
        return Ok(Action::await_change());
    }

    let expected_ca = ca.ca_cert_pem.as_bytes();
    let name = vwc.name_any();

    let existing_bundle = vwc
        .webhooks
        .as_ref()
        .and_then(|whs| whs.first())
        .and_then(|wh| wh.client_config.ca_bundle.as_ref());

    if let Some(b) = existing_bundle {
        if b.0 == expected_ca {
            return Ok(Action::await_change());
        }
    }

    let ca_b64 = ca_bundle_base64(ca);

    let json_patch: Vec<serde_json::Value> = vwc
        .webhooks
        .as_ref()
        .map(|whs| {
            whs.iter()
                .enumerate()
                .map(|(i, _wh)| {
                    serde_json::json!({
                        "op": "add",
                        "path": format!("/webhooks/{i}/clientConfig/caBundle"),
                        "value": ca_b64
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let api: Api<ValidatingWebhookConfiguration> = Api::all(client.clone());
    api.patch(&name, &PatchParams::default(), &Patch::Json::<()>(serde_json::from_value(serde_json::Value::Array(json_patch)).unwrap()))
        .await?;

    info!(name, "injected caBundle into ValidatingWebhookConfiguration");
    Ok(Action::await_change())
}

fn error_policy_vwc(
    _obj: Arc<ValidatingWebhookConfiguration>,
    _error: &kube::Error,
    _ctx: Arc<(Client, Arc<CaState>)>,
) -> Action {
    Action::requeue(std::time::Duration::from_secs(10))
}

pub async fn run_validating_webhook_controller(
    client: Client,
    ca: Arc<CaState>,
) -> Result<(), kube::Error> {
    let api: Api<ValidatingWebhookConfiguration> = Api::all(client.clone());
    let ctx = Arc::new((client, ca));

    info!("starting validating-webhook ca-inject controller");

    Controller::new(api, watcher::Config::default())
        .run(reconcile_validating_webhook, error_policy_vwc, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!("validating webhook reconcile error: {e:?}");
            }
        })
        .await;

    Ok(())
}
