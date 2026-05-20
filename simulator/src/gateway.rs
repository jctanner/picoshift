use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, ContainerPort, PodSpec, PodTemplateSpec,
    SecretVolumeSource, Service, ServicePort, ServiceSpec, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, DynamicObject, ObjectMeta, Patch, PatchParams, PostParams};
use kube::runtime::controller::Action;
use kube::runtime::watcher;
use kube::runtime::Controller;
use kube::{Client, ResourceExt};
use tracing::{info, warn};

use crate::util::{gateway_class_ar, gateway_ar, httproute_ar, destination_rule_ar};

const CONTROLLER_NAME: &str = "openshift.io/gateway-controller/v1";
const GATEWAY_NS: &str = "openshift-ingress";
const ENVOY_IMAGE: &str = "docker.io/envoyproxy/envoy:v1.31-latest";

const GATEWAY_CLASS_NAME: &str = "data-science-gateway-class";
const GATEWAY_NAME: &str = "data-science-gateway";
const KUBE_AUTH_PROXY_SVC: &str = "kube-auth-proxy";
const KUBE_AUTH_PROXY_PORT: u16 = 8443;
const ENVOY_LISTEN_PORT: i32 = 8443;
const SERVICE_PORT: i32 = 443;
const TLS_SECRET_NAME: &str = "data-science-gateway-service-tls";

fn service_full_name(gateway_name: &str, class_name: &str) -> String {
    format!("{gateway_name}-{class_name}")
}

fn is_our_gateway_class(gc: &DynamicObject) -> bool {
    gc.data
        .get("spec")
        .and_then(|s| s.get("controllerName"))
        .and_then(|c| c.as_str())
        == Some(CONTROLLER_NAME)
}

fn is_gateway_class_accepted(gc: &DynamicObject) -> bool {
    gc.data
        .get("status")
        .and_then(|s| s.get("conditions"))
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter().any(|cond| {
                cond.get("type").and_then(|t| t.as_str()) == Some("Accepted")
                    && cond.get("status").and_then(|s| s.as_str()) == Some("True")
            })
        })
        .unwrap_or(false)
}

fn is_gateway_accepted(gw: &DynamicObject) -> bool {
    gw.data
        .get("status")
        .and_then(|s| s.get("conditions"))
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter().any(|cond| {
                cond.get("type").and_then(|t| t.as_str()) == Some("Accepted")
                    && cond.get("status").and_then(|s| s.as_str()) == Some("True")
            })
        })
        .unwrap_or(false)
}

fn extract_listener_hostname(gw: &DynamicObject) -> Option<String> {
    gw.data
        .get("spec")
        .and_then(|s| s.get("listeners"))
        .and_then(|l| l.as_array())
        .and_then(|arr| arr.first())
        .and_then(|listener| listener.get("hostname"))
        .and_then(|h| h.as_str())
        .map(|s| s.to_string())
}

struct HttpRouteBackend {
    service_name: String,
    namespace: String,
    port: u16,
}

struct HttpRouteRule {
    path_prefix: String,
    backends: Vec<HttpRouteBackend>,
}

