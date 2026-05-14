# BYOIDC Support for Picoshift

## Context

Picoshift currently uses a custom OAuth server (`oauth.rs`) that issues **opaque** `sha256~` tokens and validates them via an in-memory HashMap. The ocp-shim validates these tokens by calling `/oauth/userinfo` on every API request. This works but doesn't match how real OpenShift OIDC mode works, and it prevents users from plugging in an external OIDC provider (Keycloak, Dex, Auth0, etc.).

This plan adds two capabilities:
1. **Built-in OIDC mode** — upgrade the OAuth server to issue JWTs with proper OIDC discovery (`.well-known/openid-configuration`, JWKS endpoint)
2. **BYOIDC mode** — point picoshift at an external OIDC provider for both gateway and API server auth

The ODH operator already supports dual-mode kube-auth-proxy: it reads `Authentication.spec.type` and selects either the OAuth template (`--provider=openshift`) or the OIDC template (`--provider=oidc`). So the key lever is: set the Authentication CR to `OIDC` and provide a standards-compliant OIDC issuer.

## Phase 1: Built-in OIDC Provider (JWT Issuance)

**Goal**: Make the built-in OAuth server issue JWTs and serve OIDC discovery endpoints. This is the foundation for both modes.

### 1.1 Add `jsonwebtoken` dependency

**File**: `simulator/Cargo.toml`
- Add `jsonwebtoken = "9"` (uses `ring` internally, already a transitive dep via rustls)

### 1.2 Add `--auth-mode` CLI arg

**File**: `simulator/src/main.rs`
- Add `AuthMode` enum: `Legacy` (default), `Oidc`
- Add `--auth-mode` arg to `Args` struct
- Pass `auth_mode` to `oauth::run()`

### 1.3 JWT signing and OIDC endpoints in OAuth server

**File**: `simulator/src/oauth.rs`

**New struct `JwtKeys`**: Generate RSA-2048 key pair at startup. Holds `EncodingKey` (private, for signing), public key components `n`/`e` (for JWKS), and a random `kid`.

**Modify `OAuthState`**: Add `jwt_keys: Option<Arc<JwtKeys>>` and `auth_mode` fields.

**Modify token issuance** (`handle_token`): When `auth_mode == Oidc`, issue a signed JWT instead of `sha256~{random}`. Claims: `iss`, `sub`, `aud`, `preferred_username`, `email`, `groups`, `iat`, `exp`. Still store in `state.tokens` so `/oauth/userinfo` works unchanged.

**New endpoint `/.well-known/openid-configuration`**: Returns OIDC discovery JSON with `issuer`, `authorization_endpoint`, `token_endpoint`, `userinfo_endpoint`, `jwks_uri`, `response_types_supported`, `subject_types_supported`, `id_token_signing_alg_values_supported`.

**New endpoint `/oauth/jwks`**: Returns JWK Set with the RSA public key (`kty`, `use`, `kid`, `alg`, `n`, `e`).

**Update request router** (`handle_request`): Add the two new routes.

### 1.4 Update deployment

**File**: `deploy/simulator.yaml`
- Add `--auth-mode` to args (default: leave absent = `legacy` for backward compat)
- When deploying in OIDC mode, set `--auth-mode oidc`

### What stays the same
- `sha256~` tokens in `legacy` mode (default)
- Login form, Basic Auth challenge, OAuthClient validation
- User/Identity CR creation on login
- `/oauth/authorize`, `/oauth/token`, `/oauth/userinfo` all work in both modes

---

## Phase 2: JWT Validation in ocp-shim

**Goal**: Teach the ocp-shim to validate JWTs via JWKS instead of only validating opaque tokens via the userinfo endpoint call.

### 2.1 Add JWT dependency to ocp-shim

**File**: `example.src/kind/cmd/ocp-shim/go.mod`
- Add `github.com/golang-jwt/jwt/v5`
- Run `go mod tidy` to generate `go.sum`

### 2.2 Add JWKS-based JWT validation

**File**: `example.src/kind/cmd/ocp-shim/main.go`

**New CLI flag**: `--oidc-issuer-url` (string, default empty). When set, enables JWT validation path.

**New `JWKSCache` struct**: Fetches `{issuer}/.well-known/openid-configuration`, extracts `jwks_uri`, fetches JWKS, parses RSA public keys indexed by `kid`. Background goroutine refreshes every 5 minutes.

**New `validateJWTToken()` function**: Parse JWT, extract `kid` from header, look up public key in cache, verify RS256 signature, extract `preferred_username` and `groups` claims.

**Modify Bearer token handler** (line 374): Change from sha256-only to dual-mode:
```
if token starts with "sha256~" → validate via userinfo (existing)
else if jwksCache != nil && token starts with "eyJ" → validate via JWKS (new)
else → pass through to upstream (SA tokens, etc.)
```

**Modify `handleTokenReview`** (line 66): Add JWT branch alongside sha256 handling.

**Modify WebSocket handler** (line 442): Add JWT validation alongside sha256 path.

### 2.3 Update ocp-shim sidecar injection

**File**: `example.src/kind/pkg/cluster/internal/create/actions/ocpshim/ocpshim.go`
- Add `--oidc-issuer-url https://localhost:9443` to the sidecar command template (always present — the built-in OAuth server is always reachable on localhost since both run with hostNetwork)

