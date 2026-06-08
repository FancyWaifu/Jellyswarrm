use axum::{
    extract::{Request, State},
    Json,
};
use hyper::StatusCode;
use reqwest::header::{HeaderValue, CONTENT_LENGTH, TRANSFER_ENCODING};
use reqwest::Body;
use tracing::{debug, error, warn};

use crate::{
    handlers::common::{
        execute_json_request, payload_from_request, process_media_item, process_media_source,
        track_play_session,
    },
    models::{MediaItem, PlaybackRequest, PlaybackResponse},
    request_preprocessing::preprocess_request,
    AppState,
};

//http://localhost:3000/Users/7bc57a386ab84999ad7262210a9cd253/Items/5f7e146c44d84b479cafecd3280be4ea
//http://localhost:3000/Items/430c368c5eb34534bf98363d5adbb92f?userId=520ea298ed8044338a28d912523d715f
pub async fn get_item(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<MediaItem>, StatusCode> {
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let server = preprocessed.server.clone();
    let sessions = preprocessed.sessions.clone();

    match execute_json_request::<MediaItem>(&state.reqwest_client, preprocessed.request).await {
        Ok(media_item) => {
            let server_id = { state.config.read().await.server_id.clone() };
            let mut item = process_media_item(media_item, &state, &server, false, &server_id).await?;
            // With dedup on, expose the same movie/episode on other backends as
            // selectable "versions" (MediaSources), so the user can pick which
            // server to stream from.
            if state.config.read().await.dedup_movies {
                if let Some(sessions) = &sessions {
                    aggregate_versions(&state, &mut item, &server, sessions).await;
                }
            }
            Ok(Json(item))
        }
        Err(e) => {
            error!("Failed to get MediaItem: {:?}", e);
            Err(e)
        }
    }
}

/// Token-authenticated client for one backend (for cross-backend version lookups).
async fn version_client(
    server: &crate::server_storage::Server,
    token: &str,
) -> Option<jellyfin_api::JellyfinClient> {
    let client =
        jellyfin_api::JellyfinClient::new(server.url.as_str(), crate::config::CLIENT_INFO.clone())
            .ok()?;
    client.with_token(token.to_string()).await;
    Some(client)
}

/// Merge each other backend's copy of this movie OR episode in as an additional,
/// labeled `MediaSource`, so Jellyfin's native "Version" picker lets the user
/// choose which server to stream from. Movies match by ProviderIds; episodes
/// match by their series' ProviderIds + season/episode (episode-level ids are
/// unreliable). The home backend's own sources are labeled with its name; the
/// home `MediaSources` are already virtualized by `process_media_item`.
async fn aggregate_versions(
    state: &AppState,
    item: &mut MediaItem,
    home_server: &crate::server_storage::Server,
    sessions: &[(
        crate::user_authorization_service::AuthorizationSession,
        crate::server_storage::Server,
    )],
) {
    use crate::models::enums::BaseItemKind;
    use jellyfin_api::MatchKey;

    // Build the cross-backend match key for this item.
    let match_key = match item.item_type {
        BaseItemKind::Movie => {
            let providers: Vec<(String, String)> = item
                .provider_ids
                .as_ref()
                .and_then(|p| p.as_object())
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            MatchKey::Provider(providers)
        }
        BaseItemKind::Episode => {
            // Resolve the episode's identity (series providers + S/E) from the home
            // backend, which also fetches the parent series for its provider ids.
            let Some((mapping, _)) = state
                .media_storage
                .get_media_mapping_with_server(&item.id)
                .await
                .ok()
                .flatten()
            else {
                return;
            };
            let Some((home_session, _)) = sessions.iter().find(|(_, s)| s.id == home_server.id)
            else {
                return;
            };
            let Some(home_client) =
                version_client(home_server, &home_session.jellyfin_token).await
            else {
                return;
            };
            match home_client
                .get_item_sync_info(&home_session.original_user_id, &mapping.original_media_id)
                .await
            {
                Ok((key, _)) => key,
                Err(_) => return,
            }
        }
        _ => return,
    };
    if match_key.is_empty() {
        return;
    }

    // Label the home backend's own sources.
    if let Some(srcs) = item.media_sources.as_mut() {
        for s in srcs.iter_mut() {
            s.name = Some(home_server.name.clone());
        }
    }
    for (session, server) in sessions {
        if server.id == home_server.id {
            continue;
        }
        let Some(client) = version_client(server, &session.jellyfin_token).await else {
            continue;
        };
        let peer_item_id = match client
            .find_item_by_match(&session.original_user_id, &match_key)
            .await
        {
            Ok(Some(id)) => id,
            _ => continue,
        };
        let raw_sources = match client
            .get_item_media_sources(&session.original_user_id, &peer_item_id)
            .await
        {
            Ok(s) => s,
            Err(_) => continue,
        };
        for raw in raw_sources {
            let mut src: crate::models::MediaSource = match serde_json::from_value(raw) {
                Ok(s) => s,
                Err(_) => continue,
            };
            src.name = Some(server.name.clone());
            if let Ok(v) = process_media_source(src, &state.media_storage, server).await {
                item.media_sources.get_or_insert_with(Vec::new).push(v);
            }
        }
    }
}

//http://localhost:3000/Users/7bc57a386ab84999ad7262210a9cd253/Items?SortBy=SortName%2CProductionYear&SortOrder=Ascending&IncludeItemTypes=Movie&Recursive=true&Fields=PrimaryImageAspectRatio%2CMediaSourceCount&ImageTypeLimit=1&EnableImageTypes=Primary%2CBackdrop%2CBanner%2CThumb&StartIndex=0&ParentId=5f7e146c44d84b479cafecd3280be4ea&Limit=100
//http://localhost:3000/Items/430c368c5eb34534bf98363d5adbb92f/Similar?userId=520ea298ed8044338a28d912523d715f&limit=12&fields=PrimaryImageAspectRatio%2CCanDelete
pub async fn get_items(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<crate::models::ItemsResponseVariants>, StatusCode> {
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let server = preprocessed.server;

    match execute_json_request::<crate::models::ItemsResponseVariants>(
        &state.reqwest_client,
        preprocessed.request,
    )
    .await
    {
        Ok(mut response) => {
            let server_id = { state.config.read().await.server_id.clone() };
            for item in &mut response.iter_mut_items() {
                *item =
                    process_media_item(item.clone(), &state, &server, false, &server_id).await?;
            }

            Ok(Json(response))
        }
        Err(e) => {
            error!("Failed to get ItemsResponse: {:?}", e);
            Err(e)
        }
    }
}

// can be used for special features etc.
pub async fn get_items_list(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<Vec<MediaItem>>, StatusCode> {
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let server = preprocessed.server;

    match execute_json_request::<Vec<MediaItem>>(&state.reqwest_client, preprocessed.request).await
    {
        Ok(mut response) => {
            let server_id = { state.config.read().await.server_id.clone() };
            for item in &mut response {
                *item =
                    process_media_item(item.clone(), &state, &server, false, &server_id).await?;
            }

            Ok(Json(response))
        }
        Err(e) => {
            error!("Failed to get Vec<MediaItem>: {:?}", e);
            Err(e)
        }
    }
}

/// Replace the id segment right after `/Items/` or `/Videos/` with `new_id`.
fn rewrite_item_segment(path: &str, new_id: &str) -> Option<String> {
    let mut segs: Vec<String> = path.split('/').map(String::from).collect();
    for i in 0..segs.len() {
        if (segs[i].eq_ignore_ascii_case("Items") || segs[i].eq_ignore_ascii_case("Videos"))
            && i + 1 < segs.len()
            && !segs[i + 1].is_empty()
        {
            segs[i + 1] = new_id.to_string();
            return Some(segs.join("/"));
        }
    }
    None
}

fn item_segment(path: &str) -> Option<String> {
    let segs: Vec<&str> = path.split('/').collect();
    for i in 0..segs.len() {
        if (segs[i].eq_ignore_ascii_case("Items") || segs[i].eq_ignore_ascii_case("Videos"))
            && i + 1 < segs.len()
            && !segs[i + 1].is_empty()
        {
            return Some(segs[i + 1].to_string());
        }
    }
    None
}

/// When a request carries a `MediaSourceId` (query for streams, JSON body for
/// PlaybackInfo) that lives on a *different* backend than the item in the path —
/// i.e. the user picked a federated "version" — pin routing to the source's
/// backend by rewriting the `/Items/{id}` (or `/Videos/{id}`) segment to the
/// source id. No-op for normal playback (source on the item's own backend) and
/// for native multi-version items (same backend), so it's safe to apply always.
pub async fn pin_to_media_source(state: &AppState, req: Request) -> Request {
    use axum::extract::OriginalUri;
    let (mut parts, body) = req.into_parts();

    // The router resolves the backend from the ORIGINAL uri (axum nesting strips
    // the `/Videos` or `/Items` prefix from parts.uri), so we must read and
    // rewrite that, not parts.uri.
    let original = parts.extensions.get::<OriginalUri>().map(|o| o.0.clone());
    let full_path = original
        .as_ref()
        .map(|u| u.path().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());
    let query = original
        .as_ref()
        .and_then(|u| u.query().map(String::from))
        .or_else(|| parts.uri.query().map(String::from));

    let mut msid = query.as_deref().and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k.eq_ignore_ascii_case("MediaSourceId"))
            .map(|(_, v)| v.into_owned())
    });
    let bytes = axum::body::to_bytes(body, 4 * 1024 * 1024)
        .await
        .unwrap_or_default();
    if msid.is_none() && !bytes.is_empty() {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            msid = json
                .get("MediaSourceId")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
        }
    }

    if let (Some(msid), Some(item_id)) =
        (msid.filter(|s| !s.is_empty()), item_segment(&full_path))
    {
        if msid != item_id {
            let src_srv = state
                .media_storage
                .get_media_mapping_with_server(&msid)
                .await
                .ok()
                .flatten()
                .map(|(_, s)| s.id);
            let item_srv = state
                .media_storage
                .get_media_mapping_with_server(&item_id)
                .await
                .ok()
                .flatten()
                .map(|(_, s)| s.id);
            if src_srv.is_some() && src_srv != item_srv {
                if let Some(new_path) = rewrite_item_segment(&full_path, &msid) {
                    let pq = match &query {
                        Some(q) => format!("{new_path}?{q}"),
                        None => new_path,
                    };
                    if let Ok(uri) = pq.parse::<axum::http::Uri>() {
                        debug!("Pinned routing to selected media-source backend: {}", uri);
                        parts.extensions.insert(OriginalUri(uri));
                    }
                }
            }
        }
    }
    Request::from_parts(parts, axum::body::Body::from(bytes))
}

