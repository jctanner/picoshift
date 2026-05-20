mod gateway;
mod imagestream;
mod jobset;
mod loadbalancer;
mod oauth;
mod pod_mutate;
mod project;
mod proxy;
mod route;
mod service_ca;
pub mod util;

use std::sync::Arc;

use clap::Parser;
use rcgen::{CertificateParams, KeyPair};
use tracing::{info, warn, error};

#[derive(Clone, Debug, Default, clap::ValueEnum)]
pub enum AuthMode {
    #[default]
    Legacy,
    Oidc,
    Byoidc,
}

#[derive(Parser)]
#[command(name = "ocp-sim", about = "OCP Lite Simulator")]
struct Args {
    #[arg(long, default_value_t = false, help = "Enable reverse proxy for Route traffic")]
    proxy: bool,

    #[arg(long, default_value_t = 80, help = "Port for the reverse proxy")]
    proxy_port: u16,

    #[arg(long, help = "Path to users.yaml for OAuth user definitions")]
    users_file: Option<String>,

    #[arg(long, value_enum, default_value = "legacy", help = "Auth mode: legacy (sha256~ tokens), oidc (JWT tokens), or byoidc (external OIDC)")]
    auth_mode: AuthMode,

    #[arg(long, help = "External OIDC issuer URL (used with --auth-mode=byoidc)")]
    oidc_issuer_url: Option<String>,

    #[arg(long, help = "External OIDC client ID (used with --auth-mode=byoidc)")]
    oidc_client_id: Option<String>,

    #[arg(long, help = "External OIDC client secret (used with --auth-mode=byoidc)")]
    oidc_client_secret: Option<String>,
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
    let builtin_gw = std::env::var("ENABLE_BUILTIN_GATEWAY")
        .map(|v| !matches!(v.as_str(), "0" | "false" | "no"))
        .unwrap_or(true);
    let gw_handle = if builtin_gw {
        info!("built-in gateway controller enabled");
        tokio::spawn(gateway::run(client.clone()))
    } else {
        warn!("built-in gateway controller disabled (ENABLE_BUILTIN_GATEWAY=false)");
        tokio::spawn(std::future::pending())
    };
    let user_store = Arc::new(oauth::UserStore::load(args.users_file.as_deref()));
    let auth_mode = args.auth_mode.clone();
    let byoidc_config = if matches!(auth_mode, AuthMode::Byoidc) {
        Some(oauth::ByoidcConfig {
            issuer_url: args.oidc_issuer_url.clone().expect("--oidc-issuer-url required for byoidc mode"),
            client_id: args.oidc_client_id.clone().expect("--oidc-client-id required for byoidc mode"),
            client_secret: args.oidc_client_secret.clone().unwrap_or_default(),
        })
    } else {
        None
    };
    let oauth_handle = tokio::spawn(oauth::run(client.clone(), ca.clone(), user_store, auth_mode, byoidc_config));
    let proj_handle = tokio::spawn(project::run(client.clone()));
    let scc_handle = tokio::spawn(pod_mutate::run(client.clone(), ca.clone()));
    let jobset_handle = tokio::spawn(jobset::run(client.clone()));
    let is_handle = tokio::spawn(imagestream::run(client.clone()));
    let lb_handle = tokio::spawn(loadbalancer::run(client.clone()));

    let proxy_handle = if args.proxy {
        info!(port = args.proxy_port, "starting reverse proxy");
        tokio::spawn(proxy::run(client.clone(), args.proxy_port, ca.clone()))
    } else {
        tokio::spawn(std::future::pending())
    };

    tokio::select! {
        res = route_handle => if let Ok(Err(e)) = res { error!(%e, "route controller failed"); },
        res = svc_handle => if let Ok(Err(e)) = res { error!(%e, "service-ca controller failed"); },
        res = cm_handle => if let Ok(Err(e)) = res { error!(%e, "configmap ca-inject controller failed"); },
        res = mwc_handle => if let Ok(Err(e)) = res { error!(%e, "mutating-webhook ca-inject controller failed"); },
        res = vwc_handle => if let Ok(Err(e)) = res { error!(%e, "validating-webhook ca-inject controller failed"); },
        res = gw_handle => if let Ok(Err(e)) = res { error!(%e, "gateway controller failed"); },
        res = oauth_handle => if let Ok(Err(e)) = res { error!(%e, "oauth server failed"); },
        res = proj_handle => if let Ok(Err(e)) = res { error!(%e, "project controller failed"); },
        res = scc_handle => if let Ok(Err(e)) = res { error!(%e, "scc webhook failed"); },
        res = jobset_handle => if let Ok(Err(e)) = res { error!(%e, "jobset controller failed"); },
        res = is_handle => if let Ok(Err(e)) = res { error!(%e, "imagestream controller failed"); },
        res = lb_handle => if let Ok(Err(e)) = res { error!(%e, "loadbalancer controller failed"); },
        res = proxy_handle => if let Ok(Err(e)) = res { error!(%e, "proxy failed"); },
    }

    Ok(())
}
