use std::time::Instant;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use kube::Client;
use tracing::info;
use url::Url;

use super::helpers::*;
use super::k8s::{ensure_user_and_identity, validate_client};
use super::types::*;

pub(crate) async fn issue_auth_code(
    state: &OAuthState,
    username: &str,
    client_id: &str,
    redirect_uri: &str,
    req_state: &str,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
) -> Response<Full<Bytes>> {
    let code = generate_random_string(32);
    state.codes.write().await.insert(
        code.clone(),
        AuthCode {
            username: username.to_string(),
            client_id: client_id.to_string(),
            created: Instant::now(),
            code_challenge,
            code_challenge_method,
        },
    );

    let mut redirect = Url::parse(redirect_uri).unwrap_or_else(|_| {
        Url::parse("http://localhost/error").unwrap()
    });
    redirect.query_pairs_mut().append_pair("code", &code);
    if !req_state.is_empty() {
        redirect.query_pairs_mut().append_pair("state", req_state);
    }

    info!(redirect = %redirect, username, "authorize: issuing code, redirecting");

    Response::builder()
        .status(StatusCode::FOUND)
        .header("location", redirect.as_str())
        .body(Full::new(Bytes::new()))
        .unwrap()
}

pub(crate) async fn handle_authorize_get(
    client: &Client,
    state: &OAuthState,
    req: &Request<Incoming>,
) -> Response<Full<Bytes>> {
    let params = parse_query(req.uri());

    let client_id = match params.get("client_id") {
        Some(id) => id.clone(),
        None => return text_response(StatusCode::BAD_REQUEST, "missing client_id\n"),
    };
    let redirect_uri = match params.get("redirect_uri") {
        Some(uri) => uri.clone(),
        None => return text_response(StatusCode::BAD_REQUEST, "missing redirect_uri\n"),
    };
    let req_state = params.get("state").cloned().unwrap_or_default();
    let response_type = params.get("response_type").cloned().unwrap_or_default();

    let code_challenge = params.get("code_challenge").cloned();
    let code_challenge_method = params.get("code_challenge_method").cloned();

    let (_, redirect_uris) = match validate_client(client, &client_id).await {
        Some(c) => c,
        None => return text_response(StatusCode::BAD_REQUEST, "unknown client_id\n"),
    };

    let uri_matches = redirect_uris
        .iter()
        .any(|u| redirect_uri.starts_with(u));
    if !uri_matches {
        return text_response(StatusCode::BAD_REQUEST, "redirect_uri mismatch\n");
    }

    if let Some(auth) = req.headers().get("authorization").and_then(|h| h.to_str().ok()) {
        if let Some(encoded) = auth.strip_prefix("Basic ") {
            if let Ok(decoded) = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                encoded.trim(),
            ) {
                if let Ok(cred_str) = std::str::from_utf8(&decoded) {
                    if let Some((user, pass)) = cred_str.split_once(':') {
                        if let Some(_entry) = state.user_store.read().await.authenticate(user, pass) {
                            ensure_user_and_identity(client, user).await;
                            return issue_auth_code(state, user, &client_id, &redirect_uri, &req_state, code_challenge.clone(), code_challenge_method.clone()).await;
                        }
                    }
                }
            }
            return Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header("www-authenticate", "Basic realm=\"openshift\"")
                .body(Full::new(Bytes::from("invalid credentials\n")))
                .unwrap();
        }
    }

    if req.headers().contains_key("x-csrf-token") {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("www-authenticate", "Basic realm=\"openshift\"")
            .body(Full::new(Bytes::from("challenge\n")))
            .unwrap();
    }

    let show_error = params.get("error").is_some();
    let html = login_form_html(&client_id, &redirect_uri, &req_state, &response_type, show_error);
    html_response(StatusCode::OK, &html)
}

pub(crate) async fn handle_authorize_post(
    client: &Client,
    state: &OAuthState,
    body: &[u8],
) -> Response<Full<Bytes>> {
    let params = parse_form_body(body);

    let client_id = match params.get("client_id") {
        Some(id) => id.clone(),
        None => return text_response(StatusCode::BAD_REQUEST, "missing client_id\n"),
    };
    let redirect_uri = match params.get("redirect_uri") {
        Some(uri) => uri.clone(),
        None => return text_response(StatusCode::BAD_REQUEST, "missing redirect_uri\n"),
    };
    let req_state = params.get("state").cloned().unwrap_or_default();
    let response_type = params.get("response_type").cloned().unwrap_or_default();
    let username = params.get("username").cloned().unwrap_or_default();
    let password = params.get("password").cloned().unwrap_or_default();

    if username.is_empty()
        || state.user_store.read().await.authenticate(&username, &password).is_none()
    {
        let html = login_form_html(&client_id, &redirect_uri, &req_state, &response_type, true);
        return html_response(StatusCode::OK, &html);
    }

    let (_, redirect_uris) = match validate_client(client, &client_id).await {
        Some(c) => c,
        None => return text_response(StatusCode::BAD_REQUEST, "unknown client_id\n"),
    };
    let uri_matches = redirect_uris
        .iter()
        .any(|u| redirect_uri.starts_with(u));
    if !uri_matches {
        return text_response(StatusCode::BAD_REQUEST, "redirect_uri mismatch\n");
    }

    ensure_user_and_identity(client, &username).await;
    let code_challenge = params.get("code_challenge").cloned();
    let code_challenge_method = params.get("code_challenge_method").cloned();
    issue_auth_code(state, &username, &client_id, &redirect_uri, &req_state, code_challenge, code_challenge_method).await
}

