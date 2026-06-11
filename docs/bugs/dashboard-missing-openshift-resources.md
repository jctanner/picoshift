# Dashboard Pods Stuck on Missing OpenShift Resources

## Summary

The `odh-dashboard` pods get stuck in `ContainerCreating` because they mount projected volumes that reference OpenShift-specific resources not present on kind/picoshift.

## Symptoms

- Dashboard pods stuck in `ContainerCreating` or `Pending`
- Events show `FailedMount` errors

## Missing Resources

### ConfigMap: `openshift-service-ca.crt`

The `dashboard-sa-token` and `modules-sa-token` projected volumes both reference a ConfigMap named `openshift-service-ca.crt` with key `service-ca.crt`. On OpenShift this is created by the service CA operator.

### Secret: `odh-dashboard-modules-token`

The `modules-sa-token` projected volume references this secret with keys `token` and `ca.crt`.

## Workaround

Create the missing resources manually before the dashboard pods start:

```bash
kubectl create configmap openshift-service-ca.crt -n opendatahub --from-literal=service-ca.crt=''
kubectl create secret generic odh-dashboard-modules-token -n opendatahub --from-literal=token='' --from-literal=ca.crt=''
```

## Affected Component

- `odh-dashboard` deployment in the `opendatahub` namespace

## Environment

- picoshift (kind-based OpenShift simulator)
- ODH operator built from `main` branch

## Notes

These stubs unblock pod creation but the values are empty. Features that depend on the service CA certificate chain (mutual TLS between dashboard components) may not work correctly.

The second dashboard replica may also fail to schedule due to CPU pressure (see `odh-operator-ca-bundle-hot-loop.md`).
