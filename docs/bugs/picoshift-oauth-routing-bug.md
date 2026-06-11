# picoshift bug: LB controller assigns node IP, kube-proxy steals port 443 from proxy

## Summary

When using `picoshift create --with-ossm3`, the Sail operator creates a LoadBalancer
service for the data-science-gateway on port 443. The simulator's LB controller
assigns the node's InternalIP as the external IP. This causes kube-proxy to create
iptables DNAT rules that intercept **all** port 443 traffic at the kernel level,
routing it to the Envoy gateway pod and completely bypassing the ocp-sim reverse
proxy.

## Architecture

Inside the kind node container (`ocp-sim-control-plane`), port 443 is owned by `ocp-sim`:

| Process | Port | Role |
|---------|------|------|
| `ocp-sim` | 80 | HTTP reverse proxy for `*.apps.ocp-sim.test` |
| `ocp-sim` | 443 | HTTPS reverse proxy for `*.apps.ocp-sim.test` |
| `ocp-sim` | 9443 | OAuth server (direct) |

The reverse proxy terminates TLS with a wildcard cert (`*.apps.ocp-sim.test`) and
routes by HTTP Host header to backends via the Kubernetes service network.

## Root cause

1. Sail operator creates `data-science-gateway-data-science-gateway-class` service
   as type LoadBalancer on port 443
2. Simulator's LB controller (`loadbalancer.rs`) assigns the node's InternalIP
   (e.g. `10.89.0.5`) as the external IP
3. kube-proxy creates iptables DNAT rule: `destination 10.89.0.5:443 → Envoy pod`
4. **All** port 443 traffic arriving at the node IP is intercepted by iptables
   before it reaches the ocp-sim proxy process
5. Envoy handles `rh-ai.apps.ocp-sim.test` (has an HTTPRoute), but resets
   `oauth-openshift.apps.ocp-sim.test` (no route, no cert)

The cert visible on port 443 was `CN=rh-ai.apps.ocp-sim.test` from
`opendatahub-self-signed` (Envoy's cert), **not** the proxy's
`*.apps.ocp-sim.test` wildcard cert — proving the proxy never saw the connection.

## Symptom

Browser hits `https://rh-ai.apps.ocp-sim.test` and enters an infinite redirect loop:

1. Dashboard responds 302 → `https://oauth-openshift.apps.ocp-sim.test/oauth/authorize?...`
2. Browser connects to `oauth-openshift.apps.ocp-sim.test:443`
3. Envoy gateway (via kube-proxy DNAT) receives the connection
4. Envoy has no route/cert for this hostname → TLS connection reset
5. Browser fails, retries, loops

## Reproduction

```bash
# TLS works for the gateway hostname (Envoy handles it)
openssl s_client -connect rh-ai.apps.ocp-sim.test:443 \
  -servername rh-ai.apps.ocp-sim.test </dev/null 2>&1 | grep subject
# subject=CN=rh-ai.apps.ocp-sim.test  ← Envoy's cert, NOT *.apps.ocp-sim.test

# TLS resets for the OAuth hostname (Envoy rejects it)
openssl s_client -connect oauth-openshift.apps.ocp-sim.test:443 \
  -servername oauth-openshift.apps.ocp-sim.test </dev/null 2>&1
# write:errno=104, no peer certificate available

# OAuth server works directly on 9443 (bypasses the conflict)
curl -sk https://oauth-openshift.apps.ocp-sim.test:9443/.well-known/oauth-authorization-server
# Returns valid JSON
```

## Fix

Changed the LB controller to assign virtual IPs from `10.254.0.0/24` instead of
the node's real InternalIP. kube-proxy iptables rules now match `10.254.0.x:443`
(a non-routable IP), so traffic arriving at the real node IP:443 passes through to
the ocp-sim proxy.

- The proxy routes gateway traffic to the backend via ClusterIP (kube-proxy DNATs
  correctly)
- The proxy routes OAuth traffic directly to localhost:9443
- In-cluster pods using the virtual LB IP still get DNATed to the Envoy pod

## Environment

- picoshift v2026.06.11
- RHOAI 3.4.0 (rhods-operator via OLM, redhat-operators catalog v4.20)
- Cluster created with `picoshift create --with-ossm3 --pull-secret`
