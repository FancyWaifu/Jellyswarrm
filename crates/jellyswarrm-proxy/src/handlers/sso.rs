//! SSO HTTP surface: `/sso/login/{slug}` (start the OIDC redirect) and
//! `/sso/callback` (validate, resolve identity, provision, issue a token).
//!
//! The cryptographic heavy lifting lives in [`crate::oidc`]; this module is the
//! glue between that, the session store, and the federation layer. See
//! `docs/sso.md`.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tower_sessions::Session;
use tracing::{debug, error, info, warn};

use crate::encryption::{decrypt_password, HashedPassword, Password};
use crate::oidc::OidcLoginState;
use crate::oidc_storage::OidcProvider;
use crate::AppState;

/// Session key under which the transient OIDC login state is stashed between the
/// authorize redirect and the callback.
const SSO_STATE_KEY: &str = "oidc_login_state";

type HandlerError = (StatusCode, String);

fn internal<E: std::fmt::Display>(e: E) -> HandlerError {
    error!("SSO internal error: {}", e);
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal error".to_string(),
    )
}

/// Decrypt a provider's stored client secret using the master key.
async fn decrypt_secret(state: &AppState, provider: &OidcProvider) -> Result<String, HandlerError> {
    let master_key: HashedPassword = {
        let config = state.config.read().await;
        config.password.clone().into()
    };
    decrypt_password(&provider.client_secret, &master_key)
        .map(|p| p.into_inner())
        .map_err(|e| internal(format!("client secret decrypt failed: {e}")))
}

/// Build the `redirect_uri` from the inbound request. It must match a value
/// registered with the IdP exactly, so we derive it from the Host the browser
/// actually used (+ any configured url_prefix).
async fn build_redirect_uri(state: &AppState, headers: &HeaderMap) -> String {
    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:3000");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("http");
    let prefix = {
        let config = state.config.read().await;
        config
            .url_prefix
            .as_ref()
            .map(|p| format!("/{p}"))
            .unwrap_or_default()
    };
    format!("{scheme}://{host}{prefix}/sso/callback")
}

/// `GET /sso/login/{slug}` — begin the Authorization Code + PKCE flow.
pub async fn handle_sso_login(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    session: Session,
    headers: HeaderMap,
) -> Result<Redirect, HandlerError> {
    let provider = state
        .oidc_storage
        .get_provider_by_slug(&slug)
        .await
        .map_err(internal)?
        .filter(|p| p.enabled)
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("unknown or disabled SSO provider: {slug}"),
        ))?;

    let secret = decrypt_secret(&state, &provider).await?;
    let redirect_uri = build_redirect_uri(&state, &headers).await;

    let (auth_url, login_state) = state
        .oidc_service
        .begin_login(&provider, secret, redirect_uri)
        .await
        .map_err(|e| {
            // Detail (may include internal issuer URL) stays in logs only.
            error!("OIDC begin_login failed for '{}': {}", slug, e);
            (
                StatusCode::BAD_GATEWAY,
                "could not start SSO with that provider".to_string(),
            )
        })?;

    session
        .insert(SSO_STATE_KEY, &login_state)
        .await
        .map_err(internal)?;

    info!("SSO login start via provider '{}'", slug);
    Ok(Redirect::to(auth_url.as_str()))
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// `GET /sso/callback` — validate the IdP response, resolve `(issuer, subject)`
/// to a Jellyswarrm user (provisioning if allowed), and hand back a token.
pub async fn handle_sso_callback(
    State(state): State<AppState>,
    session: Session,
    Query(q): Query<CallbackQuery>,
) -> Result<Response, HandlerError> {
    if let Some(err) = q.error {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "identity provider returned an error: {err} {}",
                q.error_description.unwrap_or_default()
            ),
        ));
    }
    let code = q
        .code
        .ok_or((StatusCode::BAD_REQUEST, "missing 'code'".to_string()))?;
    let returned_state = q
        .state
        .ok_or((StatusCode::BAD_REQUEST, "missing 'state'".to_string()))?;

    let login_state: OidcLoginState = session
        .get(SSO_STATE_KEY)
        .await
        .map_err(internal)?
        .ok_or((
            StatusCode::BAD_REQUEST,
            "no SSO login in progress (session expired?) — start again".to_string(),
        ))?;
    // One-shot: clear it regardless of outcome.
    let _ = session.remove::<OidcLoginState>(SSO_STATE_KEY).await;

    let provider = state
        .oidc_storage
        .get_provider_by_slug(&login_state.provider_slug)
        .await
        .map_err(internal)?
        .ok_or((
            StatusCode::BAD_REQUEST,
            "SSO provider no longer exists".to_string(),
        ))?;
    let secret = decrypt_secret(&state, &provider).await?;

    let claims = state
        .oidc_service
        .complete_login(&provider, secret, &login_state, &returned_state, code)
        .await
        .map_err(|e| {
            // Detail (token-endpoint URL, claim values) stays in logs only.
            error!("OIDC complete_login failed: {}", e);
            (StatusCode::UNAUTHORIZED, "SSO validation failed".to_string())
        })?;

    // Email is PII — keep it out of info-level logs.
    info!(
        "SSO validated identity issuer='{}' subject='{}'",
        claims.issuer, claims.subject
    );
    debug!("SSO identity email={:?}", claims.email);

    // Rotate the session id across the auth transition (defense-in-depth vs fixation).
    let _ = session.cycle_id().await;

    // Resolve (issuer, subject) -> Jellyswarrm user.
    if let Some(identity) = state
        .oidc_storage
        .get_identity(&claims.issuer, &claims.subject)
        .await
        .map_err(internal)?
    {
        let user = state
            .user_authorization
            .get_user_by_id(&identity.user_id)
            .await
            .map_err(internal)?
            .ok_or_else(|| internal("identity references a missing user"))?;
        // Returning user: existing backend sessions (long-lived Jellyfin tokens)
        // are reused. Re-establishing per-device sessions is Phase 3.
        return Ok(success_page(&user.original_username, &user.virtual_key, &[]).into_response());
    }

    // Unknown identity.
    if !state.config.read().await.oidc_auto_provision {
        warn!(
            "SSO identity not linked and auto-provision disabled: ({}, {})",
            claims.issuer, claims.subject
        );
        return Ok((
            StatusCode::FORBIDDEN,
            Html(unlinked_page(&claims.issuer, &claims.subject, claims.email.as_deref())),
        )
            .into_response());
    }

    // Auto-provision a BRAND-NEW user. We must NEVER adopt an existing username:
    // get_or_create_user would return a pre-existing user's virtual_key, handing
    // an attacker (whose IdP email local-part matches an existing username) that
    // user's bearer token. So derive a candidate and force uniqueness with an
    // identity-derived suffix before creating. (Audit finding C1.)
    let base = derive_username(&claims.email, &claims.subject);
    let mut username = base.clone();
    let mut attempt = 0u32;
    loop {
        let taken = state
            .user_authorization
            .get_user_by_username(&username)
            .await
            .map_err(internal)?
            .is_some();
        if !taken {
            break;
        }
        attempt += 1;
        if attempt > 6 {
            return Err(internal("could not allocate a unique username for SSO user"));
        }
        let mut h = Sha256::new();
        h.update(claims.issuer.as_bytes());
        h.update(b"|");
        h.update(claims.subject.as_bytes());
        h.update(b"|");
        h.update(attempt.to_string().as_bytes());
        let suffix = hex::encode(h.finalize());
        username = format!("{base}-{}", &suffix[..8]);
    }
    let password: Password = crate::models::generate_token().into();

    let user = state
        .user_authorization
        .create_user(&username, &password)
        .await
        .map_err(internal)?;

    state
        .oidc_storage
        .link_identity(&user.id, &claims.issuer, &claims.subject, claims.email.as_deref())
        .await
        .map_err(internal)?;

    let results = state
        .federated_users
        .provision_sso_user(&username, &user.id, &password)
        .await;
    let mapped: Vec<String> = results
        .iter()
        .filter(|r| matches!(r.status, crate::federated_users::SyncStatus::Created))
        .map(|r| r.server_name.clone())
        .collect();

    info!(
        "SSO auto-provisioned user '{}' ({}); mapped servers: {:?}",
        username, user.id, mapped
    );

    Ok(success_page(&username, &user.virtual_key, &mapped).into_response())
}

