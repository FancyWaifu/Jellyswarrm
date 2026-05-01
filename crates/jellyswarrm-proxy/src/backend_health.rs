//! Per-backend outlier ejection for the federated fanout path.
//!
//! Tracks recent failures (timeouts, network errors, 5xx) per backend in a
//! sliding window. When a backend exceeds the failure threshold, it's ejected
//! from federation fanouts for a fixed cooldown window. A revival probe
//! periodically re-checks ejected backends and clears the ejection on success.
//!
//! This is *separate* from `ServerStorageService::health_status`, which only
//! reflects the periodic `/System/Info/Public` reachability check. This module
//! reacts to real fanout outcomes — a backend can be reachable but slow enough
//! on actual federation calls to drag the dashboard down.
//!
//! Failures dropped from consideration: 4xx responses (those are user/auth
//! errors, not backend health). Only timeouts, connection errors, and 5xx
//! count toward ejection.
//!
//! 4xx responses (those are user/auth errors, not backend health) are not
//! counted toward ejection. Only timeouts, connection errors, and 5xx count.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::server_storage::ServerStorageService;

/// Default per-backend timeout for federation calls.
pub const DEFAULT_BACKEND_TIMEOUT: Duration = Duration::from_secs(3);

/// Sliding window over which failures are counted.
const FAILURE_WINDOW: Duration = Duration::from_secs(30);

/// Failures within the window required to trigger ejection.
const FAILURE_THRESHOLD: usize = 5;

/// How long a backend stays ejected before re-probing.
const EJECTION_DURATION: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
struct BackendState {
    failures: VecDeque<Instant>,
    ejected_until: Option<Instant>,
}

impl BackendState {
    fn prune(&mut self, now: Instant) {
        let cutoff = now - FAILURE_WINDOW;
        while let Some(front) = self.failures.front() {
            if *front < cutoff {
                self.failures.pop_front();
            } else {
                break;
            }
        }
    }

    fn is_ejected(&mut self, now: Instant) -> bool {
        match self.ejected_until {
            Some(until) if now < until => true,
            Some(_) => {
                // Cooldown expired; clear so the next failure starts a fresh count.
                self.ejected_until = None;
                false
            }
            None => false,
        }
    }
}

/// Concurrent-safe ejection state keyed by `Server::id`.
#[derive(Debug, Clone, Default)]
pub struct BackendHealth {
    inner: Arc<RwLock<HashMap<i64, BackendState>>>,
}

impl BackendHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true if `server_id` is currently ejected and should be skipped.
    pub async fn is_ejected(&self, server_id: i64) -> bool {
        let now = Instant::now();
        let mut guard = self.inner.write().await;
        guard
            .entry(server_id)
            .or_default()
            .is_ejected(now)
    }

    /// Record a successful fanout call. Pulls the backend out of ejection if
    /// it was ejected and clears its sliding window so prior failures don't
    /// linger.
    pub async fn record_success(&self, server_id: i64, server_name: &str) {
        let mut guard = self.inner.write().await;
        let entry = guard.entry(server_id).or_default();
        let was_ejected = entry.ejected_until.is_some();
        entry.ejected_until = None;
        entry.failures.clear();
        if was_ejected {
            info!(
                "Backend '{}' (id={}) recovered; clearing ejection",
                server_name, server_id
            );
        }
    }

    /// Record a failed fanout call (timeout, connection error, 5xx). Triggers
    /// ejection if the failure threshold within the sliding window is hit.
    pub async fn record_failure(&self, server_id: i64, server_name: &str, reason: &str) {
        let now = Instant::now();
        let mut guard = self.inner.write().await;
        let entry = guard.entry(server_id).or_default();
        entry.prune(now);
        entry.failures.push_back(now);

        if entry.ejected_until.is_some() {
            return;
        }

        if entry.failures.len() >= FAILURE_THRESHOLD {
            let until = now + EJECTION_DURATION;
            entry.ejected_until = Some(until);
            warn!(
                "Ejecting backend '{}' (id={}) after {} failures in {:?} — last reason: {}. Cooldown {:?}.",
                server_name,
                server_id,
                entry.failures.len(),
                FAILURE_WINDOW,
                reason,
                EJECTION_DURATION,
            );
        }
    }

    /// Returns the list of currently-ejected backend IDs. Used by the revival
    /// probe to know which backends to test.
    pub async fn ejected_ids(&self) -> Vec<i64> {
        let now = Instant::now();
        let guard = self.inner.read().await;
        guard
            .iter()
            .filter_map(|(id, state)| {
                state
                    .ejected_until
                    .filter(|until| now < *until)
                    .map(|_| *id)
            })
            .collect()
    }
}

/// Spawn a background probe that re-checks ejected backends every `interval`
/// and clears the ejection if the backend's `/System/Info/Public` succeeds.
/// Cheap: when nothing is ejected, the loop just sleeps.
pub fn spawn_revival_probe(
    health: BackendHealth,
    server_storage: Arc<ServerStorageService>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!(
            "Starting backend ejection revival probe (interval = {:?})",
            interval
        );
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let ejected = health.ejected_ids().await;
            if ejected.is_empty() {
                continue;
            }

            let servers = match server_storage.list_servers().await {
                Ok(s) => s,
                Err(e) => {
                    debug!("Revival probe: list_servers failed: {}", e);
                    continue;
                }
            };

            for id in ejected {
                let Some(server) = servers.iter().find(|s| s.id == id) else {
                    // Server was removed while ejected — drop the entry by
                    // recording a synthetic success.
                    health.record_success(id, "<removed>").await;
                    continue;
                };
                let client = match jellyfin_api::JellyfinClient::new_with_client(
                    server.url.as_str(),
                    server_storage.client_info.clone(),
                    server_storage.http_client.clone(),
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        debug!(
                            "Revival probe: client build failed for '{}': {}",
                            server.name, e
                        );
                        continue;
                    }
                };
                match client.get_public_system_info().await {
                    Ok(_) => {
                        health.record_success(server.id, &server.name).await;
                    }
                    Err(e) => {
                        debug!(
                            "Revival probe: '{}' still unhealthy: {}",
                            server.name, e
                        );
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fresh_backend_is_not_ejected() {
        let h = BackendHealth::new();
        assert!(!h.is_ejected(1).await);
    }

    #[tokio::test]
    async fn ejection_triggers_after_threshold() {
        let h = BackendHealth::new();
        for _ in 0..FAILURE_THRESHOLD {
            h.record_failure(1, "test", "timeout").await;
        }
        assert!(h.is_ejected(1).await);
    }

    #[tokio::test]
    async fn success_clears_ejection() {
        let h = BackendHealth::new();
        for _ in 0..FAILURE_THRESHOLD {
            h.record_failure(1, "test", "timeout").await;
        }
        assert!(h.is_ejected(1).await);

        h.record_success(1, "test").await;
        assert!(!h.is_ejected(1).await);
    }

    #[tokio::test]
    async fn ejected_ids_lists_only_currently_ejected() {
        let h = BackendHealth::new();
        for _ in 0..FAILURE_THRESHOLD {
            h.record_failure(1, "a", "timeout").await;
        }
        h.record_failure(2, "b", "timeout").await;

        let ejected = h.ejected_ids().await;
        assert_eq!(ejected, vec![1]);
    }

    #[tokio::test]
    async fn below_threshold_failures_do_not_eject() {
        let h = BackendHealth::new();
        for _ in 0..(FAILURE_THRESHOLD - 1) {
            h.record_failure(1, "test", "timeout").await;
        }
        assert!(!h.is_ejected(1).await);
    }
}
