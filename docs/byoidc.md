# BYOIDC (Bring Your Own OIDC)

Picoshift supports three authentication modes, configured via `--auth-mode`:

| Mode | Token format | Validation | Use case |
|------|-------------|------------|----------|
| `legacy` | `sha256~` opaque | userinfo endpoint call | Default, simple |
| `oidc` | JWT (RS256) | JWKS (built-in keys) | Built-in OIDC provider |
| `byoidc` | JWT (RS256) | JWKS (external provider) | External OIDC (Entra, etc.) |

## Architecture

```
                 ┌─────────────┐
  Browser ──────►│  ocp-shim   │──► kube-apiserver
                 │ (JWT/JWKS)  │
                 └──────┬──────┘
                        │ validate via JWKS
                 ┌──────▼──────┐         ┌──────────────┐
                 │  ocp-sim    │ ◄──────►│ entra-mock   │
                 │  OAuth srv  │ proxy   │ (OIDC IDP)   │
                 └─────────────┘         └──────────────┘
```

In BYOIDC mode, ocp-sim's OAuth server acts as a thin proxy for browser flows:
- `/oauth/authorize` → redirects to the external authorization endpoint
- `/oauth/token` → proxies code exchange to the external token endpoint
- `/oauth/jwks` → proxies the external JWKS
- `/oauth/userinfo` → validates JWT via external JWKS, returns user info
- User/Identity CRs are created in the cluster on successful authentication

CLI flows (`oc login`) use Basic Auth and are handled locally — they bypass
the external provider entirely.

## Quick Start with Built-in OIDC

No external provider needed — ocp-sim generates RSA keys and issues JWTs:

```bash
make all AUTH_MODE=oidc
```

Verify:
```bash
curl -sk https://localhost:443/.well-known/openid-configuration | jq .
curl -sk https://localhost:443/oauth/jwks | jq .
```

The `Authentication` CR will show `spec.type: OIDC`, which causes the ODH
operator's kube-auth-proxy to deploy with `--provider=oidc` instead of
`--provider=openshift`.

## BYOIDC with Entra ID Emulator

The entra-id-emulator (`example.src/entra-id-emulator/`) is a lightweight
Azure Entra ID mock that supports dynamic user CRUD via REST API.

### One-command deploy

```bash
make all
make deploy-byoidc
```

This builds the entra-mock image, loads it into the kind cluster, deploys
it as a Deployment+Service in the `entra-mock` namespace, then redeploys
the simulator in BYOIDC mode pointing at it.

### What gets deployed

- **entra-mock**: `entra-mock.entra-mock.svc.cluster.local:8080` (HTTP)
- **Client**: `picoshift` / `picoshift-secret`
- **Tenant**: `a1b2c3d4-e5f6-7890-abcd-ef1234567890`
- **Users**: admin/admin, user1/user1, developer/developer (matching `users.yaml`)

### Verify

```bash
# Entra mock is running
kubectl -n entra-mock get pods

# OIDC discovery works
sudo podman exec ocp-sim-control-plane \
  curl -s http://entra-mock.entra-mock.svc.cluster.local:8080/a1b2c3d4-e5f6-7890-abcd-ef1234567890/v2.0/.well-known/openid-configuration | jq .

# JWKS proxied through simulator
sudo podman exec ocp-sim-control-plane \
  curl -sk https://localhost:443/oauth/jwks | jq .

# oc login still works (Basic Auth, handled locally)
oc login https://localhost:6443 -u admin -p admin --insecure-skip-tls-verify

# Authentication CR
kubectl get authentication cluster -o jsonpath='{.spec.type}'
# → OIDC
```

### Dynamic user management

Users can be created/updated/deleted at runtime via the admin API:

```bash
# List users
kubectl -n entra-mock exec deployment/entra-mock -- \
  curl -s -u :changeme1234 http://localhost:8080/admin/api/users | python3 -m json.tool

# Create a user
kubectl -n entra-mock exec deployment/entra-mock -- \
  curl -s -u :changeme1234 -X POST http://localhost:8080/admin/api/users \
  -H 'Content-Type: application/json' \
  -d '{"tenant_id":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","upn":"newuser","email":"new@ocp-sim.test","display_name":"New User","password":"newpass"}'

# Delete a user
kubectl -n entra-mock exec deployment/entra-mock -- \
  curl -s -u :changeme1234 -X DELETE http://localhost:8080/admin/api/users/<user-id>
```

## ocp-shim JWT Validation

The ocp-shim validates JWTs from any OIDC issuer via JWKS. It supports
dual-mode: `sha256~` tokens are validated via the userinfo endpoint,
while `eyJ` (JWT) tokens are validated via JWKS signature verification.

The JWKS cache refreshes every 5 minutes. The issuer URL is configured via:
- `--oidc-issuer-url` flag (set in the sidecar template)
- `OIDC_ISSUER_URL` environment variable (overrides the flag)

## Token Flow

### Browser flow (BYOIDC)

1. Browser hits `/oauth/authorize`
2. ocp-sim redirects to entra-mock's authorization endpoint
3. User authenticates with entra-mock login form
4. entra-mock redirects back with authorization code
5. Client exchanges code at `/oauth/token` → ocp-sim proxies to entra-mock
6. entra-mock returns JWT access_token + id_token
7. ocp-sim validates JWT, creates User/Identity CRs
8. Client uses JWT for API calls → ocp-shim validates via JWKS

### CLI flow (`oc login`)

1. `oc login` sends `X-CSRF-Token` → ocp-sim returns `401 Basic` challenge
2. `oc login` sends `Authorization: Basic` → ocp-sim validates locally
3. ocp-sim issues authorization code, `oc` exchanges for token
4. Token is `sha256~` (legacy) or JWT (oidc mode) — handled by local store
