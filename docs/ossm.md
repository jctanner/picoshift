# OpenShift Service Mesh 3 (OSSM3) on Picoshift

## Overview

On real OCP, the Gateway API flow works like this:

1. ODH operator installs, DSCI creates a `GatewayConfig`
2. The GatewayConfig controller creates a `GatewayClass` with controller `openshift.io/gateway-controller/v1`
3. The cluster-ingress-operator sees the GatewayClass and creates a `servicemeshoperator3` subscription via OLM
4. Sail/OSSM3 installs → istiod starts → reconciles Gateway resources, creates Envoy deployments + services

Since picoshift has no ingress operator, we install `servicemeshoperator3` and
create the `Istio` CR manually.

## Prerequisites

- OLM installed: `picoshift olm install`
- Red Hat operators catalog with pull secret:
  ```
  picoshift olm catalog add redhat-operators \
    --image=registry.redhat.io/redhat/redhat-operator-index:v4.17 \
    --pull-secret=~/pull-secret.json
  ```
- `ConsoleCLIDownload` stub CRD (included in `deploy/crds/openshift/`)
- Full Gateway API CRDs with v1 + v1beta1 (istiod requires v1beta1):
  ```bash
  kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.3.0/standard-install.yaml
  ```
- API server must accept `istio-ca` token audience (see [API Server Configuration](#api-server-configuration))

## Install Steps

### 1. Create pull secret in openshift-operators namespace

The operator image is at `registry.redhat.io` and needs auth in the namespace
where the pod runs, not just in the `olm` namespace:

```bash
kubectl get secret catalog-pull-redhat-operators -n olm \
  -o jsonpath='{.data.\.dockerconfigjson}' | base64 -d > /tmp/rh-pull.json

kubectl -n openshift-operators create secret docker-registry rh-pull-secret \
  --from-file=.dockerconfigjson=/tmp/rh-pull.json
```

### 2. Install servicemeshoperator3

```bash
picoshift olm operator install servicemeshoperator3
```

### 3. Attach pull secret to the operator service account

OLM creates the service account but doesn't attach the pull secret automatically:

```bash
kubectl -n openshift-operators patch serviceaccount servicemesh-operator3 \
  -p '{"imagePullSecrets": [{"name": "rh-pull-secret"}]}'
```

If the pod is already in `ErrImagePull`, delete it to restart with the new secret:

```bash
kubectl -n openshift-operators delete pod -l app.kubernetes.io/name=servicemesh-operator3
```

### 4. Create the Istio CR

On real OCP, the ingress operator creates this. We do it manually:

```bash
kubectl create namespace istio-system

# Pull secret needed here too for istiod and envoy proxy images
kubectl -n istio-system create secret docker-registry rh-pull-secret \
  --from-file=.dockerconfigjson=/tmp/rh-pull.json
kubectl -n istio-system patch serviceaccount default \
  -p '{"imagePullSecrets": [{"name": "rh-pull-secret"}]}'

cat <<'EOF' | kubectl apply -f -
apiVersion: sailoperator.io/v1
kind: Istio
metadata:
  name: openshift-gateway
spec:
  namespace: istio-system
  updateStrategy:
    type: InPlace
  values:
    global:
      istioNamespace: istio-system
      priorityClassName: system-cluster-critical
    pilot:
      enabled: true
      env:
        PILOT_ENABLE_GATEWAY_API: "true"
        PILOT_ENABLE_ALPHA_GATEWAY_API: "false"
        PILOT_ENABLE_GATEWAY_API_STATUS: "true"
        PILOT_ENABLE_GATEWAY_API_DEPLOYMENT_CONTROLLER: "true"
        PILOT_ENABLE_GATEWAY_API_GATEWAYCLASS_CONTROLLER: "false"
        PILOT_GATEWAY_API_DEFAULT_GATEWAYCLASS_NAME: "openshift-default"
        PILOT_GATEWAY_API_CONTROLLER_NAME: "openshift.io/gateway-controller/v1"
        PILOT_MULTI_NETWORK_DISCOVER_GATEWAY_API: "false"
        ENABLE_GATEWAY_API_MANUAL_DEPLOYMENT: "false"
        PILOT_ENABLE_GATEWAY_API_CA_CERT_ONLY: "true"
        PILOT_ENABLE_GATEWAY_API_COPY_LABELS_ANNOTATIONS: "false"
EOF
```

### 5. Attach pull secret to istiod service account

Once the Istio CR is reconciled, Sail creates a service account for istiod:

```bash
kubectl -n istio-system patch serviceaccount istiod-openshift-gateway \
  -p '{"imagePullSecrets": [{"name": "rh-pull-secret"}]}'
```

### 6. Attach pull secret to gateway Envoy pods

When istiod reconciles a Gateway CR, it creates an Envoy deployment with its
own service account. That SA also needs the pull secret:

```bash
kubectl -n openshift-ingress create secret docker-registry rh-pull-secret \
  --from-file=.dockerconfigjson=/tmp/rh-pull.json

# The SA name is derived from the Gateway name + GatewayClass name
kubectl -n openshift-ingress patch serviceaccount \
  data-science-gateway-data-science-gateway-class \
  -p '{"imagePullSecrets": [{"name": "rh-pull-secret"}]}'
```

## API Server Configuration

istiod authenticates Envoy proxy pods via Kubernetes TokenReview with audience
`istio-ca`. By default, kind's API server only accepts its own issuer URL as a
valid audience. The `--api-audiences` flag must include `istio-ca`.

This must be set in the kind cluster config **before** cluster creation:

```yaml
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
kubeadmConfigPatches:
  - |
    kind: ClusterConfiguration
    apiServer:
      extraArgs:
        api-audiences: "https://kubernetes.default.svc.cluster.local,istio-ca"
```

To fix an existing cluster, patch the API server manifest on the node:

```bash
sudo podman exec <cluster>-control-plane sed -i \
  '/--service-account-issuer=/a\        - --api-audiences=https://kubernetes.default.svc.cluster.local,istio-ca' \
  /etc/kubernetes/manifests/kube-apiserver.yaml
```

The API server restarts automatically. Without this, Envoy pods crash-loop with:
```
KubeJWTAuthenticator: failed to validate the JWT from cluster "Kubernetes": the token is not authenticated
```

## What Happens After Setup

Once istiod is healthy and the `data-science-gateway` Gateway CR exists (created
by ODH's GatewayConfig controller):

1. istiod sees the Gateway → creates Envoy Deployment + ClusterIP Service in `openshift-ingress`
2. Envoy authenticates with istiod via `istio-ca` audience SA token
3. istiod pushes xDS config to Envoy based on attached HTTPRoutes
4. The simulator proxies `rh-ai.apps.ocp-sim.test` to the Envoy service
5. Envoy routes to kube-auth-proxy (OIDC) → backend services

## Known Issues

### CRD conflict with community sailoperator

If ODH was installed first and OLM pulled in community `sailoperator` CRDs to
satisfy dependencies, `servicemeshoperator3` will fail with:

```
risk of data loss updating "ztunnels.sailoperator.io": new CRD removes
version v1 that is listed as a stored version on the existing CRD
```

The community sailoperator ships CRDs with `storedVersions: [v1]` while
servicemeshoperator3 ships them with `v1alpha1` only.

**Fix**: delete the community sailoperator and all its CRDs before installing
servicemeshoperator3:

```bash
kubectl delete subscription sailoperator -n openshift-operators --ignore-not-found
kubectl delete csv sailoperator.v1.29.2 -n openshift-operators --ignore-not-found

# Delete all Sail/Istio CRDs owned by community sailoperator
kubectl get crd -o json | python3 -c "
import sys, json
data = json.load(sys.stdin)
for item in data['items']:
    labels = item['metadata'].get('labels', {})
    if 'operators.coreos.com/sailoperator.openshift-operators' in labels:
        print(item['metadata']['name'])
" | xargs kubectl delete crd

picoshift olm operator install servicemeshoperator3
```

On real OCP this conflict doesn't occur because both ODH and servicemeshoperator3
come from the `redhat-operators` catalog with compatible CRD versions.

### ConsoleCLIDownload CRD required

servicemeshoperator3 bundles an `istioctl` ConsoleCLIDownload resource. The
install plan fails if the CRD doesn't exist:

```
Unable to find GVK in discovery: console.openshift.io v1 ConsoleCLIDownload
```

Picoshift includes a stub CRD at `deploy/crds/openshift/console.openshift.io_consoleclidownloads.yaml`.

### Gateway API CRDs need v1beta1

Picoshift's stub Gateway API CRDs only serve `v1`. istiod requires `v1beta1`
and gets stuck in a sync loop without it. Install the upstream standard CRDs:

```bash
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.3.0/standard-install.yaml
```

### Pull secrets needed in multiple namespaces

servicemeshoperator3 images are at `registry.redhat.io`. Pull secrets must be
created and attached to service accounts in:

- `openshift-operators` — for the Sail operator pod
- `istio-system` — for istiod
- `openshift-ingress` — for Envoy gateway pods

## Verification

```bash
# Sail operator running
kubectl get csv -n openshift-operators | grep servicemesh
# servicemeshoperator3.v3.1.8   Succeeded

# istiod healthy
kubectl get istio openshift-gateway
# STATE: Healthy

kubectl get pods -n istio-system
# istiod-openshift-gateway   1/1   Running

# Gateway reconciled with Envoy pod
kubectl get pods -n openshift-ingress
# data-science-gateway-...   1/1   Running
# kube-auth-proxy-...        1/1   Running

# End-to-end test
curl -sk https://rh-ai.apps.ocp-sim.test/
# Should redirect to OIDC login
```
