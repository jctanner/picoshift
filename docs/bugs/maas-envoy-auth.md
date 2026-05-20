# MaaS API returns 404 through proxy but works directly via Envoy

## Summary

Requests to `maas.apps.ocp-sim.test/v1/models` through the ocp-sim proxy
return 404 from Envoy, but the same request sent directly to the MaaS Envoy
pod (with correct SNI via `--connect-to`) reaches the backend and returns a
real response (500 AUTH_FAILURE).

## What works

| Path | Result |
|------|--------|
| `kubectl exec maas-api -- curl https://localhost:8443/v1/api-keys -X POST` | Works (creates API key with correct headers) |
| Inside MaaS Envoy pod with correct SNI (`--connect-to`) | 500 AUTH_FAILURE (reaches backend) |
| Inside MaaS Envoy pod with HTTP/1.1 + correct SNI | 500 AUTH_FAILURE (reaches backend) |
| Through proxy: `curl https://maas.apps.ocp-sim.test/v1/models` | 404 from istio-envoy (never reaches backend) |

## Root cause

The proxy sends an **absolute-form URI** in the HTTP/1.1 request line:

```
GET https://10.96.184.233:443/v1/models HTTP/1.1
```

instead of origin-form:

```
GET /v1/models HTTP/1.1
```

Debug logging confirmed:
```
TLS upstream request host="maas.apps.ocp-sim.test" addr=10.96.184.233:443
  sni=maas.apps.ocp-sim.test method=GET
  uri=https://10.96.184.233:443/v1/models
  host_header=Some("maas.apps.ocp-sim.test")
```

Envoy sees the `https://10.96.184.233:443` authority in the request line,
can't match it to any virtual host domain (`maas.apps.ocp-sim.test`), and
returns 404.

This happens because `proxy_request` builds the URI as
`format!("{scheme}://{addr}{path}")` to pass to the hyper client. For the
HTTP (non-TLS) path, hyper-util's `Client` strips the authority and sends
origin-form automatically. But for the TLS path, we use
`hyper::client::conn::http1::handshake` + `sender.send_request()` directly,
which sends the URI exactly as given — absolute-form.

### Fix

Rewrite the URI to origin-form before calling `sender.send_request()` in
the TLS branch. The `Host` header already carries the hostname.

## API key creation (works via direct pod access)

API key creation requires:
1. A `MaaSSubscription` CR with an `owner` matching the user/group
2. Headers: `X-MaaS-Username: <user>`, `X-MaaS-Group: ["<group>"]` (JSON array)
3. POST to `/v1/api-keys` with `{"name":"...","expirationDays":30}`

These headers are normally injected by Authorino after authentication.

## Things tried

1. Fixed proxy TLS SNI — was sending IP address, now sends hostname
2. Confirmed Envoy route config has `/v1/models` route
3. Confirmed HTTP/1.1 works when connecting directly to Envoy with correct SNI
4. Created ExternalModel, MaaSModelRef, and MaaSSubscription resources
5. Successfully created API key via direct pod access

## Kuadrant WasmPlugin not created (SOLVED)

After fixing the proxy, requests reached the backend but returned
AUTH_FAILURE — Authorino's ext_authz was never wired in.

The AuthPolicy showed `Enforced: False` with message:
```
AuthPolicy waiting for the following components to sync:
  [Gateway (openshift-ingress/maas-default-gateway)]
```

Kuadrant's `IstioExtensionReconciler` logged `"no istio gateways found"`
because the GatewayClass uses `openshift.io/gateway-controller/v1` as the
controller name, but Kuadrant defaults to looking for
`istio.io/gateway-controller`.

### Fix

Set `ISTIO_GATEWAY_CONTROLLER_NAMES` env var on the Kuadrant operator
deployment, as documented in the MaaS platform-setup guide:

```
kubectl set env deployment/kuadrant-operator-controller-manager \
  -n kuadrant-system \
  ISTIO_GATEWAY_CONTROLLER_NAMES="openshift.io/gateway-controller/v1"
```

After this, Kuadrant created a WasmPlugin (`kuadrant-maas-default-gateway`)
and the AuthPolicy moved to `Enforced: True`.

## Authorino CA trust failure (SOLVED)

After fixing Kuadrant wiring, Authorino's `apiKeyValidation` metadata call
to `https://maas-api.opendatahub.svc.cluster.local:8443/internal/v1/api-keys/validate`
failed with `x509: certificate signed by unknown authority`.

Three issues, all required fixing:

1. **Missing `.cluster.local` SAN**: Service CA only generated certs with
   `{svc}.{ns}.svc` SAN, but Authorino uses the FQDN
   `{svc}.{ns}.svc.cluster.local`. Fixed in `service_ca.rs`.

2. **Trusted CA bundle not injected**: The `odh-trusted-ca-bundle` ConfigMap
   (label `config.openshift.io/inject-trusted-cabundle: "true"`) was empty.
   On real OCP the cluster proxy operator fills this; on picoshift our
   Service CA controller must handle it. Extended `reconcile_configmap` to
   inject CA into `odh-ca-bundle.crt` key for labeled ConfigMaps.

3. **subPath volume mount doesn't auto-update**: Authorino's CA cert was
   mounted via `subPath: odh-ca-bundle.crt`. Kubernetes `subPath` mounts
   snapshot ConfigMap data at pod creation time and never update. Since the
   Service CA controller populates the ConfigMap after the pod starts, the
   file was 1 byte (empty). Fixed by mounting the whole ConfigMap directory
   (`/etc/ssl/custom-certs/`) and pointing `SSL_CERT_FILE` to the file
   within it.

## Next steps

1. ~~**Fix the URI form**~~: DONE — origin-form in TLS branch
2. ~~**Check Authorino wiring**~~: DONE — missing ISTIO_GATEWAY_CONTROLLER_NAMES
3. ~~**Debug 403 on API key auth**~~: DONE — three TLS issues (SAN, CA injection, subPath)
4. **Codify Authorino patches**: The CA volume mount + SSL_CERT_FILE patches
   are runtime-only (manual kubectl patch). Need to add to seed manifests or
   a post-deploy script so they survive reinstalls.

## Environment

- ocp-sim proxy: rustls TLS client with hostname SNI, hyper HTTP/1.1
- MaaS Envoy: Istio gateway with `server_names` filter chain match
- MaaS API: Go/Gin server on :8443, requires X-MaaS-Username/X-MaaS-Group headers
- Authorino: kuadrant-system, auth via `maas-api-auth-policy`
