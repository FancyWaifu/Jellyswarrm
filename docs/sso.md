# SSO / OIDC Authentication — Design

Status: **draft / in progress**
Author: FancyWaifu fork
Scope: add OpenID Connect single sign-on as an alternative front-door login for
Jellyswarrm, without weakening the existing username/password path.

---

## 1. Goals & non-goals

**Goals**

- A user can log in to Jellyswarrm via an admin-registered OIDC identity provider
  (e.g. Authentik) instead of typing a Jellyswarrm password.
- After SSO authentication, the user is transparently mapped to exactly the
  backend Jellyfin servers they have been **explicitly granted**, with working
  per-backend sessions — identical downstream behaviour to a password login.
- Identity is keyed on **`(issuer, subject)`**, never on email alone.
- Backend access is governed by **explicit per-user grants** managed by the admin;
  an SSO login never auto-grants a backend the user wasn't already provisioned for.
- Native Jellyfin clients (which cannot do an OIDC browser redirect) still work,
  via QuickConnect.
- Password login and existing federation behaviour are untouched.

**Non-goals (v1)**

- Pushing OIDC down to the backends. Backends keep their own auth; Jellyswarrm
  brokers them using stored **admin** credentials (see §3).
- Truly arbitrary, user-self-registered IdPs. Only admin-registered providers
  are trusted (see §7).
- SCIM / automatic deprovisioning, group-to-grant mapping, role sync. Possible
  later; out of scope now.

---

## 2. The two authentication boundaries

```
            ① front door (this feature)        ② federation (already exists)
  [ client ] ───────────────────────────▶ [ Jellyswarrm ] ─────────────────▶ [ Jellyfin backend ]
       AuthenticateByName / SSO / QuickConnect            admin-cred provisioning + per-user session
```

**① Client → Jellyswarrm.** Today: `POST /Users/AuthenticateByName`
(`handlers/users.rs:107`). SSO adds a *second* way to cross this boundary. It only
establishes *who the user is*. It changes nothing about ②.

**② Jellyswarrm → backend.** Already solved by the admin-credential model:

- `server_admins` table (migration `20251122120000`) stores one admin account per
  backend, encrypted under the Jellyswarrm master password.
- `federated_users.rs::sync_user_to_all_servers` authenticates as that admin
  (`:130`), lists users (`get_users`, `:149`), and **creates** the user if missing
  (`create_user`, `:223`).
- Per user/backend, a `server_mappings` row stores the backend username + an
  **encrypted backend password**; per device, an `authorization_sessions` row
  stores the live `jellyfin_token` used to proxy requests
  (`request_preprocessing.rs:538`).

Because Jellyswarrm holds admin rights on each backend, it can mint the backend
account and choose its password. **The user never needs to know any backend
password.** This is what makes SSO clean: the front door can drop the password
entirely while ② keeps working from stored, Jellyswarrm-owned credentials.

---

## 3. The credential re-key (the one architectural change)

Today a user's `server_mappings.mapped_password` is encrypted with **the user's own
password** (`federated_users.rs:194` → `add_server_mapping(..., Some(&password.into()))`),
so decrypting it at request time requires that password. An SSO user never supplies
one.

**Change:** for SSO-provisioned mappings, encrypt `mapped_password` under the
**server master key** (the same `config.password`-derived `HashedPassword` already
used to encrypt `server_admins.password` and consumed by `decrypt_password(&admin.password, &admin_password)`
at `federated_users.rs:97`). The read path already has an admin-key fallback at
`user_authorization_service.rs:512`, so reads largely work today; we make the
*write* path use the master key for SSO users.

Implications:

- The backend password for an SSO user becomes a **random secret Jellyswarrm
  generates** at provisioning time (`rand`), set on the backend via admin
  `create_user` / password reset, and stored master-key-encrypted. The user
  never sees it.
