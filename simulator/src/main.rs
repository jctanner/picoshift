mod gateway;
mod oauth;
mod project;
mod proxy;
mod route;
mod service_ca;

use std::sync::Arc;

use clap::Parser;
use rcgen::{CertificateParams, KeyPair};
use tracing::{info, error};

#[derive(Parser)]
#[command(name = "ocp-sim", about = "OCP Lite Simulator")]
struct Args {
    #[arg(long, default_value_t = false, help = "Enable reverse proxy for Route traffic")]
    proxy: bool,

    #[arg(long, default_value_t = 80, help = "Port for the reverse proxy")]
    proxy_port: u16,
}

pub struct CaState {
    pub ca_cert_pem: String,
    pub ca_key_pem: String,
}

impl CaState {
    fn generate() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            rcgen::DnValue::Utf8String("ocp-sim-service-ca".into()),
        );

        let key_pair = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();

        CaState {
            ca_cert_pem: cert.pem(),
            ca_key_pem: key_pair.serialize_pem(),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ocp_sim=info,kube=warn".into()),
        )
        .init();

    info!("ocp-sim simulator starting");

    let ca = Arc::new(CaState::generate());
    info!("generated self-signed CA");

    let client = kube::Client::try_default().await?;
    info!("connected to cluster");

    let route_handle = tokio::spawn(route::run(client.clone()));
    let svc_handle = tokio::spawn(service_ca::run_service_controller(client.clone(), ca.clone()));
    let cm_handle = tokio::spawn(service_ca::run_configmap_controller(client.clone(), ca.clone()));
    let mwc_handle = tokio::spawn(service_ca::run_mutating_webhook_controller(client.clone(), ca.clone()));
    let vwc_handle = tokio::spawn(service_ca::run_validating_webhook_controller(client.clone(), ca.clone()));
    let gw_handle = tokio::spawn(gateway::run(client.clone()));
    let oauth_handle = tokio::spawn(oauth::run(client.clone(), ca.clone()));
    let proj_handle = tokio::spawn(project::run(client.clone()));

    if args.proxy {
        info!(port = args.proxy_port, "starting reverse proxy");
        let proxy_handle = tokio::spawn(proxy::run(client.clone(), args.proxy_port, ca.clone()));

        tokio::select! {
            res = route_handle => if let Ok(Err(e)) = res { error!(%e, "route controller failed"); },
            res = svc_handle => if let Ok(Err(e)) = res { error!(%e, "service-ca controller failed"); },
            res = cm_handle => if let Ok(Err(e)) = res { error!(%e, "configmap ca-inject controller failed"); },
            res = mwc_handle => if let Ok(Err(e)) = res { error!(%e, "mutating-webhook ca-inject controller failed"); },
            res = vwc_handle => if let Ok(Err(e)) = res { error!(%e, "validating-webhook ca-inject controller failed"); },
            res = gw_handle => if let Ok(Err(e)) = res { error!(%e, "gateway controller failed"); },
            res = oauth_handle => if let Ok(Err(e)) = res { error!(%e, "oauth server failed"); },
            res = proj_handle => if let Ok(Err(e)) = res { error!(%e, "project controller failed"); },
            res = proxy_handle => if let Ok(Err(e)) = res { error!(%e, "proxy failed"); },
        }
    } else {
        tokio::select! {
            res = route_handle => if let Ok(Err(e)) = res { error!(%e, "route controller failed"); },
            res = svc_handle => if let Ok(Err(e)) = res { error!(%e, "service-ca controller failed"); },
            res = cm_handle => if let Ok(Err(e)) = res { error!(%e, "configmap ca-inject controller failed"); },
            res = mwc_handle => if let Ok(Err(e)) = res { error!(%e, "mutating-webhook ca-inject controller failed"); },
            res = vwc_handle => if let Ok(Err(e)) = res { error!(%e, "validating-webhook ca-inject controller failed"); },
            res = gw_handle => if let Ok(Err(e)) = res { error!(%e, "gateway controller failed"); },
            res = oauth_handle => if let Ok(Err(e)) = res { error!(%e, "oauth server failed"); },
            res = proj_handle => if let Ok(Err(e)) = res { error!(%e, "project controller failed"); },
        }
    }

    Ok(())
}
