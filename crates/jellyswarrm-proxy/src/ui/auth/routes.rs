use askama::Template;
use axum::{
    extract::Query,
    http::StatusCode,
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
    Form, Router,
};
use axum_messages::{Message, Messages};
use serde::Deserialize;

use crate::{
    ui::auth::{AuthSession, Credentials},
    AppState,
};

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginTemplate {
    messages: Vec<Message>,
    next: Option<String>,
    ui_route: String,
}

// This allows us to extract the "next" field from the query string. We use this
// to redirect after log in.
#[derive(Debug, Deserialize)]
pub struct NextUrl {
    next: Option<String>,
}

/// `next` only when it's a path on this site. Anything else (`https://x`,
/// `//x`, `/\x`, control characters) would redirect off-site after login.
fn safe_next(next: Option<String>) -> Option<String> {
    next.filter(|n| {
        n.starts_with('/')
            && !n.starts_with("//")
            && !n.starts_with("/\\")
            && !n.chars().any(|c| c.is_control())
    })
}

pub fn router() -> axum::Router<AppState> {
    Router::new()
        .route("/login", post(self::post::login))
        .route("/login", get(self::get::login))
        .route("/logout", get(self::get::logout))
}

mod post {

    use axum::extract::State;

    use super::*;

    pub async fn login(
        State(state): axum::extract::State<AppState>,
        axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
        headers: axum::http::HeaderMap,
        mut auth_session: AuthSession,
        messages: Messages,
        Form(mut creds): Form<Credentials>,
    ) -> impl IntoResponse {
        creds.next = safe_next(creds.next);
        // Same per-client budget as the API login: this form checks the master
        // password and every user's password.
        if !state.auth_rate_ok(peer, &headers).await {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many login attempts. Wait a minute and try again.",
            )
                .into_response();
        }
        let user = match auth_session.authenticate(creds.clone()).await {
            Ok(Some(user)) => user,
            Ok(None) => {
                messages.error("Invalid credentials");

                let mut login_url = format!("/{}/login", state.get_ui_route().await);
                if let Some(next) = creds.next {
                    let next: String =
                        url::form_urlencoded::byte_serialize(next.as_bytes()).collect();
                    login_url = format!("{login_url}?next={next}");
                } else {
                    login_url = format!("{login_url}?next=/{}", state.get_ui_route().await);
                }

                return Redirect::to(&login_url).into_response();
            }
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };

        if auth_session.login(&user).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }

        messages.success(format!("Successfully logged in as {}", user.username));

        if let Some(ref next) = creds.next {
            Redirect::to(next)
        } else {
            Redirect::to(&format!("/{}", state.get_ui_route().await))
        }
        .into_response()
    }
}

mod get {
    use axum::extract::State;
    use tracing::info;

    use super::*;

    pub async fn login(
        State(state): axum::extract::State<AppState>,
        messages: Messages,
        Query(NextUrl { next }): Query<NextUrl>,
    ) -> Html<String> {
        info!(
            "Rendering login page, base={:?}",
            state.get_ui_route().await
        );
        Html(
            LoginTemplate {
                messages: messages.into_iter().collect(),
                next: safe_next(next),
                ui_route: state.get_ui_route().await,
            }
            .render()
            .unwrap(),
        )
    }

    pub async fn logout(
        State(state): axum::extract::State<AppState>,
        mut auth_session: AuthSession,
    ) -> impl IntoResponse {
        match auth_session.logout().await {
            Ok(_) => {
                Redirect::to(&format!("/{}/login", state.get_ui_route().await)).into_response()
            }
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::safe_next;

    #[test]
    fn next_must_stay_on_site() {
        let ok = |n: &str| safe_next(Some(n.to_string())).is_some();
        assert!(ok("/ui"));
        assert!(ok("/ui/users?tab=1"));
        for bad in [
            "https://evil.example",
            "//evil.example",
            "/\\evil.example",
            "evil.example",
            "/ui\r\nSet-Cookie: x=y",
            "",
        ] {
            assert!(!ok(bad), "{bad:?}");
        }
    }
}
