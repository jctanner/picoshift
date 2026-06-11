# ODH Operator CA Bundle Hot Loop

## Summary

The `cert-configmap-generator-controller` in the ODH operator enters a tight reconciliation loop on picoshift/kind clusters, consuming ~120% CPU. The `kube-apiserver` also spikes to ~80% CPU servicing the constant API calls.

## Symptoms

- `manager` process (ODH operator) at 118% CPU
- `kube-apiserver` at 82% CPU
- Operator logs show continuous `Adding CA bundle configmap` messages across all namespaces with no delay between reconciles
- `monitoring` controller reconciles repeatedly in the same loop

## Root Cause

The operator's `cert-configmap-generator-controller` creates `odh-trusted-ca-bundle` ConfigMaps in every namespace with the label `config.openshift.io/inject-trusted-cabundle: "true"`. On real OpenShift, the service CA operator populates the `odh-ca-bundle.crt` key in those ConfigMaps. On kind/picoshift, there is no service CA operator, so the ConfigMaps are never populated.

Each time the controller creates or updates a ConfigMap, the change triggers another reconcile event, creating an infinite loop with no backoff.

## Affected Component

- `opendatahub-operator` controller-manager
- Controller: `cert-configmap-generator-controller`

## Environment

- picoshift (kind-based OpenShift simulator)
- ODH operator built from `main` branch
- No OpenShift service CA operator present

## Workaround

None identified yet. The cluster remains functional despite the CPU burn.

## Possible Fixes

1. The operator could add a backoff/requeue delay when the CA bundle is empty
2. The operator could detect non-OpenShift clusters and skip the CA bundle injection
3. Picoshift could provide a stub service CA operator that populates the ConfigMaps
