# Istio Gateway Pods CrashLoopBackOff Due to Auth Failure

## Summary

The `data-science-gateway` istio-proxy containers fail to authenticate to istiod, causing CrashLoopBackOff. The proxy cannot obtain a workload certificate because istiod rejects its service account token.

## Symptoms

- Gateway pods in `CrashLoopBackOff` with `0/1` ready
- Proxy logs: `failed to sign CSR: create certificate: rpc error: code = Unauthenticated desc = request authenticate failure`
- Istiod logs: `Failed to authenticate client: KubeJWTAuthenticator: failed to validate the JWT from cluster "Kubernetes": the token is not authenticated`

## Root Cause

Kind's API server does not include `istio-ca` in its `--api-audiences` flag by default. Istio gateway pods mount a projected service account token with `audience: istio-ca`. When istiod performs a TokenReview against the API server, the API server rejects the token because `istio-ca` is not a recognized audience.

Picoshift's `create.go` patches the API server manifest to add `--api-audiences=https://kubernetes.default.svc.cluster.local,istio-ca`, but this only runs during picoshift's own gateway stack installation flow. When the gateway stack is installed separately (e.g. via Ansible playbooks), the patch must be applied explicitly.

Additionally, if istiod is already running when the patch is applied, it must be restarted to pick up the new API server token validation behavior.

## Why This Wasn't Seen During Development

During development, the gateway stack was always installed via `picoshift create --with-ossm3`, which calls `installOSSM3()` in `cli/cmd/create.go`. That function automatically calls `patchAPIServerAudiences()` (line ~692) to add the `istio-ca` audience before istiod starts — so the problem never surfaced.

The bug only appears when the gateway stack is installed through a separate path (e.g. `make gateway-stack`, Ansible playbooks, or manual `istioctl install`) that doesn't go through picoshift's built-in OSSM3 flow. That path skips the API server patch entirely.

## Fix Applied

1. Patch `/etc/kubernetes/manifests/kube-apiserver.yaml` inside the kind node to add `--api-audiences`
2. Wait for the API server to restart (static pod manifest change)
3. Install Istio
4. Restart istiod deployment

This is now automated in `playbooks/install-gateway-stack.yml`.

## Affected Component

- Istio gateway pods in `openshift-ingress` namespace
- istiod in `istio-system` namespace

## Environment

- picoshift (kind-based OpenShift simulator)
- Istio installed with `revision=openshift-gateway`
