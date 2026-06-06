//! SSO HTTP surface: `/sso/login/{slug}` (start the OIDC redirect) and
//! `/sso/callback` (validate, resolve identity, provision, issue a token).
//!
//! The cryptographic heavy lifting lives in [`crate::oidc`]; this module is the
//! glue between that, the session store, and the federation layer. See
//! `docs/sso.md`.

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Form, Json,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tower_sessions::Session;
use tracing::{debug, error, info};

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
        // are reused; new devices get a lazy session on first request.
        let (sid, sname) = {
            let c = state.config.read().await;
            (c.server_id.clone(), c.server_name.clone())
        };
        return Ok(login_complete_page(&sid, &sname, &user.id, &user.virtual_key).into_response());
    }

    // Unknown identity — offer to link an existing Jellyswarrm account (so the
    // user inherits their backends) or, if enabled, create a new one. Stash the
    // validated claims in the session for the /sso/link step.
    let pending = PendingLink {
        issuer: claims.issuer.clone(),
        subject: claims.subject.clone(),
        email: claims.email.clone(),
    };
    session.insert(PENDING_KEY, &pending).await.map_err(internal)?;
    let auto = state.config.read().await.oidc_auto_provision;
    Ok(Html(link_page(claims.email.as_deref(), auto)).into_response())
}

/// Validated identity awaiting an account link/create decision (session-stashed).
#[derive(Serialize, Deserialize)]
struct PendingLink {
    issuer: String,
    subject: String,
    email: Option<String>,
}
const PENDING_KEY: &str = "oidc_pending";

#[derive(Deserialize)]
pub struct LinkForm {
    pub username: Option<String>,
    pub password: Option<String>,
    pub create: Option<String>,
}

/// `POST /sso/link` — complete an unlinked SSO login by either linking to an
/// existing account (verify password + re-key its mappings to the master key)
/// or creating a new one (auto-provision).
pub async fn handle_sso_link(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<LinkForm>,
) -> Result<Response, HandlerError> {
    let pending: PendingLink = session
        .get(PENDING_KEY)
        .await
        .map_err(internal)?
        .ok_or((
            StatusCode::BAD_REQUEST,
            "no SSO login in progress (session expired?) — start again".to_string(),
        ))?;

    let (sid, sname) = {
        let c = state.config.read().await;
        (c.server_id.clone(), c.server_name.clone())
    };

    // Create-new branch (auto-provision).
    if form.create.as_deref() == Some("1") {
        if !state.config.read().await.oidc_auto_provision {
            return Err((StatusCode::FORBIDDEN, "account creation is disabled".to_string()));
        }
        let user = provision_new_user(
            &state,
            &pending.issuer,
            &pending.subject,
            pending.email.as_deref(),
        )
        .await?;
        let _ = session.remove::<PendingLink>(PENDING_KEY).await;
        return Ok(login_complete_page(&sid, &sname, &user.id, &user.virtual_key).into_response());
    }

    // Link-existing branch: prove ownership with username + password.
    let username = form.username.unwrap_or_default();
    let password_str = form.password.unwrap_or_default();
    if username.trim().is_empty() || password_str.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "username and password are required".to_string(),
        ));
    }
    let user = state
        .user_authorization
        .get_user_by_username(username.trim())
        .await
        .map_err(internal)?
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "invalid username or password".to_string(),
        ))?;
    if !user.original_password_hash.verify(&password_str) {
        return Err((
            StatusCode::UNAUTHORIZED,
            "invalid username or password".to_string(),
        ));
    }

    // Re-key the user's backend mappings: decrypt with their password, re-encrypt
    // under the master key so password-less SSO logins can use them.
    let password: Password = password_str.as_str().into();
    let user_key: HashedPassword = (&password).into();
    let master_key: HashedPassword = {
        let c = state.config.read().await;
        c.password.clone().into()
    };
    let mappings = state
        .user_authorization
        .list_server_mappings(&user.id)
        .await
        .map_err(internal)?;
    let mut rekeyed = 0;
    for m in &mappings {
        let plain = state.user_authorization.decrypt_server_mapping_password(
            m,
            &user_key,
            &master_key,
            Some(&password),
            None,
        );
        match state
            .user_authorization
            .add_server_mapping(&user.id, &m.server_url, &m.mapped_username, &plain, Some(&master_key))
            .await
        {
            Ok(_) => rekeyed += 1,
            Err(e) => error!("re-key mapping failed for {}: {}", m.server_url, e),
        }
    }

    state
        .oidc_storage
        .link_identity(&user.id, &pending.issuer, &pending.subject, pending.email.as_deref())
        .await
        .map_err(internal)?;
    let _ = session.remove::<PendingLink>(PENDING_KEY).await;

    info!(
        "Linked SSO identity ({}, {}) -> user '{}' ({}); re-keyed {}/{} mappings",
        pending.issuer,
        pending.subject,
        user.original_username,
        user.id,
        rekeyed,
        mappings.len()
    );

    Ok(login_complete_page(&sid, &sname, &user.id, &user.virtual_key).into_response())
}

