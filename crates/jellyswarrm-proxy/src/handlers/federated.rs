use axum::{
    extract::{Request, State},
    Json,
};
use hyper::StatusCode;
use regex::Regex;
use std::sync::LazyLock;
use tokio::task::JoinSet;
use tracing::{debug, error, info, trace};

use crate::{
    backend_health::DEFAULT_BACKEND_TIMEOUT,
    handlers::{
        common::{execute_json_request, process_media_item},
        items::get_items,
    },
    models::enums::{BaseItemKind, CollectionType},
    request_preprocessing::{apply_to_request, extract_request_infos, JellyfinAuthorization},
    AppState,
};

static SERIES_OR_PARENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("(?i)(seriesid|parentid)").unwrap());

pub async fn get_items_from_all_servers_if_not_restricted(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<crate::models::ItemsResponseVariants>, StatusCode> {
    // Extract request information and sessions

    if let Some(query) = req.uri().query() {
        // Check if the request is for a specific series or folder
        if SERIES_OR_PARENT_RE.is_match(query) {
            // If the parent is a *merged* library, fan out across the backends it
            // stands in for (each with its own library id) and dedup, instead of
            // routing to a single backend.
            if let Some(parent_id) = parent_id_from_query(query) {
                if let Some(members) = state.view_merge.members(&parent_id) {
                    let map: std::collections::HashMap<i64, String> =
                        members.into_iter().collect();
                    return get_items_from_all_servers_inner(state, req, Some(map)).await;
                }
            }
            return get_items(State(state), req).await;
        }
    }

    get_items_from_all_servers(State(state), req).await
}

pub async fn get_items_from_all_servers(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<crate::models::ItemsResponseVariants>, StatusCode> {
    get_items_from_all_servers_inner(state, req, None).await
}

/// `parent_override`: when set (a merged-library browse), each backend's request
/// gets its `ParentId` replaced by that backend's own library id, so a single
/// merged view fans out across the libraries it stands in for.
async fn get_items_from_all_servers_inner(
    state: AppState,
    req: Request,
    parent_override: Option<std::collections::HashMap<i64, String>>,
) -> Result<Json<crate::models::ItemsResponseVariants>, StatusCode> {
    let (original_request, _, _, sessions, _) =
        extract_request_infos(req, &state).await.map_err(|e| {
            error!("Failed to preprocess request: {}", e);
            StatusCode::BAD_REQUEST
        })?;

    let sessions = sessions.ok_or(StatusCode::UNAUTHORIZED)?;
    if sessions.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Filter out backends that are currently ejected from the federation
    // fanout. Their content won't appear in this response; the revival probe
    // will re-include them once they recover. This keeps a sick backend from
    // dragging the whole dashboard's latency to its timeout ceiling.
    let mut filtered_sessions = Vec::with_capacity(sessions.len());
    for (session, server) in sessions {
        if state.backend_health.is_ejected(server.id).await {
            info!(
                "Skipping ejected backend '{}' (id={}) on federated fanout",
                server.name, server.id
            );
            continue;
        }
        filtered_sessions.push((session, server));
    }
    if filtered_sessions.is_empty() {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    // Per-index backend priority (the higher-priority copy wins when deduping the
    // same movie), and whether movie dedup is on — captured before the move below.
    let server_priorities: Vec<i32> = filtered_sessions.iter().map(|(_, s)| s.priority).collect();
    let dedup_movies = state.config.read().await.dedup_movies;

    let mut join_set = JoinSet::new();

    for (index, (session, server)) in filtered_sessions.into_iter().enumerate() {
        let request = match original_request.try_clone() {
            Some(req) => req,
            None => {
                error!("Failed to clone request for server: {}", server.name);
                continue;
            }
        };

        let auth = JellyfinAuthorization::Authorization(session.to_authorization());
        let mut request = request;
        // Dedup needs each item's ProviderIds, which list views don't request by
        // default — ensure the backend returns them.
        if dedup_movies {
            ensure_provider_ids_field(&mut request);
        }
        // Merged-library browse: point this backend at its own library.
        if let Some(pid) = parent_override.as_ref().and_then(|m| m.get(&server.id)) {
            set_parent_id(&mut request, pid);
        }
        let state_clone = state.clone();
        let server_clone = server.clone();
        let session_clone = session.clone();

        join_set.spawn(async move {
            apply_to_request(
                &mut request,
                &server_clone,
                &Some(session_clone),
                &Some(auth),
                &state_clone,
            )
            .await;

            let inner_call =
                execute_json_request::<crate::models::ItemsResponseVariants>(
                    &state_clone.reqwest_client,
                    request,
                );

            let result = match tokio::time::timeout(DEFAULT_BACKEND_TIMEOUT, inner_call).await {
                Err(_) => {
                    error!(
                        "Backend '{}' timed out after {:?} on federated call",
                        server_clone.name, DEFAULT_BACKEND_TIMEOUT
                    );
                    state_clone
                        .backend_health
                        .record_failure(server_clone.id, &server_clone.name, "timeout")
                        .await;
                    return (index, None);
                }
                Ok(Ok(mut items_response)) => {
                    let server_id = { state_clone.config.read().await.server_id.clone() };
                    for item in items_response.iter_mut_items() {
                        match process_media_item(
                            item.clone(),
                            &state_clone,
                            &server_clone,
                            true, // Change name to include server name
                            &server_id,
                        )
                        .await
                        {
                            Ok(processed_item) => *item = processed_item,
                            Err(e) => {
                                error!(
                                    "Failed to process media item from server '{}': {:?}",
                                    server_clone.name, e
                                );
                                return (index, None);
                            }
                        }
                    }

                    let item_count = items_response.len();
                    debug!(
                        "Successfully retrieved {} items from server: {}",
                        item_count, server_clone.name
                    );
                    trace!(
                        "Items from server '{}': {}",
                        server_clone.name,
                        serde_json::to_string(&items_response).unwrap_or_default()
                    );
                    state_clone
                        .backend_health
                        .record_success(server_clone.id, &server_clone.name)
                        .await;
                    Some(items_response)
                }
                Ok(Err(status)) => {
                    error!(
                        "Failed to get items from server '{}': status {}",
                        server_clone.name, status
                    );
                    // Only count network-class failures (BAD_GATEWAY) toward
                    // ejection. 4xx-class returns from this helper indicate
                    // user/auth errors and shouldn't penalize the backend.
                    if status == StatusCode::BAD_GATEWAY {
                        state_clone
                            .backend_health
                            .record_failure(server_clone.id, &server_clone.name, "network")
                            .await;
                    }
                    None
                }
            };

            (index, result)
        });
    }

    // Wait for all tasks to complete and collect results with their original indices
    let mut indexed_results: Vec<(usize, Option<crate::models::ItemsResponseVariants>)> =
        Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok((index, items)) => indexed_results.push((index, items)),
            Err(e) => error!("Task failed: {:?}", e),
        }
    }

    // Sort results by original server order
    indexed_results.sort_by_key(|(index, _)| *index);

    // Extract items in original server order, tagged with their backend priority.
    let mut server_items: Vec<(i32, crate::models::ItemsResponseVariants)> = Vec::new();
    for (index, items) in indexed_results {
        if let Some(items) = items {
            let priority = server_priorities.get(index).copied().unwrap_or(0);
            server_items.push((priority, items));
        }
    }

    // Interleave items from all servers with Live TV filtering. Carry each item's
    // backend priority so movie dedup can keep the highest-priority copy.
    let mut interleaved_items: Vec<(crate::models::MediaItem, i32)> = Vec::new();
    let mut live_tv_count = 0;
    let max_items = server_items
        .iter()
        .map(|(_, items)| items.len())
        .max()
        .unwrap_or(0);

    for i in 0..max_items {
        for (priority, server_item_list) in &server_items {
            if let Some(item) = server_item_list.get(i) {
                // Skip additional Live TV items
                if let Some(collectiontype) = &item.collection_type {
                    if *collectiontype == CollectionType::LiveTv
                        && item.item_type == BaseItemKind::UserView
                    {
                        live_tv_count += 1;
                        if live_tv_count > 1 {
                            continue;
                        }
                    }
                }
                interleaved_items.push((item.clone(), *priority));
            }
        }
    }

    // Collapse same-type libraries across backends into one view (registering
    // the merge group for ParentId fan-out), then collapse duplicate movies.
    let interleaved_items: Vec<crate::models::MediaItem> = if dedup_movies {
        let merged = dedup_and_register_views(&state, interleaved_items).await;
        dedup_movies_by_provider(merged)
    } else {
        interleaved_items.into_iter().map(|(item, _)| item).collect()
    };

    let count = interleaved_items.len();
    debug!(
        "Returning {} items from {} servers (dedup_movies={})",
        count,
        server_items.len(),
        dedup_movies
    );

    trace!(
        "Items: {}",
        serde_json::to_string(&interleaved_items).unwrap_or_default()
    );

    if server_items
        .iter()
        .any(|(_, items)| matches!(items, crate::models::ItemsResponseVariants::WithCount(_)))
    {
        Ok(Json(crate::models::ItemsResponseVariants::WithCount(
            crate::models::ItemsResponseWithCount {
                items: interleaved_items,
                total_record_count: count as i32,
                start_index: 0,
            },
        )))
    } else {
        Ok(Json(crate::models::ItemsResponseVariants::Bare(
            interleaved_items,
        )))
    }
}

/// Ensure the request's `Fields` query includes `ProviderIds` — list views don't
/// request it by default, but dedup needs it on every item.
fn ensure_provider_ids_field(req: &mut reqwest::Request) {
    let mut url = req.url().clone();
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut had_fields = false;
    for (k, v) in pairs.iter_mut() {
        if k.eq_ignore_ascii_case("fields") {
            had_fields = true;
            if !v.to_lowercase().contains("providerids") {
                v.push_str(",ProviderIds");
            }
        }
    }
    if !had_fields {
        pairs.push(("Fields".to_string(), "ProviderIds".to_string()));
    }
    url.query_pairs_mut().clear().extend_pairs(pairs.iter());
    *req.url_mut() = url;
}

/// A stable dedup key for a movie: the value of its strongest identifying
/// provider id (Imdb > Tmdb > Tvdb). `None` for non-movies or movies without an
/// identifying id — those can't be confidently deduped, so they're kept as-is.
fn movie_dedup_key(item: &crate::models::MediaItem) -> Option<String> {
    if item.item_type != BaseItemKind::Movie {
        return None;
    }
    let obj = item.provider_ids.as_ref()?.as_object()?;
    for want in ["imdb", "tmdb", "tvdb"] {
        for (k, v) in obj {
            if k.to_lowercase() == want {
                if let Some(val) = v.as_str() {
                    return Some(format!("{want}:{val}"));
                }
            }
        }
    }
    None
}

/// Collapse duplicate movies (same provider id across backends) into one entry,
/// keeping the copy from the highest-priority backend. Position is preserved at
/// first occurrence; anything that isn't a dedupable movie passes through.
fn dedup_movies_by_provider(
    items: Vec<(crate::models::MediaItem, i32)>,
) -> Vec<crate::models::MediaItem> {
    use std::collections::HashMap;
    let mut index_of: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<(crate::models::MediaItem, i32)> = Vec::with_capacity(items.len());
    for (item, priority) in items {
        match movie_dedup_key(&item) {
            Some(key) => match index_of.get(&key) {
                Some(&idx) => {
                    // Duplicate: keep the higher-priority backend's copy.
                    if priority > out[idx].1 {
                        out[idx] = (item, priority);
                    }
                }
                None => {
                    index_of.insert(key, out.len());
                    out.push((item, priority));
                }
            },
            None => out.push((item, priority)),
        }
    }
    out.into_iter().map(|(item, _)| item).collect()
}

/// Replace (or add) the `ParentId` query param on a backend request.
fn set_parent_id(req: &mut reqwest::Request, parent_id: &str) {
    let mut url = req.url().clone();
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("ParentId"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.push(("ParentId".to_string(), parent_id.to_string()));
    url.query_pairs_mut().clear().extend_pairs(pairs.iter());
    *req.url_mut() = url;
}

fn parent_id_from_query(query: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k.eq_ignore_ascii_case("ParentId"))
        .map(|(_, v)| v.into_owned())
}

/// Collapse same-type library views across backends into one canonical view
/// (highest-priority backend), preserving order, and register each merge group
/// in the registry so a later browse of that view fans out across its backends.
/// Items without a `collection_type` (i.e. not libraries) pass through.
async fn dedup_and_register_views(
    state: &AppState,
    items: Vec<(crate::models::MediaItem, i32)>,
) -> Vec<(crate::models::MediaItem, i32)> {
    use std::collections::HashMap;
    let mut out: Vec<(crate::models::MediaItem, i32)> = Vec::with_capacity(items.len());
    let mut canonical_idx: HashMap<String, usize> = HashMap::new();
    let mut group_members: HashMap<String, Vec<(i64, String)>> = HashMap::new();

    for (item, priority) in items {
        let Some(ct) = item.collection_type.as_ref() else {
            out.push((item, priority));
            continue;
        };
        let Some(server_id) = state
            .media_storage
            .get_media_mapping_with_server(&item.id)
            .await
            .ok()
            .flatten()
            .map(|(_, s)| s.id)
        else {
            out.push((item, priority));
            continue;
        };
        // Merge only libraries that are the SAME (name + type) across backends —
        // not every library that happens to share a collection type. The proxy
        // appends " [server]" to names, so match on the base name.
        let name = item.name.as_deref().unwrap_or("");
        let base_name = name.rsplit_once(" [").map(|(b, _)| b).unwrap_or(name);
        let key = format!("{}|{ct:?}", base_name.to_lowercase());
        group_members
            .entry(key.clone())
            .or_default()
            .push((server_id, item.id.clone()));
        match canonical_idx.get(&key) {
            // Keep the higher-priority backend's view as the canonical one.
            Some(&idx) if priority <= out[idx].1 => {}
            Some(&idx) => out[idx] = (item, priority),
            None => {
                canonical_idx.insert(key, out.len());
                out.push((item, priority));
            }
        }
    }

    for (key, members) in group_members {
        if let Some(&idx) = canonical_idx.get(&key) {
            state.view_merge.register(out[idx].0.id.clone(), members);
        }
    }
    out
}

#[cfg(test)]
mod dedup_tests {
    use super::*;
    use crate::models::MediaItem;

    fn movie(id: &str, imdb: Option<&str>) -> MediaItem {
        let mut m: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": id, "Type": "Movie",
        }))
        .unwrap();
        if let Some(i) = imdb {
            m.provider_ids = Some(serde_json::json!({ "Imdb": i }));
        }
        m
    }

    #[test]
    fn keeps_highest_priority_copy_and_passes_through_rest() {
        let items = vec![
            (movie("a-lo", Some("tt1")), 50), // Movies2-ish (lower)
            (movie("a-hi", Some("tt1")), 100), // Movies (higher)
            (movie("b", Some("tt2")), 50),     // unique movie
            (movie("c-noid", None), 50),       // movie w/o provider id -> kept
        ];
        let out = dedup_movies_by_provider(items);
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        // tt1 collapsed to the priority-100 copy, in tt1's first position
        assert_eq!(ids, vec!["a-hi", "b", "c-noid"]);
    }
}
