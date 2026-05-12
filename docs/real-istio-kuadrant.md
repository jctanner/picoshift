# Real Istio + Kuadrant on Picoshift

This document captures everything learned while replacing the built-in mock gateway
controller with real Istio and Kuadrant on a kind-based picoshift cluster. It covers
installation, the istiod configuration needed to match OpenShift behavior, how the ODH
operator's gateway architecture works, and the current state of the deployment.

## Prerequisites

- `ENABLE_BUILTIN_GATEWAY=false` in `deploy/simulator.yaml` (see `docs/gateway-feature.md`)
- LoadBalancer controller and HTTPRoute-aware proxy (see `docs/loadbalancer-plan.md`)
- cert-manager (required by Kuadrant)

## Installation

All targets are in the Makefile under the "Gateway Stack" section:

```bash
make gateway-stack          # Installs everything below in order
make gateway-api            # Gateway API CRDs (v1.3.0, includes v1beta1)
make cert-manager           # cert-manager via Helm
make istio                  # Istio minimal profile + OpenShift env patches
make kuadrant               # Kuadrant operator + Authorino + Limitador via Helm
```

### Gateway API CRDs

Upstream Gateway API CRDs v1.3.0 are installed instead of our stub CRDs. This version
serves both `v1` and `v1beta1`, which Istio requires (istiod syncs `v1beta1` resources
internally).

### cert-manager

Standard Helm install into `cert-manager` namespace with CRDs enabled. Required by
Kuadrant for certificate management.

### Istio

Installed via `istioctl install` with the `openshift-gateway` revision and pilot env
vars set at install time. The revision **must** be set during installation (not patched
after) because Istio needs the `kube-gateway` Helm template bundled with the revision
to create Gateway Deployments and Services:

```bash
istioctl install --set profile=minimal \
    --set revision=openshift-gateway \
    --set values.pilot.resources.requests.cpu=100m \
    --set values.pilot.resources.requests.memory=256Mi \
    --set values.pilot.env.PILOT_GATEWAY_API_CONTROLLER_NAME="openshift.io/gateway-controller/v1" \
    --set values.pilot.env.PILOT_ENABLE_GATEWAY_API_DEPLOYMENT_CONTROLLER=true \
    -y
```

