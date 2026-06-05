-- OIDC / SSO support.
-- Admin-registered identity providers (the only trusted IdPs) and the
-- (issuer, subject) -> Jellyswarrm user identity links. See docs/sso.md.

CREATE TABLE IF NOT EXISTS oidc_providers (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    slug          TEXT NOT NULL UNIQUE,
    display_name  TEXT NOT NULL,
    issuer_url    TEXT NOT NULL,
    client_id     TEXT NOT NULL,
    client_secret TEXT NOT NULL,                 -- AES-GCM encrypted under master key
    scopes        TEXT NOT NULL DEFAULT 'openid profile email',
    enabled       INTEGER NOT NULL DEFAULT 1,
    created_at    TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at    TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS oidc_identities (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id     TEXT NOT NULL,
    issuer      TEXT NOT NULL,
    subject     TEXT NOT NULL,
    email       TEXT,
    created_at  TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE,
    UNIQUE (issuer, subject)
);

CREATE INDEX IF NOT EXISTS idx_oidc_identities_user ON oidc_identities(user_id);
