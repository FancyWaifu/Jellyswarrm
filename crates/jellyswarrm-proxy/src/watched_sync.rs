//! Cross-server playback-state sync.
//!
//! When a user's per-item state changes on one backend — marked played/unplayed,
//! favorited, or playback stopped/paused — propagate the item's UserData (resume
//! position, played, play count, favorite, last-played) to the *same title* on
//! their other backends. The cross-backend join key is the item's provider id
//! (movies/series) or its series' provider id + season/episode (episodes), so
//! e.g. pausing *The Matrix* on backend A leaves your resume point on backend B.
//!
//! The fan-out is best-effort and runs detached from the user's request. Peers
//! offline/ejected at the time are queued ([`WatchedSyncQueue`]) and drained by a
//! background retry loop once they recover, so a transient outage isn't dropped.

use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use jellyfin_api::{ItemUserData, JellyfinClient, MatchKey};
use moka::future::Cache;
use sqlx::{Row, SqlitePool};
use tracing::{debug, info, warn};

use crate::backend_health::{BackendHealth, DEFAULT_BACKEND_TIMEOUT};
use crate::config::CLIENT_INFO;
use crate::server_storage::Server;
use crate::user_authorization_service::{AuthorizationSession, UserAuthorizationService};

/// Stop retrying after ~this many attempts (≈ attempts × interval of wall-clock).
const MAX_RETRY_ATTEMPTS: i64 = 60;

/// `(peer server id, match cache-key)` -> the peer's item id (or `None` when the
/// peer genuinely has no matching title). 1h TTL. Only *successful* lookups are
/// cached — a failed lookup (peer down) must NOT poison this with a false "no
/// match", or a recovered peer would be skipped for up to an hour.
static ITEM_MATCH_CACHE: LazyLock<Cache<(i64, String), Option<String>>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(10_000)
        .time_to_live(Duration::from_secs(3600))
        .build()
});

/// Outcome of trying to sync one peer — drives the retry queue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PeerOutcome {
    /// UserData applied on the peer.
    Synced,
    /// Peer doesn't have this title (authoritative "nothing to do").
    NoMatch,
    /// Transient failure (down/ejected/auth/error) — worth retrying later.
    Failed,
}

/// Build a token-authenticated client for one backend.
async fn backend_client(server: &Server, token: &str) -> Option<JellyfinClient> {
    let client = JellyfinClient::new(server.url.as_str(), CLIENT_INFO.clone())
        .map_err(|e| warn!("sync: bad client for '{}': {}", server.name, e))
        .ok()?;
    client.with_token(token.to_string()).await;
    Some(client)
}

