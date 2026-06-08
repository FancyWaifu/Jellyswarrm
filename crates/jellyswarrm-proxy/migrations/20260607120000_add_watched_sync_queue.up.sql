-- Retry queue for cross-server watched-state sync: when a peer backend is
-- offline/ejected at mark time, the pending played/unplayed change is queued
-- and drained by a background loop once the peer recovers. Deduped per
-- (user, server, title) so only the latest intent is retried.
CREATE TABLE IF NOT EXISTS watched_sync_queue (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id      TEXT NOT NULL,
    server_id    INTEGER NOT NULL,
    title_key    TEXT NOT NULL,            -- sorted identifying provider pairs
    provider_ids TEXT NOT NULL,            -- JSON [[key,value],...] to re-find the item
    played       INTEGER NOT NULL,         -- desired state
    attempts     INTEGER NOT NULL DEFAULT 0,
    created_at   TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at   TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (user_id)   REFERENCES users (id)   ON DELETE CASCADE,
    FOREIGN KEY (server_id) REFERENCES servers (id) ON DELETE CASCADE,
    UNIQUE (user_id, server_id, title_key)
);
