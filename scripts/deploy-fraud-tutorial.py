#!/usr/bin/env python3
"""Deploy the RHOAI fraud detection tutorial end-to-end on picoshift.

Steps:
  1.  Load TensorFlow workbench + OVMS images into kind
  2.  Create fraud-detection workbench (reuses create-workbench.py)
  3.  Create S3 connection secret (my-storage)
  4.  Patch workbench to mount S3 env vars
  5.  Grant notebook RBAC for programmatic access
  6.  Clone fraud-detection repo + install deps via NotebookRunner
  7.  Run training notebook (1_experiment_train.ipynb)
  8.  Run save-model notebook (2_save_model.ipynb)
  9.  Deploy OVMS ServingRuntime + InferenceService
  10. Test inference from workbench
"""

import asyncio
import json
import os
import subprocess
import sys
import time
import warnings

warnings.filterwarnings("ignore", message="Unverified HTTPS")

CLUSTER_NAME = "ocp-sim"
NAMESPACE = "fraud-detection"
WORKBENCH = "fraud-detection"
GATEWAY_HOST = "rh-ai.apps.ocp-sim.test"
WORKBENCH_URL = f"https://{GATEWAY_HOST}/notebook/{NAMESPACE}/{WORKBENCH}"

TF_IMAGE = (
    "quay.io/opendatahub/odh-workbench-jupyter-tensorflow-cuda-py312-ubi9"
    "@sha256:db9fb04f1ec59ea81b3a1f76fa25225560461111847395dd90e01cfc7d002004"
)
OVMS_IMAGE = "quay.io/opendatahub/openvino_model_server:2025.1-release"

DEPLOY_DIR = os.path.join(os.path.dirname(__file__), "..", "deploy")
SCRIPTS_DIR = os.path.dirname(__file__)

OPERATOR_NS = "opendatahub-operator-system"
OPERATOR_DEPLOY = "opendatahub-operator-controller-manager"

WEBHOOKS_TO_CLEAR = {
    "validating": [
        "validating.odh-model-controller.opendatahub.io",
        "opendatahub-operator-validating-webhook-configuration",
    ],
    "mutating": [
        "mutating.odh-model-controller.opendatahub.io",
        "opendatahub-operator-mutating-webhook-configuration",
    ],
}


# ---------------------------------------------------------------------------
# Shell helpers (same pattern as deploy-model-serving.py)
# ---------------------------------------------------------------------------

def run(cmd, check=True, quiet=False, **kwargs):
    if not quiet:
        print(f"  $ {' '.join(cmd)}")
    result = subprocess.run(cmd, capture_output=True, text=True, **kwargs)
    if check and result.returncode != 0:
        print(f"  ERROR: {result.stderr.strip()}", file=sys.stderr)
        sys.exit(1)
    return result


def kubectl(*args, check=True, quiet=False, input=None):
    return run(["kubectl"] + list(args), check=check, quiet=quiet, input=input)


def kubectl_apply(manifest):
    return kubectl("apply", "-f", "-", input=json.dumps(manifest))


def sudo(*args, **kwargs):
    return run(["sudo"] + list(args), **kwargs)


def image_loaded(image_ref):
    """Check if an image is already loaded in the kind node."""
    result = subprocess.run(
        ["sudo", "podman", "exec", f"{CLUSTER_NAME}-control-plane",
         "ctr", "--namespace=k8s.io", "images", "ls", "-q"],
        capture_output=True, text=True,
    )
    return image_ref in result.stdout


