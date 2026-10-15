-- One mapping per (user, server): 'http://x' and 'http://x/' used to create two
-- rows, leaving a stale one (possibly still holding a legacy secret). Keep the
-- most recently updated row per (user, canonical URL), move sessions onto it,
-- and store URLs without trailing slashes from now on.
CREATE TEMP TABLE mapping_keep AS
SELECT m.id AS id,
       (SELECT k.id FROM server_mappings k
         WHERE k.user_id = m.user_id
           AND RTRIM(TRIM(k.server_url), '/') = RTRIM(TRIM(m.server_url), '/')
         ORDER BY k.updated_at DESC, k.id DESC LIMIT 1) AS keep_id
FROM server_mappings m;

-- Sessions that would collide on UNIQUE(user_id, mapping_id, device_id) after
-- the move: keep the keeper's own, else the newest.
DELETE FROM authorization_sessions WHERE id IN (
    SELECT id FROM (
        SELECT a.id, ROW_NUMBER() OVER (
                   PARTITION BY a.user_id, mk.keep_id, a.device_id
                   ORDER BY (a.mapping_id = mk.keep_id) DESC, a.updated_at DESC, a.id DESC) AS rn
        FROM authorization_sessions a JOIN mapping_keep mk ON mk.id = a.mapping_id)
    WHERE rn > 1);

UPDATE authorization_sessions
SET mapping_id = (SELECT keep_id FROM mapping_keep WHERE id = authorization_sessions.mapping_id)
WHERE mapping_id IN (SELECT id FROM mapping_keep WHERE keep_id != id);

DELETE FROM server_mappings WHERE id IN (SELECT id FROM mapping_keep WHERE keep_id != id);

UPDATE server_mappings SET server_url = RTRIM(TRIM(server_url), '/');

DROP TABLE mapping_keep;
