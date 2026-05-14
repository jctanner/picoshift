use std::collections::HashMap;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use tokio::sync::RwLock;
use tracing::{info, warn};
use url::Url;

use super::helpers::*;
use super::k8s::ensure_user_and_identity;
use super::types::{OAuthState, DOMAIN};

#[derive(Clone, Debug)]
pub struct ByoidcConfig {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ExternalOidcDiscovery {
    pub(crate) authorization_endpoint: String,
    pub(crate) token_endpoint: String,
    #[allow(dead_code)]
    pub(crate) userinfo_endpoint: String,
    pub(crate) jwks_uri: String,
}

pub(crate) struct ExternalJwksCache {
    pub(crate) keys: RwLock<HashMap<String, jsonwebtoken::DecodingKey>>,
    pub(crate) discovery: RwLock<ExternalOidcDiscovery>,
}

impl ExternalJwksCache {
    pub(crate) async fn new(issuer_url: &str) -> Self {
        let cache = Self {
            keys: RwLock::new(HashMap::new()),
            discovery: RwLock::new(ExternalOidcDiscovery::default()),
        };
        cache.refresh(issuer_url).await;
        cache
    }

    pub(crate) async fn refresh(&self, issuer_url: &str) {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();

        let discovery_url = format!("{}/.well-known/openid-configuration", issuer_url.trim_end_matches('/'));
        let resp = match client.get(&discovery_url).send().await {
            Ok(r) => r,
            Err(e) => { warn!(%e, "BYOIDC: discovery fetch failed"); return; }
        };
        let doc: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => { warn!(%e, "BYOIDC: discovery parse failed"); return; }
        };

        let disc = ExternalOidcDiscovery {
            authorization_endpoint: doc.get("authorization_endpoint").and_then(|v| v.as_str()).unwrap_or_default().into(),
            token_endpoint: doc.get("token_endpoint").and_then(|v| v.as_str()).unwrap_or_default().into(),
            userinfo_endpoint: doc.get("userinfo_endpoint").and_then(|v| v.as_str()).unwrap_or_default().into(),
            jwks_uri: doc.get("jwks_uri").and_then(|v| v.as_str()).unwrap_or_default().into(),
        };

        let jwks_uri = disc.jwks_uri.clone();
        *self.discovery.write().await = disc;

        if jwks_uri.is_empty() {
            warn!("BYOIDC: no jwks_uri in discovery");
            return;
        }

        let resp = match client.get(&jwks_uri).send().await {
            Ok(r) => r,
            Err(e) => { warn!(%e, "BYOIDC: JWKS fetch failed"); return; }
        };
        let jwks: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => { warn!(%e, "BYOIDC: JWKS parse failed"); return; }
        };

        let mut new_keys = HashMap::new();
        if let Some(keys) = jwks.get("keys").and_then(|k| k.as_array()) {
            for key in keys {
                let kty = key.get("kty").and_then(|v| v.as_str()).unwrap_or_default();
                let kid = key.get("kid").and_then(|v| v.as_str()).unwrap_or_default();
                if kty != "RSA" { continue; }
                let n = key.get("n").and_then(|v| v.as_str()).unwrap_or_default();
                let e = key.get("e").and_then(|v| v.as_str()).unwrap_or_default();
                match jsonwebtoken::DecodingKey::from_rsa_components(n, e) {
                    Ok(dk) => { new_keys.insert(kid.to_string(), dk); }
                    Err(e) => { warn!(%e, kid, "BYOIDC: failed to parse RSA key"); }
                }
            }
        }

        info!(count = new_keys.len(), "BYOIDC: loaded JWKS keys");
        *self.keys.write().await = new_keys;
    }

    pub(crate) async fn validate_token(&self, token: &str) -> Option<serde_json::Value> {
        use base64::Engine;
        let header_part = token.split('.').next()?;
        let header_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(header_part).ok()?;
        let header: serde_json::Value = serde_json::from_slice(&header_bytes).ok()?;
        let kid = header.get("kid").and_then(|v| v.as_str()).unwrap_or_default();

        let keys = self.keys.read().await;
        let dk = keys.get(kid)?;

        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.validate_aud = false;
        match jsonwebtoken::decode::<serde_json::Value>(token, dk, &validation) {
            Ok(data) => Some(data.claims),
            Err(e) => { warn!(%e, "BYOIDC: JWT validation failed"); None }
        }
    }
}