/// Auto-provision a BRAND-NEW user for an unlinked identity. Never adopts an
/// existing username (audit C1): forces uniqueness with an identity-derived
/// suffix before `create_user`.
async fn provision_new_user(
    state: &AppState,
    issuer: &str,
    subject: &str,
    email: Option<&str>,
) -> Result<crate::user_authorization_service::User, HandlerError> {
    let base = derive_username(&email.map(|s| s.to_string()), subject);
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
        h.update(issuer.as_bytes());
        h.update(b"|");
        h.update(subject.as_bytes());
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
        .link_identity(&user.id, issuer, subject, email)
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
    Ok(user)
}

/// Enabled providers, for the login-screen picker. Public (names only, no secrets).
#[derive(Serialize)]
pub struct ProviderInfo {
    pub slug: String,
    pub display_name: String,
}

pub async fn list_sso_providers(State(state): State<AppState>) -> impl IntoResponse {
    match state.oidc_storage.list_enabled_providers().await {
        Ok(ps) => Json(
            ps.into_iter()
                .map(|p| ProviderInfo {
                    slug: p.slug,
                    display_name: p.display_name,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => {
            error!("Failed to list SSO providers: {}", e);
            Json(Vec::<ProviderInfo>::new()).into_response()
        }
    }
}

/// Injected into the served web client: adds a "Sign in with SSO" button to the
/// login form and a provider picker.
const INJECT_JS: &str = r#"(function(){
  function get(cb){fetch('/sso/providers').then(function(r){return r.ok?r.json():[];}).then(cb).catch(function(){cb([]);});}
  var providers=null;
  function findForm(){
    return document.querySelector('.manualLoginForm')
      || document.querySelector('form.manualLoginForm')
      || (function(){var p=document.querySelector('#txtManualPassword');return p?p.closest('form'):null;})()
      || (function(){var p=document.querySelector('input[type=password]');return p?p.closest('form'):null;})();
  }
  function go(slug){window.location.href='/sso/login/'+encodeURIComponent(slug);}
  function picker(){
    if(document.getElementById('sso-picker'))return;
    var ov=document.createElement('div');ov.id='sso-picker';
    ov.style.cssText='position:fixed;inset:0;background:rgba(0,0,0,.6);display:flex;align-items:center;justify-content:center;z-index:99999';
    var box=document.createElement('div');
    box.style.cssText='background:#202020;color:#fff;padding:1.5em;border-radius:8px;min-width:260px';
    box.innerHTML='<h2 style="margin:0 0 .5em">Choose your sign-in server</h2>';
    providers.forEach(function(p){var b=document.createElement('button');b.className='raised block emby-button';b.style.margin='.4em 0';b.textContent=p.display_name;b.onclick=function(){go(p.slug);};box.appendChild(b);});
    var c=document.createElement('button');c.className='button-flat block emby-button';c.style.marginTop='.6em';c.textContent='Cancel';c.onclick=function(){ov.remove();};box.appendChild(c);
    ov.appendChild(box);ov.onclick=function(e){if(e.target===ov)ov.remove();};document.body.appendChild(ov);
  }
  function ensure(){
    if(!providers||!providers.length)return;
    var form=findForm();if(!form)return;
    if(document.getElementById('sso-login-btn'))return;
    var btn=document.createElement('button');btn.id='sso-login-btn';btn.type='button';btn.className='raised block emby-button';btn.style.marginTop='1em';
    btn.textContent='Sign in with SSO';
    btn.onclick=function(e){e.preventDefault();if(providers.length===1){go(providers[0].slug);}else{picker();}};
    form.appendChild(btn);
  }
  get(function(list){providers=list||[];if(!providers.length)return;ensure();new MutationObserver(ensure).observe(document.body,{childList:true,subtree:true});});
})();"#;

pub async fn sso_inject_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        INJECT_JS,
    )
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