//http://192.168.188.142:30013/Items/165a66aa5bd2e62c0df0f8da332ae47d/PlaybackInfo
#[axum::debug_handler]
pub async fn post_playback_info(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<PlaybackResponse>, StatusCode> {
    let req = pin_to_media_source(&state, req).await;
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let original_request = preprocessed
        .original_request
        .ok_or(StatusCode::BAD_REQUEST)?;
    let payload: PlaybackRequest = payload_from_request(&original_request)?;

    if payload.device_profile.is_none() {
        warn!("Got playback request from client without device profile. Transcoding will be enforced!")
    }

    let server = preprocessed.server;

    let session = preprocessed.session.ok_or(StatusCode::UNAUTHORIZED)?;

    let mut payload = payload;
    if payload.user_id.is_some() {
        payload.user_id = Some(session.original_user_id.clone());
    }

    if let Some(media_source_id) = &payload.media_source_id {
        if let Some(media_mapping) = state
            .media_storage
            .get_media_mapping_by_virtual(media_source_id)
            .await
            .unwrap_or_default()
        {
            payload.media_source_id = Some(media_mapping.original_media_id);
        }
    }

    debug!("Forwarding PlaybackRequest JSON: {:?}", &payload);

    let json = serde_json::to_vec(&payload).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Set body as a full buffer and provide Content-Length so upstream servers
    // don't wait for chunked data (which can trigger MinRequestBodyDataRate errors).
    let len = json.len();
    let mut request = preprocessed.request;
    *request.body_mut() = Some(Body::from(json));
    // Ensure Content-Length is set and remove Transfer-Encoding if present.
    request.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&len.to_string()).unwrap(),
    );
    request.headers_mut().remove(TRANSFER_ENCODING);

    match execute_json_request::<PlaybackResponse>(&state.reqwest_client, request).await {
        Ok(mut response) => {
            for item in &mut response.media_sources {
                *item = process_media_source(item.clone(), &state.media_storage, &server).await?;
                track_play_session(item, &response.play_session_id, &server, &state).await?;
            }

            debug!("Requested Playback: {:?}", response);

            Ok(Json(response))
        }
        Err(e) => {
            error!("Failed to get playback info: {:?}", e);
            Err(e)
        }
    }
}
