#!/usr/bin/env python3
"""Create a project and workbench, replicating what the ODH dashboard does.

The dashboard creates:
  1. Namespace (labeled for dashboard visibility)
  2. PersistentVolumeClaim (workbench storage)
  3. Role + RoleBinding (user can view their own notebook)
  4. Notebook CR (kubeflow.org/v1)

The odh-notebook-controller then automatically creates:
  - ServiceAccount, kube-rbac-proxy Service, HTTPRoute,
    ReferenceGrant, NetworkPolicies, ConfigMap, ClusterRoleBinding
"""

import argparse
import json
import subprocess
import sys


def kubectl(*args, input_data=None, quiet=False):
    cmd = ["kubectl"] + list(args)
    result = subprocess.run(cmd, capture_output=True, text=True, input=input_data)
    if result.returncode != 0 and not quiet:
        print(f"  ERROR: {' '.join(cmd)}", file=sys.stderr)
        print(f"  {result.stderr.strip()}", file=sys.stderr)
    return result


def kubectl_apply(manifest):
    return kubectl("apply", "-f", "-", input_data=json.dumps(manifest))


def resource_exists(kind, name, namespace=None):
    args = ["get", kind, name]
    if namespace:
        args += ["-n", namespace]
    return kubectl(*args, quiet=True).returncode == 0


def resolve_image(imagestream_name, tag):
    """Resolve an image reference from an ImageStream's status.tags."""
    result = kubectl(
        "get", "imagestream", imagestream_name,
        "-n", "opendatahub",
        "-o", f"jsonpath={{.status.tags[?(@.tag==\"{tag}\")].items[0].dockerImageReference}}",
    )
    if result.returncode == 0 and result.stdout.strip():
        return result.stdout.strip()

    result = kubectl(
        "get", "imagestream", imagestream_name,
        "-n", "opendatahub",
        "-o", f"jsonpath={{.spec.tags[?(@.name==\"{tag}\")].from.name}}",
    )
    if result.returncode == 0 and result.stdout.strip():
        return result.stdout.strip()

    return None


def create_namespace(project):
    if resource_exists("namespace", project):
        print(f"  Namespace '{project}' already exists")
        return

    print(f"  Creating namespace '{project}'...")
    kubectl("create", "namespace", project)
    kubectl(
        "label", "namespace", project,
        "opendatahub.io/dashboard=true",
        "--overwrite",
    )


def create_pvc(project, workbench, storage="5Gi"):
    name = f"{workbench}-data"
    if resource_exists("pvc", name, project):
        print(f"  PVC '{name}' already exists")
        return

    print(f"  Creating PVC '{name}'...")
    kubectl_apply({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": name,
            "namespace": project,
            "labels": {"opendatahub.io/dashboard": "true"},
        },
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "resources": {"requests": {"storage": storage}},
        },
    })


def create_rbac(project, workbench, user="admin"):
    role_name = f"{workbench}-notebook-view"

    print(f"  Creating Role/RoleBinding '{role_name}'...")
    kubectl_apply({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "Role",
        "metadata": {
            "name": role_name,
            "namespace": project,
            "labels": {"opendatahub.io/dashboard": "true"},
        },
        "rules": [{
            "apiGroups": ["kubeflow.org"],
            "resources": ["notebooks"],
            "resourceNames": [workbench],
            "verbs": ["get"],
        }],
    })

    kubectl_apply({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "RoleBinding",
        "metadata": {
            "name": role_name,
            "namespace": project,
            "labels": {"opendatahub.io/dashboard": "true"},
        },
        "subjects": [{
            "kind": "User",
            "name": user,
            "apiGroup": "rbac.authorization.k8s.io",
        }],
        "roleRef": {
            "kind": "Role",
            "name": role_name,
            "apiGroup": "rbac.authorization.k8s.io",
        },
    })


