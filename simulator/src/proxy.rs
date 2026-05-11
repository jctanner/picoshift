use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use k8s_openapi::api::core::v1::{Endpoints, Service};
use kube::api::{Api, ApiResource, DynamicObject};
use kube::runtime::watcher;
use kube::runtime::WatchStreamExt;
use kube::{Client, ResourceExt};
use rcgen::{CertificateParams, Issuer, KeyPair};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, ServerConfig, SignatureScheme};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

use crate::CaState;

struct RouteBackend {
    service_name: String,
    service_namespace: String,
    target_port: TargetPort,
    tls: bool,
}

enum TargetPort {
    Number(i32),
    Name(String),
}

#[derive(Debug)]
struct NoVerifier;

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

fn make_tls_config() -> ClientConfig {
    ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerifier))
        .with_no_client_auth()
}

type RouteTable = Arc<RwLock<HashMap<String, RouteBackend>>>;

fn route_api_resource() -> ApiResource {
    ApiResource {
        group: "route.openshift.io".into(),
        version: "v1".into(),
        api_version: "route.openshift.io/v1".into(),
        kind: "Route".into(),
        plural: "routes".into(),
    }
}

fn extract_host(route: &DynamicObject) -> Option<String> {
    route
        .data
        .get("status")
        .and_then(|s| s.get("ingress"))
        .and_then(|i| i.as_array())
        .and_then(|arr| arr.first())
        .and_then(|ing| ing.get("host"))
        .and_then(|h| h.as_str())
        .map(|s| s.to_string())
}

fn extract_backend(route: &DynamicObject) -> Option<(String, TargetPort, bool)> {
    let spec = route.data.get("spec")?;
    let to = spec.get("to")?;
    let svc_name = to.get("name")?.as_str()?.to_string();

    let port = if let Some(port_obj) = spec.get("port") {
        if let Some(target) = port_obj.get("targetPort") {
            if let Some(num) = target.as_u64() {
                TargetPort::Number(num as i32)
            } else if let Some(name) = target.as_str() {
                TargetPort::Name(name.to_string())
            } else {
                TargetPort::Number(80)
            }
        } else {
            TargetPort::Number(80)
        }
    } else {
        TargetPort::Number(80)
    };

    let tls = spec
        .get("tls")
        .and_then(|t| t.get("termination"))
        .and_then(|t| t.as_str())
        .map(|t| t == "reencrypt" || t == "passthrough")
        .unwrap_or(false);

    Some((svc_name, port, tls))
}

async fn build_route_table(client: Client, table: RouteTable) {
    let ar = route_api_resource();
    let routes: Api<DynamicObject> = Api::all_with(client, &ar);

    let stream = watcher::watcher(routes, watcher::Config::default())
        .default_backoff()
        .applied_objects();

    tokio::pin!(stream);

    while let Some(event) = stream.next().await {
        match event {
            Ok(route) => {
                let host = match extract_host(&route) {
                    Some(h) => h,
                    None => continue,
                };
                let ns = route.namespace().unwrap_or_default();
                let (svc_name, target_port, tls) = match extract_backend(&route) {
                    Some(b) => b,
                    None => continue,
                };

                info!(host, ns, svc_name, tls, "route table updated");

                table.write().await.insert(
                    host,
                    RouteBackend {
                        service_name: svc_name,
                        service_namespace: ns,
                        target_port,
                        tls,
                    },
                );
            }
            Err(e) => {
                warn!("route watch error: {e}");
            }
        }
    }
}

fn httproute_api_resource() -> ApiResource {
    ApiResource {
        group: "gateway.networking.k8s.io".into(),
        version: "v1".into(),
        api_version: "gateway.networking.k8s.io/v1".into(),
        kind: "HTTPRoute".into(),
        plural: "httproutes".into(),
    }
}

fn gateway_api_resource() -> ApiResource {
    ApiResource {
        group: "gateway.networking.k8s.io".into(),
        version: "v1".into(),
        api_version: "gateway.networking.k8s.io/v1".into(),
        kind: "Gateway".into(),
        plural: "gateways".into(),
    }
}

