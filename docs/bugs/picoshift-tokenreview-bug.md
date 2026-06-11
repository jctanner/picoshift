# picoshift bug: ocp-shim breaks TokenReview API

## Summary

The `ocp-shim` proxy sitting in front of the real kube-apiserver on port 6443 causes `TokenReview` requests to return `authenticated: false` for valid service account tokens. The real kube-apiserver (port 16443) correctly validates the same tokens.

This breaks any workload that relies on the TokenReview API, including istiod's CSR signing flow (OSSM3 gateway pods crash-loop with `request authenticate failure`).

## Architecture

Inside the kind node container (`ocp-sim-control-plane`):

| Process | Port | Role |
|---------|------|------|
| `kube-apiserver` | 16443 | Real API server |
| `ocp-shim` | 6443 | Proxy that routes some traffic to ocp-sim |
| `ocp-sim` | 9443 | Simulated OpenShift services (OIDC, etc.) |

The kubeconfig points at 6443 (mapped to the host via kind). All kubectl and in-cluster traffic goes through ocp-shim.

## Reproduction

```bash
# Create a valid SA token
TOKEN=$(kubectl create token default -n default --audience=istio-ca)

# TokenReview via ocp-shim (port 6443) — FAILS
echo "{...token...}" | kubectl create -f - -o yaml
# status:
#   authenticated: false

# Direct auth with the same token — WORKS
kubectl --token="$TOKEN" get namespaces
# returns namespace list successfully

# TokenReview directly against real kube-apiserver (port 16443) — WORKS
podman exec ocp-sim-control-plane curl -sk \
  --cacert /etc/kubernetes/pki/ca.crt \
  --cert /etc/kubernetes/pki/apiserver-kubelet-client.crt \
  --key /etc/kubernetes/pki/apiserver-kubelet-client.key \
  -X POST https://localhost:16443/apis/authentication.k8s.io/v1/tokenreviews \
  -H "Content-Type: application/json" \
  -d '{"apiVersion":"authentication.k8s.io/v1","kind":"TokenReview","spec":{"token":"...","audiences":["istio-ca"]}}'
# status:
#   authenticated: true
#   user:
#     username: "system:serviceaccount:default:default"
```

## Confirmed details

- API server `--api-audiences` includes `istio-ca` — correct
- API server `--service-account-key-file` and `--service-account-signing-key-file` use matching keys — verified via modulus comparison
- API server `--service-account-issuer` is `https://kubernetes.default.svc.cluster.local` — correct
- Projected SA tokens have the correct `aud`, `iss`, and `kid` claims
- The issue is not audience, key mismatch, or token format — it is the ocp-shim proxy layer

## Impact

Any component that uses TokenReview to validate SA tokens fails:

- **istiod CSR signing**: Gateway envoy pods can't get workload certificates, crash-loop with:
  ```
  Failed to authenticate client: Authenticator KubeJWTAuthenticator:
  failed to validate the JWT from cluster "Kubernetes": the token is not authenticated
  ```
- **Webhook admission controllers** that validate caller identity via TokenReview
- **Any custom controller** using the `authentication.k8s.io/v1` TokenReview API

## Suggested fix

The ocp-shim should pass `TokenReview` requests through to the real kube-apiserver on port 16443 without modification. The shim likely intercepts or re-handles the authentication path in a way that drops the SA token validation.
