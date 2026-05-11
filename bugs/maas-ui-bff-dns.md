# maas-ui BFF cannot reach maas-api (*.localhost resolves to loopback inside cluster)

## Summary

The `maas-ui` sidecar container in the `odh-dashboard` pod fails to reach the
maas-api because it auto-discovers the upstream URL as
`https://maas.apps.ocp-sim.test/maas-api`, and `*.localhost` resolves to
`[::1]` (IPv6 loopback) inside the pod. Nothing is listening on port 443 at
that address, so all maas-api requests fail with `connection refused`.

The dashboard shows "Error loading components" on the API keys page.

## Error

```
maas-ui level=ERROR msg="Post \"https://maas.apps.ocp-sim.test/maas-api/v1/api-keys/search\":
  dial tcp [::1]:443: connect: connection refused" method=POST uri=/api/v1/api-keys/search
```

## Root cause

The maas-ui BFF auto-discovers its upstream URL via
`packages/maas/bff/internal/api/app.go:119-129`:

```go
if cfg.MaasApiUrl == "" {
    clusterDomain, err := helper.GetClusterDomainUsingServiceAccount(...)
    cfg.MaasApiUrl = fmt.Sprintf("https://maas.%s/maas-api", clusterDomain)
}
```

On picoshift, the cluster domain is `apps.ocp-sim.test`, so the URL becomes
`https://maas.apps.ocp-sim.test/maas-api`. Per RFC 6761, `*.localhost`
resolves to loopback (`127.0.0.1` / `[::1]`), so DNS never reaches CoreDNS —
the resolver short-circuits to loopback.

The BFF supports a `MAAS_API_URL` env var override, but the dashboard deployment
is rendered by the ODH operator from kustomize manifests, so any `kubectl set env`
gets overwritten on the next reconciliation.

## Possible fixes

### 1. CoreDNS rewrite (cluster-level fix)

Add a CoreDNS rule to resolve `*.apps.ocp-sim.test` to the node IP or
relevant service ClusterIP. This fixes the problem for all in-cluster consumers
without touching operator manifests. However, `*.localhost` may be intercepted
by the pod's resolver before reaching CoreDNS (glibc/musl behavior varies).

### 2. Patch ODH manifests to set MAAS_API_URL

Patch the dashboard kustomize overlays in `opt/manifests/` to inject
`MAAS_API_URL=https://maas-api.opendatahub.svc:8443` (direct in-cluster service
URL). This bypasses DNS entirely but may not match what maas-api expects for
host-based routing.

### 3. Use a non-.localhost domain

Change the cluster domain to something like `apps.ocp-sim.local` or
`apps.ocp-sim.test` that doesn't trigger RFC 6761 loopback behavior. This is
a larger change affecting /etc/hosts, kind config, proxy certs, and all docs.

## Affected components

- **maas-ui BFF** (`packages/maas/bff/`) — auto-discovery produces unreachable URL
- **CoreDNS** — `*.localhost` never reaches it due to resolver short-circuit
- **odh-dashboard deployment** — operator-managed, env var overrides don't persist

## Environment

Discovered on picoshift (kind + ocp-sim) with ODH dashboard enabled.
Specific to `*.localhost` cluster domains.