- Mixed accounts: a user may have *both* a password identity and an SSO identity.
  To avoid two encryption domains for one user, when an SSO identity is linked we
  migrate that user's mappings to master-key encryption (done while we still have
  whatever key the old rows used, or by resetting backend passwords via admin).
  v1 keeps it simple: **a Jellyswarrm user is either password-based or SSO-based**;
  linking an SSO identity to an existing password user triggers a one-time backend
  password reset + re-key. See §6 migration note.

> Encryption primitives live in `encryption.rs` (`encrypt_password` / `decrypt_password`,
> `Password`, `HashedPassword`, AES-GCM). No new crypto is introduced.

---

## 4. Data model

Two new tables. No existing table is altered in v1 (linking is via a join table).

### `oidc_providers` — admin-registered IdPs

```sql
CREATE TABLE IF NOT EXISTS oidc_providers (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    slug          TEXT NOT NULL UNIQUE,      -- url-safe id, e.g. "authentik"
    display_name  TEXT NOT NULL,             -- shown on the login picker
    issuer_url    TEXT NOT NULL,             -- OIDC issuer; discovery = issuer + /.well-known/openid-configuration
    client_id     TEXT NOT NULL,
    client_secret TEXT NOT NULL,             -- AES-GCM encrypted under master key
    scopes        TEXT NOT NULL DEFAULT 'openid profile email',
    enabled       INTEGER NOT NULL DEFAULT 1,
    created_at    TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at    TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
```

### `oidc_identities` — (issuer, subject) → Jellyswarrm user

```sql
CREATE TABLE IF NOT EXISTS oidc_identities (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id     TEXT NOT NULL,               -- FK users.id (cascade delete)
    issuer      TEXT NOT NULL,               -- iss claim (canonical, trailing-slash normalized)
    subject     TEXT NOT NULL,               -- sub claim, opaque & stable per IdP
    email       TEXT,                        -- informational only; NEVER an auth key
    created_at  TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE,
    UNIQUE (issuer, subject)
);
CREATE INDEX IF NOT EXISTS idx_oidc_identities_user ON oidc_identities(user_id);
```

**Grants.** "Which servers may user X reach" is already expressed by the presence
of `server_mappings` rows for that user (admin-driven sync decides them). SSO reuses
this: the explicit grant *is* the set of provisioned mappings. No new grant table in
v1. (If we later want grants decoupled from provisioned mappings, add
`user_server_grants(user_id, server_id)` and gate provisioning on it.)

---

## 5. Flows

### 5.1 Web login (Authorization Code + PKCE)

```
1. GET /ui/login renders normal form + a button per enabled oidc_provider.
2. User clicks "Login with <provider>" → GET /sso/login/{slug}
     - load provider, build OIDC client from discovery (cached)
     - generate state, nonce, PKCE verifier; stash in the session (tower-sessions)
     - 302 to provider authorize endpoint
3. IdP authenticates the user, 302 back to /sso/callback?code&state
4. GET /sso/callback
     - verify state, exchange code at token endpoint (PKCE verifier)
     - validate id_token signature (JWKS), iss, aud, exp, nonce  [openidconnect crate]
     - extract (iss, sub), optional email
     - look up oidc_identities by (iss, sub):
         hit  → resolve users.id
         miss → if provider+admin policy allows auto-provision, create users row
                + oidc_identities row (see §7); else render "ask admin to link".
     - ensure backend sessions: for each granted server_mapping, open/refresh an
       authorization_session using master-key-decrypted creds (admin-provision if
       the backend account is missing). Reuses the federation path.
     - issue the Jellyswarrm virtual_key and complete login (axum-login session for
       /ui; virtual_key as access_token for the Jellyfin API surface).
```

### 5.2 Native clients via QuickConnect

Native Jellyfin apps cannot perform a browser OIDC redirect. Bridge with the
existing QuickConnect support (`handlers/quick_connect.rs`):

```
1. App requests a QuickConnect code (proxied as today).
2. User opens Jellyswarrm web, logs in via SSO (§5.1), approves the code.
3. App polls and receives a virtual_key bound to the SSO-resolved user.
```

No protocol change — SSO just becomes the way the *web* session that approves the
code is authenticated.

---

## 6. Implementation plan (phased)