def load_image_into_kind(image, tar_name="image.tar"):
    """Pull image with podman and load into kind node."""
    if image_loaded(image):
        print(f"  Image already loaded: {image[:80]}...")
        return

    tar_path = f"/tmp/{tar_name}"
    print(f"  Pulling {image[:80]}...")
    sudo("podman", "pull", image)

    print("  Saving to tar...")
    sudo("podman", "save", image, "--format", "oci-archive", "-o", tar_path)

    print("  Loading into kind node...")
    result = subprocess.run(
        ["sudo", "podman", "exec", "-i", f"{CLUSTER_NAME}-control-plane",
         "ctr", "--namespace=k8s.io", "images", "import", "--no-unpack", "-"],
        stdin=open(tar_path, "rb"),
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        print(f"  ERROR: {result.stderr.strip()}", file=sys.stderr)
        sys.exit(1)
    print(f"  {result.stdout.strip()}")
    sudo("rm", "-f", tar_path)


def wait_for_pod(namespace, pod_name, timeout=300):
    """Wait for a specific pod to be Ready."""
    print(f"  Waiting for pod {pod_name} (up to {timeout}s)...")
    deadline = time.time() + timeout

    while time.time() < deadline:
        result = kubectl("get", "pod", pod_name, "-n", namespace,
                         "-o", "jsonpath={.status.phase}", check=False, quiet=True)
        if result.returncode == 0 and result.stdout.strip():
            break
        time.sleep(5)
    else:
        print("  Pod not created yet")
        kubectl("get", "pods", "-n", namespace, check=False)
        return False

    remaining = max(1, int(deadline - time.time()))
    result = kubectl("wait", "--namespace", namespace,
                     "--for=condition=Ready", f"pod/{pod_name}",
                     f"--timeout={remaining}s", check=False)
    if result.returncode != 0:
        print("  Pod not ready yet:")
        kubectl("get", "pods", "-n", namespace, check=False)
        return False
    print(f"  Pod {pod_name} is ready")
    return True


def get_sa_token(namespace, sa="default"):
    result = kubectl("create", "token", sa, "-n", namespace, quiet=True)
    return result.stdout.strip()


# ---------------------------------------------------------------------------
# Step 1: Load images
# ---------------------------------------------------------------------------

def step_load_images():
    print("[1/10] Loading images into kind...")
    load_image_into_kind(TF_IMAGE, "tf-workbench.tar")
    load_image_into_kind(OVMS_IMAGE, "ovms.tar")


# ---------------------------------------------------------------------------
# Step 2: Create workbench
# ---------------------------------------------------------------------------

def step_create_workbench():
    print("[2/10] Creating fraud-detection workbench...")
    run([
        sys.executable, os.path.join(SCRIPTS_DIR, "create-workbench.py"),
        "--project", NAMESPACE,
        "--workbench", WORKBENCH,
        "--image", "jupyter-tensorflow-notebook:3.4",
        "--storage", "20Gi",
    ])


# ---------------------------------------------------------------------------
# Step 3: Create S3 connection secret
# ---------------------------------------------------------------------------

def step_create_s3_connection():
    print("[3/10] Creating S3 connection secret (my-storage)...")
    kubectl_apply({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": "my-storage",
            "namespace": NAMESPACE,
            "labels": {
                "opendatahub.io/dashboard": "true",
                "opendatahub.io/managed": "true",
            },
            "annotations": {
                "opendatahub.io/connection-type": "s3",
                "openshift.io/display-name": "My Storage",
            },
        },
        "stringData": {
            "AWS_ACCESS_KEY_ID": "picoshift",
            "AWS_SECRET_ACCESS_KEY": "picoshift-secret",
            "AWS_S3_ENDPOINT": "http://seaweedfs.seaweedfs.svc.cluster.local:8333",
            "AWS_DEFAULT_REGION": "us-east-1",
            "AWS_S3_BUCKET": "models",
        },
    })


# ---------------------------------------------------------------------------
# Step 4: Patch workbench to mount S3 env vars
# ---------------------------------------------------------------------------

