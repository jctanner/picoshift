use std::sync::Arc;
use std::collections::BTreeMap;

use futures::StreamExt;
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

    let secrets: Api<Secret> = Api::namespaced(client.clone(), &ns);
    if secrets.get_opt(&secret_name).await?.is_some() {
        return Ok(Action::await_change());
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