### 2.4 Update Makefile

- Copy `go.sum` alongside `go.mod` and `main.go` when building the kind base image

---

## Phase 3: External OIDC (BYOIDC Mode)

**Goal**: Allow pointing picoshift at an external OIDC provider for both gateway and API server auth.

### 3.1 Add BYOIDC CLI args

**File**: `simulator/src/main.rs`
- `--oidc-issuer-url` (external issuer URL — triggers BYOIDC mode)
- `--oidc-client-id`
- `--oidc-client-secret`
- `--oidc-username-claim` (default: `preferred_username`)
- `--oidc-groups-claim` (default: `groups`)

**Mode resolution**: `--oidc-issuer-url` set → BYOIDC; `--auth-mode=oidc` → built-in OIDC; else → legacy.

### 3.2 OAuth server behavior in BYOIDC mode

**File**: `simulator/src/oauth.rs`

When BYOIDC is active:
- `/.well-known/openid-configuration` → proxy/return the external issuer's discovery doc
- `/oauth/authorize` → redirect to external provider's authorization endpoint
- `/oauth/userinfo` → validate external JWT via JWKS, return claims, create User/Identity CRs on first contact
- Login form is not served (external provider handles login)

### 3.3 Authentication CR for OIDC mode

**New file**: `seed/authentication-oidc.yaml`
```yaml
apiVersion: config.openshift.io/v1
kind: Authentication
metadata:
  name: cluster
spec:
  type: OIDC
```
This triggers the ODH operator to deploy kube-auth-proxy with `--provider=oidc` instead of `--provider=openshift`.

### 3.4 ocp-shim for external OIDC

The `--oidc-issuer-url` flag added in Phase 2 already supports pointing at any issuer. For BYOIDC, the value changes from `https://localhost:9443` to the external provider URL. This requires either:
- A Makefile target that hot-patches the ocp-shim sidecar args, or
- Making the issuer URL configurable via an environment variable read at ocp-shim startup

### 3.5 Makefile targets

```makefile
# Deploy with built-in OIDC
make all AUTH_MODE=oidc

# Deploy with external OIDC
make deploy-byoidc OIDC_ISSUER_URL=https://keycloak.example.com/realms/picoshift \
    OIDC_CLIENT_ID=picoshift OIDC_CLIENT_SECRET=secret
```

### 3.6 User/Identity CRs for external users

Approach: keep the OAuth server running in minimal mode. When it receives `/oauth/userinfo` calls (from ocp-shim), it validates the external JWT via JWKS, creates User/Identity CRs if they don't exist, and returns the claims. This reuses existing code paths.

---

## Phase 4: Documentation

**New file**: `docs/byoidc.md`
- Architecture overview for both modes
- Configuration reference (CLI args, env vars)
- Step-by-step guide for built-in OIDC mode
- Step-by-step guide for BYOIDC with Keycloak (example)
- Troubleshooting

---

## Key Files

| File | Changes |
|------|---------|
| `simulator/Cargo.toml` | Add `jsonwebtoken = "9"` |
| `simulator/src/main.rs` | `AuthMode` enum, new CLI args, pass to oauth::run |
| `simulator/src/oauth.rs` | JWT signing, OIDC discovery, JWKS, BYOIDC proxy mode |
| `example.src/kind/cmd/ocp-shim/go.mod` | Add `golang-jwt/jwt/v5` |
| `example.src/kind/cmd/ocp-shim/main.go` | JWKS cache, JWT validation, `--oidc-issuer-url` flag |
| `example.src/kind/pkg/cluster/internal/create/actions/ocpshim/ocpshim.go` | Sidecar template: add `--oidc-issuer-url` |
| `seed/authentication.yaml` | Unchanged (IntegratedOAuth default) |
| `seed/authentication-oidc.yaml` | New: `spec.type: OIDC` |
| `deploy/simulator.yaml` | Add `--auth-mode` arg |
| `Makefile` | `AUTH_MODE` variable, `deploy-byoidc` target |
| `docs/byoidc.md` | New: user-facing documentation |

## Verification

### Phase 1
```bash
# Deploy with built-in OIDC
# Verify: curl -sk https://localhost:9443/.well-known/openid-configuration | jq .
# Verify: curl -sk https://localhost:9443/oauth/jwks | jq .
# Verify: login flow returns JWT (starts with eyJ), decode and check claims
# Verify: /oauth/userinfo still works with JWT bearer token
# Verify: legacy mode still issues sha256~ tokens (default)
```

### Phase 2
```bash
# Verify: kubectl with JWT token works (ocp-shim validates via JWKS)
# Verify: SA tokens still pass through to upstream apiserver
# Verify: oc login still works (Basic Auth → sha256~ or JWT depending on mode)
# Verify: WebSocket auth works with JWT tokens
# Verify: TokenReview API works with JWT tokens
```

### Phase 3
```bash
# Deploy Keycloak (or Dex) in/alongside the cluster
# Set OIDC_ISSUER_URL, CLIENT_ID, CLIENT_SECRET
# Verify: Authentication CR shows spec.type: OIDC
# Verify: ODH operator deploys kube-auth-proxy with --provider=oidc
# Verify: Dashboard login redirects to external provider
# Verify: kubectl with external JWT works against API server
```