async fn build_gateway_route_table(client: Client, table: RouteTable) {
    let ar = httproute_api_resource();
    let routes: Api<DynamicObject> = Api::all_with(client.clone(), &ar);

    let stream = watcher::watcher(routes, watcher::Config::default())
        .default_backoff()
        .applied_objects();

    tokio::pin!(stream);

    while let Some(event) = stream.next().await {
        match event {
            Ok(route) => {
                let route_ns = route.namespace().unwrap_or_default();

                let hostnames: Vec<String> = route
                    .data
                    .get("spec")
                    .and_then(|s| s.get("hostnames"))
                    .and_then(|h| h.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();

                let parent_refs = route
                    .data
                    .get("spec")
                    .and_then(|s| s.get("parentRefs"))
                    .and_then(|p| p.as_array())
                    .cloned()
                    .unwrap_or_default();

                for parent in &parent_refs {
                    let gw_name = match parent.get("name").and_then(|n| n.as_str()) {
                        Some(n) => n.to_string(),
                        None => continue,
                    };
                    let gw_ns = parent
                        .get("namespace")
                        .and_then(|n| n.as_str())
                        .unwrap_or(&route_ns)
                        .to_string();

                    let mut all_hosts = hostnames.clone();
                    if all_hosts.is_empty() {
                        let gw_ar = gateway_api_resource();
                        let gw_api: Api<DynamicObject> =
                            Api::namespaced_with(client.clone(), &gw_ns, &gw_ar);
                        if let Ok(gw) = gw_api.get(&gw_name).await {
                            if let Some(listeners) = gw
                                .data
                                .get("spec")
                                .and_then(|s| s.get("listeners"))
                                .and_then(|l| l.as_array())
                            {
                                for listener in listeners {
                                    if let Some(h) =
                                        listener.get("hostname").and_then(|h| h.as_str())
                                    {
                                        all_hosts.push(h.to_string());
                                    }
                                }
                            }
                        }
                    }

                    let svc_label =
                        format!("gateway.networking.k8s.io/gateway-name={gw_name}");
                    let svcs: Api<Service> = Api::namespaced(client.clone(), &gw_ns);
                    let svc_list = svcs
                        .list(&kube::api::ListParams::default().labels(&svc_label))
                        .await;

                    let (svc_name, svc_ns, svc_port, svc_tls) = match svc_list {
                        Ok(list) => match list.items.first() {
                            Some(svc) => {
                                let ports = svc
                                    .spec
                                    .as_ref()
                                    .and_then(|s| s.ports.as_ref());
                                let http_port = ports.and_then(|pp| {
                                    pp.iter().find(|p| {
                                        p.name.as_deref() == Some("http")
                                            || p.port == 80
                                    })
                                });
                                let https_port = ports.and_then(|pp| {
                                    pp.iter().find(|p| {
                                        p.name.as_deref() == Some("https")
                                            || p.port == 443
                                    })
                                });
                                let (port, tls) = if let Some(p) = https_port {
                                    (p.port, true)
                                } else if let Some(p) = http_port {
                                    (p.port, false)
                                } else {
                                    (80, false)
                                };
                                (
                                    svc.name_any(),
                                    svc.namespace().unwrap_or(gw_ns.clone()),
                                    port,
                                    tls,
                                )
                            }
                            None => continue,
                        },
                        Err(_) => continue,
                    };

                    for host in &all_hosts {
                        info!(
                            host,
                            svc_name,
                            svc_ns,
                            "gateway route table updated"
                        );
                        table.write().await.insert(
                            host.clone(),
                            RouteBackend {
                                service_name: svc_name.clone(),
                                service_namespace: svc_ns.clone(),
                                target_port: TargetPort::Number(svc_port),
                                tls: svc_tls,
                            },
                        );
                    }
                }
            }
            Err(e) => {
                warn!("httproute watch error: {e}");
            }
        }
    }
}

async fn resolve_endpoint(
    client: &Client,
    backend: &RouteBackend,
) -> Option<SocketAddr> {
    let svcs: Api<Service> = Api::namespaced(client.clone(), &backend.service_namespace);
    if let Ok(svc) = svcs.get(&backend.service_name).await {
        if let Some(cluster_ip) = svc.spec.as_ref().and_then(|s| s.cluster_ip.as_deref()) {
            if cluster_ip != "None" && !cluster_ip.is_empty() {
                let port = match &backend.target_port {
                    TargetPort::Number(n) => *n as u16,
                    TargetPort::Name(name) => {
                        svc.spec.as_ref()
                            .and_then(|s| s.ports.as_ref())
                            .and_then(|ports| ports.iter().find(|p| p.name.as_deref() == Some(name)))
                            .map(|p| p.port as u16)
                            .unwrap_or(80)
                    }
                };
                if let Ok(ip) = cluster_ip.parse() {
                    return Some(SocketAddr::new(ip, port));
                }
            }
        }
    }

    let eps: Api<Endpoints> = Api::namespaced(client.clone(), &backend.service_namespace);
    let ep = eps.get(&backend.service_name).await.ok()?;

    for subset in ep.subsets? {
        let port = match &backend.target_port {
            TargetPort::Number(n) => {
                subset.ports.as_ref()?.iter().find(|p| p.port == *n).map(|p| p.port)
            }
            TargetPort::Name(name) => {
                subset.ports.as_ref()?.iter().find(|p| p.name.as_deref() == Some(name)).map(|p| p.port)
            }
        };

        if let Some(port) = port {
            if let Some(addr) = subset.addresses.as_ref().and_then(|a| a.first()) {
                let ip = addr.ip.parse().ok()?;
                return Some(SocketAddr::new(ip, port as u16));
            }
        }
    }

    None
}

async fn ws_handshake_and_tunnel<S>(
    req: Request<Incoming>,
    host: &str,
    path: &str,
    tls_incoming: bool,
    mut upstream: S,
) -> Result<Response<Full<Bytes>>, hyper::Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut raw_req = format!(
        "{} {} HTTP/1.1\r\nHost: {host}\r\n",
        req.method(),
        path,
    );
    for (key, value) in req.headers() {
        if key != "host" {
            if let Ok(v) = value.to_str() {
                raw_req.push_str(&format!("{}: {v}\r\n", key));
            }
        }
    }
    if tls_incoming {
        raw_req.push_str("X-Forwarded-Proto: https\r\n");
    }
    raw_req.push_str("\r\n");

    if let Err(e) = upstream.write_all(raw_req.as_bytes()).await {
        warn!("upstream write failed: {e}");
        return Ok(Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Full::new(Bytes::from("upstream write failed\n")))
            .unwrap());
    }

    let mut resp_buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 1];
    loop {
        match upstream.read(&mut tmp).await {
            Ok(0) => break,
            Ok(_) => {
                resp_buf.push(tmp[0]);
                if resp_buf.len() >= 4 && &resp_buf[resp_buf.len()-4..] == b"\r\n\r\n" {
                    break;
                }
            }
            Err(e) => {
                warn!("upstream read failed: {e}");
                return Ok(Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Full::new(Bytes::from("upstream read failed\n")))
                    .unwrap());
            }
        }
    }

    let resp_str = String::from_utf8_lossy(&resp_buf);
    let first_line = resp_str.lines().next().unwrap_or("");

    if !first_line.contains("101") {
        info!("upstream did not upgrade: {first_line}");
        let status_code = first_line.split_whitespace().nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(502);
        let mut builder = Response::builder().status(status_code);
        for line in resp_str.lines().skip(1) {
            if line.is_empty() { break; }
            if let Some((k, v)) = line.split_once(':') {
                builder = builder.header(k.trim(), v.trim());
            }
        }
        return Ok(builder.body(Full::new(Bytes::new())).unwrap());
    }

    let on_upgrade = hyper::upgrade::on(req);

    let mut response = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    for line in resp_str.lines().skip(1) {
        if line.is_empty() { break; }
        if let Some((k, v)) = line.split_once(':') {
            response = response.header(k.trim(), v.trim());
        }
    }

    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let mut client_io = hyper_util::rt::TokioIo::new(upgraded);
                let (mut cr, mut cw) = tokio::io::split(&mut client_io);
                let (mut ur, mut uw) = tokio::io::split(&mut upstream);
                let c2u = tokio::io::copy(&mut cr, &mut uw);
                let u2c = tokio::io::copy(&mut ur, &mut cw);
                tokio::select! {
                    r = c2u => { if let Err(e) = r { info!("ws client→upstream closed: {e}"); } }
                    r = u2c => { if let Err(e) = r { info!("ws upstream→client closed: {e}"); } }
                }
            }
            Err(e) => warn!("upgrade failed: {e}"),
        }
    });

    Ok(response.body(Full::new(Bytes::new())).unwrap())
}

