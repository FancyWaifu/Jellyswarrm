-- Generalise the cross-server sync retry queue from "played flag only" to a full
-- UserData snapshot (resume position, played, play count, favorite, last-played)
-- matched by a richer key (provider id for movies/series, series+S/E for
-- episodes). The queue holds only transient, best-effort retries, so rebuilding
-- it (dropping any in-flight entries) is safe.
DROP TABLE IF EXISTS watched_sync_queue;
CREATE TABLE watched_sync_queue (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    TEXT NOT NULL,
    server_id  INTEGER NOT NULL,
    title_key  TEXT NOT NULL,        -- MatchKey::cache_key() (dedup key)
    match_key  TEXT NOT NULL,        -- JSON-serialized MatchKey (how to re-find the item)
    user_data  TEXT NOT NULL,        -- JSON-serialized ItemUserData (what to apply)
    attempts   INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (user_id)   REFERENCES users (id)   ON DELETE CASCADE,
    FOREIGN KEY (server_id) REFERENCES servers (id) ON DELETE CASCADE,
    UNIQUE (user_id, server_id, title_key)
);
