//! OIDC mechanics: discovery (cached), authorize-URL generation with PKCE +
//! nonce, authorization-code exchange, and ID-token claim extraction.
//!
//! This module is the security-sensitive core of the SSO feature. It is
//! deliberately transport-only: it proves *who* the user is and yields a
//! validated `(issuer, subject)`. Mapping that identity to a Jellyswarrm user
//! and provisioning backend sessions happens in the callback handler. See
//! `docs/sso.md`.

use std::time::Duration;

use moka::future::Cache;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use serde::{Deserialize, Serialize};

use crate::oidc_storage::OidcProvider;

#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    #[error("provider discovery failed: {0}")]
    Discovery(String),
    #[error("invalid url: {0}")]
    Url(String),
    #[error("token exchange failed: {0}")]
    Exchange(String),
    #[error("no id_token in token response")]
    MissingIdToken,
    #[error("id_token validation failed: {0}")]
    Claims(String),
    #[error("oauth state mismatch (possible CSRF)")]
    StateMismatch,
}

/// Per-login transient state, stashed in the user session between the authorize
/// redirect and the callback. `pkce_verifier`/`nonce`/`csrf` are secrets and must
/// only live in the server-side session store, never in a cookie value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcLoginState {
    pub provider_slug: String,
    pub csrf: String,
    pub nonce: String,
    pub pkce_verifier: String,
    pub redirect_uri: String,
}

/// Validated identity extracted from the ID token. `(issuer, subject)` is the
/// authentication key; `email` is informational only.
#[derive(Debug, Clone)]
pub struct OidcClaims {
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
}

#[derive(Clone)]
pub struct OidcService {
    http_client: reqwest::Client,
    metadata_cache: Cache<String, CoreProviderMetadata>,
}

impl Default for OidcService {
    fn default() -> Self {
        Self::new()
    }
}

impl OidcService {
    pub fn new() -> Self {
        // Redirects MUST be disabled: openidconnect/oauth2 require a
        // non-redirecting client to avoid SSRF via the discovery/token URLs.
        let http_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()
            .expect("failed to build OIDC http client");
        let metadata_cache = Cache::builder()
            .max_capacity(64)
            .time_to_live(Duration::from_secs(3600))
            .build();
        Self {
            http_client,
            metadata_cache,
        }
    }

    /// Fetch (and cache for 1h) the provider's discovery document + JWKS.
    async fn metadata(&self, issuer_url: &str) -> Result<CoreProviderMetadata, OidcError> {
        if let Some(md) = self.metadata_cache.get(issuer_url).await {
            return Ok(md);
        }
        let issuer =
            IssuerUrl::new(issuer_url.to_string()).map_err(|e| OidcError::Url(e.to_string()))?;
        let md = CoreProviderMetadata::discover_async(issuer, &self.http_client)
            .await
            .map_err(|e| OidcError::Discovery(e.to_string()))?;
        self.metadata_cache
            .insert(issuer_url.to_string(), md.clone())
            .await;
        Ok(md)
    }

    /// Step 1: build the authorize URL (PKCE + nonce) and the transient state to
    /// persist server-side until the callback.
    pub async fn begin_login(
        &self,
        provider: &OidcProvider,
        plain_secret: String,
        redirect_uri: String,
    ) -> Result<(url::Url, OidcLoginState), OidcError> {
        let md = self.metadata(&provider.issuer_url).await?;
        let client = CoreClient::from_provider_metadata(
            md,
            ClientId::new(provider.client_id.clone()),
            Some(ClientSecret::new(plain_secret)),
        )
        .set_redirect_uri(
            RedirectUrl::new(redirect_uri.clone()).map_err(|e| OidcError::Url(e.to_string()))?,
        );

        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

        let mut req = client.authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        );
        for scope in provider.scopes.split_whitespace() {
            // `openid` is implied by the authentication flow.
            if scope.eq_ignore_ascii_case("openid") {
                continue;
            }
            req = req.add_scope(Scope::new(scope.to_string()));
        }
        let (auth_url, csrf, nonce) = req.set_pkce_challenge(pkce_challenge).url();

        let state = OidcLoginState {
            provider_slug: provider.slug.clone(),
            csrf: csrf.secret().clone(),
            nonce: nonce.secret().clone(),
            pkce_verifier: pkce_verifier.into_secret(),
            redirect_uri,
        };
        Ok((auth_url, state))
    }

    /// Step 2: verify state, exchange the code, and validate the ID token
    /// (signature/iss/aud/exp/nonce all enforced by `claims`). Returns the
    /// validated `(issuer, subject)` identity.
    pub async fn complete_login(
        &self,
        provider: &OidcProvider,
        plain_secret: String,
        state: &OidcLoginState,
        returned_state: &str,
        code: String,
    ) -> Result<OidcClaims, OidcError> {
        if returned_state != state.csrf {
            return Err(OidcError::StateMismatch);
        }

        let md = self.metadata(&provider.issuer_url).await?;
        let client = CoreClient::from_provider_metadata(
            md,
            ClientId::new(provider.client_id.clone()),
            Some(ClientSecret::new(plain_secret)),
        )
        .set_redirect_uri(
            RedirectUrl::new(state.redirect_uri.clone())
                .map_err(|e| OidcError::Url(e.to_string()))?,
        );

        let token_response = client
            .exchange_code(AuthorizationCode::new(code))
            .map_err(|e| OidcError::Exchange(e.to_string()))?
            .set_pkce_verifier(PkceCodeVerifier::new(state.pkce_verifier.clone()))
            .request_async(&self.http_client)
            .await
            .map_err(|e| OidcError::Exchange(e.to_string()))?;

        let id_token = token_response
            .id_token()
            .ok_or(OidcError::MissingIdToken)?;
        let verifier = client.id_token_verifier();
        let nonce = Nonce::new(state.nonce.clone());
        let claims = id_token
            .claims(&verifier, &nonce)
            .map_err(|e| OidcError::Claims(e.to_string()))?;

        Ok(OidcClaims {
            issuer: claims.issuer().as_str().to_string(),
            subject: claims.subject().as_str().to_string(),
            email: claims.email().map(|e| e.as_str().to_string()),
        })
    }
}
