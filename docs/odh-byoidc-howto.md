# ODH Dashboard with BYOIDC on Picoshift

End-to-end guide to running the ODH dashboard with external OIDC
authentication (entra-mock) and the full OSSM3 gateway stack on picoshift.

## What You Get

- ODH dashboard at `https://rh-ai.apps.ocp-sim.test/`
- OIDC login via entra-mock (Azure Entra ID emulator)
- Production-like gateway stack: istiod Envoy with ext_authz + Lua filters
- Token validation through the full chain: Envoy → kube-auth-proxy → kube-rbac-proxy → API server

## Architecture

```
Browser
  │
  │  https://rh-ai.apps.ocp-sim.test/
  ▼
┌──────────────────┐
│  Simulator Proxy  │  TLS termination, routes by Host header
│  (proxy.rs)       │
└────────┬─────────┘
         │  TLS (reencrypt) to ClusterIP:443
         ▼
┌──────────────────┐
│  Envoy (istiod)   │  Created by istiod from Gateway CR
│  ext_authz ──────►│──► kube-auth-proxy (/oauth2/auth)
│  Lua filter       │       │
└────────┬─────────┘       │  302 → entra-mock login (unauthenticated)
         │                  │  200 + headers (authenticated)
         │                  ▼
         │           ┌──────────────┐
         │           │  entra-mock   │  OIDC provider
         │           └──────────────┘
         │
         │  Authenticated request with Authorization: Bearer <id_token>
         ▼
┌──────────────────┐
│  kube-rbac-proxy  │  TokenReview → API server validates JWT via JWKS
│  (dashboard pod)  │
└────────┬─────────┘
         ▼
┌──────────────────┐
│  odh-dashboard    │  :8080
└──────────────────┘
```

## Prerequisites

- picoshift built: `picoshift init && picoshift build`
- `~/pull-secret.json` with Red Hat registry credentials (for OSSM3 images)

## Step 1: Create Cluster

```bash
picoshift create --auth-mode=byoidc
```

This automatically:
- Creates a kind cluster with the ocp-shim sidecar
- Deploys entra-mock (OIDC provider)
- Configures the simulator in BYOIDC mode
- Patches the kube-apiserver with OIDC flags pointing to entra-mock
  (`--oidc-issuer-url`, `--oidc-client-id=picoshift`, `--oidc-ca-file`)

Verify:
```bash
picoshift status
kubectl -n entra-mock get pods       # entra-mock running
sudo podman exec ocp-sim-control-plane grep oidc-issuer /etc/kubernetes/manifests/kube-apiserver.yaml
# --oidc-issuer-url=https://entra.apps.ocp-sim.test/a1b2c3d4-.../v2.0
```

## Step 2: Install ODH Operator

Install OLM, then the ODH operator:

```bash
picoshift olm install
picoshift olm operator install opendatahub-operator
```

Wait for the operator to create the dashboard, GatewayConfig, Gateway, and HTTPRoutes:

```bash
kubectl get pods -n opendatahub                  # odh-dashboard pods
kubectl get gateway -n openshift-ingress         # data-science-gateway
kubectl get httproutes -A                        # odh-dashboard, oauth-callback-route
kubectl get gatewayconfig default-gateway -o yaml | grep -A5 oidc
```

## Step 3: Install OSSM3 (Service Mesh)

The ODH gateway stack requires istiod to reconcile the Gateway CR into
Envoy deployments. See [docs/ossm.md](ossm.md) for the full procedure.

Summary:

```bash
# Add Red Hat operators catalog
picoshift olm catalog add redhat-operators \
  --image=registry.redhat.io/redhat/redhat-operator-index:v4.17 \
  --pull-secret=~/pull-secret.json

# Install full Gateway API CRDs (istiod needs v1beta1)
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.3.0/standard-install.yaml

# Create pull secret for operator images
kubectl get secret catalog-pull-redhat-operators -n olm \
  -o jsonpath='{.data.\.dockerconfigjson}' | base64 -d > /tmp/rh-pull.json
kubectl -n openshift-operators create secret docker-registry rh-pull-secret \
  --from-file=.dockerconfigjson=/tmp/rh-pull.json

# Install servicemeshoperator3
picoshift olm operator install servicemeshoperator3

# Attach pull secret to operator SA
kubectl -n openshift-operators patch serviceaccount servicemesh-operator3 \
  -p '{"imagePullSecrets": [{"name": "rh-pull-secret"}]}'

# Create Istio CR (see docs/ossm.md for full YAML)
kubectl create namespace istio-system
kubectl -n istio-system create secret docker-registry rh-pull-secret \
  --from-file=.dockerconfigjson=/tmp/rh-pull.json
kubectl -n istio-system patch serviceaccount default \
  -p '{"imagePullSecrets": [{"name": "rh-pull-secret"}]}'
# Apply Istio CR ... (see docs/ossm.md "Create the Istio CR")

# Fix API server audiences for istiod
sudo podman exec ocp-sim-control-plane sed -i \
  '/--service-account-issuer=/a\        - --api-audiences=https://kubernetes.default.svc.cluster.local,istio-ca' \
  /etc/kubernetes/manifests/kube-apiserver.yaml

# Attach pull secrets to istiod and envoy SAs
kubectl -n istio-system patch serviceaccount istiod-openshift-gateway \
  -p '{"imagePullSecrets": [{"name": "rh-pull-secret"}]}'
kubectl -n openshift-ingress create secret docker-registry rh-pull-secret \
  --from-file=.dockerconfigjson=/tmp/rh-pull.json
kubectl -n openshift-ingress patch serviceaccount \
  data-science-gateway-data-science-gateway-class \
  -p '{"imagePullSecrets": [{"name": "rh-pull-secret"}]}'
```

