#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

CLUSTER_NAME="${CLUSTER_NAME:-ocp-sim}"
KIND_FORK_DIR="example.src/kind"
KIND_BIN="${KIND_FORK_DIR}/bin/kind"
K8S_VERSION="${K8S_VERSION:-v1.33.1}"
BASE_IMAGE="kindest/base:ocp-shim"
NODE_IMAGE="localhost/kindest/node:ocp-shim"
SIM_IMAGE="localhost/ocp-sim:latest"
SUDO="${SUDO:-sudo}"
KUBECONFIG_PATH="$(eval echo ~$(id -un))/.kube/config"
ODH_DIR="example.src/opendatahub-operator"

cd "$PROJECT_DIR"

echo "=== OCP Simulator: Full Rebuild ==="
echo ""

# ── 1. Tear down existing cluster ──────────────────────────
echo "[1/17] Tearing down existing cluster..."
if $SUDO "$KIND_BIN" get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
    $SUDO "$KIND_BIN" delete cluster --name "$CLUSTER_NAME"
else
    echo "       (no existing cluster)"
fi

# ── 2. Build kind CLI ──────────────────────────────────────
echo "[2/17] Building kind CLI..."
make -C "$KIND_FORK_DIR" build

# ── 3. Copy ocp-shim sources ──────────────────────────────
echo "[3/17] Copying ocp-shim sources..."
cp "${KIND_FORK_DIR}/cmd/ocp-shim/main.go" \
   "${KIND_FORK_DIR}/cmd/ocp-shim/go.mod" \
   "${KIND_FORK_DIR}/images/base/ocp-shim/"

# ── 4. Build kind base image ──────────────────────────────
echo "[4/17] Building kind base image (with ocp-shim)..."
$SUDO podman build \
    --build-arg GO_VERSION=1.26.2 \
    -t "$BASE_IMAGE" \
    "${KIND_FORK_DIR}/images/base/"

# ── 5. Build kind node image ─────────────────────────────
echo "[5/17] Building kind node image..."
$SUDO "$KIND_BIN" build node-image "$K8S_VERSION" \
    --type release \
    --base-image "$BASE_IMAGE" \
    --image "$NODE_IMAGE"

# ── 6. Build simulator image ─────────────────────────────
echo "[6/17] Building simulator image..."
$SUDO podman build -t "$SIM_IMAGE" ./simulator

# ── 7. Create cluster ────────────────────────────────────
echo "[7/17] Creating kind cluster..."
$SUDO "$KIND_BIN" create cluster \
    --config kind/cluster.yaml \
    --image "$NODE_IMAGE" \
    --name "$CLUSTER_NAME"

# ── 8. Export kubeconfig ─────────────────────────────────
echo "[8/17] Exporting kubeconfig to ${KUBECONFIG_PATH}..."
$SUDO "$KIND_BIN" get kubeconfig --name "$CLUSTER_NAME" > "$KUBECONFIG_PATH"

# ── 9. Wait for node ─────────────────────────────────────
echo "[9/17] Waiting for node to be ready..."
kubectl wait --for=condition=Ready node --all --timeout=120s

# ── 10. Deploy CRDs ──────────────────────────────────────
echo "[10/17] Installing CRDs..."
kubectl apply -f crds/openshift/
kubectl apply -f crds/olm/
kubectl apply -f crds/gateway/
kubectl apply -f crds/monitoring/
kubectl apply -f crds/istio/
kubectl wait --for=condition=Established crd --all --timeout=30s

# ── 11. Deploy seed resources ────────────────────────────
echo "[11/17] Deploying seed resources..."
kubectl apply -f seed/namespaces.yaml
kubectl apply -f seed/cluster-config.yaml
kubectl apply -f seed/authentication.yaml
kubectl apply -f seed/ingress.yaml
kubectl apply -f seed/infrastructure.yaml
kubectl apply -f seed/sccs.yaml

# ── 12. Create ClusterVersion with status ────────────────
echo "[12/17] Creating ClusterVersion..."
kubectl apply -f - <<'CVEOF'
{"apiVersion":"config.openshift.io/v1","kind":"ClusterVersion","metadata":{"name":"version"},"spec":{"clusterID":"ocp-sim-00000000-0000-0000-0000-000000000000","channel":"stable-4.20"}}
CVEOF

