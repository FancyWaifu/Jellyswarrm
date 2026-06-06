//! Storage for OIDC / SSO: admin-registered identity providers and the
//! (issuer, subject) -> Jellyswarrm user identity links.
//!
//! Identity is keyed on `(issuer, subject)`, never on email. Only providers
//! present in `oidc_providers` are trusted. See `docs/sso.md`.

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

use crate::encryption::EncryptedPassword;

/// An admin-registered OpenID Connect identity provider.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct OidcProvider {
    pub id: i64,
    /// URL-safe identifier used in routes and the login picker, e.g. "authentik".
    pub slug: String,
    pub display_name: String,
    /// OIDC issuer; discovery document = `issuer_url` + `/.well-known/openid-configuration`.
    pub issuer_url: String,
    pub client_id: String,
    /// AES-GCM encrypted under the master key (same scheme as `server_admins.password`).
    pub client_secret: EncryptedPassword,
    /// Space-separated scope list, e.g. "openid profile email".
    pub scopes: String,
    pub enabled: bool,
    /// Federated backend this provider signs into; `None` = available for all
    /// servers. Drives the per-server grouping in the login picker.
    pub server_id: Option<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A provider joined to its bound server's name, for the login picker and the
/// admin list. `server_name` is `None` for providers available to all servers.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct OidcProviderRow {
    pub id: i64,
    pub slug: String,
    pub display_name: String,
    pub issuer_url: String,
    pub enabled: bool,
    pub server_id: Option<i64>,
    pub server_name: Option<String>,
}

/// A link from an external identity `(issuer, subject)` to a local Jellyswarrm user.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct OidcIdentity {
    pub id: i64,
    pub user_id: String,
    pub issuer: String,
    pub subject: String,
    /// Informational only — never used as an authentication key.
    pub email: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// An identity row joined to its linked user's username, for admin display.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct OidcIdentityWithUser {
    pub id: i64,
    pub user_id: String,
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub username: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OidcStorageService {
    pool: SqlitePool,
}

