# LoadBalancer Support via ocp-sim Proxy

## Context

With the built-in gateway controller disabled and real Istio installed, Istio creates
LoadBalancer-type Services for Gateway resources. On kind there's no cloud LB provider,
so these services sit in `<pending>` forever. Rather than adding MetalLB, we want the
ocp-sim simulator to act as a LoadBalancer controller — assigning IPs to LB services
and routing traffic through the existing proxy.

The proxy already handles all `*.apps.ocp-sim.test` traffic on ports 80/443 via
hostNetwork. It watches Route CRs and forwards based on hostname. We need to extend it
to also watch Gateway/HTTPRoute resources and forward matching hostnames to the Istio
gateway service.

Additionally, we need a new controller that watches Services of type LoadBalancer and
patches their `status.loadBalancer.ingress` with an IP (the node IP), so Istio and other
controllers see the service as provisioned.

## What Istio creates for a Gateway

When a Gateway CR is created with `gatewayClassName: istio`, Istio deploys:
- Deployment `{gateway-name}-istio` — Envoy gateway pod
- Service `{gateway-name}-istio` — type LoadBalancer, port 80 (HTTP)
- Labels: `gateway.networking.k8s.io/gateway-name={gateway-name}`
- ownerReference pointing back to the Gateway CR

The Service stays `EXTERNAL-IP: <pending>` without a LB controller.

## Two changes needed

### 1. LoadBalancer controller (`simulator/src/loadbalancer.rs`) — new file

A controller that watches all Services of type LoadBalancer and patches their
`status.loadBalancer.ingress[0].ip` with the node's internal IP (same IP the proxy
already uses for OAuth — discovered via `kubectl get nodes`).

**Watch**: All Services cluster-wide  
**Filter**: `spec.type == "LoadBalancer"` AND `status.loadBalancer.ingress` is empty  
**Action**: Patch the service status with the node IP  

This is the equivalent of what MetalLB does, but simpler — single node, one IP.

Flow:
```
Service (type: LoadBalancer, status empty)
  → controller detects
  → patches status.loadBalancer.ingress = [{ip: <node-ip>}]
  → Istio sees IP assigned, updates Gateway status.addresses
```

Use the same node IP discovery pattern as `oauth.rs` (reading Node InternalIP).

### 2. Extend proxy route table to watch HTTPRoutes (`simulator/src/proxy.rs`)

Add a second watcher alongside `build_route_table()` that watches HTTPRoute resources
and adds entries to the same `RouteTable`.

**Watch**: All HTTPRoutes cluster-wide  
**For each HTTPRoute**:
1. Read `spec.parentRefs` to find the parent Gateway name/namespace
2. Look up the Gateway to get the listener hostname
3. Look up the Istio-created Service for that Gateway (label: `gateway.networking.k8s.io/gateway-name`)
4. Insert into RouteTable: hostname → service name/namespace/port (port 80, no TLS)

This way, when a request arrives at the proxy for a Gateway hostname, it forwards to
the Istio gateway service ClusterIP, which then routes via Envoy to the backend.

Flow:
```
browser → :80 (proxy, hostNetwork)
  → hostname lookup in RouteTable
  → matches HTTPRoute hostname
  → forward to Istio gateway Service ClusterIP:80
  → Envoy (Istio gateway pod) handles routing via HTTPRoute config
  → backend service
```

## Files to modify/create

| File | Action | Purpose |
|------|--------|---------|
| `simulator/src/loadbalancer.rs` | **New** | LB status controller |
| `simulator/src/proxy.rs` | **Modify** | Add HTTPRoute/Gateway watcher to populate RouteTable |
| `simulator/src/main.rs` | **Modify** | Add `mod loadbalancer`, spawn new controller |
| `deploy/simulator.yaml` | **Modify** | Add RBAC for services/status patch |

### `loadbalancer.rs` detail

- Watch all Services via `kube::runtime::watcher` on `Api<Service>::all()`
- Filter for `spec.type == "LoadBalancer"` with empty/missing `status.loadBalancer.ingress`
- Discover node IP: read Nodes, find first InternalIP (same as `oauth.rs`)
- Patch service status subresource: `status.loadBalancer.ingress = [{ip: node_ip}]`
- ~60-80 lines total

### `proxy.rs` changes

Add `build_gateway_route_table(client, table)` function (~60 lines):
- Use `ApiResource` for `gateway.networking.k8s.io/v1` HTTPRoute (DynamicObject watcher)
- For each HTTPRoute:
  - Extract hostnames from `spec.hostnames[]`
  - If no hostnames, extract parent Gateway ref → look up Gateway → get listener hostname
  - Find the gateway's Service by label `gateway.networking.k8s.io/gateway-name`
  - Insert hostname → RouteBackend (gateway service, port 80, tls: false)
- Spawn alongside `build_route_table()` in `run()`

### `main.rs` changes

- Add `mod loadbalancer;`
- Spawn `loadbalancer::run(client.clone())` unconditionally (harmless when no LB services)

### `deploy/simulator.yaml` RBAC additions

```yaml
- apiGroups: [""]
  resources: ["services/status"]
  verbs: ["get", "update", "patch"]
- apiGroups: ["gateway.networking.k8s.io"]
  resources: ["httproutes"]
  verbs: ["get", "list", "watch"]
```

Existing ClusterRole already has `services` get/list/watch and `gateways` get/list/watch.

## Verification

```bash
# 1. Create a test Gateway
kubectl apply -f - <<'EOF'
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: test-gw
  namespace: default
spec:
  gatewayClassName: istio
  listeners:
    - name: http
      hostname: test.apps.ocp-sim.test
      port: 80
      protocol: HTTP
      allowedRoutes:
        namespaces:
          from: All
EOF

# 2. Verify LB controller assigns IP
kubectl get svc test-gw-istio -n default
# Should show EXTERNAL-IP = <node-ip>, not <pending>

# 3. Create a backend + HTTPRoute
kubectl create deployment httpbin --image=kennethreitz/httpbin -n default
kubectl expose deployment httpbin --port=80 -n default
kubectl apply -f - <<'EOF'
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: test-route
  namespace: default
spec:
  parentRefs:
    - name: test-gw
  hostnames:
    - test.apps.ocp-sim.test
  rules:
    - backendRefs:
        - name: httpbin
          port: 80
EOF

# 4. Test through proxy
curl http://test.apps.ocp-sim.test/get
# Should return httpbin JSON response, routed: proxy → Istio gateway → httpbin
```
