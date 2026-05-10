# Model Serving on picoshift with KServe + SeaweedFS

End-to-end guide for serving a model via KServe, using
[SeaweedFS](https://github.com/seaweedfs/seaweedfs) as the S3-compatible
object store.

## Overview

```
SeaweedFS (S3)                  KServe
  s3://models/sklearn/linear/ ──► InferenceService (project1)
  model.joblib (~5 KB)             storage-initializer downloads model
                                   kserve-sklearnserver serves predictions
                                   kube-rbac-proxy sidecar (injected by ODH)
```

The default setup uses the canonical KServe sklearn example model (Iris SVC
classifier, 4 features). You can also train your own model in a workbench
and upload it.

### Why SeaweedFS

- The ODH operator's own test suite uses it (`opt/manifests/kserve/overlays/test/s3-local-backend/`)
- Public image on Docker Hub: `docker.io/chrislusf/seaweedfs:4.07` — no build step
- Minimal footprint: `mini -dir=/data -s3` gives you a full S3 endpoint in one process
- Already validated against KServe's storage-initializer

## Quick start

```bash
make deploy-model-serving
```

This runs `scripts/deploy-model-serving.py` which:
1. Pulls and loads SeaweedFS image into kind
2. Deploys SeaweedFS (namespace, deployment, service)
3. Runs an init Job to create S3 bucket and upload the example model
4. Creates `storage-config` secret in project1
5. Patches KServe `ingressDomain` to `apps.ocp-sim.localhost`
6. Creates sklearn ServingRuntime in project1
7. Creates sklearn InferenceService in project1
8. Waits for predictor pod

**Note:** The odh-model-controller validating webhook has a rate-limiter
bug that causes InferenceService creation to time out. The script handles
this by temporarily scaling down the operator, clearing the webhook,
creating the ISVC, then scaling the operator back up.

## Testing

```bash
# From inside the cluster (project1 namespace):
kubectl run curl-test --rm -i --restart=Never --namespace=project1 \
  --image=curlimages/curl -- \
  curl -s http://sklearn-linear-predictor.project1.svc.cluster.local/v1/models/sklearn-linear:predict \
  -H 'Content-Type: application/json' \
  -d '{"instances": [[6.8, 2.8, 4.8, 1.4], [6.0, 3.4, 4.5, 1.6]]}'

# Expected: {"predictions":[1,1]}

# v2 API:
kubectl run curl-test2 --rm -i --restart=Never --namespace=project1 \
  --image=curlimages/curl -- \
  curl -s http://sklearn-linear-predictor.project1.svc.cluster.local/v2/models/sklearn-linear/infer \
  -H 'Content-Type: application/json' \
  -d '{"inputs": [{"name": "input-0", "shape": [2, 4], "datatype": "FP64", "data": [6.8, 2.8, 4.8, 1.4, 6.0, 3.4, 4.5, 1.6]}]}'

# Expected: {"model_name":"sklearn-linear", ..., "outputs":[{"name":"output-0","shape":[2],"datatype":"INT32","data":[1,1]}]}
```

The example model is a scikit-learn SVC trained on the Iris dataset
(4 features: sepal length, sepal width, petal length, petal width → class 0/1/2).

## Training your own model

From a Jupyter workbench:

```python
import numpy as np
import joblib
from sklearn.linear_model import LinearRegression

X = np.array([[1], [2], [3], [4], [5]], dtype=np.float64)
y = np.array([3, 5, 7, 9, 11], dtype=np.float64)

model = LinearRegression()
model.fit(X, y)
print(f"predict([10]) = {model.predict([[10]])[0]:.1f}")  # 21.0

joblib.dump(model, "/tmp/model.joblib")

# Upload via boto3
import boto3
s3 = boto3.client("s3",
    endpoint_url="http://seaweedfs.seaweedfs.svc.cluster.local:8333",
    aws_access_key_id="picoshift",
    aws_secret_access_key="picoshift-secret",
    region_name="us-east-1",
)
s3.upload_file("/tmp/model.joblib", "models", "sklearn/linear/model.joblib")
```

Then restart the predictor pod to pick up the new model:
```bash
kubectl rollout restart deployment sklearn-linear-predictor -n project1
```

## Manifests

All manifests are in `deploy/`:

| File | What |
|------|------|
| `seaweedfs.yaml` | Namespace, credentials secret, deployment, service |
| `seaweedfs-init.yaml` | Job: create bucket, download and upload example model |
| `storage-config.yaml` | KServe S3 credentials for project1 |
| `sklearn-serving-runtime.yaml` | ServingRuntime (namespaced, project1) |
| `sklearn-isvc.yaml` | InferenceService pointing at s3://models/sklearn/linear |

## Architecture notes

### Why project1, not opendatahub

The `opendatahub` namespace has restrictive NetworkPolicies that block
ingress from unlabeled namespaces. It also has multiple validating/mutating
webhooks with rate-limiter issues. Deploying in a user project namespace
avoids both problems.

### ClusterServingRuntime CRD missing

The ODH operator normally creates ClusterServingRuntimes, but the CRD
doesn't exist on picoshift. We use a namespaced `ServingRuntime` instead.
KServe checks both — namespaced runtimes take priority.

### kube-rbac-proxy sidecar

The odh-model-controller injects a kube-rbac-proxy sidecar into predictor
pods. This means:
- Port 8080 (sklearn server) is only accessible from localhost inside the pod
- Port 8443 (kube-rbac-proxy) serves HTTPS with auth
- The KServe-created Service maps port 80 → 8080, which works because
  KServe targets the pod directly and the container binds 0.0.0.0:8080

### RawDeployment mode

KServe is configured with `defaultDeploymentMode: RawDeployment`. This
creates a Deployment + Service directly (no Knative). The service name
is `<isvc-name>-predictor`. No Istio VirtualService or HTTPRoute is
created when `disableIngressCreation: true`.

### Webhook workaround

The odh-model-controller's validating webhook calls back to the API server
to fetch the DSCInitialization resource during validation. Under load, its
internal rate limiter saturates and the 10-second webhook timeout expires.
The deploy script works around this by temporarily removing the webhook
during ISVC creation.

## Resource usage

| Component | Image | Resources |
|-----------|-------|-----------|
| SeaweedFS | `docker.io/chrislusf/seaweedfs:4.07` | 10m CPU, 64Mi RAM |
| sklearn server | `docker.io/kserve/sklearnserver:v0.14.1` | 100m CPU, 256Mi RAM |
| storage-initializer | `quay.io/opendatahub/kserve-storage-initializer:odh-stable` | (init container) |
