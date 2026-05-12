# OAuth Server: Real Login + User Management

## Reference Implementation

The real OpenShift OAuth server lives in
[openshift/oauth-server](https://github.com/openshift/oauth-server) (Go, uses
the [osin](https://github.com/openshift/osin) OAuth2 library). Key source
files:

| Path | Purpose |
|------|---------|
| `pkg/oauthserver/auth.go` | Wires identity providers, login handlers, session, CSRF |
| `pkg/server/login/login.go` | GET (render form) / POST (validate creds) login handler |
| `pkg/server/login/templates.go` | Default login page HTML (PatternFly v6) |
| `pkg/authenticator/password/htpasswd/htpasswd.go` | HTPasswd file authenticator (bcrypt, apr1-MD5, SHA-1) |
| `pkg/osinserver/osinserver.go` | `/oauth/authorize` + `/oauth/token` + `/oauth/info` endpoints |
| `pkg/osinserver/tokengen.go` | Token generation — `sha256~<random>` format |
| `pkg/osinserver/registrystorage/storage.go` | Persists tokens as OAuthAccessToken / OAuthAuthorizeToken API objects |
| `pkg/server/crypto/sha256.go` | `SHA256Prefix = "sha256~"`, hashing for object names |
| `pkg/userregistry/identitymapper/provision.go` | Creates User + Identity API objects on first login |
| `pkg/userregistry/identitymapper/strategy_claim.go` | Default "claim" mapping: first identity to claim a username wins |
| `pkg/groupmapper/` | Enriches authenticated user info with Group memberships |

A shallow clone is checked in at `example.src/oauth-server/` for easy
reference.

---

## How Real OpenShift Login Works

### Full authorize → login → token flow

```
 oc login / browser
   │
   ▼
 GET /oauth/authorize?client_id=...&redirect_uri=...&response_type=code
   │
   │  osin parses the authorize request
   │  AuthorizeAuthenticator checks for existing session (cookie)
   │  No session → AuthenticationNeeded
   │
   ▼
 unionAuthenticationHandler:
   │  if single password IDP → redirect to /login?then=%2Foauth%2Fauthorize%3F...
   │  if multiple IDPs       → redirect to /login/<provider-name>?then=...
   │
   ▼
 GET /login (or /login/<provider>)
   │  Login.handleLoginForm():
   │    - generate CSRF token (cookie-based)
   │    - render HTML form with hidden fields: csrf, then
   │    - return 200 text/html
   │
   ▼
 POST /login  (form: username, password, csrf, then)
   │  Login.handleLogin():
   │    1. Check CSRF token
   │    2. Validate username non-empty, password non-empty
   │    3. Call auth.AuthenticatePassword(ctx, username, password)
   │       │
   │       ├─ HTPasswd: loadIfNeeded() (hot-reload on file mtime change)
   │       │            parse "user:hash" lines
   │       │            testPassword: try bcrypt ($2y$/$2a$), apr1-MD5 ($apr1$), SHA-1 ({SHA})
   │       │            on match → NewDefaultUserIdentityInfo(providerName, username)
   │       │            → identitymapper.ResponseFor(mapper, identity)
   │       │
   │       └─ BasicAuth: POST username:password to remote HTTP endpoint
   │                     check 2xx response
   │                     → identitymapper.ResponseFor(mapper, identity)
   │
   │    4. On success → auth.AuthenticationSucceeded(user, then, w, req)
   │       │
   │       ├─ SessionAuth: set session cookie (remembers user is logged in)
   │       └─ redirectSuccessHandler: http.Redirect(then) → back to /oauth/authorize
   │
   │    5. On failure → re-render login form with ?reason=access_denied
   │
   ▼
 GET /oauth/authorize (second time, now with session cookie)
   │  AuthorizeAuthenticator: session cookie → user is authenticated
   │  GrantCheck: auto-approve or show approval page depending on OAuthClient config
   │  FinishAuthorizeRequest:
   │    - generate auth code: "sha256~" + random256BitsString()
   │    - persist as OAuthAuthorizeToken API object (name = sha256 hash of code)
   │    - OAuthAuthorizeToken stores: UserName, UserUID, ClientName, Scopes, RedirectURI
   │
   ▼
 302 redirect_uri?code=sha256~...&state=...
   │
   ▼
 POST /oauth/token  (grant_type=authorization_code, code, client_id, client_secret)
   │  osin HandleAccessRequest:
   │    - LoadAuthorize: GET OAuthAuthorizeToken by sha256-hashed name
   │    - verify client_id, client_secret (supports SA JWT tokens via TokenReview)
   │    - generate access token: "sha256~" + randomToken()
   │    - persist as OAuthAccessToken API object:
   │        name:       sha256(token)     ← hashed, the raw token is never stored
   │        userName:   from authorize token
   │        userUID:    from authorize token
   │        clientName: client_id
   │        scopes:     from authorize token
   │    - delete the used OAuthAuthorizeToken (one-time use)
   │
   ▼
 { "access_token": "sha256~<random>", "token_type": "Bearer", "expires_in": 86400 }
```

### Token format and storage

The raw token the client receives is `sha256~<base64url-random-256-bits>`.
The OAuthAccessToken object **name** stored in etcd is a SHA-256 hash of the
random portion: `sha256~base64url(sha256(random))`. This means the API server
never stores the raw token — it can only validate by hashing what the client
presents and looking up the result.

```
raw token:    sha256~ABCxyz123...   ← what the user sees / sends in Authorization header
object name:  sha256~<hash>        ← sha256 of "ABCxyz123..." base64url-encoded
```

### User + Identity provisioning (first login)

When `identitymapper.ResponseFor()` is called after password validation, the
provisioning flow is:

```
AuthenticatePassword succeeds
  → NewDefaultUserIdentityInfo(providerName="htpasswd", userName="alice")
  → identityMapper.UserFor(info)
      │
      │  identity name = "htpasswd:alice"
      │
      ├─ GET Identity "htpasswd:alice" → 404 Not Found (first login)
      │
      ├─ provisioningStrategy.UserForNewIdentity("alice", identity)
      │  │
      │  │  (StrategyClaim — default)
      │  │
      │  ├─ GET User "alice" → 404 → CREATE User{name:"alice", identities:["htpasswd:alice"]}
      │  │                     200 + no identities → claim it (update user.identities)
      │  │                     200 + other identities → claimError (conflict)
      │  │
      │  └─ return persisted User (with UID)
      │
      ├─ CREATE Identity{name:"htpasswd:alice", providerName:"htpasswd",
      │                   providerUserName:"alice",
      │                   user: {name:"alice", uid:"<user-uid>"}}
      │
      └─ return user.Info{name:"alice", uid:"<user-uid>"}
```

On subsequent logins the Identity already exists, so it skips straight to
`getMapping()` — which GETs the Identity, GETs the referenced User, validates
UID consistency, and returns the user info.

Race conditions (double-click, multiple instances) are handled with up to
3 retries on `AlreadyExists` / `Conflict` errors.

### Group enrichment

After identity mapping, `groupmapper.NewUserGroupsMapper()` wraps the result:
it queries Group objects where `spec.users` contains the username and adds
matching group names to `user.Info.Groups`. This means groups are defined as
Group API objects (not in the htpasswd file) and looked up at authentication
time.

### Challenge path (oc login)

When `oc login` sends credentials, it uses HTTP Basic Auth (`Authorization:
Basic base64(user:pass)`). The OAuth server detects this via
`BasicAuthAuthentication` and responds with a `401 WWW-Authenticate: Basic
realm="openshift"` challenge if unauthenticated. If credentials are provided,
it authenticates directly without a login page redirect — same identity
mapping + provisioning flow, just no HTML/session involved.

---

## Problem

The simulator OAuth server (`simulator/src/oauth.rs`) auto-grants an
authorization code on `GET /oauth/authorize` without any login prompt. Every
token maps to the hardcoded user `admin`. There is no way to:

- Log in as a different user
- Require a password
- Manage users, passwords, or group memberships
- Create User / Identity API objects (real OCP does this automatically)

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

**Simplification vs. real OCP**: real OpenShift redirects from `/oauth/authorize`
to `/login`, uses session cookies, and redirects back. Our simulator collapses
this into the `/oauth/authorize` endpoint itself — the login form POSTs back
to the same URL. This avoids session/cookie management while achieving the same
end result: the user must authenticate before an auth code is issued.

---

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
not a production auth system). Real OpenShift uses htpasswd format (bcrypt,
apr1-MD5, SHA-1) via `htpasswd.testPassword()`, but that's overkill here —
anyone who can read the file is already on the dev machine.

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

Real OpenShift uses PatternFly v6 with full i18n support
(`pkg/server/login/templates.go`). We don't need any of that — a basic
centered form is fine.

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

### 4. User + Identity API Object Creation

Real OpenShift creates `User` and `Identity` API objects on first login. Our
simulator should do the same to match real cluster behavior (some operators
and tools query these objects).

On successful authentication, before issuing the auth code:

```rust
// Create Identity: "htpasswd:<username>"
// Create User: { name: "<username>", identities: ["htpasswd:<username>"] }
// Link: identity.user = { name: "<username>", uid: "<user-uid>" }
```

Use the "claim" strategy: if a User with that name already exists and has no
identities (or already has our identity), claim it. If it has a different
identity, reject.

The Identity and User CRDs are already registered in the simulator's CRD set.
Creating these objects means `oc get users` and `oc get identities` will work
as expected.

### 5. Userinfo Response

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

### 6. Group Propagation

The ocp-shim (`main.go`) already reads `preferred_username` from userinfo and
sets `X-Remote-User`. It sets `X-Remote-Group: system:authenticated` hardcoded.

For group support, two options:

**Option A (simple)**: Add `groups` to the userinfo response. Have ocp-shim
read `groups` from userinfo and set `X-Remote-Group` headers accordingly.
This means ocp-shim needs a small change to `validateOAuthToken`.

**Option B (no shim change)**: Keep groups only in the user store, rely on
RBAC bindings per-user rather than per-group. Doesn't match real OCP behavior.

**Recommendation**: Option A. The ocp-shim change is ~5 lines.

Note: in real OpenShift, groups come from Group API objects looked up at auth
time by `groupmapper`, not from the htpasswd file. Our approach of embedding
groups in `users.yaml` is simpler and sufficient for a simulator.

### 7. AuthCode Changes

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

### 8. Challenge Path (oc login support)

`oc login` uses HTTP Basic Auth, not the browser form flow. Add support:

- On `GET /oauth/authorize` with `Authorization: Basic <base64>` header:
  decode credentials, validate against user store, and if valid issue the
  auth code + redirect immediately (no login page).
- On `GET /oauth/authorize` without Basic Auth and with a request that looks
  like a CLI (`X-CSRF-Token: 1` header, which `oc` sends): return
  `401 WWW-Authenticate: Basic realm="openshift"` to trigger the challenge.

This mirrors the real `BasicAuthAuthentication` → `passwordchallenger` path.

## Files to Change

| File | Change |
|------|--------|
| `simulator/src/oauth.rs` | Login page, user store, token→user mapping, User/Identity creation, Basic Auth challenge |
| `simulator/src/main.rs` | Pass `--users-file` CLI arg to oauth module |
| `users.yaml` (new) | Default user definitions |
| `example.src/kind/cmd/ocp-shim/main.go` | Read groups from userinfo response |
| `Makefile` | Copy `users.yaml` into kind node or mount it |

## Implementation Order

1. **User store**: add `UserStore` struct, load from YAML, fallback default
2. **AuthCode + TokenInfo**: add `username` field to both structs
3. **POST /oauth/authorize**: validate credentials, issue code with username
4. **GET /oauth/authorize**: serve HTML login form
5. **Basic Auth challenge**: support `oc login` without a browser
6. **handle_userinfo**: return actual user info from store
7. **User + Identity objects**: create API resources on first login
8. **ocp-shim groups**: read groups array from userinfo, set X-Remote-Group
9. **users.yaml + Makefile**: ship default file, wire into deploy

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
- Password hashing (htpasswd format support)
- Session cookies (remember login across authorize calls)
- OAuthAccessToken API objects (persist tokens as real API resources,
  like real OCP does via `registrystorage`)
- User CRUD API (`POST /oauth/users`)
- OIDC ID tokens
- Grant approval page