/// Completes login by writing the jellyfin-web credential store (so the served
/// web client auto-connects) and redirecting into the app. Replaces the old
/// token-display page.
fn login_complete_page(server_id: &str, server_name: &str, user_id: &str, token: &str) -> Html<String> {
    // serde_json escapes the values safely; replace `<` to prevent a `</script>`
    // breakout from any free-text (server_name).
    let creds = serde_json::json!({
        "Servers": [{
            "Id": server_id,
            "Name": server_name,
            "AccessToken": token,
            "UserId": user_id,
            "ManualAddress": "",
            "DateLastAccessed": 0_i64,
            "LastConnectionMode": 1
        }]
    });
    let creds_json = serde_json::to_string(&creds)
        .unwrap_or_else(|_| "{\"Servers\":[]}".to_string())
        .replace('<', "\\u003c");
    Html(format!(
        "<!doctype html><html><head><meta charset=utf-8><title>Signing in…</title></head>\
         <body style=\"font-family:sans-serif;text-align:center;margin-top:4rem\">\
         <p>Signing you in…</p>\
         <script>(function(){{try{{var c={creds_json};c.Servers[0].ManualAddress=window.location.origin;c.Servers[0].DateLastAccessed=Date.now();localStorage.setItem('jellyfin_credentials',JSON.stringify(c));}}catch(e){{}}window.location.replace('/');}})();</script>\
         <noscript><a href=\"/\">Continue to Jellyfin</a></noscript>\
         </body></html>",
        creds_json = creds_json
    ))
}

/// Shown to an unlinked SSO identity: link an existing account (username +
/// password) or, if enabled, create a new one.
fn link_page(email: Option<&str>, auto_provision: bool) -> String {
    let create_section = if auto_provision {
        "<hr style=\"margin:1.5rem 0\"><p style=\"color:#666\">Don't have a Jellyswarrm account yet?</p>\
         <form method=post action=\"/sso/link\"><input type=hidden name=create value=1>\
         <button type=submit style=\"width:100%\">Create a new account</button></form>"
    } else {
        ""
    };
    format!(
        "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>Link your account</title></head>\
         <body style=\"font-family:sans-serif;max-width:24rem;margin:3rem auto;padding:0 1rem\">\
         <h1>Link your account</h1>\
         <p>Signed in as <strong>{email}</strong> via SSO. Enter your existing Jellyswarrm \
         username and password <em>once</em> to link this login to your account and its servers.</p>\
         <form method=post action=\"/sso/link\">\
         <p><label>Username<br><input name=username autocomplete=username required style=\"width:100%;padding:.5rem\"></label></p>\
         <p><label>Password<br><input name=password type=password autocomplete=current-password required style=\"width:100%;padding:.5rem\"></label></p>\
         <button type=submit style=\"width:100%;padding:.6rem\">Link &amp; sign in</button>\
         </form>{create_section}\
         </body></html>",
        email = esc(email.unwrap_or("your SSO account")),
        create_section = create_section
    )
}