kubectl proxy --port=8199 &
PROXY_PID=$!
sleep 1

RV=$(curl -s http://localhost:8199/apis/config.openshift.io/v1/clusterversions/version \
    | python3 -c "import sys,json; print(json.load(sys.stdin)['metadata']['resourceVersion'])")

curl -s -X PUT http://localhost:8199/apis/config.openshift.io/v1/clusterversions/version/status \
    -H "Content-Type: application/json" \
    -d "{\"apiVersion\":\"config.openshift.io/v1\",\"kind\":\"ClusterVersion\",\"metadata\":{\"name\":\"version\",\"resourceVersion\":\"${RV}\"},\"spec\":{\"clusterID\":\"ocp-sim-00000000-0000-0000-0000-000000000000\",\"channel\":\"stable-4.20\"},\"status\":{\"desired\":{\"version\":\"4.20.0\"},\"history\":[{\"state\":\"Completed\",\"version\":\"4.20.0\",\"startedTime\":\"2024-01-01T00:00:00Z\",\"completionTime\":\"2024-01-01T01:00:00Z\",\"verified\":true}],\"conditions\":[{\"type\":\"Available\",\"status\":\"True\",\"lastTransitionTime\":\"2024-01-01T01:00:00Z\",\"reason\":\"ClusterVersionAvailable\",\"message\":\"Simulated OCP cluster\"},{\"type\":\"Progressing\",\"status\":\"False\",\"lastTransitionTime\":\"2024-01-01T01:00:00Z\",\"reason\":\"ClusterVersionNotProgressing\"},{\"type\":\"Failing\",\"status\":\"False\",\"lastTransitionTime\":\"2024-01-01T01:00:00Z\",\"reason\":\"ClusterVersionNotFailing\"}]}}" > /dev/null

kill $PROXY_PID 2>/dev/null; wait $PROXY_PID 2>/dev/null || true
echo "       ClusterVersion created with status"

# ── 13. Patch kubernetes endpoints ───────────────────────
echo "[13/17] Patching kubernetes endpoints to route through ocp-shim..."
kubectl patch endpoints kubernetes -n default --type='json' \
    -p='[{"op":"replace","path":"/subsets/0/ports/0/port","value":6443}]' 2>/dev/null || true

# ── 14. Load and deploy simulator ────────────────────────
echo "[14/17] Loading simulator image into cluster..."
$SUDO podman save "$SIM_IMAGE" --format oci-archive -o /tmp/ocp-sim-oci.tar
$SUDO podman exec -i "${CLUSTER_NAME}-control-plane" \
    ctr --namespace=k8s.io images import --no-unpack - < /tmp/ocp-sim-oci.tar
$SUDO rm -f /tmp/ocp-sim-oci.tar

echo "       Deploying simulator..."
kubectl apply -f deploy/simulator.yaml
kubectl -n ocp-sim delete pod --all --wait=false 2>/dev/null || true
sleep 3
kubectl wait --namespace ocp-sim \
    --for=condition=Ready pod \
    --selector=app=ocp-sim \
    --timeout=120s

# ── 15. Install ODH CRDs ────────────────────────────────
echo "[15/17] Installing ODH operator CRDs..."
make -C "$ODH_DIR" manifests
make -C "$ODH_DIR" install

# ── 16. Apply DSCI ───────────────────────────────────────
echo "[16/17] Creating DSCInitialization..."
kubectl apply -f "${ODH_DIR}/config/samples/dscinitialization_v2_dscinitialization.yaml"

# ── 17. Done ─────────────────────────────────────────────
echo "[17/17] Done!"
echo ""
echo "=== Rebuild complete ==="
echo ""
echo "Next steps:"
echo "  1. In a separate terminal:  make operator-run"
echo "  2. Create DSC:              make dsc"
echo "  3. Patch GatewayConfig:     kubectl patch gatewayconfig default-gateway --type=merge -p '{\"spec\":{\"verifyProviderCertificate\":false}}'"
echo "  4. Open browser:            https://rh-ai.apps.ocp-sim.test/"
echo ""
