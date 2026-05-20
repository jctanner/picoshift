#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

CLUSTER_NAME="${CLUSTER_NAME:-ocp-sim}"
KIND_BIN="${PROJECT_DIR}/deps/kind/bin/kind"
SUDO="${SUDO:-sudo}"

echo "=== Tearing down OCP Simulator ==="

if $SUDO "$KIND_BIN" get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
    $SUDO "$KIND_BIN" delete cluster --name "$CLUSTER_NAME"
    echo "Cluster '${CLUSTER_NAME}' deleted."
else
    echo "Cluster '${CLUSTER_NAME}' not found, nothing to do."
fi
