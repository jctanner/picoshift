# Fraud Detection Tutorial on picoshift

## Context

The user wants to complete the RHOAI 3.4 fraud detection tutorial end-to-end on picoshift. This means:
- Create a "fraud-detection" project with a TensorFlow workbench
- Clone the tutorial repo, train the fraud model, save as ONNX to S3
- Deploy with OpenVINO Model Server
- Test predictions from inside the workbench

Most infrastructure exists (SeaweedFS, workbench creation, KServe). What's missing: TensorFlow workbench image loading, OpenVINO ServingRuntime, fraud-specific deployment script, and a connection secret for the workbench to reach S3.

## Steps

### 1. Create `scripts/deploy-fraud-tutorial.py` — orchestration script

Single script that automates the full tutorial flow. Steps:

1. **Pull and load images into kind** — TensorFlow workbench image + OpenVINO Model Server image
   - `quay.io/opendatahub/odh-workbench-jupyter-tensorflow-cuda-py312-ubi9@sha256:db9fb04f1ec59ea81b3a1f76fa25225560461111847395dd90e01cfc7d002004` (3.4 tag)
   - `quay.io/opendatahub/openvino_model_server:2025.1-release`
   
2. **Create fraud-detection namespace** — labeled for dashboard visibility

3. **Create workbench** — reuse `create-workbench.py` with:
   - `--project fraud-detection`
   - `--workbench fraud-detection`
   - `--image jupyter-tensorflow-notebook:3.4`
   - `--storage 20Gi` (tutorial uses 20GiB)

4. **Create S3 connection secret** in fraud-detection namespace — the tutorial calls this "My Storage", it's a Secret the workbench mounts as env vars so boto3 can reach SeaweedFS:
   ```yaml
   apiVersion: v1
   kind: Secret
   metadata:
     name: my-storage
     namespace: fraud-detection
     labels:
       opendatahub.io/dashboard: "true"
       opendatahub.io/managed: "true"
     annotations:
       opendatahub.io/connection-type: s3
       openshift.io/display-name: My Storage
   stringData:
     AWS_ACCESS_KEY_ID: picoshift
     AWS_SECRET_ACCESS_KEY: picoshift-secret
     AWS_S3_ENDPOINT: http://seaweedfs.seaweedfs.svc.cluster.local:8333
     AWS_DEFAULT_REGION: us-east-1
     AWS_S3_BUCKET: models
   ```

5. **Patch workbench to mount S3 env vars** — add envFrom secretRef to the Notebook CR so the workbench container has boto3 credentials. Alternatively, add this to `create-workbench.py` as a `--connection` flag.

6. **Wait for workbench pod readiness**

7. **Clone fraud-detection repo into workbench** — `kubectl exec` to run:
   ```bash
   cd /opt/app-root/src && git clone --branch v3.2 https://github.com/rh-aiservices-bu/fraud-detection.git
   ```

8. **Install Python deps and run training** — `kubectl exec` into the workbench pod to:
   ```bash
   pip install tf2onnx onnx
   cd /opt/app-root/src/fraud-detection
   jupyter nbconvert --to script 1_experiment_train.ipynb --stdout | python3
   jupyter nbconvert --to script 2_save_model.ipynb --stdout | python3
   ```
   This trains the model and uploads `models/fraud/1/model.onnx` to S3.

9. **Create storage-config secret** for KServe in fraud-detection namespace (same pattern as `deploy/storage-config.yaml` but in fraud-detection ns)

10. **Create OpenVINO ServingRuntime** in fraud-detection namespace

11. **Create InferenceService** for fraud model (with webhook workaround from existing script)

12. **Wait for predictor pod**

13. **Run inference test** from inside the workbench pod using the `3_rest_requests.ipynb` notebook (or a curl equivalent)

### 2. Create `deploy/ovms-serving-runtime.yaml`

Based on the ODH operator's template at `example.src/opendatahub-operator/opt/manifests/modelcontroller/runtimes/ovms-kserve-template.yaml`, but as a concrete ServingRuntime (not a Template):

