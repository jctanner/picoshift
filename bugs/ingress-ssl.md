# HTTPS ingress (port 443) broken from host — TLS handshake reset

## Summary

After deploying the MaaS gateway (`maas-default-gateway`), HTTPS on port 443
from the host stopped working. TLS handshakes failed with "Connection reset by
peer" during the ClientHello. HTTP on port 80 worked fine.

## Root cause

Istio creates a backing Service of type `LoadBalancer` for each Gateway by
default. The ocp-sim LoadBalancer controller then assigns the node IP
(`10.89.0.22`) to that Service. kube-proxy's iptables PREROUTING rules match
traffic to `10.89.0.22:443` and DNAT it to the MaaS Istio gateway pod — **before
it ever reaches ocp-sim's reverse proxy listener on port 443.**

This explains every symptom:
- From inside the container, `curl localhost:443` works because loopback traffic
  goes through the OUTPUT chain, not PREROUTING — so iptables doesn't intercept
  it.
- From the host (or directly to 10.89.0.22), traffic enters the container
  network via PREROUTING and gets hijacked by the kube-proxy DNAT rule.
- Port 80 and 9443 were unaffected because no LoadBalancer Service used those
  ports.

## Fix

Tell Istio to create the MaaS gateway's backing Service as `ClusterIP` instead
of `LoadBalancer`, using the same `infrastructure.parametersRef` mechanism that
the ODH operator uses for the data-science-gateway:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: maas-gateway-config
  namespace: openshift-ingress
data:
  service: |
    spec:
      type: ClusterIP
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: maas-default-gateway
  namespace: openshift-ingress
spec:
  infrastructure:
    parametersRef:
      group: ""
      kind: ConfigMap
      name: maas-gateway-config
  # ... rest of spec
```

ocp-sim's reverse proxy already routes `*.apps.ocp-sim.test` hostnames to the
correct Istio gateway Service via ClusterIP — no LoadBalancer IP is needed.

## Lesson

Any Gateway API Gateway in the cluster that uses port 443 and gets a
LoadBalancer Service with the node IP will hijack traffic from the ocp-sim
proxy. All Gateways in picoshift should use `ClusterIP` via
`infrastructure.parametersRef`.
