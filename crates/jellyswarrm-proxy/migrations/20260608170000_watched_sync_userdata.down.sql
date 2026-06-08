-- Restore the played-flag-only queue schema.
DROP TABLE IF EXISTS watched_sync_queue;
CREATE TABLE watched_sync_queue (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id      TEXT NOT NULL,
    server_id    INTEGER NOT NULL,
    title_key    TEXT NOT NULL,
    provider_ids TEXT NOT NULL,
    played       INTEGER NOT NULL,
    attempts     INTEGER NOT NULL DEFAULT 0,
    created_at   TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at   TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (user_id)   REFERENCES users (id)   ON DELETE CASCADE,
    FOREIGN KEY (server_id) REFERENCES servers (id) ON DELETE CASCADE,
    UNIQUE (user_id, server_id, title_key)
);
