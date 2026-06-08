//! Cross-server watched-state sync.
//!
//! When a user marks an item played/unplayed on one backend, propagate that to
//! the *same title* (matched by `ProviderIds`) on their other backends. Watching
//! *Night of the Living Dead* on backend A also marks the copy on backend B.
//!
//! v1 scope: movies, played/unplayed only. The fan-out is best-effort and runs
//! detached from the user's request. Peers that are offline/ejected at mark time
//! are queued ([`WatchedSyncQueue`]) and drained by a background retry loop once
//! they recover, so a transient outage doesn't silently drop the change.

use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use jellyfin_api::JellyfinClient;
use moka::future::Cache;
use sqlx::{Row, SqlitePool};
use tracing::{debug, info, warn};

use crate::backend_health::{BackendHealth, DEFAULT_BACKEND_TIMEOUT};
use crate::config::CLIENT_INFO;
use crate::server_storage::Server;
use crate::user_authorization_service::{AuthorizationSession, UserAuthorizationService};

/// Stop retrying after ~this many attempts (≈ attempts × interval of wall-clock).
const MAX_RETRY_ATTEMPTS: i64 = 60;

/// `(peer server id, "imdb.tt..,tmdb..")` -> the peer's item id (or `None` when
/// the peer genuinely has no matching title). 1h TTL. Only *successful* lookups
/// are cached — a failed lookup (peer down) must NOT poison this with a false
/// "no match", or a recovered peer would be skipped for up to an hour.
static ITEM_MATCH_CACHE: LazyLock<Cache<(i64, String), Option<String>>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(10_000)
        .time_to_live(Duration::from_secs(3600))
        .build()
});

/// Outcome of trying to sync one peer — drives the retry queue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PeerOutcome {
    /// Played state set on the peer.
    Synced,
    /// Peer doesn't have this title (authoritative "nothing to do").
    NoMatch,
    /// Transient failure (down/ejected/auth/error) — worth retrying later.
    Failed,
}

/// Build a token-authenticated client for one backend.
async fn backend_client(server: &Server, token: &str) -> Option<JellyfinClient> {
    let client = JellyfinClient::new(server.url.as_str(), CLIENT_INFO.clone())
        .map_err(|e| warn!("watched-sync: bad client for '{}': {}", server.name, e))
        .ok()?;
    client.with_token(token.to_string()).await;
    Some(client)
}

/// Propagate a played/unplayed change from the source backend to every other
/// backend the user is mapped to. Spawned detached. Peers that fail are queued
/// for retry; peers that succeed clear any stale queue entry.
pub async fn fan_out(
    queue: Arc<WatchedSyncQueue>,
    user_auth: Arc<UserAuthorizationService>,
    source_server: Server,
    source_session: AuthorizationSession,
    source_item_id: String,
    played: bool,
) {
    // Peers = the user's OTHER backends. Use the *full* session list (NOT the
    // request's ejected-filtered one) and dedupe to one session per server, so a
    // peer that's down/ejected right now still gets queued for retry.
    let all = match user_auth
        .get_user_sessions_by_user_id(&source_session.user_id)
        .await
    {
        Ok(Some((_, s))) => s,
        _ => return,
    };
    let mut seen = std::collections::HashSet::new();
    let peers: Vec<(AuthorizationSession, Server)> = all
        .into_iter()
        .filter(|(_, server)| server.id != source_server.id && seen.insert(server.id))
        .collect();
    if peers.is_empty() {
        return;
    }

    // Resolve the source item's ProviderIds (the cross-backend join key) AND its
    // *current* played state. Re-reading the authoritative source state makes
    // rapid play/unplay toggles converge regardless of task ordering.
    let Some(src) = backend_client(&source_server, &source_session.jellyfin_token).await else {
        return;
    };
    let (provider_ids, source_played) = match src
        .get_item_match_info(&source_session.original_user_id, &source_item_id)
        .await
    {
        Ok((p, _)) if p.is_empty() => {
            debug!("watched-sync: item {source_item_id} has no ProviderIds; nothing to match on");
            return;
        }
        Ok((p, played)) => (p, played),
        Err(e) => {
            warn!("watched-sync: couldn't read source item: {e}");
            return;
        }
    };
    let played = source_played.unwrap_or(played);
    let provider_vec: Vec<(String, String)> = provider_ids.into_iter().collect();
    let title_key = title_key(&provider_vec);

    // Fan out to each peer backend.
    for (session, server) in peers {
        let outcome = tokio::time::timeout(
            DEFAULT_BACKEND_TIMEOUT,
            sync_one_peer(&session, &server, &provider_vec, &title_key, played),
        )
        .await
        .unwrap_or(PeerOutcome::Failed);

        match outcome {
            PeerOutcome::Synced | PeerOutcome::NoMatch => {
                queue.remove(&session.user_id, server.id, &title_key).await;
            }
            PeerOutcome::Failed => {
                warn!("watched-sync: '{}' unreachable; queued for retry", server.name);
                queue
                    .enqueue(&session.user_id, server.id, &title_key, &provider_vec, played)
                    .await;
            }
        }
    }
}

