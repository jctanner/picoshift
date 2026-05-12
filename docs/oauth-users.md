# OAuth Server: Real Login + User Management

## Problem

The OAuth server (`simulator/src/oauth.rs`) auto-grants an authorization code
on `GET /oauth/authorize` without any login prompt. Every token maps to the
hardcoded user `admin`. There is no way to:

- Log in as a different user
- Require a password
- Manage users, passwords, or group memberships

## Current Flow

```
Browser → GET /oauth/authorize?client_id=...&redirect_uri=...
  ↓ (no login prompt — immediately issues code)
302 redirect_uri?code=<random>
  ↓
POST /oauth/token  (code + client_id + client_secret)
  ↓
{ access_token: "sha256~...", ... }
  ↓
GET /oauth/userinfo  (Bearer sha256~...)
  ↓
{ preferred_username: "admin" }   ← always "admin"
```

## Proposed Flow

```
Browser → GET /oauth/authorize?client_id=...&redirect_uri=...
  ↓
200 HTML login form (username + password fields)
  ↓
POST /oauth/authorize  (username, password, + original query params)
  ↓ (validate credentials against user store)
302 redirect_uri?code=<random>     ← on success
200 login form + error message     ← on failure
  ↓
POST /oauth/token  (unchanged)
  ↓
GET /oauth/userinfo
  ↓
{ preferred_username: "<actual-user>" }   ← from token→user mapping
```

## Design

### 1. User Store: `users.yaml` file

A YAML file that defines users, passwords, and group memberships. Loaded at
startup; changes require a simulator restart (fine for dev).

**Location**: configurable via `--users-file` flag, default `users.yaml` in
the working directory.

**Format**:
```yaml
users:
  - username: admin
    password: admin
    email: admin@ocp-sim.test
    groups:
      - system:cluster-admins
      - system:authenticated

  - username: user1
    password: user1
    email: user1@ocp-sim.test
    groups:
      - system:authenticated

  - username: developer
    password: developer
    email: developer@ocp-sim.test
    groups:
      - system:authenticated
      - devteam
```

**Passwords**: stored as plaintext in the file (this is a local dev simulator,
not a production auth system). bcrypt or argon2 would be overkill and add
dependencies — anyone who can read the file is already on the dev machine.

**Default**: if no `--users-file` is provided or the file doesn't exist, fall
back to a single built-in user `admin`/`admin` (preserves current behavior for
zero-config startup).

### 2. Login Page

`handle_authorize` currently returns an immediate 302 redirect. Change it to:

- **GET /oauth/authorize**: return an HTML login form. The form POSTs back to
  the same URL with `username` and `password` fields, plus the original OAuth
  query params (`client_id`, `redirect_uri`, `state`, `response_type`) as
  hidden fields.

- **POST /oauth/authorize**: validate credentials against the user store.
  On success, issue an auth code and 302 redirect (same as today). On failure,
  re-render the login form with an error message.

The HTML should be minimal — inline CSS, no JS frameworks. Mimic the OpenShift
login page layout (centered card, red error banner) loosely enough to not
confuse ODH dashboard's redirect detection.

### 3. Token→User Mapping

Currently `OAuthState.tokens` maps `token → TokenInfo { client_id, created }`.
The token has no user association — `handle_userinfo` always returns `admin`.

Change `TokenInfo` to include the authenticated username:

```rust
struct TokenInfo {
    username: String,
    _client_id: String,
    created: Instant,
}
```

Thread the username through:
- `handle_authorize` (POST) → stores username in `AuthCode`
- `handle_token` → copies username from `AuthCode` into `TokenInfo`
- `handle_userinfo` → reads username from `TokenInfo`, looks up email/groups
  from the user store

### 4. Userinfo Response

Currently hardcoded. Change to look up the user from the store:

```json
{
  "sub": "<username>",
  "name": "<username>",
  "preferred_username": "<username>",
  "email": "<email from users.yaml>",
  "groups": ["system:authenticated", ...]
}
```

### 5. Group Propagation

The ocp-shim (`main.go`) already reads `preferred_username` from userinfo and
sets `X-Remote-User`. It sets `X-Remote-Group: system:authenticated` hardcoded.

For group support, two options:

**Option A (simple)**: Add `groups` to the userinfo response. Have ocp-shim
read `groups` from userinfo and set `X-Remote-Group` headers accordingly.
This means ocp-shim needs a small change to `validateOAuthToken`.

**Option B (no shim change)**: Keep groups only in the user store, rely on
RBAC bindings per-user rather than per-group. Doesn't match real OCP behavior.

**Recommendation**: Option A. The ocp-shim change is ~5 lines.

### 6. AuthCode Changes

`AuthCode` needs to carry the authenticated username so `handle_token` can
associate it with the issued token:

```rust
struct AuthCode {
    username: String,
    client_id: String,
    _redirect_uri: String,
    created: Instant,
}
```

## Files to Change

| File | Change |
|------|--------|
| `simulator/src/oauth.rs` | Login page, user store, token→user mapping |
| `simulator/src/main.rs` | Pass `--users-file` CLI arg to oauth module |
| `users.yaml` (new) | Default user definitions |
| `example.src/kind/cmd/ocp-shim/main.go` | Read groups from userinfo response |
| `Makefile` | Copy `users.yaml` into kind node or mount it |

## Implementation Order

1. **User store**: add `UserStore` struct, load from YAML, fallback default
2. **AuthCode + TokenInfo**: add `username` field to both structs
3. **POST /oauth/authorize**: validate credentials, issue code with username
4. **GET /oauth/authorize**: serve HTML login form
5. **handle_userinfo**: return actual user info from store
6. **ocp-shim groups**: read groups array from userinfo, set X-Remote-Group
7. **users.yaml + Makefile**: ship default file, wire into deploy

## What Doesn't Change

- Token format (`sha256~...`) — unchanged
- Token exchange (`POST /oauth/token`) — unchanged except storing username
- Well-known discovery — unchanged
- OAuthClient validation — unchanged
- TLS setup — unchanged
- ocp-shim TokenReview handling — unchanged (it calls userinfo which now
  returns the right user)

## Stretch Goals (not in scope now)

- Hot-reload `users.yaml` without restart (watch file for changes)
- Password hashing
- Session cookies (remember login across authorize calls)
- User CRUD API (`POST /oauth/users`)
- OIDC ID tokens