fn extract_httproute_rules(route: &DynamicObject) -> Vec<HttpRouteRule> {
    let mut rules = Vec::new();
    let Some(spec) = route.data.get("spec") else {
        return rules;
    };
    let route_ns = route.namespace().unwrap_or_default();

    let Some(rule_arr) = spec.get("rules").and_then(|r| r.as_array()) else {
        return rules;
    };

    for rule in rule_arr {
        let path_prefix = rule
            .get("matches")
            .and_then(|m| m.as_array())
            .and_then(|arr| arr.first())
            .and_then(|m| m.get("path"))
            .and_then(|p| p.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("/")
            .to_string();

        let mut backends = Vec::new();
        if let Some(backend_arr) = rule.get("backendRefs").and_then(|b| b.as_array()) {
            for backend in backend_arr {
                let svc_name = backend
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let ns = backend
                    .get("namespace")
                    .and_then(|n| n.as_str())
                    .unwrap_or(&route_ns)
                    .to_string();
                let port = backend
                    .get("port")
                    .and_then(|p| p.as_u64())
                    .unwrap_or(80) as u16;

                if !svc_name.is_empty() {
                    backends.push(HttpRouteBackend {
                        service_name: svc_name,
                        namespace: ns,
                        port,
                    });
                }
            }
        }

        rules.push(HttpRouteRule {
            path_prefix,
            backends,
        });
    }

    rules
}

fn generate_bootstrap_yaml() -> String {
    format!(
        r#"node:
  cluster: ocp-sim-gateway
  id: ocp-sim-gateway-node

dynamic_resources:
  lds_config:
    path_config_source:
      path: /etc/envoy/xds/lds.yaml
      watched_directory:
        path: /etc/envoy/xds
  cds_config:
    path_config_source:
      path: /etc/envoy/xds/cds.yaml
      watched_directory:
        path: /etc/envoy/xds

admin:
  address:
    socket_address:
      address: 127.0.0.1
      port_value: 15000
"#
    )
}

fn generate_lds_yaml(_hostname: &str) -> String {
    let lua_script = LUA_FILTER_SCRIPT.replace("{{cookie_name}}", "_oauth2_proxy");

    format!(
        r#"resources:
- "@type": type.googleapis.com/envoy.config.listener.v3.Listener
  name: gateway-https
  address:
    socket_address:
      address: 0.0.0.0
      port_value: {ENVOY_LISTEN_PORT}
  filter_chains:
  - transport_socket:
      name: envoy.transport_sockets.tls
      typed_config:
        "@type": type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.DownstreamTlsContext
        common_tls_context:
          tls_certificates:
          - certificate_chain:
              filename: /etc/envoy/tls/tls.crt
            private_key:
              filename: /etc/envoy/tls/tls.key
    filters:
    - name: envoy.filters.network.http_connection_manager
      typed_config:
        "@type": type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager
        stat_prefix: gateway
        rds:
          route_config_name: gateway-routes
          config_source:
            path_config_source:
              path: /etc/envoy/xds/rds.yaml
              watched_directory:
                path: /etc/envoy/xds
        http_filters:
        - name: envoy.filters.http.ext_authz
          typed_config:
            "@type": type.googleapis.com/envoy.extensions.filters.http.ext_authz.v3.ExtAuthz
            transport_api_version: V3
            http_service:
              server_uri:
                uri: https://{KUBE_AUTH_PROXY_SVC}.{GATEWAY_NS}.svc.cluster.local:{KUBE_AUTH_PROXY_PORT}/oauth2/auth
                cluster: {KUBE_AUTH_PROXY_SVC}
                timeout: 5s
              authorization_request:
                allowed_headers:
                  patterns:
                  - exact: cookie
                  - exact: authorization
              authorization_response:
                allowed_upstream_headers:
                  patterns:
                  - exact: x-auth-request-user
                  - exact: x-auth-request-email
                  - exact: x-auth-request-access-token
                  - exact: authorization
                allowed_client_headers:
                  patterns:
                  - exact: set-cookie
        - name: envoy.filters.http.lua
          typed_config:
            "@type": type.googleapis.com/envoy.extensions.filters.http.lua.v3.Lua
            inline_code: |
{lua_indented}
        - name: envoy.filters.http.router
          typed_config:
            "@type": type.googleapis.com/envoy.extensions.filters.http.router.v3.Router
        upgrade_configs:
        - upgrade_type: websocket
"#,
        lua_indented = indent_lines(&lua_script, 14),
    )
}

fn generate_rds_yaml(hostname: &str, routes: &[HttpRouteRule]) -> String {
    let mut route_entries = String::new();

    let mut sorted_routes: Vec<&HttpRouteRule> = routes.iter().collect();
    sorted_routes.sort_by(|a, b| b.path_prefix.len().cmp(&a.path_prefix.len()));

    for rule in sorted_routes {
        if let Some(backend) = rule.backends.first() {
            let cluster_name = format!(
                "{}_{}_{}",
                backend.service_name, backend.namespace, backend.port
            );
            route_entries.push_str(&format!(
                r#"            - match:
                prefix: "{}"
              route:
                cluster: {}
                upgrade_configs:
                - upgrade_type: websocket
"#,
                rule.path_prefix, cluster_name
            ));
        }
    }

    // Default catch-all route returns 404
    if route_entries.is_empty() {
        route_entries.push_str(
            r#"            - match:
                prefix: "/"
              direct_response:
                status: 503
                body:
                  inline_string: "no backends configured"
"#,
        );
    }

    format!(
        r#"resources:
- "@type": type.googleapis.com/envoy.config.route.v3.RouteConfiguration
  name: gateway-routes
  virtual_hosts:
  - name: gateway
    domains:
    - "{hostname}"
    - "*"
    routes:
{route_entries}"#,
    )
}

fn extract_tls_ports(dr: &DynamicObject) -> std::collections::HashSet<u16> {
    let mut ports = std::collections::HashSet::new();
    let settings = dr
        .data
        .get("spec")
        .and_then(|s| s.get("trafficPolicy"))
        .and_then(|tp| tp.get("portLevelSettings"))
        .and_then(|p| p.as_array());

    if let Some(settings) = settings {
        for setting in settings {
            let has_tls = setting
                .get("tls")
                .and_then(|t| t.get("mode"))
                .and_then(|m| m.as_str())
                .is_some();
            if has_tls {
                if let Some(port) = setting
                    .get("port")
                    .and_then(|p| p.get("number"))
                    .and_then(|n| n.as_u64())
                {
                    ports.insert(port as u16);
                }
            }
        }
    }
    ports
}

fn generate_cds_yaml(routes: &[HttpRouteRule], tls_ports: &std::collections::HashSet<u16>) -> String {
    let mut clusters = String::new();

    // kube-auth-proxy cluster (always needed for ext_authz)
    clusters.push_str(&format!(
        r#"- "@type": type.googleapis.com/envoy.config.cluster.v3.Cluster
  name: {KUBE_AUTH_PROXY_SVC}
  type: STRICT_DNS
  lb_policy: ROUND_ROBIN
  transport_socket:
    name: envoy.transport_sockets.tls
    typed_config:
      "@type": type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext
      common_tls_context:
        validation_context:
          trust_chain_verification: ACCEPT_UNTRUSTED
  load_assignment:
    cluster_name: {KUBE_AUTH_PROXY_SVC}
    endpoints:
    - lb_endpoints:
      - endpoint:
          address:
            socket_address:
              address: {KUBE_AUTH_PROXY_SVC}.{GATEWAY_NS}.svc.cluster.local
              port_value: {KUBE_AUTH_PROXY_PORT}
"#,
    ));

    // Backend clusters from HTTPRoutes
    let mut seen = std::collections::HashSet::new();
    for rule in routes {
        for backend in &rule.backends {
            let cluster_name = format!(
                "{}_{}_{}",
                backend.service_name, backend.namespace, backend.port
            );
            if seen.contains(&cluster_name) {
                continue;
            }
            seen.insert(cluster_name.clone());

            let tls_block = if tls_ports.contains(&backend.port) {
                r#"  transport_socket:
    name: envoy.transport_sockets.tls
    typed_config:
      "@type": type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext
      common_tls_context:
        validation_context:
          trust_chain_verification: ACCEPT_UNTRUSTED
"#
            } else {
                ""
            };

            clusters.push_str(&format!(
                r#"- "@type": type.googleapis.com/envoy.config.cluster.v3.Cluster
  name: {cluster_name}
  type: STRICT_DNS
  lb_policy: ROUND_ROBIN
{tls_block}  load_assignment:
    cluster_name: {cluster_name}
    endpoints:
    - lb_endpoints:
      - endpoint:
          address:
            socket_address:
              address: {svc}.{ns}.svc.cluster.local
              port_value: {port}
"#,
                svc = backend.service_name,
                ns = backend.namespace,
                port = backend.port,
            ));
        }
    }

    format!("resources:\n{clusters}")
}

fn indent_lines(s: &str, spaces: usize) -> String {
    let prefix = " ".repeat(spaces);
    s.lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const LUA_FILTER_SCRIPT: &str = r#"function envoy_on_request(request_handle)
  local access_token = request_handle:headers():get("x-auth-request-access-token")
  local auth_user = request_handle:headers():get("x-auth-request-user")

  if access_token then
    request_handle:headers():add("x-forwarded-access-token", access_token)
    request_handle:headers():replace("authorization", "Bearer " .. access_token)
  elseif auth_user then
    local auth_header = request_handle:headers():get("authorization")
    if auth_header then
      local token = auth_header:match("^Bearer%s+(.+)")
      if token then
        request_handle:headers():add("x-forwarded-access-token", token)
      end
    end
  end

  if access_token or auth_user then
    local cookie_header = request_handle:headers():get("cookie")
    if cookie_header then
      local filtered_cookies = {}
      local cookie_pattern = "^{{cookie_name}}"
      for cookie in cookie_header:gmatch("([^;]+)") do
        local trimmed = cookie:match("^%s*(.-)%s*$")
        if trimmed and trimmed ~= "" then
          local cookie_name = trimmed:match("^([^=]+)")
          if cookie_name and not cookie_name:match(cookie_pattern) then
            table.insert(filtered_cookies, trimmed)
          end
        end
      end
      if #filtered_cookies > 0 then
        request_handle:headers():replace("cookie", table.concat(filtered_cookies, "; "))
      else
        request_handle:headers():remove("cookie")
      end
    end
  end
end
"#;

// --- GatewayClass controller ---

async fn reconcile_gateway_class(
    gc: Arc<DynamicObject>,
    ctx: Arc<Client>,
) -> Result<Action, kube::Error> {
    if !is_our_gateway_class(&gc) {
        return Ok(Action::await_change());
    }

    let name = gc.name_any();

    if is_gateway_class_accepted(&gc) {
        return Ok(Action::await_change());
    }

    let ar = gateway_class_ar();
    let api: Api<DynamicObject> = Api::all_with(ctx.as_ref().clone(), &ar);
    let now = chrono::Utc::now().to_rfc3339();

    let status_patch = serde_json::json!({
        "status": {
            "conditions": [{
                "type": "Accepted",
                "status": "True",
                "reason": "Accepted",
                "message": "GatewayClass accepted by ocp-sim",
                "lastTransitionTime": now,
                "observedGeneration": gc.metadata.generation.unwrap_or(0)
            }]
        }
    });

    api.patch_status(
        &name,
        &PatchParams::apply("ocp-sim-gateway"),
        &Patch::Merge(&status_patch),
    )
    .await?;

    info!(name, "accepted GatewayClass");
    Ok(Action::await_change())
}

// --- Gateway controller ---

async fn reconcile_gateway(
    gw: Arc<DynamicObject>,
    ctx: Arc<Client>,
) -> Result<Action, kube::Error> {
    let name = gw.name_any();
    let ns = gw.namespace().unwrap_or_default();

    let class_name = gw
        .data
        .get("spec")
        .and_then(|s| s.get("gatewayClassName"))
        .and_then(|c| c.as_str())
        .unwrap_or("");

    if class_name != GATEWAY_CLASS_NAME {
        return Ok(Action::await_change());
    }

    let hostname = extract_listener_hostname(&gw).unwrap_or_default();
    let svc_name = service_full_name(&name, class_name);

    // Create Envoy infrastructure
    ensure_envoy_configmaps(&ctx, &ns, &hostname).await?;
    ensure_envoy_deployment(&ctx, &ns, &name).await?;
    ensure_envoy_service(&ctx, &ns, &svc_name).await?;

    // Update HTTPRoute-based config
    sync_httproute_config(&ctx, &ns, &hostname).await?;

    // Patch Gateway status
    if !is_gateway_accepted(&gw) {
        let ar = gateway_ar();
        let api: Api<DynamicObject> = Api::namespaced_with(ctx.as_ref().clone(), &ns, &ar);
        let now = chrono::Utc::now().to_rfc3339();

        let status_patch = serde_json::json!({
            "status": {
                "conditions": [
                    {
                        "type": "Accepted",
                        "status": "True",
                        "reason": "Accepted",
                        "message": "Gateway accepted by ocp-sim",
                        "lastTransitionTime": now,
                        "observedGeneration": gw.metadata.generation.unwrap_or(0)
                    },
                    {
                        "type": "Programmed",
                        "status": "True",
                        "reason": "Programmed",
                        "message": "Envoy configured by ocp-sim",
                        "lastTransitionTime": now,
                        "observedGeneration": gw.metadata.generation.unwrap_or(0)
                    }
                ],
                "listeners": [{
                    "name": "https",
                    "supportedKinds": [{"group": "gateway.networking.k8s.io", "kind": "HTTPRoute"}],
                    "attachedRoutes": 0,
                    "conditions": [{
                        "type": "Accepted",
                        "status": "True",
                        "reason": "Accepted",
                        "lastTransitionTime": now,
                        "observedGeneration": gw.metadata.generation.unwrap_or(0)
                    }]
                }]
            }
        });

        api.patch_status(
            &name,
            &PatchParams::apply("ocp-sim-gateway"),
            &Patch::Merge(&status_patch),
        )
        .await?;

        info!(ns, name, hostname, svc_name, "accepted Gateway, created Envoy infrastructure");
    }

    Ok(Action::await_change())
}

async fn ensure_envoy_configmaps(
    client: &Client,
    ns: &str,
    hostname: &str,
) -> Result<(), kube::Error> {
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), ns);

    // Bootstrap ConfigMap
    if cms.get_opt("envoy-bootstrap").await?.is_none() {
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some("envoy-bootstrap".into()),
                namespace: Some(ns.into()),
                ..Default::default()
            },
            data: Some(BTreeMap::from([(
                "bootstrap.yaml".into(),
                generate_bootstrap_yaml(),
            )])),
            ..Default::default()
        };
        cms.create(&PostParams::default(), &cm).await?;
        info!(ns, "created envoy-bootstrap ConfigMap");
    }

    // xDS ConfigMap (initial, will be updated by HTTPRoute sync)
    if cms.get_opt("envoy-xds-config").await?.is_none() {
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some("envoy-xds-config".into()),
                namespace: Some(ns.into()),
                ..Default::default()
            },
            data: Some(BTreeMap::from([
                ("lds.yaml".into(), generate_lds_yaml(hostname)),
                ("rds.yaml".into(), generate_rds_yaml(hostname, &[])),
                ("cds.yaml".into(), generate_cds_yaml(&[], &std::collections::HashSet::new())),
            ])),
            ..Default::default()
        };
        cms.create(&PostParams::default(), &cm).await?;
        info!(ns, "created envoy-xds-config ConfigMap");
    }

    Ok(())
}

