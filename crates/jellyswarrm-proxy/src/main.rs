use axum::{
    body::Body,
    extract::{Request, State},
    http::{self, HeaderName, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post},
    Router,
};

use axum_messages::MessagesManagerLayer;
use percent_encoding::percent_decode_str;
use rust_embed::RustEmbed;
use sqlx::sqlite::SqliteConnectOptions;
use std::{net::SocketAddr, str::FromStr};
use std::{sync::Arc, time::Duration};
use tokio::task::AbortHandle;
use tower::ServiceBuilder;
use tower_http::{cors::CorsLayer, set_header::SetResponseHeaderLayer, trace::TraceLayer};
use tower_sessions::cookie::Key;
use tower_sessions_sqlx_store::SqliteStore;
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use axum_login::{
    tower_sessions::{ExpiredDeletion, Expiry, SessionManagerLayer},
    AuthManagerLayerBuilder,
};

mod backend_health;
mod config;
mod encryption;
mod federated_users;
mod handlers;
mod media_storage_service;
mod models;
mod oidc;
mod oidc_storage;
mod processors;
mod rate_limiter;
mod request_preprocessing;
mod server_storage;
mod session_storage;
mod ui;
mod url_helper;
mod user_authorization_service;
mod validation;

use backend_health::BackendHealth;
use federated_users::FederatedUserService;
use handlers::syncplay::SyncPlayService;
use media_storage_service::MediaStorageService;
use rate_limiter::AuthRateLimiter;
use oidc::OidcService;
use oidc_storage::OidcStorageService;
use server_storage::ServerStorageService;
use user_authorization_service::UserAuthorizationService;

use crate::{
    config::AppConfig,
    handlers::quick_connect::{self, QuickConnectStorage},
    processors::{
        request_analyzer::RequestAnalyzer,
        request_processor::{RequestProcessingContext, RequestProcessor},
    },
    request_preprocessing::body_to_json,
    ui::Backend,
};
use crate::{
    config::{MediaStreamingMode, DATA_DIR},
    encryption::Password,
    request_preprocessing::preprocess_request,
    session_storage::SessionStorage,
    ui::ui_routes,
};
use jellyswarrm_macros::lowercase_routes;

#[derive(Clone)]
pub struct AppState {
    pub reqwest_client: reqwest::Client,
    pub streaming_reqwest_client: reqwest::Client,
    pub user_authorization: Arc<UserAuthorizationService>,
    pub server_storage: Arc<ServerStorageService>,
    pub oidc_storage: Arc<OidcStorageService>,
    pub oidc_service: OidcService,
    pub media_storage: Arc<MediaStorageService>,
    pub play_sessions: Arc<SessionStorage>,
    pub config: Arc<tokio::sync::RwLock<AppConfig>>,
    pub processors: Arc<JsonProcessors>,
    pub quick_connect: QuickConnectStorage,
    pub federated_users: Arc<FederatedUserService>,
    pub syncplay: Arc<SyncPlayService>,
    pub auth_rate_limiter: Arc<AuthRateLimiter>,
    pub backend_health: BackendHealth,
}

impl AppState {
    pub fn new(
        reqwest_client: reqwest::Client,
        streaming_reqwest_client: reqwest::Client,
        data_context: DataContext,
        json_processors: JsonProcessors,
        quick_connect: QuickConnectStorage,
    ) -> Self {
        // Create temporary state to initialize FederatedUserService
        // This is a bit circular but FederatedUserService needs parts of AppState
        // We can construct it manually here since we have all components
        let federated_users = Arc::new(FederatedUserService::new_from_components(
            data_context.server_storage.clone(),
            data_context.user_authorization.clone(),
            data_context.config.clone(),
        ));

        Self {
            reqwest_client,
            streaming_reqwest_client,
            user_authorization: data_context.user_authorization,
            server_storage: data_context.server_storage,
            oidc_storage: data_context.oidc_storage,
            oidc_service: OidcService::new(),
            media_storage: data_context.media_storage,
            play_sessions: data_context.play_sessions,
            config: data_context.config,
            processors: Arc::new(json_processors),
            quick_connect,
            federated_users,
            syncplay: Arc::new(SyncPlayService::new()),
            auth_rate_limiter: Arc::new(AuthRateLimiter::default_auth_limiter()),
            backend_health: BackendHealth::new(),
        }
    }