async fn sync_one_peer(
    session: &AuthorizationSession,
    server: &Server,
    provider_vec: &[(String, String)],
    title_key: &str,
    played: bool,
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
            .find_item_id_by_provider_ids(&session.original_user_id, provider_vec)
            .await
        {
            Ok(found) => {
                ITEM_MATCH_CACHE.insert(cache_key, found.clone()).await;
                found
            }
            Err(e) => {
                warn!("watched-sync: lookup on '{}' failed: {}", server.name, e);
                return PeerOutcome::Failed;
            }
        },
    };

    let Some(peer_item_id) = peer_item_id else {
        debug!("watched-sync: '{}' has no match for {title_key}", server.name);
        return PeerOutcome::NoMatch;
    };

    match client
        .set_played(&session.original_user_id, &peer_item_id, played)
        .await
    {
        Ok(()) => {
            info!(
                "watched-sync: set played={played} on '{}' (item {peer_item_id})",
                server.name
            );
            PeerOutcome::Synced
        }
        Err(e) => {
            warn!("watched-sync: set played on '{}' failed: {}", server.name, e);
            PeerOutcome::Failed
        }
    }
}

/// Stable per-title key: sorted, lowercased `key.value` provider pairs.
fn title_key(provider_vec: &[(String, String)]) -> String {
    let mut keyed: Vec<String> = provider_vec
        .iter()
        .map(|(k, v)| format!("{}.{}", k.to_lowercase(), v))
        .collect();
    keyed.sort();
    keyed.join(",")
}

/// Parse `/Users/{user_id}/PlayedItems/{item_id}` -> `item_id`. Returns `None`
/// for any other path so the hook only fires on mark-played/unplayed.
pub fn played_item_id_from_path(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/Users/")?;
    let (_user, rest) = rest.split_once('/')?;
    let item = rest.strip_prefix("PlayedItems/")?;
    if item.is_empty() || item.contains('/') {
        return None;
    }
    Some(item)
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
    provider_ids: Vec<(String, String)>,
    played: bool,
}

impl WatchedSyncQueue {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Queue (or refresh) a pending change. Deduped per (user, server, title):
    /// a newer intent overwrites the old one and resets the attempt counter.
    async fn enqueue(
        &self,
        user_id: &str,
        server_id: i64,
        title_key: &str,
        provider_ids: &[(String, String)],
        played: bool,
    ) {
        let json = match serde_json::to_string(provider_ids) {
            Ok(j) => j,
            Err(e) => {
                warn!("watched-sync: can't serialize provider ids: {e}");
                return;
            }
        };
        let res = sqlx::query(
            "INSERT INTO watched_sync_queue \
             (user_id, server_id, title_key, provider_ids, played, attempts, updated_at) \
             VALUES (?, ?, ?, ?, ?, 0, CURRENT_TIMESTAMP) \
             ON CONFLICT(user_id, server_id, title_key) DO UPDATE SET \
               provider_ids = excluded.provider_ids, played = excluded.played, \
               attempts = 0, updated_at = CURRENT_TIMESTAMP",
        )
        .bind(user_id)
        .bind(server_id)
        .bind(title_key)
        .bind(&json)
        .bind(played)
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            warn!("watched-sync: enqueue failed: {e}");
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
            "SELECT id, user_id, server_id, title_key, provider_ids, played \
             FROM watched_sync_queue WHERE attempts < ? ORDER BY updated_at LIMIT 500",
        )
        .bind(MAX_RETRY_ATTEMPTS)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();
        rows.iter()
            .filter_map(|r| {
                let json: String = r.try_get("provider_ids").ok()?;
                Some(QueueEntry {
                    id: r.try_get("id").ok()?,
                    user_id: r.try_get("user_id").ok()?,
                    server_id: r.try_get("server_id").ok()?,
                    title_key: r.try_get("title_key").ok()?,
                    provider_ids: serde_json::from_str(&json).ok()?,
                    played: r.try_get::<i64, _>("played").ok()? != 0,
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
        info!("Starting watched-sync retry loop (interval {interval}s)");
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
    debug!("watched-sync: draining {} queued change(s)", pending.len());
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
        match sync_one_peer(&session, &server, &e.provider_ids, &e.title_key, e.played).await {
            PeerOutcome::Synced | PeerOutcome::NoMatch => {
                queue.remove_by_id(e.id).await;
                info!("watched-sync: drained queued change for '{}'", server.name);
            }
            PeerOutcome::Failed => queue.bump(e.id).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{played_item_id_from_path, title_key};

    #[test]
    fn parses_played_items_path() {
        assert_eq!(
            played_item_id_from_path("/Users/abc123/PlayedItems/item789"),
            Some("item789")
        );
    }

    #[test]
    fn rejects_unrelated_paths() {
        assert_eq!(played_item_id_from_path("/Users/abc/Items/xyz"), None);
        assert_eq!(played_item_id_from_path("/Users/abc/FavoriteItems/x"), None);
        assert_eq!(played_item_id_from_path("/Users/abc/PlayedItems/"), None);
        assert_eq!(played_item_id_from_path("/Users/abc/PlayedItems/x/extra"), None);
        assert_eq!(played_item_id_from_path("/System/Info"), None);
    }

    #[test]
    fn title_key_is_order_independent() {
        let a = title_key(&[("Imdb".into(), "tt1".into()), ("Tmdb".into(), "22".into())]);
        let b = title_key(&[("Tmdb".into(), "22".into()), ("Imdb".into(), "tt1".into())]);
        assert_eq!(a, b);
        assert_eq!(a, "imdb.tt1,tmdb.22");
    }
}