- **Phase 1 — foundation (compiles, no behaviour change).**
  Add `openidconnect` dep; migration for the two tables; `OidcProviderStorage`
  (CRUD, secret encryption) in the storage layer; config plumbing
  (`auto_provision` policy flag). `cargo check` clean.
- **Phase 2 — login/callback.**
  `/sso/login/{slug}` + `/sso/callback`; discovery client w/ moka cache; PKCE +
  nonce in `tower-sessions`; identity resolution; the master-key re-key in the
  provisioning path; issue virtual_key.
- **Phase 3 — admin UI + e2e.**
  Admin screens to register/edit IdPs and to link an `(issuer, sub)` to a user +
  choose granted servers; the login-page picker; full docker e2e with Dex.

**Re-key migration note.** When an SSO identity is first linked to a user that has
password-encrypted mappings, run a one-time per-backend admin password reset +
re-encrypt under the master key, then mark the user SSO-managed. New SSO users are
master-key from the start.

---

## 7. Security & trust model

- **Identity = `(issuer, sub)`, never email.** Email is display-only. Two different
  IdPs may emit the same email; `sub` is unique only within an issuer, so both parts
  are required and stored together.
- **Only admin-registered IdPs are trusted.** The login picker is the
  `oidc_providers` table. There is no user self-service IdP registration — that would
  let anyone mint tokens claiming any identity. "Choose your server to SSO with" in
  the UI means *pick from the admin's list*, not *bring an arbitrary issuer*.
- **No grant escalation on login.** A successful SSO login maps the user only to
  backends they're already granted (provisioned mappings). Unknown `(iss, sub)` →
  either no access (default) or a tightly-scoped auto-provision policy the admin
  opts into per provider.
- **`auto_provision` policy (per provider).**
  `off` (default): unknown identity is rejected with "ask your admin to link your
  account." `link_by_email`: if exactly one existing user has a matching, verified
  email *and no other SSO identity*, link it (convenience for the admin's own
  migration; documented as lower-assurance). New-user creation on SSO is admin-gated.
- **Standard OIDC hardening** (handled by `openidconnect`): validate `iss`, `aud`,
  `exp`/`iat`, signature against JWKS, `nonce` echo, `state` CSRF, PKCE `S256`.
- **Secrets at rest:** `client_secret` AES-GCM under master key, same as
  `server_admins.password`. Backend passwords for SSO users are random and
  master-key encrypted.
- **CSP note:** the `/ui` CSP keeps `script-src 'self' 'unsafe-inline'` for the admin
  modals (see project notes) — the SSO login button must not rely on anything
  stricter.

---

## 8. Local test harness (docker)

`dev/docker-compose.yml` already runs three Jellyfin backends
(movies:8096, tv:8097, music:8098; admin `admin/password`). For SSO testing we add:

- **Dex** — a tiny OIDC provider with a static test user
  (`sso-test@example.com`), issuer reachable from the Jellyswarrm container on the
  `jellyfin-dev-net` network.
- **jellyswarrm-proxy** — built from this repo, configured with the three backends
  as servers + their admin creds, and Dex registered as an `oidc_provider`.

E2e acceptance:

1. Hit Jellyswarrm web login → "Login with Dex" → authenticate as the test user.
2. Land logged in; verify a `virtual_key` is issued and backend
   `authorization_sessions` exist for the granted servers only.
3. Proxy a real browse/playback request and confirm it resolves to the backend
   with the rewritten token.
4. QuickConnect: request a code from a CLI client, approve via the SSO web session,
   confirm the app receives a working token.

---

## 9. Open questions / decisions log

- **Decided:** identity key = `(issuer, sub)`; grants explicit; only admin-registered
  IdPs.
- **Decided:** v1 keeps a user single-domain (password *or* SSO) to avoid dual
  encryption; linking triggers a re-key.
- **Open:** group→grant mapping (defer). 
- **Open:** whether to add a dedicated `user_server_grants` table or keep grants
  implicit in `server_mappings` (v1 = implicit).
- **Open:** token refresh strategy for long-lived sessions vs. re-auth on expiry.