/// Derive a backend username from the email local-part (preferred) or subject,
/// restricted to a safe character set.
fn derive_username(email: &Option<String>, subject: &str) -> String {
    let raw = email
        .as_deref()
        .and_then(|e| e.split('@').next())
        .filter(|s| !s.is_empty())
        .unwrap_or(subject);
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    if cleaned.is_empty() {
        // Slice by char, not byte — a multibyte subject would panic on a byte slice.
        let s: String = subject.chars().take(12).collect();
        format!("sso-{s}")
    } else {
        cleaned
    }
}

/// Minimal HTML-escape for any IdP-derived string rendered into a page.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

fn success_page(username: &str, virtual_key: &str, mapped: &[String]) -> Html<String> {
    let servers = if mapped.is_empty() {
        "<em>(existing sessions reused)</em>".to_string()
    } else {
        mapped
            .iter()
            .map(|s| format!("<li>{}</li>", esc(s)))
            .collect::<String>()
    };
    Html(format!(
        "<!doctype html><html><head><meta charset=utf-8><title>Jellyswarrm SSO</title></head>\
         <body style=\"font-family:sans-serif;max-width:40rem;margin:3rem auto\">\
         <h1>✅ Signed in as {username}</h1>\
         <p>Your Jellyswarrm access token (virtual key):</p>\
         <pre style=\"background:#eee;padding:1rem;border-radius:6px;word-break:break-all\">{virtual_key}</pre>\
         <p>Mapped backends:</p><ul>{servers}</ul>\
         <p style=\"color:#666\">Native clients: use QuickConnect and approve from here.</p>\
         </body></html>",
        username = esc(username),
        virtual_key = esc(virtual_key)
    ))
}

fn unlinked_page(issuer: &str, subject: &str, email: Option<&str>) -> String {
    format!(
        "<!doctype html><html><head><meta charset=utf-8><title>Account not linked</title></head>\
         <body style=\"font-family:sans-serif;max-width:40rem;margin:3rem auto\">\
         <h1>Account not linked</h1>\
         <p>Your identity was verified but isn't linked to a Jellyswarrm account yet. \
         Ask your administrator to link it.</p>\
         <pre style=\"background:#eee;padding:1rem;border-radius:6px\">issuer:  {issuer}\nsubject: {subject}\nemail:   {email}</pre>\
         </body></html>",
        issuer = esc(issuer),
        subject = esc(subject),
        email = esc(email.unwrap_or("(none)"))
    )
}
