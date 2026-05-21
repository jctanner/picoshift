#!/bin/bash
set -euo pipefail

CLUSTER_NAME="${CLUSTER_NAME:-ocp-sim}"
SUDO="${SUDO:-sudo}"
ENTRA_SRC="deps/entra-id-emulator"
ENTRA_IMAGE="localhost/entra-mock:latest"

echo "=== Building entra-mock image ==="

# Build with source code baked in (the upstream Dockerfile expects volume mounts)
${SUDO} podman build -t "${ENTRA_IMAGE}" -f - "${ENTRA_SRC}" <<'DOCKERFILE'
FROM python:3.11-slim
RUN apt-get update && apt-get install -y --no-install-recommends curl && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY . .
RUN mkdir -p /app/data
EXPOSE 8080
CMD ["python", "run.py"]
DOCKERFILE

echo "=== Loading image into kind cluster ==="

${SUDO} podman save "${ENTRA_IMAGE}" --format oci-archive -o /tmp/entra-mock-oci.tar
${SUDO} podman exec -i "${CLUSTER_NAME}-control-plane" \
    ctr --namespace=k8s.io images import --no-unpack - < /tmp/entra-mock-oci.tar
${SUDO} rm -f /tmp/entra-mock-oci.tar

echo "=== Deploying entra-mock ==="

kubectl apply -f deploy/entra-mock.yaml

echo "=== Waiting for entra-mock to be ready ==="

kubectl -n entra-mock rollout status deployment/entra-mock --timeout=120s

echo "=== Verifying OIDC discovery ==="

TENANT="a1b2c3d4-e5f6-7890-abcd-ef1234567890"
for i in $(seq 1 10); do
    ISSUER=$(kubectl -n entra-mock exec deployment/entra-mock -- \
        python -c "import urllib.request, json; print(json.loads(urllib.request.urlopen('http://localhost:8080/${TENANT}/v2.0/.well-known/openid-configuration').read())['issuer'])" 2>/dev/null) || true
    if [ -n "${ISSUER:-}" ]; then
        echo "OIDC discovery OK: issuer=${ISSUER}"
        break
    fi
    if [ "$i" -eq 10 ]; then
        echo "WARNING: Could not verify discovery endpoint"
    fi
    sleep 3
done

echo ""
echo "=== entra-mock deployed ==="
echo "  Issuer:  http://entra-mock.entra-mock.svc.cluster.local:8080/${TENANT}/v2.0"
echo "  Client:  picoshift / picoshift-secret"
echo "  Users:   admin/admin, user1/user1, developer/developer"
echo "  Admin:   http://entra-mock.entra-mock.svc.cluster.local:8080/admin/ (password: changeme1234)"
echo ""
echo "  Dynamic user management:"
echo "    kubectl -n entra-mock exec deployment/entra-mock -- \\"
echo "      curl -s -u :changeme1234 http://localhost:8080/admin/api/users | python3 -m json.tool"