async fn proxy_upgrade(
    req: Request<Incoming>,
    host: &str,
    _uri: &str,
    addr: SocketAddr,
    tls: bool,
    tls_incoming: bool,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let path = req.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/").to_string();
    let bad_gw = |msg: &str| {
        Ok(Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Full::new(Bytes::from(msg.to_string())))
            .unwrap())
    };

    if tls {
        let tcp = match tokio::net::TcpStream::connect(addr).await {
            Ok(t) => t,
            Err(e) => { warn!("upstream connect failed: {e}"); return bad_gw("upstream connect failed\n"); }
        };
        let tls_config = make_tls_config();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
        let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
            .unwrap_or_else(|_| rustls::pki_types::ServerName::IpAddress(addr.ip().into()));
        match connector.connect(server_name, tcp).await {
            Ok(tls_stream) => ws_handshake_and_tunnel(req, host, &path, tls_incoming, tls_stream).await,
            Err(e) => { warn!("upstream TLS failed: {e}"); bad_gw("upstream TLS error\n") }
        }
    } else {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(tcp) => ws_handshake_and_tunnel(req, host, &path, tls_incoming, tcp).await,
            Err(e) => { warn!("upstream connect failed: {e}"); bad_gw("upstream connect failed\n") }
        }
    }
}

async fn proxy_request(
    client: Client,
    table: RouteTable,
    tls_incoming: bool,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let host = req
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or(h).to_string())
        .unwrap_or_default();

    let backend = {
        let t = table.read().await;
        match t.get(&host) {
            Some(b) => (b.service_name.clone(), b.service_namespace.clone(), match &b.target_port {
                TargetPort::Number(n) => TargetPort::Number(*n),
                TargetPort::Name(s) => TargetPort::Name(s.clone()),
            }, b.tls),
            None => {
                return Ok(Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Full::new(Bytes::from(format!("no route for host: {host}\n"))))
                    .unwrap());
            }
        }
    };

    let backend_ref = RouteBackend {
        service_name: backend.0,
        service_namespace: backend.1,
        target_port: backend.2,
        tls: backend.3,
    };

    let addr = match resolve_endpoint(&client, &backend_ref).await {
        Some(a) => a,
        None => {
            return Ok(Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Full::new(Bytes::from(format!(
                    "no endpoints for {}/{}\n",
                    backend_ref.service_namespace, backend_ref.service_name
                ))))
                .unwrap());
        }
    };

    let path = req.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let uri = if backend_ref.tls {
        path.to_string()
    } else {
        format!("http://{addr}{path}")
    };

    let mut proxy_req = Request::builder()
        .method(req.method())
        .uri(&uri);

    for (key, value) in req.headers() {
        if key != "host" {
            proxy_req = proxy_req.header(key, value);
        }
    }
    proxy_req = proxy_req.header("host", &host);

    if tls_incoming {
        proxy_req = proxy_req.header("x-forwarded-proto", "https");
        proxy_req = proxy_req.header("x-forwarded-scheme", "https");
        proxy_req = proxy_req.header("x-forwarded-port", "443");
    } else {
        proxy_req = proxy_req.header("x-forwarded-proto", "http");
    }

    let is_upgrade = req.headers().get("upgrade").is_some();

    if is_upgrade {
        info!(host, uri = %uri, "proxying WebSocket upgrade");
        return proxy_upgrade(req, &host, &uri, addr, backend_ref.tls, tls_incoming).await;
    }

    let body = req.collect().await?.to_bytes();
    let proxy_req = proxy_req.body(Full::new(body)).unwrap();

    let result = if backend_ref.tls {
        let tcp = match tokio::net::TcpStream::connect(addr).await {
            Ok(t) => t,
            Err(e) => {
                warn!("upstream connect failed: {e}");
                return Ok(Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Full::new(Bytes::from(format!("proxy error: connect failed\n"))))
                    .unwrap());
            }
        };
        let connector = tokio_rustls::TlsConnector::from(Arc::new(make_tls_config()));
        let server_name = rustls::pki_types::ServerName::try_from(host.clone())
            .unwrap_or_else(|_| rustls::pki_types::ServerName::IpAddress(addr.ip().into()));
        let tls_stream = match connector.connect(server_name, tcp).await {
            Ok(s) => s,
            Err(e) => {
                warn!("upstream TLS failed: {e}");
                return Ok(Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Full::new(Bytes::from(format!("proxy error: TLS failed\n"))))
                    .unwrap());
            }
        };
        let io = hyper_util::rt::TokioIo::new(tls_stream);
        let (mut sender, conn) = match hyper::client::conn::http1::handshake(io).await {
            Ok(v) => v,
            Err(e) => {
                warn!("upstream HTTP handshake failed: {e}");
                return Ok(Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Full::new(Bytes::from(format!("proxy error: handshake failed\n"))))
                    .unwrap());
            }
        };
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let sni_host = host.clone();
        info!(
            host,
            %addr,
            sni = %sni_host,
            method = %proxy_req.method(),
            uri = %proxy_req.uri(),
            host_header = ?proxy_req.headers().get("host").map(|v| v.to_str().unwrap_or("?")),
            x_fwd_proto = ?proxy_req.headers().get("x-forwarded-proto").map(|v| v.to_str().unwrap_or("?")),
            "TLS upstream request"
        );
        match sender.send_request(proxy_req).await {
            Ok(resp) => Ok(resp.map(|b| b.map_err(|e| std::io::Error::other(e)).boxed())),
            Err(e) => Err(e.to_string()),
        }
    } else {
        let client_http = hyper_util::client::legacy::Client::builder(
            hyper_util::rt::TokioExecutor::new(),
        )
        .build_http();
        match client_http.request(proxy_req).await {
            Ok(resp) => Ok(resp.map(|b| b.map_err(|e| std::io::Error::other(e)).boxed())),
            Err(e) => Err(e.to_string()),
        }
    };

    match result {
        Ok(resp) => {
            let status = resp.status();
            let headers = resp.headers().clone();
            let body = resp.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();

            let mut response = Response::builder().status(status);
            for (key, value) in headers.iter() {
                response = response.header(key, value);
            }
            Ok(response.body(Full::new(body)).unwrap())
        }
        Err(e) => {
            Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Full::new(Bytes::from(format!("proxy error: {e}\n"))))
                .unwrap())
        }
    }
}