pub(crate) async fn handle_byoidc_authorize(
    state: &OAuthState,
    req: &Request<Incoming>,
) -> Response<Full<Bytes>> {
    let byoidc = match &state.byoidc {
        Some(c) => c,
        None => return text_response(StatusCode::INTERNAL_SERVER_ERROR, "BYOIDC not configured\n"),
    };
    let external_jwks = match &state.external_jwks {
        Some(c) => c,
        None => return text_response(StatusCode::INTERNAL_SERVER_ERROR, "BYOIDC JWKS not loaded\n"),
    };

    let disc = external_jwks.discovery.read().await;
    if disc.authorization_endpoint.is_empty() {
        return text_response(StatusCode::BAD_GATEWAY, "external authorization_endpoint not discovered\n");
    }

    let params = parse_query(req.uri());
    let redirect_uri = params.get("redirect_uri").cloned().unwrap_or_default();
    let req_state = params.get("state").cloned().unwrap_or_default();

    let ext_url = match Url::parse(&disc.authorization_endpoint) {
        Ok(u) => u,
        Err(_) => return text_response(StatusCode::BAD_GATEWAY, "invalid external authorization_endpoint\n"),
    };
    let base = format!("https://entra.{DOMAIN}");
    let mut auth_url = Url::parse(&format!("{}{}", base, ext_url.path())).unwrap();

    auth_url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &byoidc.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", "openid email profile api://picoshift/.default")
        .append_pair("state", &req_state);

    info!(redirect = %auth_url, "BYOIDC: redirecting to external provider (proxied)");

    Response::builder()
        .status(StatusCode::FOUND)
        .header("location", auth_url.as_str())
        .body(Full::new(Bytes::new()))
        .unwrap()
}

