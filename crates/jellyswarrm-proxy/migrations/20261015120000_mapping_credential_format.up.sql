-- 'legacy': password-derived/admin-key AES (or plaintext); 'v2': HKDF(session_key) AES-GCM.
ALTER TABLE server_mappings ADD COLUMN credential_format TEXT NOT NULL DEFAULT 'legacy';
