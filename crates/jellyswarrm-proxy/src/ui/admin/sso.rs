use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Form,
};
use serde::Deserialize;
use tracing::{error, info};

use crate::{
    encryption::{encrypt_password, Password},
    oidc_storage::{OidcIdentityWithUser, OidcProvider},
    AppState,
};

#[derive(Template)]
#[template(path = "admin/sso.html")]
pub struct SsoPageTemplate {
    pub ui_route: String,
}

#[derive(Template)]
#[template(path = "admin/sso_list.html")]
pub struct SsoListTemplate {
    pub providers: Vec<OidcProvider>,
    pub ui_route: String,
}

#[derive(Deserialize)]
pub struct AddProviderForm {
    pub slug: String,
    pub display_name: String,
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub scopes: Option<String>,
}

async fn render_provider_list(state: &AppState) -> Result<String, String> {
    match state.oidc_storage.list_providers().await {
        Ok(providers) => SsoListTemplate {
            providers,
            ui_route: state.get_ui_route().await,
        }
        .render()
        .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Main SSO providers management page.
pub async fn sso_page(State(state): State<AppState>) -> impl IntoResponse {
    let template = SsoPageTemplate {
        ui_route: state.get_ui_route().await,
    };
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render SSO page: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// Provider list partial (HTMX).
pub async fn get_provider_list(State(state): State<AppState>) -> impl IntoResponse {
    match render_provider_list(&state).await {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render provider list: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error").into_response()
        }
    }
}

/// Add (or update, by slug) an OIDC provider. The client secret is encrypted
/// under the master key before storage.
pub async fn add_provider(
    State(state): State<AppState>,
    Form(form): Form<AddProviderForm>,
) -> Response {
    let slug = form.slug.trim();
    if slug.is_empty()
        || !slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Slug must be non-empty and url-safe (letters, digits, - or _).</div>"),
        )
            .into_response();
    }
    let issuer = form.issuer_url.trim();
    if form.display_name.trim().is_empty()
        || issuer.is_empty()
        || form.client_id.trim().is_empty()
        || form.client_secret.trim().is_empty()
    {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Display name, issuer URL, client ID and client secret are required.</div>"),
        )
            .into_response();
    }
    if !(issuer.starts_with("https://") || issuer.starts_with("http://")) {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Issuer URL must start with http:// or https://</div>"),
        )
            .into_response();
    }
    let scopes = form
        .scopes
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("openid profile email");

    // Encrypt the client secret with the master key (same scheme as server admins).
    let secret_plain: Password = form.client_secret.trim().into();
    let master = {
        let config = state.config.read().await;
        config.password.clone().into()
    };
    let encrypted = match encrypt_password(&secret_plain, &master) {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to encrypt client secret: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to encrypt client secret.</div>"),
            )
                .into_response();
        }
    };

    match state
        .oidc_storage
        .upsert_provider(
            slug,
            form.display_name.trim(),
            issuer,
            form.client_id.trim(),
            &encrypted,
            scopes,
            true,
        )
        .await
    {
        Ok(_) => {
            info!("Upserted OIDC provider '{}'", slug);
            get_provider_list(State(state)).await.into_response()
        }
        Err(e) => {
            error!("Failed to save OIDC provider: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to save provider.</div>"),
            )
                .into_response()
        }
    }
}

// ----- linked identities -----

#[derive(Template)]
#[template(path = "admin/sso_identity_list.html")]
pub struct SsoIdentityListTemplate {
    pub identities: Vec<OidcIdentityWithUser>,
    pub ui_route: String,
}

#[derive(Deserialize)]
pub struct AddIdentityForm {
    pub username: String,
    pub provider_slug: String,
    pub subject: String,
}

async fn render_identity_list(state: &AppState) -> Result<String, String> {
    match state.oidc_storage.list_identities().await {
        Ok(identities) => SsoIdentityListTemplate {
            identities,
            ui_route: state.get_ui_route().await,
        }
        .render()
        .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

pub async fn get_identity_list(State(state): State<AppState>) -> impl IntoResponse {
    match render_identity_list(&state).await {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render identity list: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error").into_response()
        }
    }
}

/// Admin: link an SSO identity `(provider issuer, subject)` to an existing user.
pub async fn add_identity(
    State(state): State<AppState>,
    Form(form): Form<AddIdentityForm>,
) -> Response {
    let provider = match state
        .oidc_storage
        .get_provider_by_slug(form.provider_slug.trim())
        .await
    {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Html("<div class=\"alert alert-error\">Unknown provider slug.</div>"),
            )
                .into_response()
        }
    };
    let user = match state
        .user_authorization
        .get_user_by_username(form.username.trim())
        .await
    {
        Ok(Some(u)) => u,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Html("<div class=\"alert alert-error\">No such Jellyswarrm user.</div>"),
            )
                .into_response()
        }
    };
    let subject = form.subject.trim();
    if subject.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Subject is required.</div>"),
        )
            .into_response();
    }
    match state
        .oidc_storage
        .link_identity(&user.id, &provider.issuer_url, subject, None)
        .await
    {
        Ok(_) => {
            info!(
                "Admin linked identity ({}, {}) -> user '{}'",
                provider.issuer_url, subject, user.original_username
            );
            get_identity_list(State(state)).await.into_response()
        }
        Err(e) => {
            error!("Failed to link identity: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to link identity.</div>"),
            )
                .into_response()
        }
    }
}

/// Admin: unlink an identity by id.
pub async fn delete_identity(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match state.oidc_storage.unlink_identity(id).await {
        Ok(true) => {
            info!("Admin unlinked SSO identity id {}", id);
            get_identity_list(State(state)).await.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Identity not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to unlink identity: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to unlink</div>"),
            )
                .into_response()
        }
    }
}

/// Delete an OIDC provider.
pub async fn delete_provider(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match state.oidc_storage.delete_provider(id).await {
        Ok(true) => {
            info!("Deleted OIDC provider id {}", id);
            get_provider_list(State(state)).await.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Provider not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to delete OIDC provider: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to delete provider</div>"),
            )
                .into_response()
        }
    }
}