impl OidcStorageService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ----- providers -----

    pub async fn list_providers(&self) -> Result<Vec<OidcProvider>, sqlx::Error> {
        sqlx::query_as::<_, OidcProvider>(
            "SELECT id, slug, display_name, issuer_url, client_id, client_secret, scopes, \
             enabled, server_id, created_at, updated_at FROM oidc_providers ORDER BY display_name",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Enabled providers only — what the login picker should offer.
    pub async fn list_enabled_providers(&self) -> Result<Vec<OidcProvider>, sqlx::Error> {
        sqlx::query_as::<_, OidcProvider>(
            "SELECT id, slug, display_name, issuer_url, client_id, client_secret, scopes, \
             enabled, server_id, created_at, updated_at FROM oidc_providers WHERE enabled = 1 \
             ORDER BY display_name",
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn get_provider_by_slug(
        &self,
        slug: &str,
    ) -> Result<Option<OidcProvider>, sqlx::Error> {
        sqlx::query_as::<_, OidcProvider>(
            "SELECT id, slug, display_name, issuer_url, client_id, client_secret, scopes, \
             enabled, server_id, created_at, updated_at FROM oidc_providers WHERE slug = ?",
        )
        .bind(slug)
        .fetch_optional(&self.pool)
        .await
    }

    /// Insert or replace a provider by `slug`. `client_secret` must already be encrypted.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_provider(
        &self,
        slug: &str,
        display_name: &str,
        issuer_url: &str,
        client_id: &str,
        client_secret: &EncryptedPassword,
        scopes: &str,
        enabled: bool,
        server_id: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO oidc_providers \
             (slug, display_name, issuer_url, client_id, client_secret, scopes, enabled, server_id, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(slug) DO UPDATE SET \
               display_name = excluded.display_name, \
               issuer_url   = excluded.issuer_url, \
               client_id    = excluded.client_id, \
               client_secret= excluded.client_secret, \
               scopes       = excluded.scopes, \
               enabled      = excluded.enabled, \
               server_id    = excluded.server_id, \
               updated_at   = CURRENT_TIMESTAMP",
        )
        .bind(slug)
        .bind(display_name)
        .bind(issuer_url)
        .bind(client_id)
        .bind(client_secret.as_str())
        .bind(scopes)
        .bind(enabled)
        .bind(server_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// All providers joined to their bound server's name (admin list).
    pub async fn list_providers_with_server(&self) -> Result<Vec<OidcProviderRow>, sqlx::Error> {
        sqlx::query_as::<_, OidcProviderRow>(
            "SELECT p.id, p.slug, p.display_name, p.issuer_url, p.enabled, p.server_id, \
             s.name AS server_name \
             FROM oidc_providers p LEFT JOIN servers s ON s.id = p.server_id \
             ORDER BY (p.server_id IS NULL), s.name, p.display_name",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Enabled providers joined to their server name — what the login picker
    /// groups by. Server-bound providers first (grouped by server), then global.
    pub async fn list_enabled_providers_with_server(
        &self,
    ) -> Result<Vec<OidcProviderRow>, sqlx::Error> {
        sqlx::query_as::<_, OidcProviderRow>(
            "SELECT p.id, p.slug, p.display_name, p.issuer_url, p.enabled, p.server_id, \
             s.name AS server_name \
             FROM oidc_providers p LEFT JOIN servers s ON s.id = p.server_id \
             WHERE p.enabled = 1 \
             ORDER BY (p.server_id IS NULL), s.name, p.display_name",
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn delete_provider(&self, id: i64) -> Result<bool, sqlx::Error> {
        let res = sqlx::query("DELETE FROM oidc_providers WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    // ----- identities -----

    /// Resolve an external identity to a local user. The core SSO lookup.
    pub async fn get_identity(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<OidcIdentity>, sqlx::Error> {
        sqlx::query_as::<_, OidcIdentity>(
            "SELECT id, user_id, issuer, subject, email, created_at FROM oidc_identities \
             WHERE issuer = ? AND subject = ?",
        )
        .bind(issuer)
        .bind(subject)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn list_identities_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<OidcIdentity>, sqlx::Error> {
        sqlx::query_as::<_, OidcIdentity>(
            "SELECT id, user_id, issuer, subject, email, created_at FROM oidc_identities \
             WHERE user_id = ? ORDER BY created_at",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
    }

    /// All identities joined to their user's username (for the admin UI).
    pub async fn list_identities(&self) -> Result<Vec<OidcIdentityWithUser>, sqlx::Error> {
        sqlx::query_as::<_, OidcIdentityWithUser>(
            "SELECT oi.id, oi.user_id, oi.issuer, oi.subject, oi.email, \
             u.original_username AS username \
             FROM oidc_identities oi LEFT JOIN users u ON u.id = oi.user_id \
             ORDER BY oi.created_at DESC",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Link `(issuer, subject)` to a Jellyswarrm user. Idempotent on the unique pair.
    pub async fn link_identity(
        &self,
        user_id: &str,
        issuer: &str,
        subject: &str,
        email: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO oidc_identities (user_id, issuer, subject, email) VALUES (?, ?, ?, ?) \
             ON CONFLICT(issuer, subject) DO UPDATE SET \
               user_id = excluded.user_id, email = excluded.email",
        )
        .bind(user_id)
        .bind(issuer)
        .bind(subject)
        .bind(email)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn unlink_identity(&self, id: i64) -> Result<bool, sqlx::Error> {
        let res = sqlx::query("DELETE FROM oidc_identities WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MIGRATOR;
    use crate::encryption::EncryptedPassword;
    use sqlx::SqlitePool;

    async fn store() -> OidcStorageService {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        OidcStorageService::new(pool)
    }

    #[tokio::test]
    async fn provider_roundtrip_and_enabled_filter() {
        let s = store().await;
        let secret = EncryptedPassword::from_raw("enc-secret".into());
        s.upsert_provider(
            "authentik",
            "Authentik",
            "https://auth.example/application/o/js/",
            "client-123",
            &secret,
            "openid profile email",
            true,
            None,
        )
        .await
        .unwrap();

        let p = s.get_provider_by_slug("authentik").await.unwrap().unwrap();
        assert_eq!(p.display_name, "Authentik");
        assert_eq!(p.client_secret.as_str(), "enc-secret");
        assert!(p.enabled);
        assert_eq!(p.server_id, None);

        // upsert by slug updates in place and can disable
        s.upsert_provider(
            "authentik",
            "Authentik (prod)",
            "https://auth.example/application/o/js/",
            "client-123",
            &secret,
            "openid profile email",
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(s.list_providers().await.unwrap().len(), 1);
        assert!(s.list_enabled_providers().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn identity_keyed_on_issuer_subject() {
        let s = store().await;
        // identities FK to users(id); insert a bare user row first.
        sqlx::query(
            "INSERT INTO users (id, virtual_key, original_username, original_password_hash) \
             VALUES (?, ?, ?, ?)",
        )
        .bind("user-1")
        .bind("vk-1")
        .bind("bryson")
        .bind("hash")
        .execute(&s.pool)
        .await
        .unwrap();

        s.link_identity("user-1", "https://idp/", "sub-abc", Some("b@example.com"))
            .await
            .unwrap();

        let got = s
            .get_identity("https://idp/", "sub-abc")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.user_id, "user-1");
        // same subject under a different issuer is a distinct identity
        assert!(s.get_identity("https://other/", "sub-abc").await.unwrap().is_none());
    }
}