Verify istiod and Envoy are running:
```bash
kubectl get istio openshift-gateway              # STATE: Healthy
kubectl get pods -n istio-system                 # istiod running
kubectl get pods -n openshift-ingress            # envoy + kube-auth-proxy running
```

## Step 4: Patch GatewayConfig with OIDC Settings

The GatewayConfig tells kube-auth-proxy which OIDC provider to use. Patch it
to point at entra-mock:

```bash
# Create the client secret
kubectl create secret generic oidc-client-secret \
  --from-literal=client-secret=picoshift-secret \
  -n openshift-ingress --dry-run=client -o yaml | kubectl apply -f -

# Patch GatewayConfig
kubectl patch gatewayconfig default-gateway --type=merge \
  -p '{"spec":{"oidc":{"issuerURL":"https://entra.apps.ocp-sim.test/a1b2c3d4-e5f6-7890-abcd-ef1234567890/v2.0","clientID":"picoshift","clientSecretRef":{"name":"oidc-client-secret","key":"client-secret"},"verifyProviderCertificate":false}}}'
```

Wait for kube-auth-proxy to restart with the new OIDC config:
```bash
kubectl rollout status deployment/kube-auth-proxy -n openshift-ingress
```

## Verification

```bash
# Envoy has ext_authz + Lua filters from the EnvoyFilter CR
kubectl exec -n openshift-ingress \
  deployment/data-science-gateway-data-science-gateway-class \
  -- pilot-agent request GET /config_dump 2>/dev/null \
  | python3 -c "
import json, sys
data = json.load(sys.stdin)
for c in data.get('configs', []):
    if c.get('@type', '').endswith('ListenersConfigDump'):
        for l in c.get('dynamic_listeners', []):
            for fc in l['active_state']['listener']['filter_chains']:
                for f in fc['filters']:
                    for hf in f.get('typed_config', {}).get('http_filters', []):
                        print(hf['name'])
"
# Should include: envoy.filters.http.ext_authz, envoy.filters.http.lua

# End-to-end: unauthenticated request gets 302 to entra login
curl -sk https://rh-ai.apps.ocp-sim.test/ -D - -o /dev/null | head -3
# HTTP/1.1 302 Found
# location: https://entra.apps.ocp-sim.test/.../oauth2/v2.0/authorize?...
# server: istio-envoy

# Open in browser: https://rh-ai.apps.ocp-sim.test/
# Login with admin / admin (or any user from users.yaml)
# Dashboard should load after login
```

## Troubleshooting

### "invalid bearer token" from kube-rbac-proxy

```
E0521 auth.go:47] Unable to authenticate the request due to an error: invalid bearer token
```

The API server can't validate the entra-mock id_token. Check that the OIDC
flags were applied:

```bash
sudo podman exec ocp-sim-control-plane grep oidc /etc/kubernetes/manifests/kube-apiserver.yaml
```

Should show `--oidc-issuer-url=https://entra.apps.ocp-sim.test/...` and
`--oidc-client-id=picoshift`. If missing, the cluster was created without
the BYOIDC API server patch. Re-run with `picoshift create --auth-mode=byoidc`
or apply manually:

```bash
sudo podman exec ocp-sim-control-plane sh -c \
  'echo | openssl s_client -connect localhost:443 -servername entra.apps.ocp-sim.test 2>/dev/null \
  | openssl x509 -outform PEM > /etc/kubernetes/pki/oidc-ca.crt'

sudo podman exec ocp-sim-control-plane sed -i \
  "/--secure-port=16443/a\\        - --oidc-issuer-url=https://entra.apps.ocp-sim.test/a1b2c3d4-e5f6-7890-abcd-ef1234567890/v2.0\n        - --oidc-client-id=picoshift\n        - --oidc-username-claim=preferred_username\n        - --oidc-groups-claim=groups\n        - --oidc-username-prefix=-\n        - --oidc-groups-prefix=\n        - --oidc-ca-file=/etc/kubernetes/pki/oidc-ca.crt" \
  /etc/kubernetes/manifests/kube-apiserver.yaml
```

### Envoy crash-loop: "the token is not authenticated"

istiod authenticates Envoy via TokenReview with audience `istio-ca`. The API
server must include `istio-ca` in `--api-audiences`. See
[docs/ossm.md - API Server Configuration](ossm.md#api-server-configuration).

### CRD conflict: "risk of data loss updating ztunnels.sailoperator.io"

The community `sailoperator` CRDs conflict with `servicemeshoperator3`. Delete
the community sailoperator and its CRDs first. See
[docs/ossm.md - Known Issues](ossm.md#crd-conflict-with-community-sailoperator).

### Pull secrets needed in 3 namespaces

servicemeshoperator3 images are at `registry.redhat.io`. Pull secrets and SA
patches are needed in `openshift-operators`, `istio-system`, and
`openshift-ingress`. See [docs/ossm.md - Pull secrets](ossm.md#pull-secrets-needed-in-multiple-namespaces).

### kube-auth-proxy issuer mismatch

If kube-auth-proxy logs show issuer URL errors, verify the GatewayConfig OIDC
`issuerURL` matches entra-mock's discovery doc. The external URL must be used
(not the in-cluster service URL):

```bash
kubectl get gatewayconfig default-gateway -o jsonpath='{.spec.oidc.issuerURL}'
# https://entra.apps.ocp-sim.test/a1b2c3d4-e5f6-7890-abcd-ef1234567890/v2.0

curl -sk https://entra.apps.ocp-sim.test/a1b2c3d4-e5f6-7890-abcd-ef1234567890/v2.0/.well-known/openid-configuration | python3 -c "import json,sys; print(json.load(sys.stdin)['issuer'])"
# Must match the GatewayConfig issuerURL exactly
```
