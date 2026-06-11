# ODH Dashboard with OAuth on Picoshift

End-to-end guide to running the ODH dashboard with picoshift's built-in OAuth
server (legacy `sha256~` tokens or internal OIDC/JWT). For external OIDC
authentication via entra-mock, see [odh-byoidc-howto.md](odh-byoidc-howto.md).

## Recommended path

The most reliable way to get ODH running on picoshift is the OCP-realistic
path: OLM + OSSM3 via the servicemeshoperator3, then the ODH operator on top.

```bash
# 1. Create the cluster with OLM + OSSM3 (requires a pull secret for registry.redhat.io)
picoshift create --with-ossm3 --pull-secret ~/.docker/config.json

# 2. Build and install the ODH operator
make operator-install
```

`--with-ossm3` implies `--with-olm` and handles everything in one shot:

- Installs OLM (Operator Lifecycle Manager)
- Adds the `redhat-operators` catalog (authenticated via the pull secret)
- Installs Gateway API CRDs
- Removes stub Istio CRDs that would conflict with the real operator
- Installs `servicemeshoperator3` from the Red Hat catalog
- Creates the `Istio` CR for the Sail operator to reconcile
- Patches the API server to add `istio-ca` to `--api-audiences`
- Waits for istiod to become healthy

`make operator-install` then builds the ODH operator image, loads it into the
cluster, deploys it, and creates the DSCI + DSC + admin RBAC.

### Why this path

On real OpenShift, OSSM3 is installed via OLM and manages Istio through the
Sail operator. There is no standalone "OCP ingress controller" that magically
wires up the service mesh — it's the servicemeshoperator3 doing the work. The
`--with-ossm3` flag replicates that same mechanism on picoshift.

The pull secret is required because the servicemeshoperator3 images live on
`registry.redhat.io`, which needs authenticated access.

## Alternative path (shortcut)

```bash
make all            # build everything, create cluster, deploy simulator
make gateway-stack  # install Istio (via istioctl), cert-manager, Kuadrant
make operator-install
```

This works but it's a different mechanism than real OCP:

- Installs Istio directly via `istioctl` instead of through OLM/Sail
- Does **not** install OLM
- Does **not** patch the API server `--api-audiences` for `istio-ca`

If Istio gateway pods CrashLoop with authentication failures after using this
path, the API server audience patch is missing. See
[bugs/istio-gateway-auth-failure.md](bugs/istio-gateway-auth-failure.md).

## Known issues

After ODH is running, you may hit these issues on picoshift/kind:

- **Dashboard pods stuck in ContainerCreating** — missing `openshift-service-ca.crt`
  ConfigMap and `odh-dashboard-modules-token` Secret. See
  [bugs/dashboard-missing-openshift-resources.md](bugs/dashboard-missing-openshift-resources.md).

- **ODH operator high CPU** — the `cert-configmap-generator-controller` enters a
  tight reconciliation loop because there is no service CA operator to populate
  the CA bundle ConfigMaps. See
  [bugs/odh-operator-ca-bundle-hot-loop.md](bugs/odh-operator-ca-bundle-hot-loop.md).