def step_patch_workbench_env():
    print("[4/10] Patching workbench to mount S3 connection and bump resources...")

    result = kubectl("get", "notebook", WORKBENCH, "-n", NAMESPACE,
                     "-o", "json", quiet=True)
    nb = json.loads(result.stdout)
    containers = nb["spec"]["template"]["spec"]["containers"]
    container = containers[0]

    patches = []

    env_from = container.get("envFrom", [])
    if not any(ef.get("secretRef", {}).get("name") == "my-storage" for ef in env_from):
        if env_from:
            patches.append({
                "op": "add",
                "path": "/spec/template/spec/containers/0/envFrom/-",
                "value": {"secretRef": {"name": "my-storage"}},
            })
        else:
            patches.append({
                "op": "add",
                "path": "/spec/template/spec/containers/0/envFrom",
                "value": [{"secretRef": {"name": "my-storage"}}],
            })

    resources = container.get("resources", {})
    if resources.get("limits", {}).get("cpu") != "4":
        patches.append({
            "op": "replace",
            "path": "/spec/template/spec/containers/0/resources",
            "value": {
                "limits": {"cpu": "4", "memory": "4Gi"},
                "requests": {"cpu": "2", "memory": "2Gi"},
            },
        })

    if not patches:
        print("  Already configured")
        return

    kubectl("patch", "notebook", WORKBENCH, "-n", NAMESPACE,
            "--type=json", f"-p={json.dumps(patches)}")

    print("  Waiting for pod restart...")
    time.sleep(5)
    wait_for_pod(NAMESPACE, f"{WORKBENCH}-0", timeout=300)


# ---------------------------------------------------------------------------
# Step 5: Grant notebook RBAC for programmatic access
# ---------------------------------------------------------------------------

def step_grant_rbac():
    print("[5/10] Granting notebook RBAC for programmatic access...")
    kubectl_apply({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "Role",
        "metadata": {
            "name": "notebook-access",
            "namespace": NAMESPACE,
        },
        "rules": [{
            "apiGroups": ["kubeflow.org"],
            "resources": ["notebooks"],
            "resourceNames": [WORKBENCH],
            "verbs": ["get"],
        }],
    })
    kubectl_apply({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "RoleBinding",
        "metadata": {
            "name": "notebook-access",
            "namespace": NAMESPACE,
        },
        "subjects": [{
            "kind": "ServiceAccount",
            "name": "default",
            "namespace": NAMESPACE,
        }],
        "roleRef": {
            "kind": "Role",
            "name": "notebook-access",
            "apiGroup": "rbac.authorization.k8s.io",
        },
    })


# ---------------------------------------------------------------------------
# Steps 6-8, 10: Notebook execution via NotebookRunner
# ---------------------------------------------------------------------------