/// Propagate an item's current UserData from the source backend to every other
/// backend the user is mapped to. Spawned detached. Peers that fail are queued
/// for retry; peers that succeed clear any stale queue entry.
///
/// `source_server_id` is the backend that OWNS `source_item_id` (the caller
/// resolves it from the item's own media mapping). Resolving the source by item
/// owner — rather than by whichever backend the request happened to be routed to
/// — keeps the source read consistent even when the request was routed elsewhere
/// (e.g. a version-picker source on a different backend than the item's owner).
pub async fn fan_out(
    queue: Arc<WatchedSyncQueue>,
    user_auth: Arc<UserAuthorizationService>,
    user_id: String,
    source_server_id: i64,
    source_item_id: String,
) {
    // Use the *full* session list (NOT the request's ejected-filtered one) and
    // dedupe to one session per server, so a peer that's down/ejected right now
    // still gets queued for retry. Split into the source (its owner) and peers.
    let all = match user_auth.get_user_sessions_by_user_id(&user_id).await {
        Ok(Some((_, s))) => s,
        _ => return,
    };
    let mut seen = std::collections::HashSet::new();
    let mut source: Option<(AuthorizationSession, Server)> = None;
    let mut peers: Vec<(AuthorizationSession, Server)> = Vec::new();
    for (session, server) in all {
        if !seen.insert(server.id) {
            continue;
        }
        if server.id == source_server_id {
            source = Some((session, server));
        } else {
            peers.push((session, server));
        }
    }
    let Some((source_session, source_server)) = source else {
        warn!("sync: no session on source backend {source_server_id}; can't read source item");
        return;
    };
    if peers.is_empty() {
        return;
    }

    // Read the source item's match key + current UserData. Re-reading the
    // authoritative source state makes rapid changes converge regardless of task
    // ordering, and captures play count / last-played the backend just computed.
    let Some(src) = backend_client(&source_server, &source_session.jellyfin_token).await else {
        return;
    };
    let (match_key, user_data) = match src
        .get_item_sync_info(&source_session.original_user_id, &source_item_id)
        .await
    {
        Ok((key, _)) if key.is_empty() => {
            debug!("sync: item {source_item_id} has no identifying ids; nothing to match on");
            return;
        }
        Ok(pair) => pair,
        Err(e) => {
            warn!(
                "sync: couldn't read source item {source_item_id} on '{}' (backend user {}): {e}",
                source_server.name, source_session.original_user_id
            );
            return;
        }
    };
    let title_key = match_key.cache_key();

    // Fan out to each peer backend.
    for (session, server) in peers {
        let outcome = tokio::time::timeout(
            DEFAULT_BACKEND_TIMEOUT,
            sync_one_peer(&session, &server, &match_key, &title_key, &user_data),
        )
        .await
        .unwrap_or(PeerOutcome::Failed);

        match outcome {
            PeerOutcome::Synced | PeerOutcome::NoMatch => {
                queue.remove(&session.user_id, server.id, &title_key).await;
            }
            PeerOutcome::Failed => {
                warn!("sync: '{}' unreachable; queued for retry", server.name);
                queue
                    .enqueue(&session.user_id, server.id, &title_key, &match_key, &user_data)
                    .await;
            }
        }
    }
}

async fn sync_one_peer(
    session: &AuthorizationSession,
    server: &Server,
    match_key: &MatchKey,
    title_key: &str,
    user_data: &ItemUserData,
) -> PeerOutcome {
    let Some(client) = backend_client(server, &session.jellyfin_token).await else {
        return PeerOutcome::Failed;
    };

    // Resolve the peer's item id for this title (memoised per peer+title). Cache
    // ONLY on a successful lookup so a down peer can't poison it with "no match".
    let cache_key = (server.id, title_key.to_string());
    let peer_item_id = match ITEM_MATCH_CACHE.get(&cache_key).await {
        Some(cached) => cached,
        None => match client
            .find_item_by_match(&session.original_user_id, match_key)
            .await
        {
            Ok(found) => {
                ITEM_MATCH_CACHE.insert(cache_key, found.clone()).await;
                found
            }
            Err(e) => {
                warn!("sync: lookup on '{}' failed: {}", server.name, e);
                return PeerOutcome::Failed;
            }
        },
    };

    let Some(peer_item_id) = peer_item_id else {
        debug!("sync: '{}' has no match for {title_key}", server.name);
        return PeerOutcome::NoMatch;
    };

    match client
        .apply_user_data(&session.original_user_id, &peer_item_id, user_data)
        .await
    {
        Ok(()) => {
            info!(
                "sync: applied userdata (played={}, pos={}, fav={}) on '{}' (item {peer_item_id})",
                user_data.played, user_data.playback_position_ticks, user_data.is_favorite, server.name
            );
            PeerOutcome::Synced
        }
        Err(e) => {
            warn!("sync: apply userdata on '{}' failed: {}", server.name, e);
            PeerOutcome::Failed
        }
    }
}

/// Parse `/Users/{user_id}/{segment}/{item_id}` -> `item_id` for a given action
/// segment (e.g. `PlayedItems`, `FavoriteItems`). `None` for any other path.
fn user_action_item_id<'a>(path: &'a str, segment: &str) -> Option<&'a str> {
    let rest = path.strip_prefix("/Users/")?;
    let (_user, rest) = rest.split_once('/')?;
    let item = rest.strip_prefix(segment)?.strip_prefix('/')?;
    if item.is_empty() || item.contains('/') {
        return None;
    }
    Some(item)
}

/// `/Users/{id}/PlayedItems/{itemId}` -> item id (mark played/unplayed hook).
pub fn played_item_id_from_path(path: &str) -> Option<&str> {
    user_action_item_id(path, "PlayedItems")
}