This creates `istiod-openshift-gateway` (not `istiod`). See [Istiod Configuration](#istiod-configuration) for why these settings are needed.

**Important**: If you set `REVISION` via `kubectl set env` after installing with the
default revision, istiod will fail with `no "kube-gateway" template defined` when trying
to create Gateway resources. The revision must be set at install time.

### Kuadrant

Before Helm install, any stub CRDs that conflict with Kuadrant's own CRDs must be
deleted:

```bash
kubectl delete crd authconfigs.authorino.kuadrant.io
kubectl delete crd authorinos.operator.authorino.kuadrant.io
kubectl delete crd authpolicies.kuadrant.io
kubectl delete crd tokenratelimitpolicies.kuadrant.io
```

After Helm install, a Kuadrant CR is created to trigger operator reconciliation:

```yaml
apiVersion: kuadrant.io/v1beta1
kind: Kuadrant
metadata:
  name: kuadrant
  namespace: kuadrant-system
```

Kuadrant deploys: Authorino (auth), Limitador (rate limiting), DNS operator.

### Tear-down

```bash
make gateway-stack-delete   # Removes Kuadrant, Istio, cert-manager
make kuadrant-delete
make istio-delete
make cert-manager-delete
```

## Istiod Configuration

On real OpenShift, the gateway stack is a three-layer architecture:

1. **OpenShift Cluster Ingress Operator** — creates GatewayClass with
   `controllerName: openshift.io/gateway-controller/v1`, labels Gateways with
   `istio.io/rev: openshift-gateway`
2. **Sail Operator** — manages a lightweight Istio (gateway-only, no service mesh)
3. **Istiod** — reconciles Gateway CRs, creates Deployment + Service for each Gateway

On picoshift we skip layers 1-2 and use vanilla Istio, but istiod needs two env vars
to behave like the OpenShift version:

### `PILOT_GATEWAY_API_CONTROLLER_NAME`

```
PILOT_GATEWAY_API_CONTROLLER_NAME=openshift.io/gateway-controller/v1
```

By default, vanilla Istio watches GatewayClasses with `controllerName:
istio.io/gateway-controller`. The ODH operator creates `data-science-gateway-class`
with `controllerName: openshift.io/gateway-controller/v1` (hardcoded in
`gateway_support.go:40`). This env var tells istiod to watch the OpenShift controller
name instead.

Without this, istiod ignores the GatewayClass and no gateway infrastructure is created.

### `REVISION`

```
REVISION=openshift-gateway
```

By default, vanilla Istio uses revision `default`. The ODH operator labels every Gateway
it creates with `istio.io/rev: openshift-gateway` (hardcoded in `gateway_support.go:74`).
Istio only reconciles Gateways whose `istio.io/rev` label matches its own revision.

Without this, istiod accepts the GatewayClass but ignores the Gateway — it stays in
`Waiting for controller` / `Pending` state indefinitely.

### Verification

After patching, both GatewayClass and Gateway should be reconciled:

```bash
$ kubectl get gatewayclass data-science-gateway-class
NAME                       CONTROLLER                           ACCEPTED
data-science-gateway-class openshift.io/gateway-controller/v1   True

$ kubectl get gateway -n openshift-ingress
NAME                   CLASS                        PROGRAMMED
data-science-gateway   data-science-gateway-class   True
```

Istio creates the gateway Deployment and Service automatically:

```bash
$ kubectl get deploy,svc -n openshift-ingress -l gateway.networking.k8s.io/gateway-name=data-science-gateway
NAME                                                              READY
deployment.apps/data-science-gateway-data-science-gateway-class   1/1

NAME                                                      TYPE        CLUSTER-IP
service/data-science-gateway-data-science-gateway-class   ClusterIP   10.96.x.x
```

## How the ODH Operator Gateway Architecture Works

### Key constants (from `gateway_support.go`)

| Constant | Value | Purpose |
|----------|-------|---------|
| `GatewayClassName` | `data-science-gateway-class` | GatewayClass name |
| `GatewayControllerName` | `openshift.io/gateway-controller/v1` | Controller name |
| `DefaultGatewayName` | `data-science-gateway` | Gateway CR name |
| `IstioRevisionLabel` | `istio.io/rev` | Revision label key |
| `IstioRevisionValue` | `openshift-gateway` | Revision label value |
| `DefaultGatewaySubdomain` | `rh-ai` | Default subdomain prefix |

### What the ODH operator creates

The GatewayConfig service controller (`internal/controller/services/gateway/`) creates
the full gateway infrastructure in this order:

1. **GatewayClass** `data-science-gateway-class`
   - `controllerName: openshift.io/gateway-controller/v1`

2. **ConfigMap** `data-science-gateway-config` (in `openshift-ingress`)
   - Configures the Istio-created Service as ClusterIP (not LoadBalancer)
   - Adds `service.beta.openshift.io/serving-cert-secret-name` annotation

3. **Gateway** `data-science-gateway` (in `openshift-ingress`)
   - `gatewayClassName: data-science-gateway-class`
   - `istio.io/rev: openshift-gateway` label
   - HTTPS listener on port 443 with TLS termination
   - References `data-science-gateway-service-tls` secret for TLS cert
   - `infrastructure.parametersRef` → ConfigMap for ClusterIP mode

4. **kube-auth-proxy** — OAuth2/OIDC proxy deployment (2 replicas)
   - Deployment + Service + HPA in `openshift-ingress`
   - HTTPRoute for `/oauth2` callbacks
   - DestinationRule for mTLS to auth proxy

5. **EnvoyFilter** `data-science-authn-filter`
   - `ext_authz` filter calling kube-auth-proxy
   - Lua filter for token forwarding
   - Selects pods with `gateway.networking.k8s.io/gateway-name: data-science-gateway`

6. **OCP Route** (only in OcpRoute ingress mode)
   - Exposes the ClusterIP gateway service via OpenShift Route

7. **Dashboard redirect** — nginx pods for legacy URL forwarding

### Service naming convention

Istio names the auto-created service as `{gateway-name}-{gatewayclass-name}`:

```
data-science-gateway-data-science-gateway-class
```

The ODH operator expects this exact name (defined as `GatewayServiceFullName` in
`ocp_routes.go:30-32`) for ingress mode detection and Route creation.

### Ingress mode detection

The GatewayConfig controller detects whether to use LoadBalancer or OcpRoute mode by
checking the existing gateway Service type (`detectAndSetIngressMode()` in
`gateway_support.go:652`). On picoshift, the ConfigMap forces ClusterIP → OcpRoute mode.

### GatewayConfig domain

The GatewayConfig derives its domain from the cluster's ingress domain:
`rh-ai.apps.ocp-sim.test` (subdomain `rh-ai` + cluster apps domain).

## MaaS Gateway Architecture

MaaS uses a **separate gateway** from the platform gateway:

| | Platform Gateway | MaaS Gateway |
|---|---|---|
| Name | `data-science-gateway` | `maas-default-gateway` |
| GatewayClass | `data-science-gateway-class` | `openshift-default` |
| Created by | ODH operator (GatewayConfig controller) | MaaS deploy script |
| Purpose | Dashboard, OAuth callbacks, component ingress | Model serving endpoints |

### MaaS controller behavior

The maas-controller **does not care about the GatewayClass controllerName**. It only
needs:

- A Gateway by **name** (flag: `--gateway-name`, default: `maas-default-gateway`)
- In a specific **namespace** (flag: `--gateway-namespace`, default: `openshift-ingress`)
- A Service labeled `gateway.networking.k8s.io/gateway-name=<gateway-name>` with port 443

MaaS-api discovers the gateway service via:
1. Label selector: `gateway.networking.k8s.io/gateway-name=<gatewayName>`
2. Owner references validation: Service must have `ownerReferences` with `kind: Gateway`
3. Port filter: only accepts services exposing port 443

HTTPRoutes created by maas-controller reference the gateway by name in `parentRefs`,
not by GatewayClass.

## MaaS Prerequisites

Before MaaS can become `Ready`, three prerequisites must be satisfied:

### 1. MaaS Gateway

MaaS uses a **separate gateway** from the platform `data-science-gateway`. The ODH
operator does NOT create this gateway — it must exist before MaaS is enabled.

```bash
# GatewayClass (if not already created)
kubectl apply -f - <<'EOF'
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: openshift-default
spec:
  controllerName: "openshift.io/gateway-controller/v1"
EOF

# Infrastructure ConfigMap (tells Istio to use ClusterIP, not LoadBalancer)
kubectl apply -f - <<'EOF'
apiVersion: v1
kind: ConfigMap
metadata:
  name: maas-gateway-config
  namespace: openshift-ingress
data:
  service: |
    spec:
      type: ClusterIP
EOF

# MaaS Gateway with HTTP + HTTPS listeners
# HTTPS is required — maas-api looks for a gateway service with port 443
kubectl apply -f - <<'EOF'
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: maas-default-gateway
  namespace: openshift-ingress
  labels:
    istio.io/rev: openshift-gateway
spec:
  gatewayClassName: openshift-default
  infrastructure:
    parametersRef:
      group: ""
      kind: ConfigMap
      name: maas-gateway-config
  listeners:
    - name: http
      hostname: "maas.apps.ocp-sim.test"
      port: 80
      protocol: HTTP
      allowedRoutes:
        namespaces:
          from: All
    - name: https
      hostname: "maas.apps.ocp-sim.test"
      port: 443
      protocol: HTTPS
      tls:
        mode: Terminate
        certificateRefs:
          - name: maas-gateway-tls-cert
      allowedRoutes:
        namespaces:
          from: All
EOF
```

The Gateway needs both HTTP and HTTPS listeners. maas-api discovers the gateway service
by label `gateway.networking.k8s.io/gateway-name=maas-default-gateway` and requires
port 443 with an ownerReference to the Gateway.

The `infrastructure.parametersRef` ConfigMap is required because Istio's minimal profile
(with a custom revision) uses Helm templates to render the Deployment and Service. Without
it, istiod logs `no "kube-gateway" template defined`.

### 2. PostgreSQL Database

```bash
# Deploy PostgreSQL and create the connection secret
kubectl apply -f - <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: maas-postgres
  namespace: opendatahub
spec:
  replicas: 1
  selector:
    matchLabels:
      app: maas-postgres
  template:
    metadata:
      labels:
        app: maas-postgres
    spec:
      containers:
      - name: postgres
        image: docker.io/library/postgres:16-alpine
        ports:
        - containerPort: 5432
        env:
        - name: POSTGRES_DB
          value: maas
        - name: POSTGRES_USER
          value: maas
        - name: POSTGRES_PASSWORD
          value: maaspassword
---
apiVersion: v1
kind: Service
metadata:
  name: maas-postgres
  namespace: opendatahub
spec:
  selector:
    app: maas-postgres
  ports:
  - port: 5432
---
apiVersion: v1
kind: Secret
metadata:
  name: maas-db-config
  namespace: opendatahub
type: Opaque
stringData:
  DB_CONNECTION_URL: "postgres://maas:maaspassword@maas-postgres.opendatahub.svc:5432/maas?sslmode=disable"
EOF
```

### 3. Authorino TLS

```bash
# Create a self-signed ClusterIssuer (if not already present)
kubectl apply -f - <<'EOF'
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: selfsigned-issuer
spec:
  selfSigned: {}
EOF

# Create TLS cert for Authorino
kubectl apply -f - <<'EOF'
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: authorino-tls
  namespace: kuadrant-system
spec:
  secretName: authorino-tls-cert
  issuerRef:
    name: selfsigned-issuer
    kind: ClusterIssuer
  dnsNames:
    - authorino-authorino-authorization.kuadrant-system.svc
    - authorino-authorino-authorization.kuadrant-system.svc.cluster.local
EOF

# Enable TLS on Authorino
kubectl patch authorino authorino -n kuadrant-system --type=merge -p '{
  "spec": {
    "listener": {
      "tls": {
        "enabled": true,
        "certSecretRef": { "name": "authorino-tls-cert" }
      }
    }
  }
}'
```

### 4. User Workload Monitoring (stub)

```bash
kubectl create namespace openshift-monitoring
kubectl apply -f - <<'EOF'
apiVersion: v1
kind: ConfigMap
metadata:
  name: cluster-monitoring-config
  namespace: openshift-monitoring
data:
  config.yaml: |
    enableUserWorkload: true
EOF
```

### DSC Configuration

MaaS requires kserve to be `Managed` (not just MaaS itself). The `IsEnabled()` check
in `modelsasservice.go:85` returns false if `kserve.managementState != Managed`:

```yaml
spec:
  components:
    kserve:
      managementState: "Managed"        # REQUIRED for MaaS
      modelsAsService:
        managementState: "Managed"
```

## Current State

### Working

- Istio accepts `data-science-gateway-class` and `openshift-default` GatewayClasses
- Istio reconciles both gateways (creates Deployment + Service for each)
- `data-science-gateway` is `Accepted: True, Programmed: True` (platform gateway)
- `maas-default-gateway` is `Accepted: True, Programmed: True` (MaaS gateway)
- GatewayConfig is `Ready: True`, domain: `rh-ai.apps.ocp-sim.test`
- kube-auth-proxy pods running (2 replicas, with EnvoyFilter for ext_authz)
- TLS certs generated by simulator's service-ca and cert-manager
- LoadBalancer controller assigns node IP to LB-type Services
- Proxy routes HTTPRoute hostnames to Istio gateway services
- DSC `Ready: True`, `ComponentsReady: True`
- `ModelsAsServiceReady: True (Reconciled)`
- `KserveReady: True`
- Tenant `default-tenant` is `Ready: True, phase: Active`
- Running pods: kserve-controller, llmisvc-controller, maas-controller, maas-api,
  model-serving-api, odh-model-controller, payload-processing, maas-postgres

### Known issues

**Webhook TLS on first deploy**: The operator pod starts before the simulator's
service-ca has issued its webhook cert. This causes `TLS handshake error: tls: bad
certificate` flooding in logs. Fix: restart the operator pod after initial deployment
(`kubectl -n opendatahub-operator-system rollout restart deployment/...`). After restart,
TLS works correctly.

**KServeLLMInferenceServiceDependencies**: Shows `Subscription not found` — expects OLM
subscriptions for KServe's LLM inference features. Expected on picoshift since we don't
have OLM. Does not affect MaaS functionality.

**maas-api-key-cleanup image pull**: The `maas-api-key-cleanup` CronJob image defaults to
`registry.redhat.io/ubi9/ubi-minimal:9.7`, which requires Red Hat registry auth. The image
is sourced from the `maas-parameters` ConfigMap (`maas-api-key-cleanup-image` key), which
is rendered from `opt/manifests/maas/overlays/odh/params.env`. We patch that file to use
the public mirror `registry.access.redhat.com/ubi9/ubi-minimal:9.7`.

Do **not** override `RELATED_IMAGE_UBI_MINIMAL_IMAGE` via `kubectl set env` on the operator
deployment. The maas-controller manifest uses `valueFrom: configMapKeyRef` for this env var.
If the operator's image substitution also injects a `value` field, Kubernetes rejects the
deployment with `may not be specified when 'value' is not empty`. Fix the source ConfigMap
data via `params.env` instead.

Even with the correct image, the CronJob still fails because `ubi-minimal` does not
include `curl`, which the cleanup command requires. See `bugs.odh/maas-api-key-cleanup-image.md`
for details.

## ODH Operator Deployment

### Deploy

```bash
make operator-deploy    # Build image, load into kind, install CRDs, deploy
```

### Create DSCI and minimal DSC

```bash
# DSCI with monitoring and trustedCABundle disabled
kubectl apply -f - <<'EOF'
apiVersion: dscinitialization.opendatahub.io/v2
kind: DSCInitialization
metadata:
  name: default-dsci
spec:
  applicationsNamespace: opendatahub
  monitoring:
    managementState: "Removed"
    namespace: opendatahub
  trustedCABundle:
    managementState: "Removed"
EOF

# Minimal DSC with KServe + MaaS enabled
# NOTE: kserve.managementState MUST be "Managed" — MaaS requires it
kubectl apply -f - <<'EOF'
apiVersion: datasciencecluster.opendatahub.io/v2
kind: DataScienceCluster
metadata:
  name: default-dsc
spec:
  components:
    kserve:
      managementState: "Managed"
      modelsAsService:
        managementState: "Managed"
    dashboard:
      managementState: "Removed"
    workbenches:
      managementState: "Removed"
    ray:
      managementState: "Removed"
    trustyai:
      managementState: "Removed"
    modelregistry:
      managementState: "Removed"
    kueue:
      managementState: "Removed"
    trainingoperator:
      managementState: "Removed"
EOF
```

### What happens on DSC creation

The DSC controller triggers the GatewayConfig service controller, which creates the full
platform gateway infrastructure (GatewayClass, Gateway, kube-auth-proxy, EnvoyFilter,
dashboard-redirect, etc.). This is a platform-level service, not a component.

With KServe + MaaS enabled, the DSC also provisions:
- KServe controller + LLM InferenceService controller
- ODH model controller + model-serving-api
- maas-controller (from kustomize manifests)
- A `Tenant` CR (`default-tenant` in `models-as-a-service` namespace)

The Tenant CR triggers maas-controller to deploy maas-api and payload-processing.

The MaaS gateway (`maas-default-gateway`) is NOT created by the operator — it must be
created manually before enabling MaaS (see [MaaS Prerequisites](#maas-prerequisites)).

## Architecture Reference

### On real OpenShift

```
OpenShift Cluster Ingress Operator
  ├── Creates GatewayClass (openshift.io/gateway-controller/v1)
  ├── Labels Gateways with istio.io/rev: openshift-gateway
  └── Manages DNS records from Gateway Services

Sail Operator (OSSM 3.0)
  ├── Manages lightweight Istio (gateway-only, no mesh)
  └── Creates Istio CR → IstioRevision CR

Istiod (configured by Sail)
  ├── PILOT_GATEWAY_API_CONTROLLER_NAME=openshift.io/gateway-controller/v1
  ├── REVISION=openshift-gateway
  ├── Reconciles Gateway CRs → creates Deployment + Service
  └── Updates Gateway status with addresses

ODH Operator
  ├── Creates GatewayClass, Gateway, ConfigMap
  ├── Creates kube-auth-proxy, EnvoyFilter, DestinationRule
  └── Creates OCP Route (in OcpRoute mode)
```

### On picoshift

```
Vanilla Istio (patched with env vars)
  ├── PILOT_GATEWAY_API_CONTROLLER_NAME=openshift.io/gateway-controller/v1
  ├── REVISION=openshift-gateway
  ├── Reconciles Gateway CRs → creates Deployment + Service
  └── Updates Gateway status

Simulator
  ├── service-ca: generates TLS certs, injects CA bundles
  ├── LoadBalancer controller: patches LB Service status with node IP
  └── Proxy: routes HTTPRoute hostnames to Istio gateway services

Kuadrant (via Helm)
  ├── Authorino: auth policy enforcement
  ├── Limitador: rate limiting
  └── DNS operator

ODH Operator (same binary as real OpenShift)
  └── Creates identical gateway infrastructure
```

## Files

| File | Purpose |
|------|---------|
| `Makefile` (gateway-stack section) | Install/uninstall targets |
| `simulator/src/loadbalancer.rs` | LB status controller |
| `simulator/src/proxy.rs` | HTTPRoute-aware proxy |
| `simulator/src/main.rs` | Feature gate, controller spawning |
| `deploy/simulator.yaml` | RBAC, `ENABLE_BUILTIN_GATEWAY=false` |
| `docs/gateway-feature.md` | Built-in gateway feature gate docs |
| `docs/loadbalancer-plan.md` | LB controller + proxy design |