pub(crate) async fn handle_token(
    client: &Client,
    state: &OAuthState,
    body: &[u8],
    authorization: Option<&str>,
) -> Response<Full<Bytes>> {
    let params = parse_form_body(body);

    let grant_type = params.get("grant_type").map(|s| s.as_str()).unwrap_or("");
    if grant_type != "authorization_code" {
        return json_response(
            StatusCode::BAD_REQUEST,
            r#"{"error":"unsupported_grant_type"}"#,
        );
    }

    let code = match params.get("code") {
        Some(c) => c.clone(),
        None => {
            return json_response(StatusCode::BAD_REQUEST, r#"{"error":"invalid_request","error_description":"missing code"}"#)
        }
    };

    let client_id = if let Some(id) = params.get("client_id") {
        id.clone()
    } else if let Some(id) = extract_client_id_from_basic(authorization) {
        id
    } else {
        return json_response(StatusCode::BAD_REQUEST, r#"{"error":"invalid_request","error_description":"missing client_id"}"#);
    };

    let client_secret = params.get("client_secret").cloned().unwrap_or_default();
    let code_verifier = params.get("code_verifier").cloned();

    let auth_code = {
        let mut codes = state.codes.write().await;
        match codes.remove(&code) {
            Some(ac) => ac,
            None => {
                return json_response(StatusCode::BAD_REQUEST, r#"{"error":"invalid_grant"}"#)
            }
        }
    };

    if auth_code.created.elapsed() > CODE_TTL {
        return json_response(StatusCode::BAD_REQUEST, r#"{"error":"invalid_grant","error_description":"code expired"}"#);
    }

    if auth_code.client_id != client_id {
        return json_response(StatusCode::BAD_REQUEST, r#"{"error":"invalid_grant","error_description":"client_id mismatch"}"#);
    }

    if let Some(challenge) = &auth_code.code_challenge {
        match &code_verifier {
            Some(verifier) => {
                let method = auth_code.code_challenge_method.as_deref().unwrap_or("S256");
                if !verify_pkce(verifier, challenge, method) {
                    return json_response(StatusCode::BAD_REQUEST, r#"{"error":"invalid_grant","error_description":"code_verifier mismatch"}"#);
                }
            }
            None => {
                return json_response(StatusCode::BAD_REQUEST, r#"{"error":"invalid_request","error_description":"missing code_verifier"}"#);
            }
        }
    } else {
        let (expected_secret, _) = match validate_client(client, &client_id).await {
            Some(c) => c,
            None => {
                return json_response(StatusCode::BAD_REQUEST, r#"{"error":"invalid_client"}"#)
            }
        };

        if !expected_secret.is_empty() && client_secret != expected_secret {
            return json_response(
                StatusCode::UNAUTHORIZED,
                r#"{"error":"invalid_client","error_description":"bad client_secret"}"#,
            );
        }
    }

    let username = &auth_code.username;
    let token = if let Some(jwt_keys) = &state.jwt_keys {
        let (email, groups) = {
            let store = state.user_store.read().await;
            match store.get(username) {
                Some(entry) => {
                    let email = entry.email.clone().unwrap_or_else(|| format!("{username}@ocp-sim.test"));
                    let groups = entry.groups.clone().unwrap_or_else(|| vec!["system:authenticated".into()]);
                    (email, groups)
                }
                None => (format!("{username}@ocp-sim.test"), vec!["system:authenticated".into()]),
            }
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let issuer = format!("https://{OAUTH_HOST}.{DOMAIN}");
        let claims = serde_json::json!({
            "iss": issuer,
            "sub": username,
            "aud": client_id,
            "preferred_username": username,
            "email": email,
            "groups": groups,
            "iat": now,
            "exp": now + TOKEN_TTL.as_secs(),
        });
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some(jwt_keys.kid.clone());
        match jsonwebtoken::encode(&header, &claims, &jwt_keys.encoding_key) {
            Ok(jwt) => jwt,
            Err(e) => {
                tracing::warn!(%e, "failed to encode JWT");
                return json_response(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"server_error"}"#);
            }
        }
    } else {
        format!("sha256~{}", generate_random_string(43))
    };

    state.tokens.write().await.insert(
        token.clone(),
        TokenInfo {
            username: auth_code.username.clone(),
            created: Instant::now(),
        },
    );

    info!(username = %auth_code.username, jwt = state.jwt_keys.is_some(), "token: issued access_token");

    json_response(
        StatusCode::OK,
        &serde_json::json!({
            "access_token": token,
            "token_type": "Bearer",
            "expires_in": TOKEN_TTL.as_secs(),
        })
        .to_string(),
    )
}

pub(crate) fn decode_jwt_username(token: &str) -> Option<String> {
    use base64::Engine;
    let parts: Vec<&str> = token.splitn(3, '.').collect();
    if parts.len() < 2 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    claims.get("preferred_username")
        .or_else(|| claims.get("sub"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub(crate) fn handle_jwks(state: &OAuthState) -> Response<Full<Bytes>> {
    let keys = if let Some(jwt_keys) = &state.jwt_keys {
        serde_json::json!([{
            "kty": "RSA",
            "use": "sig",
            "kid": jwt_keys.kid,
            "alg": "RS256",
            "n": jwt_keys.n_b64,
            "e": jwt_keys.e_b64,
        }])
    } else {
        serde_json::json!([])
    };
    json_response(StatusCode::OK, &serde_json::json!({"keys": keys}).to_string())
}

pub(crate) async fn handle_userinfo(state: &OAuthState, req: &Request<Incoming>) -> Response<Full<Bytes>> {
    let auth = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();

    let token = if let Some(t) = auth.strip_prefix("Bearer ") {
        t.to_string()
    } else {
        return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"missing bearer token"}"#);
    };

    let username: String = if token.starts_with("eyJ") {
        match decode_jwt_username(&token) {
            Some(u) => u,
            None => return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"invalid_token"}"#),
        }
    } else {
        let tokens = state.tokens.read().await;
        match tokens.get(&token) {
            Some(info) if info.created.elapsed() < TOKEN_TTL => info.username.clone(),
            _ => return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"invalid_token"}"#),
        }
    };

    let username = &username;
    let (email, groups) = {
        let store = state.user_store.read().await;
        match store.get(username) {
            Some(entry) => {
                let email = entry
                    .email
                    .clone()
                    .unwrap_or_else(|| format!("{username}@ocp-sim.test"));
                let groups = entry
                    .groups
                    .clone()
                    .unwrap_or_else(|| vec!["system:authenticated".into()]);
                (email, groups)
            }
            None => (
                format!("{username}@ocp-sim.test"),
                vec!["system:authenticated".into()],
            ),
        }
    };

    json_response(
        StatusCode::OK,
        &serde_json::json!({
            "sub": username,
            "name": username,
            "preferred_username": username,
            "email": email,
            "groups": groups,
        })
        .to_string(),
    )
}

fn verify_pkce(verifier: &str, challenge: &str, method: &str) -> bool {
    use base64::Engine;
    match method {
        "S256" => {
            let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
            let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref());
            computed == challenge
        }
        "plain" => verifier == challenge,
        _ => false,
    }
}

fn extract_client_id_from_basic(authorization: Option<&str>) -> Option<String> {
    let auth = authorization?;
    let encoded = auth.strip_prefix("Basic ")?;
    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        encoded.trim(),
    ).ok()?;
    let cred_str = std::str::from_utf8(&decoded).ok()?;
    let (user, _) = cred_str.split_once(':')?;
    Some(user.to_string())
}

pub(crate) fn is_cli_auth_request(req: &Request<Incoming>) -> bool {
    req.headers().contains_key("authorization") || req.headers().contains_key("x-csrf-token")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jwt(claims: &serde_json::Value) -> String {
        use base64::Engine;
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(claims).unwrap());
        format!("{header}.{payload}.fake-signature")
    }

    #[test]
    fn decode_jwt_preferred_username() {
        let token = make_jwt(&serde_json::json!({
            "sub": "user-id",
            "preferred_username": "alice"
        }));
        assert_eq!(decode_jwt_username(&token).unwrap(), "alice");
    }

    #[test]
    fn decode_jwt_falls_back_to_sub() {
        let token = make_jwt(&serde_json::json!({ "sub": "bob" }));
        assert_eq!(decode_jwt_username(&token).unwrap(), "bob");
    }

    #[test]
    fn decode_jwt_returns_none_for_garbage() {
        assert!(decode_jwt_username("not-a-jwt").is_none());
    }

    #[test]
    fn decode_jwt_returns_none_for_no_username_claims() {
        let token = make_jwt(&serde_json::json!({ "iss": "test" }));
        assert!(decode_jwt_username(&token).is_none());
    }
}