    pub async fn get_ui_route(&self) -> String {
        let config = self.config.read().await;
        if let Some(prefix) = &config.url_prefix {
            format!("{}/{}", prefix, config.ui_route)
        } else {
            config.ui_route.to_string()
        }
    }

    pub async fn get_url_prefix(&self) -> Option<String> {
        let config = self.config.read().await;
        config.url_prefix.as_ref().map(|prefix| prefix.to_string())
    }

    pub async fn get_admin_password(&self) -> Password {
        let config = self.config.read().await;
        config.password.clone()
    }

    pub async fn can_change_item_names(&self) -> bool {
        let config = self.config.read().await;
        config.include_server_name_in_media
    }

    pub async fn remove_prefix_from_path<'a>(&self, path: &'a str) -> &'a str {
        let config = self.config.read().await;
        if let Some(prefix) = &config.url_prefix {
            path.trim_start_matches(&format!("/{}", prefix))
        } else {
            path
        }
    }

    pub async fn get_media_streaming_mode(&self) -> MediaStreamingMode {
        let config = self.config.read().await;
        config.media_streaming_mode
    }

    pub async fn auto_create_users_on_login(&self) -> bool {
        let config = self.config.read().await;
        config.auto_create_users_on_login
    }
}

#[derive(Clone)]
/// Struct holding shared services and configuration
pub struct DataContext {
    pub user_authorization: Arc<UserAuthorizationService>,
    pub server_storage: Arc<ServerStorageService>,
    pub oidc_storage: Arc<OidcStorageService>,
    pub media_storage: Arc<MediaStorageService>,
    pub play_sessions: Arc<SessionStorage>,
    pub config: Arc<tokio::sync::RwLock<AppConfig>>,
}

pub struct JsonProcessors {
    pub request_processor: RequestProcessor,
    pub request_analyzer: RequestAnalyzer,
}

