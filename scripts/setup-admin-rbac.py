#!/usr/bin/env python3
"""Grant the admin user cluster-admin-like RBAC permissions.

On real OpenShift the admin user gets broad permissions via OAuth identity
bindings. On picoshift we need to create these explicitly so the dashboard
and CLI can list Projects, Namespaces, and other cluster-scoped resources.
"""

import json
import subprocess
import sys


def kubectl_apply(manifest):
    result = subprocess.run(
        ["kubectl", "apply", "-f", "-"],
        capture_output=True, text=True,
        input=json.dumps(manifest),
    )
    if result.returncode != 0:
        print(f"  ERROR: {result.stderr.strip()}", file=sys.stderr)
        sys.exit(1)
    print(f"  {result.stdout.strip()}")


def main():
    user = "admin"

    print(f"Setting up RBAC for user '{user}'...")

    print("\n[1/2] Creating ClusterRole 'ocp-sim-admin'...")
    kubectl_apply({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {
            "name": "ocp-sim-admin",
            "labels": {"app.kubernetes.io/managed-by": "ocp-sim"},
        },
        "rules": [
            {
                "apiGroups": ["project.openshift.io"],
                "resources": ["projects", "projectrequests"],
                "verbs": ["get", "list", "watch", "create", "update", "delete"],
            },
            {
                "apiGroups": [""],
                "resources": ["namespaces"],
                "verbs": ["get", "list", "watch", "create", "update", "delete"],
            },
            {
                "apiGroups": ["kubeflow.org"],
                "resources": ["notebooks"],
                "verbs": ["get", "list", "watch", "create", "update", "patch", "delete"],
            },
            {
                "apiGroups": [""],
                "resources": [
                    "pods", "services", "configmaps", "secrets",
                    "persistentvolumeclaims", "events", "serviceaccounts",
                ],
                "verbs": ["get", "list", "watch", "create", "update", "patch", "delete"],
            },
            {
                "apiGroups": ["apps"],
                "resources": ["deployments", "statefulsets", "replicasets"],
                "verbs": ["get", "list", "watch"],
            },
            {
                "apiGroups": ["rbac.authorization.k8s.io"],
                "resources": ["roles", "rolebindings", "clusterroles", "clusterrolebindings"],
                "verbs": ["get", "list", "watch", "create", "update", "patch", "delete"],
            },
            {
                "apiGroups": ["route.openshift.io"],
                "resources": ["routes"],
                "verbs": ["get", "list", "watch"],
            },
            {
                "apiGroups": ["image.openshift.io"],
                "resources": ["imagestreams"],
                "verbs": ["get", "list", "watch"],
            },
            {
                "apiGroups": ["gateway.networking.k8s.io"],
                "resources": ["httproutes", "gateways"],
                "verbs": ["get", "list", "watch"],
            },
            {
                "apiGroups": ["networking.k8s.io"],
                "resources": ["networkpolicies"],
                "verbs": ["get", "list", "watch", "create", "update", "patch", "delete"],
            },
            {
                "apiGroups": ["authorization.k8s.io"],
                "resources": ["subjectaccessreviews"],
                "verbs": ["create"],
            },
            {
                "apiGroups": ["authentication.k8s.io"],
                "resources": ["tokenreviews"],
                "verbs": ["create"],
            },
        ],
    })

    print("\n[2/2] Creating ClusterRoleBinding 'ocp-sim-admin'...")
    kubectl_apply({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRoleBinding",
        "metadata": {
            "name": "ocp-sim-admin",
            "labels": {"app.kubernetes.io/managed-by": "ocp-sim"},
        },
        "subjects": [{
            "kind": "User",
            "name": user,
            "apiGroup": "rbac.authorization.k8s.io",
        }],
        "roleRef": {
            "kind": "ClusterRole",
            "name": "ocp-sim-admin",
            "apiGroup": "rbac.authorization.k8s.io",
        },
    })

    print("\nDone.")


if __name__ == "__main__":
    main()
