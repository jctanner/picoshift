mod byoidc;
mod handlers;
mod helpers;
mod infra;
mod k8s;
pub(crate) mod types;
mod watcher;

pub use byoidc::ByoidcConfig;
pub use types::UserStore;

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use kube::Client;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

use tokio::sync::RwLock;

use crate::{AuthMode, CaState};

use byoidc::*;
use handlers::*;
use helpers::*;
use types::*;

async fn handle_request(
    client: Client,
    state: Arc<OAuthState>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    use http_body_util::BodyExt;

    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let is_byoidc = matches!(state.auth_mode, AuthMode::Byoidc);

    match (method.clone(), path.as_str()) {
        (Method::GET, "/.well-known/oauth-authorization-server")
        | (Method::GET, "/.well-known/openid-configuration") => {
            return Ok(json_response(
                StatusCode::OK,
                &discovery_json(&state.auth_mode),
            ));
        }
        (Method::GET, "/oauth/jwks") => {
            return if is_byoidc {
                Ok(handle_byoidc_jwks(&state).await)
            } else {
                Ok(handle_jwks(&state))
            };
        }
        (Method::GET, "/oauth/authorize") => {
            if is_byoidc && !is_cli_auth_request(&req) {
                return Ok(handle_byoidc_authorize(&state, &req).await);
            } else {
                return Ok(handle_authorize_get(&client, &state, &req).await);
            }
        }
        (Method::GET, "/oauth/userinfo") => {
            return if is_byoidc {
                Ok(handle_byoidc_userinfo(&state, &req).await)
            } else {
                Ok(handle_userinfo(&state, &req).await)
            };
        }
        _ => {}
    }

    let query = req.uri().query().map(|q| q.to_string());
    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let authorization = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body = req.collect().await?.to_bytes();

    match (method.clone(), path.as_str()) {
        (Method::POST, "/oauth/authorize") => {
            Ok(handle_authorize_post(&client, &state, &body).await)
        }
        (Method::POST, "/oauth/token") => {
            if is_byoidc {
                Ok(handle_byoidc_token(&client, &state, &body).await)
            } else {
                Ok(handle_token(&client, &state, &body, authorization.as_deref()).await)
            }
        }
        _ if is_byoidc
            && (path.contains("/oauth2/")
                || path.contains("/v2.0/")
                || path.starts_with("/oidc/")
                || path.starts_with("/admin")) =>
        {
            Ok(handle_byoidc_proxy(
                &state,
                &method,
                &path,
                query.as_deref(),
                &body,
                content_type.as_deref(),
                authorization.as_deref(),
            )
            .await)
        }
        _ => Ok(text_response(StatusCode::NOT_FOUND, "not found\n")),
    }
}

pub async fn run(
    client: Client,
    ca: Arc<CaState>,
    user_store: Arc<RwLock<UserStore>>,
    auth_mode: AuthMode,
    byoidc_config: Option<ByoidcConfig>,
) -> anyhow::Result<()> {
    let tls_config = infra::generate_tls_config(&ca)
        .map_err(|e| anyhow::anyhow!("failed to generate TLS config: {e}"))?;
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));

    if let Err(e) = infra::setup_infrastructure(&client, &auth_mode).await {
        warn!(%e, "failed to set up OAuth infrastructure (will retry on next restart)");
    }

    let mode_label = match &auth_mode {
        AuthMode::Legacy => "legacy (sha256~)",
        AuthMode::Oidc => "oidc (JWT)",
        AuthMode::Byoidc => "byoidc (external OIDC)",
    };
    info!(mode = mode_label, "OAuth auth mode");

    let state = Arc::new(OAuthState::new(user_store.clone(), auth_mode, byoidc_config).await);

    tokio::spawn(watcher::watch_htpasswd_secret(client.clone(), user_store));

    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                interval.tick().await;
                let code_ttl = std::time::Duration::from_secs(300);
                let token_ttl = std::time::Duration::from_secs(86400);
                let codes_removed = {
                    let mut codes = state.codes.write().await;
                    let before = codes.len();
                    codes.retain(|_, v| v.created.elapsed() < code_ttl);
                    before - codes.len()
                };
                let tokens_removed = {
                    let mut tokens = state.tokens.write().await;
                    let before = tokens.len();
                    tokens.retain(|_, v| v.created.elapsed() < token_ttl);
                    before - tokens.len()
                };
                if codes_removed > 0 || tokens_removed > 0 {
                    info!(codes_removed, tokens_removed, "evicted expired oauth entries");
                }
            }
        });
    }

    let addr = SocketAddr::from(([0, 0, 0, 0], OAUTH_PORT));
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "mock OAuth server listening (HTTPS)");

    loop {
        let (stream, _) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let client = client.clone();
        let state = state.clone();

        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("TLS handshake failed: {e}");
                    return;
                }
            };

            let service = service_fn(move |req| {
                handle_request(client.clone(), state.clone(), req)
            });

            if let Err(e) = http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(tls_stream), service)
                .await
            {
                if !e.to_string().contains("connection closed") {
                    warn!("oauth connection error: {e}");
                }
            }
        });
    }
}