async def step_run_notebooks():
    from odh_mcp.notebook import NotebookRunner

    token = get_sa_token(NAMESPACE)
    runner = NotebookRunner(WORKBENCH_URL, token=token, verify_ssl=False)

    try:
        # Step 6: Clone repo + install deps
        print("[6/10] Cloning fraud-detection repo and installing deps...")
        runner.start_kernel()

        # The workbench sets GIT_SSL_CAINFO, PIP_CERT, REQUESTS_CA_BUNDLE,
        # and pip index-url to Red Hat internal endpoints. Override them so
        # git clone and pip install can reach public internet.
        outputs = await runner.execute_code_async(
            "import subprocess, os\n"
            "env = os.environ.copy()\n"
            "env.pop('GIT_SSL_CAINFO', None)\n"
            "repo_dir = '/opt/app-root/src/fraud-detection'\n"
            "if not os.path.isdir(repo_dir):\n"
            "    r = subprocess.run(\n"
            "        ['git', 'clone', '--branch', 'v3.2',\n"
            "         'https://github.com/rh-aiservices-bu/fraud-detection.git'],\n"
            "        cwd='/opt/app-root/src', env=env, capture_output=True, text=True)\n"
            "    print(r.stdout)\n"
            "    print(r.stderr)\n"
            "    if r.returncode != 0:\n"
            "        raise RuntimeError(f'git clone failed: {r.stderr}')\n"
            "else:\n"
            "    print('Repo already cloned')\n"
            "os.chdir(repo_dir)\n"
            "print(f'CWD: {os.getcwd()}')\n",
            timeout=120,
        )
        for out in outputs:
            print(f"  {out}")

        # Fix pip and SSL config for the rest of the session so notebook
        # cells that run !pip install can reach upstream PyPI.
        outputs = await runner.execute_code_async(
            "import os\n"
            "os.environ.pop('PIP_CERT', None)\n"
            "os.environ.pop('REQUESTS_CA_BUNDLE', None)\n"
            "os.environ.pop('SSL_CERT_FILE', None)\n"
            "os.environ['PIP_INDEX_URL'] = 'https://pypi.org/simple/'\n"
            "print('pip/SSL env vars fixed')\n",
            timeout=10,
        )
        for out in outputs:
            print(f"  {out}")

        # Pre-install notebook dependencies via subprocess.run (reliable)
        # rather than relying on !pip shell magic which can timeout.
        print("  Pre-installing notebook dependencies...")
        outputs = await runner.execute_code_async(
            "import subprocess, sys\n"
            "cmds = [\n"
            "    [sys.executable, '-m', 'pip', 'install', '--upgrade', 'pip'],\n"
            "    [sys.executable, '-m', 'pip', 'install',\n"
            "     'onnx==1.17.0', 'onnxruntime==1.19.2', 'tf2onnx==1.16.1'],\n"
            "    [sys.executable, '-m', 'pip', 'install', '--upgrade',\n"
            "     'protobuf==5.28.3'],\n"
            "]\n"
            "for cmd in cmds:\n"
            "    r = subprocess.run(cmd, capture_output=True, text=True)\n"
            "    last_lines = r.stdout.strip().split('\\n')[-3:]\n"
            "    print('\\n'.join(last_lines))\n"
            "    if r.returncode != 0:\n"
            "        print(f'FAILED: {r.stderr[-300:]}')\n"
            "        raise RuntimeError(f'pip install failed')\n"
            "print('All deps installed')\n",
            timeout=300,
        )
        for out in outputs:
            print(f"  {out}")

        # Step 7: Run training notebook
        print()
        print("[7/10] Running training notebook (1_experiment_train.ipynb)...")
        results = await runner.execute_notebook(
            "fraud-detection/1_experiment_train.ipynb",
            timeout_per_cell=1200,
        )
        failed = [r for r in results if not r.ok]
        print(f"  Executed {len(results)} cells ({len(failed)} failed)")
        for r in results:
            print(f"    {r.summary()}")
        if failed:
            print("  ERROR: Training notebook failed", file=sys.stderr)
            sys.exit(1)

        # Step 8: Run save-model notebook
        print()
        print("[8/10] Running save-model notebook (2_save_model.ipynb)...")
        results = await runner.execute_notebook(
            "fraud-detection/2_save_model.ipynb",
            timeout_per_cell=600,
        )
        failed = [r for r in results if not r.ok]
        print(f"  Executed {len(results)} cells ({len(failed)} failed)")
        for r in results:
            print(f"    {r.summary()}")
        if failed:
            print("  ERROR: Save-model notebook failed", file=sys.stderr)
            sys.exit(1)

        # Verify model in S3
        print()
        print("  Verifying model in S3...")
        outputs = await runner.execute_code_async(
            "import boto3, os\n"
            "s3 = boto3.client('s3',\n"
            "    endpoint_url=os.environ['AWS_S3_ENDPOINT'],\n"
            "    aws_access_key_id=os.environ['AWS_ACCESS_KEY_ID'],\n"
            "    aws_secret_access_key=os.environ['AWS_SECRET_ACCESS_KEY'],\n"
            ")\n"
            "resp = s3.list_objects_v2(Bucket='models', Prefix='fraud/')\n"
            "for obj in resp.get('Contents', []):\n"
            "    print(f\"  {obj['Key']}  ({obj['Size']} bytes)\")\n",
            timeout=30,
        )
        for out in outputs:
            print(f"  {out}")

        return runner

    except Exception as e:
        runner.shutdown()
        raise


# ---------------------------------------------------------------------------
# Step 9: Deploy model serving
# ---------------------------------------------------------------------------

