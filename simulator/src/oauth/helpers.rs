use std::collections::HashMap;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};
use rand::Rng;

use crate::AuthMode;

use super::types::{DOMAIN, OAUTH_HOST, OAUTH_PORT};

pub(crate) fn generate_random_string(len: usize) -> String {
    use rand::distributions::Alphanumeric;
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

pub(crate) fn discovery_json(auth_mode: &AuthMode) -> String {
    if matches!(auth_mode, AuthMode::Byoidc) {
        let local = format!("https://localhost:{OAUTH_PORT}");
        return serde_json::json!({
            "issuer": local,
            "jwks_uri": format!("{local}/oauth/jwks"),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code"],
            "id_token_signing_alg_values_supported": ["RS256"],
            "subject_types_supported": ["public"]
        }).to_string();
    }
    let base = format!("https://{OAUTH_HOST}.{DOMAIN}");
    let mut doc = serde_json::json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "scopes_supported": ["user:check-access", "user:full", "user:info", "user:list-projects"],
        "response_types_supported": ["code", "token"],
        "grant_types_supported": ["authorization_code", "implicit"],
        "code_challenge_methods_supported": ["plain", "S256"]
    });
    if matches!(auth_mode, AuthMode::Oidc) {
        let obj = doc.as_object_mut().unwrap();
        obj.insert("userinfo_endpoint".into(), format!("{base}/oauth/userinfo").into());
        obj.insert("jwks_uri".into(), format!("{base}/oauth/jwks").into());
        obj.insert("id_token_signing_alg_values_supported".into(), serde_json::json!(["RS256"]));
        obj.insert("subject_types_supported".into(), serde_json::json!(["public"]));
        obj.insert("token_endpoint_auth_methods_supported".into(), serde_json::json!(["client_secret_post", "client_secret_basic"]));
    }
    doc.to_string()
}

pub(crate) fn json_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

pub(crate) fn text_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

pub(crate) fn html_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

pub(crate) fn parse_query(uri: &hyper::Uri) -> HashMap<String, String> {
    uri.query()
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn parse_form_body(body: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(body)
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub(crate) fn login_form_html(
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    response_type: &str,
    error: bool,
) -> String {
    let error_block = if error {
        r#"<div style="background:#c9190b;color:#fff;padding:8px 12px;border-radius:4px;margin-bottom:16px;font-size:14px">Invalid username or password. Please try again.</div>"#
    } else {
        ""
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Log in to ocp-sim</title>
<style>
  body {{ margin:0; font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif;
         background:#151515; color:#e0e0e0; display:flex; align-items:center; justify-content:center; min-height:100vh; }}
  .card {{ background:#1e1e1e; border:1px solid #333; border-radius:8px; padding:32px; width:340px; }}
  h1 {{ font-size:20px; margin:0 0 24px; text-align:center; }}
  label {{ display:block; font-size:13px; margin-bottom:4px; }}
  input[type=text], input[type=password] {{ width:100%; padding:8px; margin-bottom:16px;
         border:1px solid #555; border-radius:4px; background:#2a2a2a; color:#e0e0e0;
         font-size:14px; box-sizing:border-box; }}
  button {{ width:100%; padding:10px; background:#0066cc; color:#fff; border:none;
           border-radius:4px; font-size:14px; cursor:pointer; }}
  button:hover {{ background:#004c99; }}
</style>
</head>
<body>
<div class="card">
  <h1>Log in to ocp-sim</h1>
  {error_block}
  <form method="POST" action="/oauth/authorize">
    <input type="hidden" name="client_id" value="{client_id_escaped}">
    <input type="hidden" name="redirect_uri" value="{redirect_uri_escaped}">
    <input type="hidden" name="state" value="{state_escaped}">
    <input type="hidden" name="response_type" value="{response_type_escaped}">
    <label for="username">Username</label>
    <input type="text" id="username" name="username" autocomplete="username" autofocus>
    <label for="password">Password</label>
    <input type="password" id="password" name="password" autocomplete="current-password">
    <button type="submit">Log in</button>
  </form>
</div>
</body>
</html>"#,
        error_block = error_block,
        client_id_escaped = html_escape(client_id),
        redirect_uri_escaped = html_escape(redirect_uri),
        state_escaped = html_escape(state),
        response_type_escaped = html_escape(response_type),
    )
}
