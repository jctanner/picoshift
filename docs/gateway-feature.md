# Built-in Gateway Controller

The simulator includes a built-in gateway controller (`simulator/src/gateway.rs`) that
watches Gateway API resources (`GatewayClass`, `Gateway`, `HTTPRoute`) and deploys
Envoy proxy pods to serve ingress traffic. It uses the controller name
`openshift.io/gateway-controller/v1` and handles the `data-science-gateway-class`
GatewayClass — mirroring what OpenShift Service Mesh (Istio) does on a real cluster.

## Feature gate

The built-in gateway controller can be disabled via the `ENABLE_BUILTIN_GATEWAY`
environment variable. This allows swapping in a real gateway implementation such as
Istio.

| Value | Behavior |
|-------|----------|
| *(unset)* | Gateway controller **enabled** (default) |
| `true`, `1`, `yes` | Gateway controller **enabled** |
| `false`, `0`, `no` | Gateway controller **disabled** |

To disable it in the simulator deployment:

```yaml
# deploy/simulator.yaml — add to the container env section
env:
  - name: ENABLE_BUILTIN_GATEWAY
    value: "false"
```

When disabled, the simulator logs a warning at startup:

```
WARN ocp_sim: built-in gateway controller disabled (ENABLE_BUILTIN_GATEWAY=false)
```

## What the built-in controller does

1. Reconciles `GatewayClass` — sets status to Accepted when `controllerName` matches
2. Reconciles `Gateway` — deploys an Envoy proxy Deployment + Service in the gateway's
   namespace, generates xDS bootstrap config, and updates gateway status with listener
   and address conditions
3. Reconciles `HTTPRoute` — generates Envoy route configuration (RDS) and updates
   HTTPRoute status with parent references
4. Watches `DestinationRule` — applies Istio-style traffic policy (mTLS settings) to
   Envoy cluster configuration

## Why disable it

On real OpenShift, `openshift.io/gateway-controller/v1` is backed by Istio (istiod
control plane + Envoy data plane). The built-in controller is a lightweight
reimplementation that works without Istio, but it does not support:

- mTLS between services (Istio's PERMISSIVE/STRICT modes)
- AuthPolicy / TokenRateLimitPolicy enforcement (requires Kuadrant + Istio
  ExtensionProviders)
- Full xDS features (health checking, circuit breaking, outlier detection)

To get these capabilities, disable the built-in controller and deploy real Istio +
Kuadrant:

```bash
# 1. Disable built-in gateway in simulator
kubectl set env daemonset/ocp-sim -n ocp-sim ENABLE_BUILTIN_GATEWAY=false

# 2. Install Istio (e.g., via istioctl or Sail operator Helm chart)
# 3. Install cert-manager
# 4. Install Kuadrant (Helm chart — deploys Authorino + Limitador)
```

The MaaS controller, ODH operator, and all Gateway API resources work identically
regardless of which gateway implementation is active — only the underlying data plane
changes.
