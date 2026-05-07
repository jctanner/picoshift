use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use k8s_openapi::api::core::v1::{Endpoints, EndpointAddress, EndpointPort, EndpointSubset, Namespace, Service, ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, ApiResource, DynamicObject, Patch, PatchParams, PostParams};
use kube::Client;
use rand::Rng;
use rcgen::{CertificateParams, Issuer, KeyPair};
use rustls::ServerConfig;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};
use url::Url;

use crate::CaState;

const OAUTH_PORT: u16 = 9443;
const DOMAIN: &str = "apps.ocp-sim.localhost";
const OAUTH_HOST: &str = "oauth-openshift";
const OAUTH_NS: &str = "openshift-authentication";
const CODE_TTL: Duration = Duration::from_secs(300);
const TOKEN_TTL: Duration = Duration::from_secs(86400);

struct AuthCode {
    client_id: String,
    _redirect_uri: String,
    created: Instant,
}

struct TokenInfo {
    _client_id: String,
    created: Instant,
}

struct OAuthState {
    codes: RwLock<HashMap<String, AuthCode>>,
    tokens: RwLock<HashMap<String, TokenInfo>>,
}

impl OAuthState {
    fn new() -> Self {
        Self {
            codes: RwLock::new(HashMap::new()),
            tokens: RwLock::new(HashMap::new()),
        }
    }
}

fn generate_random_string(len: usize) -> String {
    use rand::distributions::Alphanumeric;
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

fn discovery_json() -> String {
    let base = format!("https://{OAUTH_HOST}.{DOMAIN}:{OAUTH_PORT}");
    serde_json::json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "scopes_supported": ["user:check-access", "user:full", "user:info", "user:list-projects"],
        "response_types_supported": ["code", "token"],
        "grant_types_supported": ["authorization_code", "implicit"],
        "code_challenge_methods_supported": ["plain", "S256"]
    })
    .to_string()
}

fn json_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

fn text_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

