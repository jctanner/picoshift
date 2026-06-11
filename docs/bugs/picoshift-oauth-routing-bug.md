# picoshift bug: ocp-sim reverse proxy resets TLS for oauth-openshift hostname

## Summary

When using `picoshift create --with-ossm3`, the ocp-sim reverse proxy on port 443 resets TLS connections for `oauth-openshift.apps.ocp-sim.test`. The OAuth server itself works fine on port 9443, but the reverse proxy layer doesn't route the OAuth hostname correctly. This breaks the RHOAI dashboard login flow with an infinite redirect loop.

## Architecture

Inside the kind node container (`ocp-sim-control-plane`), port 443 is owned by `ocp-sim`:

| Process | Port | Role |
|---------|------|------|
| `ocp-sim` | 80 | HTTP reverse proxy for `*.apps.ocp-sim.test` |
| `ocp-sim` | 443 | HTTPS reverse proxy for `*.apps.ocp-sim.test` |
| `ocp-sim` | 9443 | OAuth server (direct) |

The reverse proxy is supposed to route by SNI/hostname:
- `rh-ai.apps.ocp-sim.test` → data-science gateway (istio envoy)
- `oauth-openshift.apps.ocp-sim.test` → ocp-sim OAuth server (port 9443)

## Symptom

Browser hits `https://rh-ai.apps.ocp-sim.test` and enters an infinite redirect loop:

1. Dashboard responds 302 → `https://oauth-openshift.apps.ocp-sim.test/oauth/authorize?...`
2. Browser connects to `oauth-openshift.apps.ocp-sim.test:443`
3. ocp-sim reverse proxy resets the TLS connection
4. Browser fails, retries, loops

## Reproduction

```bash
# OAuth server works directly on 9443
curl -sk https://oauth-openshift.apps.ocp-sim.test:9443/.well-known/oauth-authorization-server
# Returns valid JSON with authorization_endpoint, token_endpoint, etc.

# Reverse proxy on 443 resets TLS for the same hostname
curl -vsk https://oauth-openshift.apps.ocp-sim.test:443/
# * TLSv1.3 (OUT), TLS handshake, Client hello (1):
# * Recv failure: Connection reset by peer

# Dashboard redirect chain shows the problem
curl -vsk -L --max-redirs 5 https://rh-ai.apps.ocp-sim.test 2>&1 | grep location
# location: https://oauth-openshift.apps.ocp-sim.test/oauth/authorize?...
```

## Related: kubernetes endpoint patch

The `kube-auth-proxy` (OAuth2 proxy in the RHOAI gateway stack) discovers OAuth endpoints via `/.well-known/oauth-authorization-server` on the in-cluster kubernetes service (`10.96.0.1:443`). This requires the kubernetes service endpoint to point to ocp-shim on port 6443 (which serves OpenShift discovery endpoints), not the real kube-apiserver on port 16443.

On a fresh `--with-ossm3` cluster, the endpoint points to 16443:
```bash
kubectl get endpoints kubernetes -n default -o jsonpath='{.subsets[0].ports[0].port}'
# 16443

# Must be patched to 6443 for OAuth discovery to work:
kubectl patch endpoints kubernetes -n default --type='json' \
  -p='[{"op":"replace","path":"/subsets/0/ports/0/port","value":6443}]'
```

Without this patch, `kube-auth-proxy` logs:
```
Failed to discover OpenShift OAuth endpoints: got 404 404 page not found
```

## Environment

- picoshift v2026.06.11
- RHOAI 3.4.0 (rhods-operator via OLM, redhat-operators catalog v4.20)
- Cluster created with `picoshift create --with-ossm3 --pull-secret`

## Impact

The RHOAI dashboard is completely inaccessible from a browser. The OAuth login flow cannot complete because the OAuth server is unreachable through the reverse proxy on port 443.