/// `/Users/{id}/FavoriteItems/{itemId}` -> item id (favorite/unfavorite hook).
pub fn favorite_item_id_from_path(path: &str) -> Option<&str> {
    user_action_item_id(path, "FavoriteItems")
}

/// Whether a path is a playback progress/stop report whose body carries the
/// played item id (`/Sessions/Playing`, `/Sessions/Playing/Progress`,
/// `/Sessions/Playing/Stopped`). The caller pulls `ItemId` from the JSON body.
pub fn is_playing_report_path(path: &str) -> bool {
    matches!(
        path.trim_end_matches('/'),
        "/Sessions/Playing" | "/Sessions/Playing/Progress" | "/Sessions/Playing/Stopped"
    )
}

// ===================== persistent retry queue =====================

/// Persistent, deduped retry queue for syncs that couldn't reach a peer.
#[derive(Clone)]
pub struct WatchedSyncQueue {
    pool: SqlitePool,
}

struct QueueEntry {
    id: i64,
    user_id: String,
    server_id: i64,
    title_key: String,
    match_key: MatchKey,
    user_data: ItemUserData,
}

impl WatchedSyncQueue {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Queue (or refresh) a pending sync. Deduped per (user, server, title): a
    /// newer intent overwrites the old one and resets the attempt counter.
    async fn enqueue(
        &self,
        user_id: &str,
        server_id: i64,
        title_key: &str,
        match_key: &MatchKey,
        user_data: &ItemUserData,
    ) {
        let (Ok(mk), Ok(ud)) = (
            serde_json::to_string(match_key),
            serde_json::to_string(user_data),
        ) else {
            warn!("sync: can't serialize queue payload");
            return;
        };
        let res = sqlx::query(
            "INSERT INTO watched_sync_queue \
             (user_id, server_id, title_key, match_key, user_data, attempts, updated_at) \
             VALUES (?, ?, ?, ?, ?, 0, CURRENT_TIMESTAMP) \
             ON CONFLICT(user_id, server_id, title_key) DO UPDATE SET \
               match_key = excluded.match_key, user_data = excluded.user_data, \
               attempts = 0, updated_at = CURRENT_TIMESTAMP",
        )
        .bind(user_id)
        .bind(server_id)
        .bind(title_key)
        .bind(&mk)
        .bind(&ud)
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            warn!("sync: enqueue failed: {e}");
        }
    }

    async fn remove(&self, user_id: &str, server_id: i64, title_key: &str) {
        let _ = sqlx::query(
            "DELETE FROM watched_sync_queue WHERE user_id = ? AND server_id = ? AND title_key = ?",
        )
        .bind(user_id)
        .bind(server_id)
        .bind(title_key)
        .execute(&self.pool)
        .await;
    }

    async fn remove_by_id(&self, id: i64) {
        let _ = sqlx::query("DELETE FROM watched_sync_queue WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await;
    }

    async fn bump(&self, id: i64) {
        let _ = sqlx::query(
            "UPDATE watched_sync_queue SET attempts = attempts + 1, \
             updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await;
    }

    /// Drop entries that have exhausted their retries (give up gracefully).
    async fn purge_exhausted(&self) {
        let _ = sqlx::query("DELETE FROM watched_sync_queue WHERE attempts >= ?")
            .bind(MAX_RETRY_ATTEMPTS)
            .execute(&self.pool)
            .await;
    }

    async fn list_pending(&self) -> Vec<QueueEntry> {
        let rows = sqlx::query(
            "SELECT id, user_id, server_id, title_key, match_key, user_data \
             FROM watched_sync_queue WHERE attempts < ? ORDER BY updated_at LIMIT 500",
        )
        .bind(MAX_RETRY_ATTEMPTS)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();
        rows.iter()
            .filter_map(|r| {
                let mk: String = r.try_get("match_key").ok()?;
                let ud: String = r.try_get("user_data").ok()?;
                Some(QueueEntry {
                    id: r.try_get("id").ok()?,
                    user_id: r.try_get("user_id").ok()?,
                    server_id: r.try_get("server_id").ok()?,
                    title_key: r.try_get("title_key").ok()?,
                    match_key: serde_json::from_str(&mk).ok()?,
                    user_data: serde_json::from_str(&ud).ok()?,
                })
            })
            .collect()
    }
}

/// Background loop: periodically drain the retry queue against recovered peers.
pub fn spawn_retry_loop(
    queue: Arc<WatchedSyncQueue>,
    user_auth: Arc<UserAuthorizationService>,
    backend_health: BackendHealth,
    interval_secs: u64,
) {
    let interval = interval_secs.max(5);
    tokio::spawn(async move {
        info!("Starting playback-sync retry loop (interval {interval}s)");
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            drain_queue(&queue, &user_auth, &backend_health).await;
        }
    });
}