async fn ensure_envoy_deployment(
    client: &Client,
    ns: &str,
    gateway_name: &str,
) -> Result<(), kube::Error> {
    let deploys: Api<Deployment> = Api::namespaced(client.clone(), ns);

    if deploys.get_opt(gateway_name).await?.is_some() {
        return Ok(());
    }

    let labels = BTreeMap::from([
        ("app".to_string(), "envoy-gateway".to_string()),
        (
            "gateway.networking.k8s.io/gateway-name".to_string(),
            gateway_name.to_string(),
        ),
    ]);

    let deploy = Deployment {
        metadata: ObjectMeta {
            name: Some(gateway_name.into()),
            namespace: Some(ns.into()),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            selector: LabelSelector {
                match_labels: Some(labels.clone()),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    containers: vec![Container {
                        name: "envoy".into(),
                        image: Some(ENVOY_IMAGE.into()),
                        args: Some(vec![
                            "-c".into(),
                            "/etc/envoy/bootstrap/bootstrap.yaml".into(),
                            "--log-level".into(),
                            "info".into(),
                        ]),
                        ports: Some(vec![ContainerPort {
                            container_port: ENVOY_LISTEN_PORT,
                            name: Some("https".into()),
                            protocol: Some("TCP".into()),
                            ..Default::default()
                        }]),
                        volume_mounts: Some(vec![
                            VolumeMount {
                                name: "bootstrap".into(),
                                mount_path: "/etc/envoy/bootstrap".into(),
                                read_only: Some(true),
                                ..Default::default()
                            },
                            VolumeMount {
                                name: "xds".into(),
                                mount_path: "/etc/envoy/xds".into(),
                                read_only: Some(true),
                                ..Default::default()
                            },
                            VolumeMount {
                                name: "tls".into(),
                                mount_path: "/etc/envoy/tls".into(),
                                read_only: Some(true),
                                ..Default::default()
                            },
                        ]),
                        ..Default::default()
                    }],
                    volumes: Some(vec![
                        Volume {
                            name: "bootstrap".into(),
                            config_map: Some(ConfigMapVolumeSource {
                                name: "envoy-bootstrap".into(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        Volume {
                            name: "xds".into(),
                            config_map: Some(ConfigMapVolumeSource {
                                name: "envoy-xds-config".into(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        Volume {
                            name: "tls".into(),
                            secret: Some(SecretVolumeSource {
                                secret_name: Some(TLS_SECRET_NAME.into()),
                                optional: Some(true),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    deploys.create(&PostParams::default(), &deploy).await?;
    info!(ns, gateway_name, "created Envoy Deployment");
    Ok(())
}

async fn ensure_envoy_service(
    client: &Client,
    ns: &str,
    svc_name: &str,
) -> Result<(), kube::Error> {
    let svcs: Api<Service> = Api::namespaced(client.clone(), ns);

    if svcs.get_opt(svc_name).await?.is_some() {
        return Ok(());
    }

    let svc = Service {
        metadata: ObjectMeta {
            name: Some(svc_name.into()),
            namespace: Some(ns.into()),
            annotations: Some(BTreeMap::from([(
                "service.beta.openshift.io/serving-cert-secret-name".into(),
                TLS_SECRET_NAME.into(),
            )])),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(BTreeMap::from([(
                "app".to_string(),
                "envoy-gateway".to_string(),
            )])),
            ports: Some(vec![ServicePort {
                name: Some("https".into()),
                port: SERVICE_PORT,
                target_port: Some(IntOrString::Int(ENVOY_LISTEN_PORT)),
                protocol: Some("TCP".into()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };

    svcs.create(&PostParams::default(), &svc).await?;
    info!(ns, svc_name, "created Envoy Service");
    Ok(())
}

async fn sync_httproute_config(
    client: &Client,
    ns: &str,
    hostname: &str,
) -> Result<(), kube::Error> {
    let ar = httproute_ar();
    let httproutes: Api<DynamicObject> = Api::all_with(client.clone(), &ar);

    let route_list = httproutes.list(&Default::default()).await?;

    let mut all_rules = Vec::new();
    for route in &route_list.items {
        let parent_refs = route
            .data
            .get("spec")
            .and_then(|s| s.get("parentRefs"))
            .and_then(|p| p.as_array());

        let references_our_gateway = parent_refs
            .map(|refs| {
                refs.iter().any(|pr| {
                    pr.get("name").and_then(|n| n.as_str()) == Some(GATEWAY_NAME)
                })
            })
            .unwrap_or(false);

        if references_our_gateway {
            all_rules.extend(extract_httproute_rules(route));
        }
    }

    // Read DestinationRules to find TLS-enabled ports
    let dr_ar = destination_rule_ar();
    let dest_rules: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, &dr_ar);
    let mut tls_ports = std::collections::HashSet::new();
    if let Ok(dr_list) = dest_rules.list(&Default::default()).await {
        for dr in &dr_list.items {
            tls_ports.extend(extract_tls_ports(dr));
        }
    }

    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    let rds = generate_rds_yaml(hostname, &all_rules);
    let cds = generate_cds_yaml(&all_rules, &tls_ports);
    let lds = generate_lds_yaml(hostname);

    let patch = serde_json::json!({
        "data": {
            "lds.yaml": lds,
            "rds.yaml": rds,
            "cds.yaml": cds
        }
    });

    cms.patch(
        "envoy-xds-config",
        &PatchParams::apply("ocp-sim-gateway"),
        &Patch::Merge(&patch),
    )
    .await?;

    if !all_rules.is_empty() {
        info!(ns, routes = all_rules.len(), "synced HTTPRoute config to Envoy xDS");
    }

    Ok(())
}

// --- HTTPRoute controller ---

async fn reconcile_httproute(
    route: Arc<DynamicObject>,
    ctx: Arc<Client>,
) -> Result<Action, kube::Error> {
    let parent_refs = route
        .data
        .get("spec")
        .and_then(|s| s.get("parentRefs"))
        .and_then(|p| p.as_array());

    let references_our_gateway = parent_refs
        .map(|refs| {
            refs.iter().any(|pr| {
                pr.get("name").and_then(|n| n.as_str()) == Some(GATEWAY_NAME)
            })
        })
        .unwrap_or(false);

    if !references_our_gateway {
        return Ok(Action::await_change());
    }

    // Re-read Gateway to get hostname
    let ar = gateway_ar();
    let gateways: Api<DynamicObject> = Api::namespaced_with(ctx.as_ref().clone(), GATEWAY_NS, &ar);
    let hostname = match gateways.get_opt(GATEWAY_NAME).await? {
        Some(gw) => extract_listener_hostname(&gw).unwrap_or_default(),
        None => String::new(),
    };

    sync_httproute_config(&ctx, GATEWAY_NS, &hostname).await?;

    Ok(Action::await_change())
}

// --- DestinationRule controller ---

async fn reconcile_destination_rule(
    _dr: Arc<DynamicObject>,
    ctx: Arc<Client>,
) -> Result<Action, kube::Error> {
    let ar = gateway_ar();
    let gateways: Api<DynamicObject> =
        Api::namespaced_with(ctx.as_ref().clone(), GATEWAY_NS, &ar);
    let hostname = match gateways.get_opt(GATEWAY_NAME).await? {
        Some(gw) => extract_listener_hostname(&gw).unwrap_or_default(),
        None => String::new(),
    };

    sync_httproute_config(&ctx, GATEWAY_NS, &hostname).await?;

    Ok(Action::await_change())
}

fn error_policy(
    _obj: Arc<DynamicObject>,
    _error: &kube::Error,
    _ctx: Arc<Client>,
) -> Action {
    Action::requeue(std::time::Duration::from_secs(10))
}

pub async fn run(client: Client) -> anyhow::Result<()> {
    let ctx = Arc::new(client.clone());

    info!("starting Gateway API controller");

    let gc_ar = gateway_class_ar();
    let gc_api: Api<DynamicObject> = Api::all_with(client.clone(), &gc_ar);

    let gw_ar = gateway_ar();
    let gw_api: Api<DynamicObject> = Api::all_with(client.clone(), &gw_ar);

    let hr_ar = httproute_ar();
    let hr_api: Api<DynamicObject> = Api::all_with(client.clone(), &hr_ar);

    let dr_ar = destination_rule_ar();
    let dr_api: Api<DynamicObject> = Api::all_with(client.clone(), &dr_ar);

    let gc_ctrl = Controller::new_with(gc_api, watcher::Config::default(), gc_ar)
        .run(reconcile_gateway_class, error_policy, ctx.clone())
        .for_each(|res| async {
            if let Err(e) = res {
                warn!("GatewayClass reconcile error: {e:?}");
            }
        });

    let gw_ctrl = Controller::new_with(gw_api, watcher::Config::default(), gw_ar)
        .run(reconcile_gateway, error_policy, ctx.clone())
        .for_each(|res| async {
            if let Err(e) = res {
                warn!("Gateway reconcile error: {e:?}");
            }
        });

    let hr_ctrl = Controller::new_with(hr_api, watcher::Config::default(), hr_ar)
        .run(reconcile_httproute, error_policy, ctx.clone())
        .for_each(|res| async {
            if let Err(e) = res {
                warn!("HTTPRoute reconcile error: {e:?}");
            }
        });

    let dr_ctrl = Controller::new_with(dr_api, watcher::Config::default(), dr_ar)
        .run(reconcile_destination_rule, error_policy, ctx.clone())
        .for_each(|res| async {
            if let Err(e) = res {
                warn!("DestinationRule reconcile error: {e:?}");
            }
        });

    tokio::join!(gc_ctrl, gw_ctrl, hr_ctrl, dr_ctrl);

    Ok(())
}
