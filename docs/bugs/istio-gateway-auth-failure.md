# Istio Gateway Pods CrashLoopBackOff Due to Auth Failure

## Summary

The `data-science-gateway` istio-proxy containers fail to authenticate to istiod, causing CrashLoopBackOff. The proxy cannot obtain a workload certificate because istiod rejects its service account token.

## Symptoms

- Gateway pods in `CrashLoopBackOff` with `0/1` ready
- Proxy logs: `failed to sign CSR: create certificate: rpc error: code = Unauthenticated desc = request authenticate failure`
- Istiod logs: `Failed to authenticate client: KubeJWTAuthenticator: failed to validate the JWT from cluster "Kubernetes": the token is not authenticated`

## Root Cause

**The actual root cause is the ocp-shim TokenReview bug** — see
[picoshift-tokenreview-bug.md](picoshift-tokenreview-bug.md).

Istiod validates gateway pod service account tokens by submitting a
`TokenReview` request to the API server. On picoshift, all in-cluster traffic
goes through ocp-shim on port 6443. The ocp-shim proxy breaks `TokenReview`
responses, returning `authenticated: false` for valid tokens. The same tokens
validate correctly when submitted directly to the real kube-apiserver on port
16443.

### Red herring: --api-audiences

The initial diagnosis blamed a missing `--api-audiences` flag on the API
server. Kind's API server does not include `istio-ca` in `--api-audiences` by
default, and picoshift's `create.go` patches this during `--with-ossm3`
cluster creation. This patch makes the projected token claims correct, but it
does not fix the problem — the gateway pods still CrashLoop with the exact
same error after the patch is applied, because the TokenReview itself is broken
at the ocp-shim layer.

## Affected Component

- Istio gateway pods in `openshift-ingress` namespace
- istiod in `istio-system` namespace
- ocp-shim proxy (root cause)

## Environment

- picoshift (kind-based OpenShift simulator)
- Istio installed with `revision=openshift-gateway`
