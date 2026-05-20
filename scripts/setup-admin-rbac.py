#!/usr/bin/env python3
"""Grant the admin user cluster-admin RBAC permissions.

On real OpenShift the kubeadmin user has cluster-admin. On picoshift we
create an explicit ClusterRoleBinding so the dashboard and CLI work.
"""

import json
import subprocess
import sys


CLUSTER_NAME = "ocp-sim"


def kubectl_apply(manifest):
    result = subprocess.run(
        ["kubectl", "--context", f"kind-{CLUSTER_NAME}", "apply", "-f", "-"],
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

    print("\nCreating ClusterRoleBinding 'ocp-sim-admin' -> cluster-admin...")
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
            "name": "cluster-admin",
            "apiGroup": "rbac.authorization.k8s.io",
        },
    })

    print("\nDone.")


if __name__ == "__main__":
    main()