pub(crate) async fn handle_byoidc_token(
    client_k8s: &kube::Client,
    state: &OAuthState,
    body: &[u8],
) -> Response<Full<Bytes>> {
    let byoidc = match &state.byoidc {
        Some(c) => c,
        None => return json_response(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"byoidc_not_configured"}"#),
    };
    let external_jwks = match &state.external_jwks {
        Some(c) => c,
        None => return json_response(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"byoidc_jwks_not_loaded"}"#),
    };

    let disc = external_jwks.discovery.read().await;
    if disc.token_endpoint.is_empty() {
        return json_response(StatusCode::BAD_GATEWAY, r#"{"error":"no_external_token_endpoint"}"#);
    }

    let params = parse_form_body(body);
    let code = params.get("code").cloned().unwrap_or_default();
    let redirect_uri = params.get("redirect_uri").cloned().unwrap_or_default();

    let http_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let resp = match http_client
        .post(&disc.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("client_id", &byoidc.client_id),
            ("client_secret", &byoidc.client_secret),
            ("redirect_uri", &redirect_uri),
        ])
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(%e, "BYOIDC: token exchange failed");
            return json_response(StatusCode::BAD_GATEWAY, r#"{"error":"token_exchange_failed"}"#);
        }
    };

    let token_resp: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            warn!(%e, "BYOIDC: token response parse failed");
            return json_response(StatusCode::BAD_GATEWAY, r#"{"error":"token_parse_failed"}"#);
        }
    };

    if let Some(access_token) = token_resp.get("access_token").and_then(|v| v.as_str()) {
        if let Some(claims) = external_jwks.validate_token(access_token).await {
            let username = claims.get("preferred_username")
                .or_else(|| claims.get("email"))
                .or_else(|| claims.get("sub"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            ensure_user_and_identity(client_k8s, username).await;
            info!(username, "BYOIDC: token exchange successful");
        } else if let Some(id_token) = token_resp.get("id_token").and_then(|v| v.as_str()) {
            if let Some(claims) = external_jwks.validate_token(id_token).await {
                let username = claims.get("preferred_username")
                    .or_else(|| claims.get("email"))
                    .or_else(|| claims.get("sub"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                ensure_user_and_identity(client_k8s, username).await;
                info!(username, "BYOIDC: token exchange successful (via id_token)");
            }
        }
    }

    json_response(StatusCode::OK, &token_resp.to_string())
}

pub(crate) async fn handle_byoidc_userinfo(
    state: &OAuthState,
    req: &Request<Incoming>,
) -> Response<Full<Bytes>> {
    let external_jwks = match &state.external_jwks {
        Some(c) => c,
        None => return json_response(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"byoidc_not_configured"}"#),
    };

    let auth = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();

    let token = if let Some(t) = auth.strip_prefix("Bearer ") {
        t
    } else {
        return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"missing bearer token"}"#);
    };

    match external_jwks.validate_token(token).await {
        Some(claims) => {
            let username = claims.get("preferred_username")
                .or_else(|| claims.get("email"))
                .or_else(|| claims.get("sub"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let email = claims.get("email")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let groups: Vec<String> = claims.get("groups")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_else(|| vec!["system:authenticated".into()]);

            json_response(StatusCode::OK, &serde_json::json!({
                "sub": username,
                "name": username,
                "preferred_username": username,
                "email": email,
                "groups": groups,
            }).to_string())
        }
        None => json_response(StatusCode::UNAUTHORIZED, r#"{"error":"invalid_token"}"#),
    }
}

pub(crate) async fn handle_byoidc_jwks(state: &OAuthState) -> Response<Full<Bytes>> {
    let external_jwks = match &state.external_jwks {
        Some(c) => c,
        None => return json_response(StatusCode::OK, r#"{"keys":[]}"#),
    };
    let _byoidc = match &state.byoidc {
        Some(c) => c,
        None => return json_response(StatusCode::OK, r#"{"keys":[]}"#),
    };

    let http_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let disc = external_jwks.discovery.read().await;
    if disc.jwks_uri.is_empty() {
        return json_response(StatusCode::OK, r#"{"keys":[]}"#);
    }

    match http_client.get(&disc.jwks_uri).send().await {
        Ok(resp) => match resp.text().await {
            Ok(body) => json_response(StatusCode::OK, &body),
            Err(_) => json_response(StatusCode::OK, r#"{"keys":[]}"#),
        },
        Err(e) => {
            warn!(%e, url = %disc.jwks_uri, "BYOIDC: failed to fetch external JWKS");
            json_response(StatusCode::OK, r#"{"keys":[]}"#)
        }
    }
}

pub(crate) async fn handle_byoidc_proxy(
    state: &OAuthState,
    method: &Method,
    path: &str,
    query: Option<&str>,
    body: &[u8],
    content_type: Option<&str>,
    authorization: Option<&str>,
) -> Response<Full<Bytes>> {
    let byoidc = match &state.byoidc {
        Some(c) => c,
        None => return text_response(StatusCode::BAD_GATEWAY, "BYOIDC not configured\n"),
    };

    let issuer_parsed = match Url::parse(&byoidc.issuer_url) {
        Ok(u) => u,
        Err(_) => return text_response(StatusCode::BAD_GATEWAY, "invalid issuer URL\n"),
    };
    let base = format!("{}://{}", issuer_parsed.scheme(), issuer_parsed.host_str().unwrap_or("localhost"));
    let port = issuer_parsed.port().map(|p| format!(":{}", p)).unwrap_or_default();
    let mut target_url = format!("{}{}{}", base, port, path);
    if let Some(q) = query {
        target_url.push('?');
        target_url.push_str(q);
    }

    info!(%target_url, %method, "BYOIDC: proxying to external provider");

    let http_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let mut req_builder = match *method {
        Method::POST => {
            let mut b = http_client.post(&target_url).body(body.to_vec());
            if let Some(ct) = content_type {
                b = b.header("content-type", ct);
            }
            b
        }
        _ => http_client.get(&target_url),
    };
    if let Some(auth) = authorization {
        req_builder = req_builder.header("authorization", auth);
    }

    let resp = match req_builder.send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(%e, "BYOIDC: proxy request failed");
            return text_response(StatusCode::BAD_GATEWAY, "proxy request failed\n");
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);

    for name in &["location", "content-type", "www-authenticate", "cache-control"] {
        if let Some(val) = resp.headers().get(*name) {
            builder = builder.header(*name, val);
        }
    }
    for val in resp.headers().get_all("set-cookie").iter() {
        builder = builder.header("set-cookie", val);
    }

    let resp_body = resp.bytes().await.unwrap_or_default();
    builder.body(Full::new(resp_body)).unwrap()
}