def step_deploy_model_serving():
    print("[9/10] Deploying OVMS model serving...")

    # The odh-model-controller watches data connection secrets
    # (annotated with opendatahub.io/connection-type: s3) and
    # automatically generates the storage-config secret from them.
    # The my-storage secret created in step 3 is the data connection;
    # the InferenceService references it via storage.key: my-storage.

    # Create ServingRuntime
    print("  Creating OVMS ServingRuntime...")
    kubectl("apply", "-f", os.path.join(DEPLOY_DIR, "ovms-serving-runtime.yaml"))

    # Create InferenceService (with webhook workaround)
    print("  Creating InferenceService...")
    isvc_path = os.path.join(DEPLOY_DIR, "fraud-isvc.yaml")
    result = kubectl("apply", "-f", isvc_path, check=False)
    if result.returncode != 0:
        if "failed calling webhook" not in result.stderr:
            print(f"  ERROR: {result.stderr.strip()}", file=sys.stderr)
            sys.exit(1)

        print("  Webhook timeout — applying workaround...")
        kubectl("scale", "deployment", OPERATOR_DEPLOY, "-n", OPERATOR_NS,
                "--replicas=0", quiet=True)
        time.sleep(5)

        for kind, names in WEBHOOKS_TO_CLEAR.items():
            resource = f"{kind}webhookconfiguration"
            for name in names:
                kubectl("delete", resource, name,
                        "--ignore-not-found", check=False, quiet=True)

        kubectl("apply", "-f", isvc_path)

        print("  Scaling ODH operator back up...")
        kubectl("scale", "deployment", OPERATOR_DEPLOY, "-n", OPERATOR_NS,
                "--replicas=1", quiet=True)

    # Wait for predictor pod
    print("  Waiting for predictor pod...")
    deadline = time.time() + 180
    pod_name = None
    while time.time() < deadline:
        result = kubectl("get", "pods", "-n", NAMESPACE,
                         "-l", "serving.kserve.io/inferenceservice=fraud",
                         "-o", "jsonpath={.items[*].metadata.name}",
                         check=False, quiet=True)
        if result.stdout.strip():
            pod_name = result.stdout.strip().split()[0]
            break
        time.sleep(5)

    if pod_name:
        print(f"  Found pod: {pod_name}")
        kubectl("wait", "--namespace", NAMESPACE,
                "--for=condition=Ready", f"pod/{pod_name}",
                "--timeout=120s", check=False)
    else:
        print("  WARNING: Predictor pod not created yet")
        kubectl("get", "pods", "-n", NAMESPACE, check=False)


# ---------------------------------------------------------------------------
# Step 10: Test inference
# ---------------------------------------------------------------------------

async def step_test_inference(runner):
    print("[10/10] Testing inference from workbench...")

    test_code = """
import requests, json

url = "http://fraud-predictor.fraud-detection.svc.cluster.local/v2/models/fraud/infer"
payload = {
    "inputs": [{
        "name": "dense_input",
        "shape": [1, 5],
        "datatype": "FP32",
        "data": [0.3111400080477545, 1.9459399775518593, 1.0, 0.0, 0.0]
    }]
}

resp = requests.post(url, json=payload, timeout=30)
print(f"Status: {resp.status_code}")
print(f"Response: {json.dumps(resp.json(), indent=2)}")
"""
    outputs = await runner.execute_code_async(test_code, timeout=30)
    for out in outputs:
        print(f"  {out}")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

async def async_main():
    # Steps 1-5: Infrastructure (synchronous)
    step_load_images()
    print()

    step_create_workbench()
    print()

    step_create_s3_connection()
    print()

    step_patch_workbench_env()
    print()

    step_grant_rbac()
    print()

    # Steps 6-8: Notebook execution (async)
    runner = await step_run_notebooks()
    print()

    # Step 9: Model serving (synchronous)
    step_deploy_model_serving()
    print()

    # Step 10: Inference test (async, reusing the runner/kernel)
    await step_test_inference(runner)
    runner.shutdown()
    print()

    # Summary
    print("=== Done ===")
    result = kubectl("get", "inferenceservice", "fraud", "-n", NAMESPACE,
                     "-o", "jsonpath={.status.conditions[?(@.type==\"Ready\")].status}",
                     check=False, quiet=True)
    ready = result.stdout.strip()
    print(f"  InferenceService ready: {ready or 'unknown'}")
    print(f"  Workbench URL: {WORKBENCH_URL}")
    print(f"  Inference endpoint: http://fraud-predictor.{NAMESPACE}.svc.cluster.local/v2/models/fraud/infer")


def main():
    asyncio.run(async_main())


if __name__ == "__main__":
    main()