async fn drain_queue(
    queue: &WatchedSyncQueue,
    user_auth: &UserAuthorizationService,
    backend_health: &BackendHealth,
) {
    queue.purge_exhausted().await;
    let pending = queue.list_pending().await;
    if pending.is_empty() {
        return;
    }
    debug!("sync: draining {} queued change(s)", pending.len());
    for e in pending {
        // Leave it queued while the peer is still ejected/down.
        if backend_health.is_ejected(e.server_id).await {
            continue;
        }
        let sessions = match user_auth.get_user_sessions_by_user_id(&e.user_id).await {
            Ok(Some((_, s))) => s,
            // user no longer exists -> drop the orphaned entry
            Ok(None) => {
                queue.remove_by_id(e.id).await;
                continue;
            }
            Err(_) => continue,
        };
        let Some((session, server)) = sessions.into_iter().find(|(_, s)| s.id == e.server_id) else {
            // no session on this backend right now -> count an attempt and move on
            queue.bump(e.id).await;
            continue;
        };
        match sync_one_peer(&session, &server, &e.match_key, &e.title_key, &e.user_data).await {
            PeerOutcome::Synced | PeerOutcome::NoMatch => {
                queue.remove_by_id(e.id).await;
                info!("sync: drained queued change for '{}'", server.name);
            }
            PeerOutcome::Failed => queue.bump(e.id).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_played_items_path() {
        assert_eq!(
            played_item_id_from_path("/Users/abc123/PlayedItems/item789"),
            Some("item789")
        );
    }

    #[test]
    fn parses_favorite_items_path() {
        assert_eq!(
            favorite_item_id_from_path("/Users/abc/FavoriteItems/xyz"),
            Some("xyz")
        );
        assert_eq!(played_item_id_from_path("/Users/abc/FavoriteItems/xyz"), None);
    }

    #[test]
    fn rejects_unrelated_paths() {
        assert_eq!(played_item_id_from_path("/Users/abc/Items/xyz"), None);
        assert_eq!(played_item_id_from_path("/Users/abc/PlayedItems/"), None);
        assert_eq!(played_item_id_from_path("/Users/abc/PlayedItems/x/extra"), None);
        assert_eq!(played_item_id_from_path("/System/Info"), None);
    }

    #[test]
    fn recognizes_playing_report_paths() {
        assert!(is_playing_report_path("/Sessions/Playing"));
        assert!(is_playing_report_path("/Sessions/Playing/Progress"));
        assert!(is_playing_report_path("/Sessions/Playing/Stopped"));
        assert!(!is_playing_report_path("/Sessions/Playing/Ping"));
        assert!(!is_playing_report_path("/Items"));
    }

    #[test]
    fn cache_key_is_order_independent() {
        let a = MatchKey::Provider(vec![
            ("Imdb".into(), "tt1".into()),
            ("Tmdb".into(), "22".into()),
        ]);
        let b = MatchKey::Provider(vec![
            ("Tmdb".into(), "22".into()),
            ("Imdb".into(), "tt1".into()),
        ]);
        assert_eq!(a.cache_key(), b.cache_key());
        assert_eq!(a.cache_key(), "imdb.tt1,tmdb.22");
    }

    #[test]
    fn episode_cache_key_includes_season_episode() {
        let k = MatchKey::Episode {
            series: vec![("Tvdb".into(), "999".into())],
            season: 2,
            episode: 5,
        };
        assert_eq!(k.cache_key(), "tvdb.999|s2e5");
        assert!(!k.is_empty());
    }
}