fn parse_query(uri: &hyper::Uri) -> HashMap<String, String> {
    uri.query()
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_form_body(body: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(body)
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn oauth_client_api_resource() -> ApiResource {
    ApiResource {
        group: "oauth.openshift.io".into(),
        version: "v1".into(),
        api_version: "oauth.openshift.io/v1".into(),
        kind: "OAuthClient".into(),
        plural: "oauthclients".into(),
    }
}

async fn validate_client(
    client: &Client,
    client_id: &str,
) -> Option<(String, Vec<String>)> {
    let ar = oauth_client_api_resource();
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
    let oauth_client = api.get(client_id).await.ok()?;

    let secret = oauth_client
        .data
        .get("secret")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();

    let redirect_uris = oauth_client
        .data
        .get("redirectURIs")
        .and_then(|u| u.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    Some((secret, redirect_uris))
}

async fn handle_authorize(
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

    let code = generate_random_string(32);
    state.codes.write().await.insert(
        code.clone(),
        AuthCode {
            client_id,
            _redirect_uri: redirect_uri.clone(),
            created: Instant::now(),
        },
    );

    let mut redirect = Url::parse(&redirect_uri).unwrap_or_else(|_| {
        Url::parse("http://localhost/error").unwrap()
    });
    redirect.query_pairs_mut().append_pair("code", &code);
    if !req_state.is_empty() {
        redirect.query_pairs_mut().append_pair("state", &req_state);
    }

    info!(redirect = %redirect, "authorize: issuing code, redirecting");

    Response::builder()
        .status(StatusCode::FOUND)
        .header("location", redirect.as_str())
        .body(Full::new(Bytes::new()))
        .unwrap()
}

async fn handle_token(
    client: &Client,
    state: &OAuthState,
    body: &[u8],
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
    let client_id = match params.get("client_id") {
        Some(id) => id.clone(),
        None => {
            return json_response(StatusCode::BAD_REQUEST, r#"{"error":"invalid_request","error_description":"missing client_id"}"#)
        }
    };
    let client_secret = params.get("client_secret").cloned().unwrap_or_default();

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

    let token = format!("sha256~{}", generate_random_string(43));
    state.tokens.write().await.insert(
        token.clone(),
        TokenInfo {
            _client_id: client_id,
            created: Instant::now(),
        },
    );

    info!("token: issued access_token");

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

async fn handle_userinfo(state: &OAuthState, req: &Request<Incoming>) -> Response<Full<Bytes>> {
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

    let tokens = state.tokens.read().await;
    match tokens.get(&token) {
        Some(info) if info.created.elapsed() < TOKEN_TTL => {}
        _ => {
            return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"invalid_token"}"#);
        }
    }

    json_response(
        StatusCode::OK,
        &serde_json::json!({
            "sub": "admin",
            "name": "admin",
            "preferred_username": "admin",
            "email": "admin@ocp-sim.localhost"
        })
        .to_string(),
    )
}

async fn handle_request(
    client: Client,
    state: Arc<OAuthState>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    match (method, path.as_str()) {
        (Method::GET, "/.well-known/oauth-authorization-server") => {
            Ok(json_response(StatusCode::OK, &discovery_json()))
        }
        (Method::GET, "/oauth/authorize") => {
            Ok(handle_authorize(&client, &state, &req).await)
        }
        (Method::POST, "/oauth/token") => {
            use http_body_util::BodyExt;
            let body = req.collect().await?.to_bytes();
            Ok(handle_token(&client, &state, &body).await)
        }
        (Method::GET, "/oauth/userinfo") => {
            Ok(handle_userinfo(&state, &req).await)
        }
        _ => Ok(text_response(StatusCode::NOT_FOUND, "not found\n")),
    }
}

fn generate_tls_config(ca: &CaState) -> Result<ServerConfig, Box<dyn std::error::Error + Send + Sync>> {
    let cn = format!("{OAUTH_HOST}.{DOMAIN}");
    let ca_key = KeyPair::from_pem(&ca.ca_key_pem)?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let issuer = Issuer::from_params(&ca_params, &ca_key);

    let mut params = CertificateParams::new(vec![cn.clone()])?;
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

async fn setup_infrastructure(client: &Client) -> anyhow::Result<()> {
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

    create_oauth_route(client).await?;
    patch_coredns(client, &node_ip).await?;

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

async fn create_oauth_route(client: &Client) -> anyhow::Result<()> {
    let ar = ApiResource {
        group: "route.openshift.io".into(),
        version: "v1".into(),
        api_version: "route.openshift.io/v1".into(),
        kind: "Route".into(),
        plural: "routes".into(),
    };
    let routes: Api<DynamicObject> = Api::namespaced_with(client.clone(), OAUTH_NS, &ar);

    let host = format!("{OAUTH_HOST}.{DOMAIN}");
    let route = serde_json::json!({
        "apiVersion": "route.openshift.io/v1",
        "kind": "Route",
        "metadata": {
            "name": OAUTH_HOST,
            "namespace": OAUTH_NS
        },
        "spec": {
            "host": host,
            "to": {
                "kind": "Service",
                "name": OAUTH_HOST
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
            OAUTH_HOST,
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

async fn patch_coredns(client: &Client, node_ip: &str) -> anyhow::Result<()> {
    let cm_api: Api<k8s_openapi::api::core::v1::ConfigMap> =
        Api::namespaced(client.clone(), "kube-system");

    let cm = cm_api.get("coredns").await?;
    let corefile = cm
        .data
        .as_ref()
        .and_then(|d| d.get("Corefile"))
        .cloned()
        .unwrap_or_default();

    if corefile.contains("apps.ocp-sim.localhost") {
        info!("CoreDNS already patched for apps.ocp-sim.localhost");
        return Ok(());
    }

    let hosts_block = format!(
        "\napps.ocp-sim.localhost:53 {{\n    hosts {{\n        {node_ip} oauth-openshift.apps.ocp-sim.localhost\n        fallthrough\n    }}\n}}\n"
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

    info!(node_ip, "patched CoreDNS for apps.ocp-sim.localhost");

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

pub async fn run(client: Client, ca: Arc<CaState>) -> anyhow::Result<()> {
    let tls_config = generate_tls_config(&ca)
        .map_err(|e| anyhow::anyhow!("failed to generate TLS config: {e}"))?;
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));

    if let Err(e) = setup_infrastructure(&client).await {
        warn!(%e, "failed to set up OAuth infrastructure (will retry on next restart)");
    }

    let state = Arc::new(OAuthState::new());

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