fn generate_tls_config(ca: &CaState) -> Result<ServerConfig, Box<dyn std::error::Error + Send + Sync>> {
    let cn = "*.apps.ocp-sim.test";
    let ca_key = KeyPair::from_pem(&ca.ca_key_pem)?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let issuer = Issuer::from_params(&ca_params, &ca_key);

    let mut params = CertificateParams::new(vec![cn.to_string()])?;
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        rcgen::DnValue::Utf8String(cn.to_string()),
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

pub async fn run(client: Client, port: u16, ca: Arc<CaState>) -> anyhow::Result<()> {
    let table: RouteTable = Arc::new(RwLock::new(HashMap::new()));

    let table_clone = table.clone();
    let client_clone = client.clone();
    tokio::spawn(async move {
        build_route_table(client_clone, table_clone).await;
    });

    let gw_table = table.clone();
    let gw_client = client.clone();
    tokio::spawn(async move {
        build_gateway_route_table(gw_client, gw_table).await;
    });

    // HTTP listener
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "reverse proxy listening (HTTP)");

    // HTTPS listener on port 443
    let tls_config = generate_tls_config(&ca)
        .map_err(|e| anyhow::anyhow!("failed to generate proxy TLS config: {e}"))?;
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));
    let tls_addr = SocketAddr::from(([0, 0, 0, 0], 443));
    let tls_listener = TcpListener::bind(tls_addr).await?;
    info!(%tls_addr, "reverse proxy listening (HTTPS)");

    let tls_client = client.clone();
    let tls_table = table.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match tls_listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    warn!("TLS accept error: {e}");
                    continue;
                }
            };
            let acceptor = acceptor.clone();
            let client = tls_client.clone();
            let table = tls_table.clone();

            tokio::spawn(async move {
                let tls_stream = match acceptor.accept(stream).await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("proxy TLS handshake failed: {e}");
                        return;
                    }
                };

                let service = service_fn(move |req| {
                    proxy_request(client.clone(), table.clone(), true, req)
                });

                if let Err(e) = http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(tls_stream), service)
                    .with_upgrades()
                    .await
                {
                    if !e.to_string().contains("connection closed") {
                        warn!("proxy TLS connection error: {e}");
                    }
                }
            });
        }
    });

    // HTTP accept loop (runs on main task)
    loop {
        let (stream, _) = listener.accept().await?;
        let client = client.clone();
        let table = table.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                proxy_request(client.clone(), table.clone(), false, req)
            });

            if let Err(e) = http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .with_upgrades()
                .await
            {
                warn!("connection error: {e}");
            }
        });
    }
}