#[derive(RustEmbed)]
#[folder = "static/"]
struct Asset;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize file logging

    let file_appender = tracing_appender::rolling::daily(DATA_DIR.join("logs"), "jellyswarm.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Create an environment filter with configurable log level
    // Defaults to "jellyswarrm_proxy=info" but can be overridden with RUST_LOG env var
    // Examples:
    //   RUST_LOG=debug                           - Enable debug for all modules
    //   RUST_LOG=jellyswarrm_proxy=debug         - Enable debug for this app only
    //   RUST_LOG=jellyswarrm_proxy=trace,tower=info - Debug this app, info for tower
    let default_filter = "jellyswarrm_proxy=info,jellyfin_api=info";
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
        .init();

    let loaded_config = crate::config::load_config();
    info!("Loaded configuration: {:?}", loaded_config);

    // Resolve database path inside DATA_DIR
    let db_path = DATA_DIR.join("jellyswarrm.db");
    let db_url = format!("sqlite://{}", db_path.to_string_lossy());
    let options = SqliteConnectOptions::from_str(&db_url)?
        .create_if_missing(true)
        .busy_timeout(Duration::from_secs(5))
        .optimize_on_close(true, None)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .pragma("cache_size", "-65536")
        .pragma("mmap_size", "268435456")
        .pragma("temp_store", "MEMORY")
        .pragma("wal_autocheckpoint", "1000");

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(16)
        .min_connections(2)
        .acquire_timeout(Duration::from_secs(30))
        .connect_with(options)
        .await?;

    // Tolerate migrations applied by older fork builds that no longer exist in
    // the source tree (e.g. the dropped admin/audit/api-key modules). Their
    // tables remain in the DB unread; the binary only requires forward progress.
    let mut migrator = sqlx::migrate!();
    migrator.set_ignore_missing(true);
    migrator.run(&pool).await.unwrap_or_else(|e| {
        error!("Failed to run database migrations: {}", e);
        std::process::exit(1);
    });

    sqlx::query("PRAGMA foreign_keys = ON;")
        .execute(&pool)
        .await?;

    // Create reqwest client for regular API traffic.
    let reqwest_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(loaded_config.timeout))
        .build()
        .unwrap_or_else(|e| {
            error!("Failed to create reqwest client: {}", e);
            std::process::exit(1);
        });

    // Create a dedicated client for proxied media streams.
    // Avoid a global request timeout on long-lived responses and disable automatic
    // response decompression so we forward bytes as-is with less CPU overhead.
    let streaming_reqwest_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(loaded_config.timeout))
        .tcp_nodelay(true)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .build()
        .unwrap_or_else(|e| {
            error!("Failed to create streaming reqwest client: {}", e);
            std::process::exit(1);
        });

    // Initialize user authorization service
    let user_authorization = UserAuthorizationService::new(pool.clone());

    // Initialize server storage service
    let server_storage = ServerStorageService::new(pool.clone());
    server_storage.start_health_check_loop(loaded_config.server_background_check_interval_secs);

    // Initialize media storage service
    let media_storage = MediaStorageService::new(pool.clone());

    // Initialize OIDC provider/identity storage
    let oidc_storage = OidcStorageService::new(pool.clone());

    if !loaded_config.preconfigured_servers.is_empty() {
        info!(
            "Adding {} preconfigured servers from config",
            loaded_config.preconfigured_servers.len()
        );
        for server in &loaded_config.preconfigured_servers {
            match server_storage
                .add_server(
                    &server.name,
                    &server.url,
                    server.priority,
                    server.media_streaming_mode,
                )
                .await
            {
                Ok(_) => {
                    info!(
                        "  Added preconfigured server: {} ({}) with priority {}",
                        server.name, server.url, server.priority
                    );
                }
                Err(e) => {
                    error!(
                        "  Failed to add preconfigured server {} ({}): {}",
                        server.name, server.url, e
                    );
                }
            }
        }
    }

    match server_storage.list_servers().await {
        Ok(servers) => {
            if servers.is_empty() {
                warn!("No servers found, configure them via the UI.");
            } else {
                info!("Found {} configured servers", servers.len());
                for server in &servers {
                    info!(
                        "  {} ({}): priority {}",
                        server.name, server.url, server.priority,
                    );
                }
            }
        }
        Err(e) => {
            error!("Failed to check existing servers: {}", e);
        }
    }

    // Seed server admin credentials + OIDC providers from config (master-key
    // encrypted). Lets the dev harness / deployments bootstrap SSO declaratively
    // until the Phase 3 admin UI lands. See docs/sso.md.
    {
        let master_key: encryption::HashedPassword = loaded_config.password.clone().into();

        // Admin creds for preconfigured servers (needed for SSO provisioning).
        if let Ok(servers) = server_storage.list_servers().await {
            for pre in &loaded_config.preconfigured_servers {
                let (Some(admin_user), Some(admin_pass)) =
                    (&pre.admin_username, &pre.admin_password)
                else {
                    continue;
                };
                let Some(server) = servers.iter().find(|s| {
                    s.url.as_str().trim_end_matches('/') == pre.url.trim_end_matches('/')
                }) else {
                    continue;
                };
                match encryption::encrypt_password(&admin_pass.clone().into(), &master_key) {
                    Ok(enc) => {
                        if let Err(e) =
                            server_storage.add_server_admin(server.id, admin_user, &enc).await
                        {
                            error!("Failed to seed admin creds for {}: {}", pre.name, e);
                        } else {
                            info!("  Seeded admin credentials for server {}", pre.name);
                        }
                    }
                    Err(e) => error!("Failed to encrypt admin password for {}: {}", pre.name, e),
                }
            }
        }

        // OIDC providers.
        for p in &loaded_config.preconfigured_oidc_providers {
            match encryption::encrypt_password(&p.client_secret.clone().into(), &master_key) {
                Ok(enc_secret) => {
                    if let Err(e) = oidc_storage
                        .upsert_provider(
                            &p.slug,
                            &p.display_name,
                            &p.issuer_url,
                            &p.client_id,
                            &enc_secret,
                            &p.scopes,
                            p.enabled,
                        )
                        .await
                    {
                        error!("Failed to seed OIDC provider {}: {}", p.slug, e);
                    } else {
                        info!(
                            "  Seeded OIDC provider '{}' ({})",
                            p.display_name, p.issuer_url
                        );
                    }
                }
                Err(e) => error!("Failed to encrypt client secret for {}: {}", p.slug, e),
            }
        }
        if loaded_config.oidc_auto_provision {
            info!("OIDC auto-provision is ENABLED (unknown identities create users)");
        }
    }

    let data_context = DataContext {
        user_authorization: Arc::new(user_authorization.clone()),
        server_storage: Arc::new(server_storage.clone()),
        oidc_storage: Arc::new(oidc_storage.clone()),
        media_storage: Arc::new(media_storage.clone()),
        play_sessions: Arc::new(SessionStorage::new()),
        config: Arc::new(tokio::sync::RwLock::new(loaded_config.clone())),
    };

    let json_processors = JsonProcessors {
        request_processor: RequestProcessor::new(data_context.clone()),
        request_analyzer: RequestAnalyzer::new(data_context.clone()),
    };

    let app_state = AppState::new(
        reqwest_client,
        streaming_reqwest_client,
        data_context,
        json_processors,
        quick_connect::QuickConnectStorage::new(),
    );

    quick_connect::QuickConnectStorage::start_cleanup_task(app_state.quick_connect.clone());

    let session_store = SqliteStore::new(pool);
    session_store.migrate().await?;

    let deletion_task = tokio::task::spawn(
        session_store
            .clone()
            .continuously_delete_expired(tokio::time::Duration::from_secs(60)),
    );

    let revival_task = backend_health::spawn_revival_probe(
        app_state.backend_health.clone(),
        app_state.server_storage.clone(),
        Duration::from_secs(10),
    );

    let key = Key::from(loaded_config.session_key.as_slice());

    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(loaded_config.secure_cookies)
        .with_same_site(tower_sessions::cookie::SameSite::Lax)
        .with_expiry(Expiry::OnInactivity(time::Duration::days(1))) // 24 hour
        .with_signed(key);

    let backend = Backend::new(
        app_state.config.clone(),
        app_state.user_authorization.clone(),
    );
    let auth_layer = AuthManagerLayerBuilder::new(backend, session_layer).build();

    let ui_route = loaded_config.ui_route.to_string();

    // Security headers applied to the management UI only — the bundled Jellyfin web
    // client needs a more permissive policy and CORS is handled separately for the
    // proxied API surface.
    let security_headers = ServiceBuilder::new()
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("x-frame-options"),
            http::HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("x-content-type-options"),
            http::HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("x-xss-protection"),
            http::HeaderValue::from_static("1; mode=block"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("referrer-policy"),
            http::HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("content-security-policy"),
            // The admin UI uses inline onclick handlers (e.g. dialog.showModal())
            // and an inline theme-toggle script in base.html. 'unsafe-inline' is
            // required for those to run. The UI is auth-gated, so the residual XSS
            // surface is admin-only; all other CSP directives stay strict.
            http::HeaderValue::from_static(
                "default-src 'self'; \
                 script-src 'self' 'unsafe-inline'; \
                 style-src 'self' 'unsafe-inline'; \
                 img-src 'self' data: https:; \
                 media-src 'self' https:; \
                 connect-src 'self' wss: ws: https:; \
                 frame-ancestors 'none'; \
                 form-action 'self'; \
                 base-uri 'self'",
            ),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("permissions-policy"),
            http::HeaderValue::from_static(
                "accelerometer=(), camera=(), geolocation=(), gyroscope=(), magnetometer=(), microphone=(), payment=(), usb=()",
            ),
        ));

    let app = lowercase_routes! {
        Router::new()
            // UI Management routes
            .nest(&format!("/{ui_route}"), ui_routes().layer(security_headers))
            .route("/", get(index_handler))
            .route(
                "/sso/login/{slug}",
                get(handlers::sso::handle_sso_login),
            )
            .route("/sso/callback", get(handlers::sso::handle_sso_callback))
            .route(
                "/QuickConnect/Enabled",
                get(handlers::quick_connect::handle_quick_connect_enabled),
            )
            .route(
                "/QuickConnect/Initiate",
                post(handlers::quick_connect::handle_quick_connect_initiate),
            )
            .route(
                "/QuickConnect/Connect",
                get(handlers::quick_connect::handle_quick_connect_connect),
            )
            .route(
                "/QuickConnect/Authorize",
                post(handlers::quick_connect::handle_quick_connect_authorize),
            )
            .route(
                "/Branding/Configuration",
                get(handlers::branding::handle_branding),
            )
            .route("/websocket", get(handlers::syncplay::websocket))
            .route("/socket", get(handlers::syncplay::websocket))
            .route("/GetUtcTime", get(handlers::syncplay::get_utc_time))
            //.route("/GetUTCTime", get(handlers::syncplay::get_utc_time))
            .nest(
                "/SyncPlay",
                Router::new()
                    .route("/New", post(handlers::syncplay::create_group))
                    .route("/Join", post(handlers::syncplay::join_group))
                    .route("/Leave", post(handlers::syncplay::leave_group))
                    .route("/List", get(handlers::syncplay::list_groups))
                    .route("/{id}", get(handlers::syncplay::get_group))
                    .route("/SetNewQueue", post(handlers::syncplay::set_new_queue))
                    .route("/SetPlaylistItem", post(handlers::syncplay::set_playlist_item))
                    .route(
                        "/RemoveFromPlaylist",
                        post(handlers::syncplay::remove_from_playlist),
                    )
                    .route(
                        "/MovePlaylistItem",
                        post(handlers::syncplay::move_playlist_item),
                    )
                    .route("/Queue", post(handlers::syncplay::queue_items))
                    .route("/Unpause", post(handlers::syncplay::unpause))
                    .route("/Pause", post(handlers::syncplay::pause))
                    .route("/Stop", post(handlers::syncplay::stop))
                    .route("/Seek", post(handlers::syncplay::seek))
                    .route("/Buffering", post(handlers::syncplay::buffering))
                    .route("/Ready", post(handlers::syncplay::ready))
                    .route("/SetIgnoreWait", post(handlers::syncplay::set_ignore_wait))
                    .route("/NextItem", post(handlers::syncplay::next_item))
                    .route("/PreviousItem", post(handlers::syncplay::previous_item))
                    .route("/SetRepeatMode", post(handlers::syncplay::set_repeat_mode))
                    .route("/SetShuffleMode", post(handlers::syncplay::set_shuffle_mode))
                    .route("/Ping", post(handlers::syncplay::ping)),
            )
            // User authentication and profile routes
            .nest(
                "/Users",
                Router::new()
                    .route(
                        "/AuthenticateByName",
                        post(handlers::users::handle_authenticate_by_name),
                    )
                    .route(
                        "/AuthenticateWithQuickConnect",
                        post(handlers::quick_connect::handle_authenticate_with_quick_connect),
                    )
                    .route("/Public", get(handlers::users::handle_public))
                    .route("/Me", get(handlers::users::handle_get_me))
                    .route("/{user_id}", get(handlers::users::handle_get_user_by_id))
                    .route(
                        "/{user_id}/Views",
                        get(handlers::federated::get_items_from_all_servers),
                    )
                    .route(
                        "/{user_id}/Items",
                        get(handlers::federated::get_items_from_all_servers_if_not_restricted),
                    )
                    .route(
                        "/{user_id}/Items/Resume",
                        get(handlers::federated::get_items_from_all_servers),
                    )
                    .route(
                        "/{user_id}/Items/Latest",
                        get(handlers::federated::get_items_from_all_servers_if_not_restricted),
                    )
                    .route("/{user_id}/Items/{item_id}", get(handlers::items::get_item))
                    .route(
                        "/{user_id}/Items/{item_id}/SpecialFeatures",
                        get(handlers::items::get_items_list),
                    ),
            )
            .route(
                "/UserViews",
                get(handlers::federated::get_items_from_all_servers),
            )
            // System info routes
            .nest(
                "/System",
                Router::new()
                    .route("/Info", get(handlers::system::info))
                    .route("/Info/Public", get(handlers::system::info_public)),
            )
            // Item routes (non-user specific)
            .nest(
                "/Items",
                Router::new()
                    .route(
                        "/",
                        get(handlers::federated::get_items_from_all_servers_if_not_restricted),
                    )
                    .route(
                        "/Suggestions",
                        get(handlers::federated::get_items_from_all_servers_if_not_restricted),
                    )
                    .route(
                        "/Latest",
                        get(handlers::federated::get_items_from_all_servers_if_not_restricted),
                    )
                    .route("/{item_id}", get(handlers::items::get_item))
                    .route("/{item_id}/Similar", get(handlers::items::get_items))
                    .route("/{item_id}/LocalTrailers", get(handlers::items::get_items))
                    .route(
                        "/{item_id}/Download",
                        get(handlers::videos::get_download),
                    )
                    .route(
                        "/{item_id}/SpecialFeatures",
                        get(handlers::items::get_items),
                    )
                    .route(
                        "/{item_id}/PlaybackInfo",
                        post(handlers::items::post_playback_info),
                    ),
            )
            .route("/MediaSegments/{item_id}", get(handlers::items::get_items))
            // Show-specific routes
            .nest(
                "/Shows",
                Router::new()
                    .route("/{item_id}/Seasons", get(handlers::items::get_items))
                    .route("/{item_id}/Episodes", get(handlers::items::get_items))
                    .route(
                        "/NextUp",
                        get(handlers::federated::get_items_from_all_servers_if_not_restricted),
                    ),
            )
            .nest(
                "/LiveTv",
                Router::new()
                    .route(
                        "/Channels",
                        get(handlers::federated::get_items_from_all_servers),
                    )
                    .route("/Channels/{item_id}", get(handlers::items::get_item))
                    .route(
                        "/Programs",
                        get(handlers::federated::get_items_from_all_servers),
                    )
                    .route(
                        "/Programs/Recommended",
                        get(handlers::federated::get_items_from_all_servers),
                    )
                    .route("/Programs/{item_id}", get(handlers::items::get_item))
                    .route(
                        "/Recordings",
                        get(handlers::federated::get_items_from_all_servers),
                    )
                    .route(
                        "/Recordings/Folders",
                        get(handlers::federated::get_items_from_all_servers),
                    )
                    .route("/Recordings/{item_id}", get(handlers::items::get_item))
                    .route(
                        "/LiveRecordings/{recordingId}/stream",
                        get(handlers::videos::get_stream),
                    )
                    .route(
                        "/LiveStreamFiles/{streamId}/stream.{container}",
                        get(handlers::videos::get_stream),
                    ),
            )
            // Video streaming routes
            .nest(
                "/Videos",
                Router::new()
                    .route("/{stream_id}/Trickplay/{*path}", get(proxy_handler))
                    .route("/{item_id}/stream", get(handlers::videos::get_stream))
                    .route("/{item_id}/stream.mkv", get(handlers::videos::get_stream))
                    .route("/{item_id}/stream.mp4", get(handlers::videos::get_stream))
                    .route("/{item_id}/stream.mov", get(handlers::videos::get_stream))
                    .route(
                        "/{stream_id}/{*path}",
                        get(handlers::videos::get_video_resource),
                    ),
            )
            // Persons
            .nest(
                "/Persons",
                Router::new().route("/", get(handlers::federated::get_items_from_all_servers)),
            )
            // Artists
            .nest(
                "/Artists",
                Router::new().route("/", get(handlers::federated::get_items_from_all_servers)),
            )
            .route("/{*path}", any(proxy_handler))
            .fallback(proxy_handler)
            .layer(
                ServiceBuilder::new()
                    .layer(TraceLayer::new_for_http())
                    .layer(CorsLayer::permissive()),
            )
            .layer(MessagesManagerLayer)
            .layer(auth_layer)
            .with_state(app_state)
    };

    // Create socket address
    let addr = match format!("{}:{}", loaded_config.host, loaded_config.port).parse::<SocketAddr>()
    {
        Ok(addr) => addr,
        Err(e) => {
            error!(
                "Invalid address {}:{}: {}",
                loaded_config.host, loaded_config.port, e
            );
            std::process::exit(1);
        }
    };

    let app = if let Some(url_prefix) = loaded_config.url_prefix {
        let url_prefix = url_prefix.to_string();
        info!("Using URL prefix: {}", url_prefix);

        info!("Starting reverse proxy on http://{}/{}", addr, url_prefix);
        info!(
            "UI Management routes available at: http://{}/{}/{}",
            addr,
            url_prefix,
            ui_route.trim_start_matches('/')
        );

        Router::new()
            .nest(&format!("/{}", url_prefix), app)
            .fallback(
                // Redirect any request outside the prefixed subtree into the prefixed route,
                // preserving the original path. e.g. /foo/bar -> /{url_prefix}/foo/bar
                // capture url_prefix by value
                {
                    let prefix = url_prefix.clone();
                    move |req: Request| {
                        let prefix = prefix.clone();
                        async move {
                            let orig = req.uri().path().trim_end_matches("/");
                            let prefix_slash = format!("/{}", prefix);
                            let target = if orig.starts_with(&prefix_slash) {
                                // already has prefix - avoid double-appending
                                orig
                            } else {
                                &format!("{prefix_slash}{orig}")
                            };
                            axum::response::Redirect::temporary(target).into_response()
                        }
                    }
                },
            )
    } else {
        info!("No URL prefix configured, using root path");
        info!("Starting reverse proxy on http://{}", addr);
        info!(
            "UI Management routes available at: http://{}/{}",
            addr, ui_route
        );
        app
    };

    // Start the server
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            error!("Failed to bind to {}: {}", addr, e);
            std::process::exit(1);
        }
    };

    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal(
            deletion_task.abort_handle(),
            revival_task.abort_handle(),
        ))
        .await?;

    deletion_task.await??;
    Ok(())
}

