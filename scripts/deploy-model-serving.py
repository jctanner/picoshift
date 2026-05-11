#!/usr/bin/env python3
"""Deploy model serving infrastructure: SeaweedFS + sklearn ServingRuntime + InferenceService.

Steps:
  1. Load SeaweedFS image into kind
  2. Deploy SeaweedFS (namespace, deployment, service)
  3. Run S3 init job (create bucket, upload sklearn model)
  4. Create storage-config secret in project1
  5. Patch KServe inferenceservice-config (ingressDomain)
  6. Create sklearn ServingRuntime in project1
  7. Create sklearn InferenceService in project1
     (works around odh-model-controller webhook rate-limiter bug)
  8. Wait for predictor pod to become ready
"""

import json
import subprocess
import sys
import time

CLUSTER_NAME = "ocp-sim"
SEAWEEDFS_IMAGE = "docker.io/chrislusf/seaweedfs:4.07"
ISVC_NAMESPACE = "project1"

DEPLOY_DIR = "deploy"
MANIFESTS = {
    "seaweedfs": f"{DEPLOY_DIR}/seaweedfs.yaml",
    "seaweedfs-init": f"{DEPLOY_DIR}/seaweedfs-init.yaml",
    "storage-config": f"{DEPLOY_DIR}/storage-config.yaml",
    "sklearn-serving-runtime": f"{DEPLOY_DIR}/sklearn-serving-runtime.yaml",
    "sklearn-isvc": f"{DEPLOY_DIR}/sklearn-isvc.yaml",
}

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


def sudo(*args, **kwargs):
    return run(["sudo"] + list(args), **kwargs)


