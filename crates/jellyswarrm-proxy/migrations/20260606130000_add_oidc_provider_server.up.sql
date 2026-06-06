-- Per-server SSO: optionally bind an OIDC provider to a specific federated
-- backend, so the login picker can group providers by which Jellyfin server
-- they sign you into. NULL server_id = available for all servers (global).
-- ON DELETE CASCADE: removing a backend also removes its bound providers.
ALTER TABLE oidc_providers
ADD COLUMN server_id INTEGER REFERENCES servers (id) ON DELETE CASCADE;