def create_notebook(project, workbench, image_ref, imagestream_name, tag):
    if resource_exists("notebook", workbench, project):
        print(f"  Notebook '{workbench}' already exists")
        return

    print(f"  Creating Notebook '{workbench}'...")
    kubectl_apply({
        "apiVersion": "kubeflow.org/v1",
        "kind": "Notebook",
        "metadata": {
            "name": workbench,
            "namespace": project,
            "labels": {
                "app": workbench,
                "opendatahub.io/dashboard": "true",
                "opendatahub.io/odh-managed": "true",
            },
            "annotations": {
                "notebooks.opendatahub.io/inject-auth": "true",
                "notebooks.opendatahub.io/last-image-selection": f"{imagestream_name}:{tag}",
                "opendatahub.io/image-display-name": imagestream_name,
                "opendatahub.io/user": "admin",
                "opendatahub.io/username": "admin",
                "openshift.io/display-name": workbench,
            },
        },
        "spec": {
            "template": {
                "spec": {
                    "containers": [{
                        "name": workbench,
                        "image": image_ref,
                        "ports": [{
                            "containerPort": 8888,
                            "name": "notebook-port",
                            "protocol": "TCP",
                        }],
                        "resources": {
                            "limits": {"cpu": "1", "memory": "2Gi"},
                            "requests": {"cpu": "1", "memory": "2Gi"},
                        },
                        "env": [{
                            "name": "NOTEBOOK_ARGS",
                            "value": (
                                f"--NotebookApp.token='' "
                                f"--NotebookApp.password='' "
                                f"--NotebookApp.base_url=/notebook/{project}/{workbench}"
                            ),
                        }],
                        "volumeMounts": [{
                            "name": f"{workbench}-data",
                            "mountPath": "/opt/app-root/src",
                        }],
                    }],
                    "volumes": [{
                        "name": f"{workbench}-data",
                        "persistentVolumeClaim": {
                            "claimName": f"{workbench}-data",
                        },
                    }],
                },
            },
        },
    })


def wait_for_pod(project, workbench, timeout=120):
    import time

    print(f"  Waiting for pod {workbench}-0 (up to {timeout}s)...")

    deadline = time.time() + timeout
    while time.time() < deadline:
        result = kubectl("get", "pod", f"{workbench}-0", "-n", project,
                         "-o", "jsonpath={.status.phase}", quiet=True)
        if result.returncode == 0 and result.stdout.strip():
            break
        time.sleep(3)
    else:
        print("  Pod not created yet, current status:")
        result = kubectl("get", "pods", "-n", project)
        if result.stdout:
            print(result.stdout)
        return

    remaining = max(1, int(deadline - time.time()))
    result = kubectl(
        "wait", "--namespace", project,
        "--for=condition=Ready", "pod",
        f"{workbench}-0",
        f"--timeout={remaining}s",
    )
    if result.returncode != 0:
        print("  Pod not ready yet, current status:")
        result = kubectl("get", "pods", "-n", project)
        if result.stdout:
            print(result.stdout)
    else:
        print(f"  Pod {workbench}-0 is ready (2/2)")


def main():
    parser = argparse.ArgumentParser(description="Create an ODH project and workbench")
    parser.add_argument("--project", default="project1", help="Project/namespace name")
    parser.add_argument("--workbench", default="workbench1", help="Workbench name")
    parser.add_argument("--image", default="jupyter-minimal-notebook:3.4",
                        help="ImageStream name:tag (default: jupyter-minimal-notebook:3.4)")
    parser.add_argument("--storage", default="5Gi", help="PVC storage size")
    parser.add_argument("--no-wait", action="store_true", help="Don't wait for pod readiness")
    args = parser.parse_args()

    is_name, is_tag = args.image.rsplit(":", 1)

    print(f"=== Creating project '{args.project}' with workbench '{args.workbench}' ===")
    print()

    # 1. Resolve image
    print(f"[1/5] Resolving image {is_name}:{is_tag}...")
    image_ref = resolve_image(is_name, is_tag)
    if not image_ref:
        print(f"  ERROR: Could not resolve ImageStream {is_name}:{is_tag}", file=sys.stderr)
        sys.exit(1)
    print(f"  {image_ref}")
    print()

    # 2. Create namespace
    print("[2/5] Namespace...")
    create_namespace(args.project)
    print()

    # 3. Create PVC
    print("[3/5] PVC...")
    create_pvc(args.project, args.workbench, args.storage)
    print()

    # 4. Create RBAC
    print("[4/5] RBAC...")
    create_rbac(args.project, args.workbench)
    print()

    # 5. Create Notebook
    print("[5/5] Notebook...")
    create_notebook(args.project, args.workbench, image_ref, is_name, is_tag)
    print()

    if not args.no_wait:
        wait_for_pod(args.project, args.workbench)
        print()

    print("=== Done ===")
    print(f"  Project:   {args.project}")
    print(f"  Workbench: {args.workbench}")
    print(f"  URL:       https://rh-ai.apps.ocp-sim.test/notebook/{args.project}/{args.workbench}")


if __name__ == "__main__":
    main()