def load_image_into_kind(image):
    """Pull image with podman and load into kind node."""
    print(f"  Pulling {image}...")
    sudo("podman", "pull", image)

    print("  Saving to tar...")
    sudo("podman", "save", image,
         "--format", "oci-archive", "-o", "/tmp/seaweedfs.tar")

    print("  Loading into kind node...")
    result = subprocess.run(
        ["sudo", "podman", "exec", "-i", f"{CLUSTER_NAME}-control-plane",
         "ctr", "--namespace=k8s.io", "images", "import", "--no-unpack", "-"],
        stdin=open("/tmp/seaweedfs.tar", "rb"),
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        print(f"  ERROR: {result.stderr.strip()}", file=sys.stderr)
        sys.exit(1)
    print(f"  {result.stdout.strip()}")

    sudo("rm", "-f", "/tmp/seaweedfs.tar")


def wait_for_pod(namespace, label, timeout=60):
    kubectl("wait", "--namespace", namespace,
            "--for=condition=Ready", "pod",
            f"--selector={label}",
            f"--timeout={timeout}s")


def wait_for_job(namespace, job_name, timeout=120):
    deadline = time.time() + timeout
    while time.time() < deadline:
        result = kubectl("get", "job", job_name, "-n", namespace,
                         "-o", "jsonpath={.status.succeeded}",
                         check=False, quiet=True)
        if result.stdout.strip() == "1":
            return
        time.sleep(5)
    print(f"  ERROR: Job {job_name} did not complete within {timeout}s", file=sys.stderr)
    kubectl("logs", f"job/{job_name}", "-n", namespace, check=False)
    sys.exit(1)


def patch_ingress_domain():
    """Patch inferenceservice-config to set ingressDomain."""
    result = kubectl("get", "configmap", "inferenceservice-config",
                     "-n", "opendatahub", "-o", "json", quiet=True)
    cm = json.loads(result.stdout)
    ingress = json.loads(cm["data"]["ingress"])

    if ingress.get("ingressDomain") == "apps.ocp-sim.test":
        print("  Already patched")
        return

    ingress["ingressDomain"] = "apps.ocp-sim.test"
    cm["data"]["ingress"] = json.dumps(ingress)

    kubectl("apply", "-f", "-", input=json.dumps(cm))


def apply_isvc_with_webhook_workaround():
    """Create InferenceService, working around the odh-model-controller webhook bug.

    The odh-model-controller's validating webhook hits its internal API client
    rate limiter when fetching DSCInitialization, causing a 10s timeout on every
    InferenceService create/update. The operator reconciles the webhook config,
    so we must scale it down first, delete the webhooks, apply the ISVC, then
    scale back up.
    """
    # Try the happy path first
    result = kubectl("apply", "-f", MANIFESTS["sklearn-isvc"], check=False)
    if result.returncode == 0:
        return

    if "failed calling webhook" not in result.stderr:
        print(f"  ERROR: {result.stderr.strip()}", file=sys.stderr)
        sys.exit(1)

    print("  Webhook timeout detected — applying workaround...")

    # Scale down operator
    print("  Scaling down ODH operator...")
    kubectl("scale", "deployment", OPERATOR_DEPLOY, "-n", OPERATOR_NS,
            "--replicas=0", quiet=True)
    time.sleep(5)

    # Delete webhooks the operator manages
    for kind, names in WEBHOOKS_TO_CLEAR.items():
        resource = f"{kind}webhookconfiguration"
        for name in names:
            kubectl("delete", resource, name,
                    "--ignore-not-found", check=False, quiet=True)

    # Apply the InferenceService
    kubectl("apply", "-f", MANIFESTS["sklearn-isvc"])

    # Scale operator back up
    print("  Scaling ODH operator back up...")
    kubectl("scale", "deployment", OPERATOR_DEPLOY, "-n", OPERATOR_NS,
            "--replicas=1", quiet=True)


def main():
    print("=== Model Serving Deployment ===")
    print()

    # 1. Load SeaweedFS image
    print("[1/8] Loading SeaweedFS image into kind...")
    load_image_into_kind(SEAWEEDFS_IMAGE)
    print()

    # 2. Deploy SeaweedFS
    print("[2/8] Deploying SeaweedFS...")
    kubectl("apply", "-f", MANIFESTS["seaweedfs"])
    wait_for_pod("seaweedfs", "app=seaweedfs", timeout=60)
    print()

    # 3. Init S3 bucket and upload model
    print("[3/8] Initializing S3 bucket and uploading model...")
    kubectl("delete", "job", "s3-init", "-n", "seaweedfs",
            "--ignore-not-found", quiet=True)
    kubectl("apply", "-f", MANIFESTS["seaweedfs-init"])
    wait_for_job("seaweedfs", "s3-init", timeout=120)
    print("  Job completed:")
    result = kubectl("logs", "job/s3-init", "-n", "seaweedfs", quiet=True)
    for line in result.stdout.strip().split("\n"):
        print(f"    {line}")
    print()

    # 4. Create storage-config secret
    print(f"[4/8] Creating storage-config secret in {ISVC_NAMESPACE}...")
    kubectl("apply", "-f", MANIFESTS["storage-config"])
    print()

    # 5. Patch KServe ingress config
    print("[5/8] Patching KServe ingressDomain...")
    patch_ingress_domain()
    print()

    # 6. Create ServingRuntime
    print(f"[6/8] Creating sklearn ServingRuntime in {ISVC_NAMESPACE}...")
    kubectl("apply", "-f", MANIFESTS["sklearn-serving-runtime"])
    print()

    # 7. Create InferenceService (with webhook workaround)
    print(f"[7/8] Creating sklearn InferenceService in {ISVC_NAMESPACE}...")
    apply_isvc_with_webhook_workaround()
    print()

    # 8. Wait for predictor
    print("[8/8] Waiting for predictor pod...")
    deadline = time.time() + 180
    pod_found = False
    while time.time() < deadline:
        result = kubectl("get", "pods", "-n", ISVC_NAMESPACE,
                         "-l", "serving.kserve.io/inferenceservice=sklearn-linear",
                         "-o", "jsonpath={.items[*].metadata.name}",
                         check=False, quiet=True)
        if result.stdout.strip():
            pod_found = True
            break
        time.sleep(5)

    if not pod_found:
        print("  WARNING: Predictor pod not created yet")
        print(f"  Check: kubectl get pods -n {ISVC_NAMESPACE} | grep sklearn")
        print(f"  Check: kubectl describe inferenceservice sklearn-linear -n {ISVC_NAMESPACE}")
    else:
        pod_name = result.stdout.strip().split()[0]
        print(f"  Found pod: {pod_name}")
        kubectl("wait", "--namespace", ISVC_NAMESPACE,
                "--for=condition=Ready", f"pod/{pod_name}",
                "--timeout=120s", check=False)

    print()

    # Summary
    print("=== Done ===")
    print()
    result = kubectl("get", "inferenceservice", "sklearn-linear",
                     "-n", ISVC_NAMESPACE,
                     "-o", "jsonpath={.status.conditions[?(@.type==\"Ready\")].status}",
                     check=False, quiet=True)
    ready = result.stdout.strip()
    print(f"  InferenceService ready: {ready or 'unknown'}")
    print()
    print("  Test with:")
    print(f"    kubectl run curl-test --rm -i --restart=Never --namespace={ISVC_NAMESPACE} \\")
    print(f"      --image=curlimages/curl -- \\")
    print(f"      curl -s http://sklearn-linear-predictor.{ISVC_NAMESPACE}.svc.cluster.local/v1/models/sklearn-linear:predict \\")
    print(f"        -H 'Content-Type: application/json' \\")
    print(f"        -d '{{\"instances\": [[6.8, 2.8, 4.8, 1.4], [6.0, 3.4, 4.5, 1.6]]}}'")
    print()
    print('  Expected: {"predictions": [1, 1]}')


if __name__ == "__main__":
    main()