```yaml
apiVersion: serving.kserve.io/v1alpha1
kind: ServingRuntime
metadata:
  name: kserve-ovms
  namespace: fraud-detection
  annotations:
    openshift.io/display-name: OpenVINO Model Server
  labels:
    opendatahub.io/dashboard: "true"
spec:
  multiModel: false
  supportedModelFormats:
    - name: openvino_ir
      version: opset13
      autoSelect: true
    - name: onnx
      version: "1"
    - name: tensorflow
      version: "1"
      autoSelect: true
    - name: tensorflow
      version: "2"
      autoSelect: true
  protocolVersions:
    - v2
    - grpc-v2
  containers:
    - name: kserve-container
      image: quay.io/opendatahub/openvino_model_server:2025.1-release
      args:
        - --model_name={{.Name}}
        - --port=8001
        - --rest_port=8888
        - --model_path=/mnt/models
        - --file_system_poll_wait_seconds=0
        - --metrics_enable
      ports:
        - containerPort: 8888
          protocol: TCP
```

### 3. Create `deploy/fraud-isvc.yaml`

```yaml
apiVersion: serving.kserve.io/v1beta1
kind: InferenceService
metadata:
  name: fraud
  namespace: fraud-detection
spec:
  predictor:
    model:
      modelFormat:
        name: onnx
        version: "1"
      storageUri: "s3://models/fraud"
      storage:
        key: "seaweedfs"
      resources:
        requests:
          cpu: 100m
          memory: 256Mi
        limits:
          cpu: 500m
          memory: 512Mi
```

### 4. Add Makefile target

```makefile
deploy-fraud-tutorial:
	python3 scripts/deploy-fraud-tutorial.py
```

### 5. Update `create-workbench.py` (optional enhancement)

Add a `--connection` flag to mount an S3 connection secret as env vars on the workbench container. This replicates the dashboard's "Attach existing connection" action from the tutorial.

## Key decisions

- **Image loading**: The TensorFlow workbench image is ~6GB. We need to `podman pull` and load into kind, same as we do for SeaweedFS. This will be slow on first run.
- **Training approach**: Run notebooks headlessly via `jupyter nbconvert --execute` inside the workbench pod. This matches the tutorial (training happens in the workbench) without requiring manual UI interaction.
- **OVMS port**: OpenVINO Model Server uses port 8888 for REST (not 8080 like sklearn). The tutorial confirms this — when testing from a workbench you hit `http://fraud-predictor.fraud-detection.svc.cluster.local:8888`.
- **Model path**: The tutorial saves to `models/fraud/1/model.onnx`. The InferenceService `storageUri` is `s3://models/fraud` — OVMS expects model versioning subdirectories.
- **Webhook workaround**: Reuse the same pattern from `deploy-model-serving.py` for creating the InferenceService.

## Files to create/modify

| File | Action |
|------|--------|
| `scripts/deploy-fraud-tutorial.py` | **New** — main orchestration script |
| `deploy/ovms-serving-runtime.yaml` | **New** — OpenVINO ServingRuntime |
| `deploy/fraud-isvc.yaml` | **New** — Fraud InferenceService |
| `Makefile` | **Modify** — add `deploy-fraud-tutorial` target |
| `scripts/create-workbench.py` | **Modify** — add `--connection` flag for S3 env vars |

## Verification

```bash
# Run the full tutorial
make deploy-fraud-tutorial

# Check workbench is running
kubectl get notebook fraud-detection -n fraud-detection

# Check model is in S3
kubectl exec -n seaweedfs deploy/seaweedfs -- ls /data/  # or use aws-cli pod

# Check OVMS predictor is running
kubectl get pods -n fraud-detection -l serving.kserve.io/inferenceservice=fraud

# Check InferenceService is ready
kubectl get inferenceservice fraud -n fraud-detection

# Test inference (from workbench or curl pod)
kubectl exec -n fraud-detection fraud-detection-0 -- \
  curl -s http://fraud-predictor.fraud-detection.svc.cluster.local:8888/v2/models/fraud/infer \
  -H 'Content-Type: application/json' \
  -d '{"inputs": [{"name": "dense_input", "shape": [1, 3], "datatype": "FP32", "data": [0.3111400080477545, 1.9459399775518593, 1.0]}]}'
```
