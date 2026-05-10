#!/usr/bin/env bash
# Post-deploy fixups for MaaS on picoshift.
# Run after operator-install + dsc-enable-maas, once the maas-default-gateway
# service has been created by the gateway controller.
set -euo pipefail

SVC_NAME="maas-default-gateway-data-science-gateway-class"
NS="openshift-ingress"
GW_NAME="maas-default-gateway"

echo "Waiting for service ${NS}/${SVC_NAME} ..."
for i in $(seq 1 30); do
    if kubectl get svc "$SVC_NAME" -n "$NS" &>/dev/null; then
        break
    fi
    sleep 5
done

GW_UID=$(kubectl get gateway "$GW_NAME" -n "$NS" -o jsonpath='{.metadata.uid}')

kubectl label svc "$SVC_NAME" -n "$NS" \
    "gateway.networking.k8s.io/gateway-name=$GW_NAME" --overwrite

kubectl patch svc "$SVC_NAME" -n "$NS" --type=json \
    -p "[{\"op\":\"add\",\"path\":\"/metadata/ownerReferences\",\"value\":[{\"apiVersion\":\"gateway.networking.k8s.io/v1\",\"kind\":\"Gateway\",\"name\":\"$GW_NAME\",\"uid\":\"$GW_UID\"}]}]"

echo "Gateway service ${NS}/${SVC_NAME} patched with label and ownerReference."

# Restart maas-api to pick up the gateway service
kubectl -n opendatahub delete pod -l app.kubernetes.io/component=api --ignore-not-found
echo "maas-api pods restarted."