async fn index_handler(
    State(state): State<AppState>,
    _req: Request,
) -> Result<Response<Body>, StatusCode> {
    let servers = state.server_storage.list_servers().await.map_err(|e| {
        error!("Failed to list servers: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if servers.is_empty() {
        // No servers configured, redirect to UI management
        Ok(Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header("Location", "/ui")
            .body(Body::empty())
            .unwrap())
    } else {
        // Servers exist, return the index.html page
        if let Some(content) = Asset::get("index.html") {
            Ok(Response::builder()
                .header("Content-Type", "text/html")
                .body(Body::from(content.data.into_owned()))
                .unwrap())
        } else {
            // Fallback if index.html is not found in assets
            error!("index.html not found in static assets");
            Err(StatusCode::NOT_FOUND)
        }
    }
}

#[axum::debug_handler]
async fn proxy_handler(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response<Body>, StatusCode> {
    // check if a resource was requested
    let path = req.uri().path();
    debug!("Using generic processing for path: {}", path);
    let path = if let Some(path) = path.strip_prefix('/') {
        path
    } else {
        path
    };
    let path = if path.is_empty() { "index.html" } else { path };
    let decoded_path = percent_decode_str(path).decode_utf8_lossy().to_string();
    if let Some(content) = Asset::get(&decoded_path) {
        let mime = mime_guess::from_path(decoded_path).first_or_octet_stream();
        return Ok(Response::builder()
            .header("Content-Type", mime.as_ref())
            .body(Body::from(content.data.into_owned()))
            .unwrap());
    }

    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let request_url = preprocessed.request.url().clone();
    trace!(
        "Proxy request details:\n  Original: {:?}\n  Target URL: {}\n  Transformed: {:?}",
        preprocessed.original_request,
        preprocessed.request.url(),
        preprocessed.request
    );

    let payload_processing_context = RequestProcessingContext::new(&preprocessed);
    let mut request = preprocessed.request;

    let preprocessor = &state.processors.request_processor;
    if let Some(mut json_value) = body_to_json(&request) {
        let response =
            processors::process_json(&mut json_value, preprocessor, &payload_processing_context)
                .await
                .map_err(|e| {
                    error!("Failed to process JSON body: {}", e);
                    StatusCode::BAD_REQUEST
                })?;
        if response.was_modified {
            debug!("Modified JSON body for request to {}", request_url);
            let new_body = serde_json::to_vec(&response.data).map_err(|e| {
                error!("Failed to serialize processed JSON body: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            *request.body_mut() = Some(reqwest::Body::from(new_body.clone()));
            // Update Content-Length header
            request.headers_mut().insert(
                reqwest::header::CONTENT_LENGTH,
                reqwest::header::HeaderValue::from_str(&new_body.len().to_string()).unwrap(),
            );
        }
    }
    let response = state.reqwest_client.execute(request).await.map_err(|e| {
        error!("Failed to execute proxy request: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    let status = response.status();
    if !status.is_success() {
        warn!(
            "Upstream server returned error status: {} for Request to: {}",
            status, request_url
        );
    }
    let headers = response.headers().clone();
    let body_bytes = response.bytes().await.map_err(|e| {
        error!("Failed to read response body: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    let mut response_builder = Response::builder().status(status);

    // Copy headers, filtering out hop-by-hop headers
    for (name, value) in headers.iter() {
        if !is_hop_by_hop_header(name) {
            response_builder = response_builder.header(name, value);
        }
    }

    let response = response_builder.body(Body::from(body_bytes)).map_err(|e| {
        error!("Failed to build response: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(response)
}

fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    // RFC 7230 Section 6.1: Hop-by-hop headers
    matches!(
        name.as_str().to_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

async fn shutdown_signal(
    deletion_task_abort_handle: AbortHandle,
    revival_task_abort_handle: AbortHandle,
) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            deletion_task_abort_handle.abort();
            revival_task_abort_handle.abort();
        },
        _ = terminate => {
            deletion_task_abort_handle.abort();
            revival_task_abort_handle.abort();
        },
    }
}
