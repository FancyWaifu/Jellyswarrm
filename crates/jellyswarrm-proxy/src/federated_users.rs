use std::sync::Arc;

use tracing::{error, info, warn};

use crate::{
    encryption::{decrypt_password, HashedPassword, Password},
    server_storage::ServerStorageService,
    user_authorization_service::UserAuthorizationService,
    AppState,
};
use jellyfin_api::JellyfinClient;

#[derive(Debug, Clone)]
pub enum SyncStatus {
    Created,
    AlreadyExists,
    ExistsWithDifferentPassword,
    Failed,
    Skipped,
    Deleted,
    NotFound,
}

#[derive(Debug, Clone)]
pub struct ServerSyncResult {
    pub server_name: String,
    pub status: SyncStatus,
    pub message: Option<String>,
}

#[derive(Clone)]
pub struct FederatedUserService {
    server_storage: Arc<ServerStorageService>,
    user_authorization: Arc<UserAuthorizationService>,
    config: Arc<tokio::sync::RwLock<crate::config::AppConfig>>,
}

impl FederatedUserService {
    pub fn new(state: &AppState) -> Self {
        Self {
            server_storage: state.server_storage.clone(),
            user_authorization: state.user_authorization.clone(),
            config: state.config.clone(),
        }
    }

    pub fn new_from_components(
        server_storage: Arc<ServerStorageService>,
        user_authorization: Arc<UserAuthorizationService>,
        config: Arc<tokio::sync::RwLock<crate::config::AppConfig>>,
    ) -> Self {
        Self {
            server_storage,
            user_authorization,
            config,
        }
    }

    /// Syncs a user to all configured servers where an admin account is available.
    /// If the user does not exist on a server, it is created.
    /// If the user exists, we assume it's fine (we don't update passwords for existing users here to avoid conflicts).
    pub async fn sync_user_to_all_servers(
        &self,
        username: &str,
        password: &Password,
        user_id: &str,
    ) -> Vec<ServerSyncResult> {
        let mut results = Vec::new();
        let servers = match self.server_storage.list_servers().await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to list servers for sync: {}", e);
                return results;
            }
        };

        let config = self.config.read().await;
        let admin_password: HashedPassword = config.password.clone().into();

        drop(config);

        for server in servers {
            // Check if we have admin credentials for this server
            if let Some(admin) = match self.server_storage.get_server_admin(server.id).await {
                Ok(a) => a,
                Err(e) => {
                    results.push(ServerSyncResult {
                        server_name: server.name.clone(),
                        status: SyncStatus::Failed,
                        message: Some(format!("Failed to get admin creds: {}", e)),
                    });
                    continue;
                }
            } {
                // Decrypt admin password
                let decrypted_admin_password =
                    match decrypt_password(&admin.password, &admin_password) {
                        Ok(p) => p,
                        Err(e) => {
                            error!(
                                "Failed to decrypt admin password for server {}: {}",
                                server.name, e
                            );
                            results.push(ServerSyncResult {
                                server_name: server.name.clone(),
                                status: SyncStatus::Failed,
                                message: Some("Failed to decrypt admin password".to_string()),
                            });
                            continue;
                        }
                    };

                let client_info = crate::config::CLIENT_INFO.clone();

                let client = match JellyfinClient::new(server.url.as_str(), client_info.clone()) {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to create jellyfin client: {}", e);
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status: SyncStatus::Failed,
                            message: Some(format!("Client error: {}", e)),
                        });
                        continue;
                    }
                };

                // Authenticate as admin to get token
                match client
                    .authenticate_by_name(&admin.username, decrypted_admin_password.as_str())
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        error!(
                            "Failed to authenticate as admin on server {}: {}",
                            server.name, e
                        );
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status: SyncStatus::Failed,
                            message: Some(format!("Admin auth failed: {}", e)),
                        });
                        continue;
                    }
                };

                // Check if user exists
                let users = match client.get_users().await {
                    Ok(u) => u,
                    Err(e) => {
                        error!("Failed to list users on server {}: {}", server.name, e);
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status: SyncStatus::Failed,
                            message: Some(format!("Failed to list users: {}", e)),
                        });
                        continue;
                    }
                };

                let existing_user = users.iter().find(|u| u.name.eq_ignore_ascii_case(username));

                if let Some(remote_user) = existing_user {
                    // User exists. Check if password matches.
                    // We need a new client to check user password
                    let user_client =
                        match JellyfinClient::new(server.url.as_str(), client_info.clone()) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };

                    let (status, should_map) = match user_client
                        .authenticate_by_name(username, password.as_str())
                        .await
                    {
                        Ok(_) => (SyncStatus::AlreadyExists, true),
                        Err(_) => (SyncStatus::ExistsWithDifferentPassword, false),
                    };

                    info!(
                        "Synced user {} to server {} (Remote ID: {}, Status: {:?})",
                        username, server.name, remote_user.id, status
                    );

                    if should_map {
                        if let Err(e) = self
                            .user_authorization
                            .add_server_mapping(
                                user_id,
                                server.url.as_str(),
                                username,
                                password,
                                Some(&password.into()), // Encrypt with their own password so they can use it
                            )
                            .await
                        {
                            error!(
                                "Failed to create local mapping for synced user on server {}: {}",
                                server.name, e
                            );
                            results.push(ServerSyncResult {
                                server_name: server.name.clone(),
                                status: SyncStatus::Failed,
                                message: Some(format!("Failed to save local mapping: {}", e)),
                            });
                        } else {
                            results.push(ServerSyncResult {
                                server_name: server.name.clone(),
                                status,
                                message: None,
                            });
                        }
                    } else {
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status,
                            message: Some("User exists with different password".to_string()),
                        });
                    }
                } else {
                    // Create user
                    match client.create_user(username, Some(password.as_str())).await {
                        Ok(new_user) => {
                            info!(
                                "Synced user {} to server {} (Remote ID: {}, Status: Created)",
                                username, server.name, new_user.id
                            );

                            if let Err(e) = self
                                .user_authorization
                                .add_server_mapping(
                                    user_id,
                                    server.url.as_str(),
                                    username,
                                    password,
                                    Some(&password.into()), // Encrypt with their own password so they can use it
                                )
                                .await
                            {
                                error!(
                                    "Failed to create local mapping for synced user on server {}: {}",
                                    server.name, e
                                );
                                results.push(ServerSyncResult {
                                    server_name: server.name.clone(),
                                    status: SyncStatus::Failed,
                                    message: Some(format!("Failed to save local mapping: {}", e)),
                                });
                            } else {
                                results.push(ServerSyncResult {
                                    server_name: server.name.clone(),
                                    status: SyncStatus::Created,
                                    message: None,
                                });
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Failed to sync user {} to server {}: {}",
                                username, server.name, e
                            );
                            results.push(ServerSyncResult {
                                server_name: server.name.clone(),
                                status: SyncStatus::Failed,
                                message: Some(format!("Sync failed: {}", e)),
                            });
                        }
                    }
                }
            } else {
                warn!(
                    "Skipping sync for server {}: No admin credentials configured",
                    server.name
                );
                results.push(ServerSyncResult {
                    server_name: server.name.clone(),
                    status: SyncStatus::Skipped,
                    message: Some("No admin credentials".to_string()),
                });
            }
        }

        results
    }

    /// Provision an SSO-authenticated user across every server that has admin
    /// credentials, then open an initial backend session per server.
    ///
    /// Unlike [`Self::sync_user_to_all_servers`], mappings are encrypted under
    /// the **master key** (not a user password) so future SSO logins — which
    /// carry no password — can still decrypt them (see docs/sso.md §3). The
    /// session is stamped with a synthetic device; binding real-client device
    /// ids is handled lazily on first proxied request (Phase 3).
    pub async fn provision_sso_user(
        &self,
        username: &str,
        user_id: &str,
        password: &Password,
    ) -> Vec<ServerSyncResult> {
        let mut results = Vec::new();
        let servers = match self.server_storage.list_servers().await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to list servers for SSO provisioning: {}", e);
                return results;
            }
        };

        let master_key: HashedPassword = {
            let config = self.config.read().await;
            config.password.clone().into()
        };
        let client_info = crate::config::CLIENT_INFO.clone();

        for server in servers {
            let admin = match self.server_storage.get_server_admin(server.id).await {
                Ok(Some(a)) => a,
                Ok(None) => {
                    results.push(ServerSyncResult {
                        server_name: server.name.clone(),
                        status: SyncStatus::Skipped,
                        message: Some("No admin credentials".to_string()),
                    });
                    continue;
                }
                Err(e) => {
                    results.push(ServerSyncResult {
                        server_name: server.name.clone(),
                        status: SyncStatus::Failed,
                        message: Some(format!("Failed to get admin creds: {}", e)),
                    });
                    continue;
                }
            };

            let decrypted_admin_password = match decrypt_password(&admin.password, &master_key) {
                Ok(p) => p,
                Err(e) => {
                    error!(
                        "Failed to decrypt admin password for {}: {}",
                        server.name, e
                    );
                    results.push(ServerSyncResult {
                        server_name: server.name.clone(),
                        status: SyncStatus::Failed,
                        message: Some("Failed to decrypt admin password".to_string()),
                    });
                    continue;
                }
            };

            let admin_client = match JellyfinClient::new(server.url.as_str(), client_info.clone()) {
                Ok(c) => c,
                Err(e) => {
                    results.push(ServerSyncResult {
                        server_name: server.name.clone(),
                        status: SyncStatus::Failed,
                        message: Some(format!("Client error: {}", e)),
                    });
                    continue;
                }
            };

            if let Err(e) = admin_client
                .authenticate_by_name(&admin.username, decrypted_admin_password.as_str())
                .await
            {
                error!("Admin auth failed on {}: {}", server.name, e);
                results.push(ServerSyncResult {
                    server_name: server.name.clone(),
                    status: SyncStatus::Failed,
                    message: Some(format!("Admin auth failed: {}", e)),
                });
                continue;
            }

            // Ensure the backend account exists; create it with our generated
            // password if missing.
            let users = match admin_client.get_users().await {
                Ok(u) => u,
                Err(e) => {
                    results.push(ServerSyncResult {
                        server_name: server.name.clone(),
                        status: SyncStatus::Failed,
                        message: Some(format!("Failed to list users: {}", e)),
                    });
                    continue;
                }
            };
            let existing_remote_id = users
                .iter()
                .find(|u| u.name.eq_ignore_ascii_case(username))
                .map(|u| u.id.clone());
            if existing_remote_id.is_none() {
                if let Err(e) = admin_client.create_user(username, Some(password.as_str())).await {
                    results.push(ServerSyncResult {
                        server_name: server.name.clone(),
                        status: SyncStatus::Failed,
                        message: Some(format!("create_user failed: {}", e)),
                    });
                    continue;
                }
            }

            // Does Jellyswarrm already manage this (user, server)? A prior mapping
            // proves we provisioned the backend account ourselves — only then is it
            // ours to reset.
            let we_manage_mapping = self
                .user_authorization
                .get_server_mapping(user_id, server.url.as_str())
                .await
                .ok()
                .flatten()
                .is_some();

            // Authenticate as the user to obtain a backend access token.
            let user_client = match JellyfinClient::new(server.url.as_str(), client_info.clone()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let mut auth_res = user_client
                .authenticate_by_name_typed::<jellyfin_api::models::AuthResponse>(
                    username,
                    password.as_str(),
                )
                .await;
            if auth_res.is_err() {
                if let Some(remote_id) = &existing_remote_id {
                    if !we_manage_mapping {
                        // SAFETY (audit finding G): the account pre-existed on the
                        // backend and Jellyswarrm never provisioned it — it belongs
                        // to a real backend user. NEVER reset a stranger's password.
                        // Skip this backend with a clear warning.
                        warn!(
                            "SSO user '{}' collides with a pre-existing account on {} that Jellyswarrm did not create; refusing to reset it — skipping this backend.",
                            username, server.name
                        );
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status: SyncStatus::Skipped,
                            message: Some(
                                "Username collides with a pre-existing backend account; not reset (safety)."
                                    .to_string(),
                            ),
                        });
                        continue;
                    }
                    // Our own managed account whose password we no longer hold —
                    // safe to reset and retry.
                    info!(
                        "SSO user '{}' on {}: resetting password for Jellyswarrm-managed account",
                        username, server.name
                    );
                    if let Err(e) =
                        admin_client.set_user_password(remote_id, password.as_str()).await
                    {
                        warn!("admin password reset failed on {}: {}", server.name, e);
                    } else {
                        auth_res = user_client
                            .authenticate_by_name_typed::<jellyfin_api::models::AuthResponse>(
                                username,
                                password.as_str(),
                            )
                            .await;
                    }
                }
            }
            let auth: jellyfin_api::models::AuthResponse = match auth_res {
                Ok(a) => a,
                Err(e) => {
                    warn!(
                        "SSO user auth failed on {} (after reset attempt): {}",
                        server.name, e
                    );
                    results.push(ServerSyncResult {
                        server_name: server.name.clone(),
                        status: SyncStatus::Failed,
                        message: Some("Could not establish backend session".to_string()),
                    });
                    continue;
                }
            };

            // Store the mapping (master-key encrypted) + an initial session.
            if let Err(e) = self
                .user_authorization
                .add_server_mapping(
                    user_id,
                    server.url.as_str(),
                    username,
                    password,
                    Some(&master_key),
                )
                .await
            {
                results.push(ServerSyncResult {
                    server_name: server.name.clone(),
                    status: SyncStatus::Failed,
                    message: Some(format!("mapping failed: {}", e)),
                });
                continue;
            }

            let synthetic = crate::models::Authorization {
                client: "Jellyswarrm SSO".to_string(),
                device: "Web".to_string(),
                device_id: format!("sso-{}", user_id),
                version: env!("CARGO_PKG_VERSION").to_string(),
                token: None,
            };
            if let Err(e) = self
                .user_authorization
                .store_authorization_session(
                    user_id,
                    server.url.as_str(),
                    &synthetic,
                    auth.access_token.clone(),
                    auth.user.id.clone(),
                    None,
                )
                .await
            {
                results.push(ServerSyncResult {
                    server_name: server.name.clone(),
                    status: SyncStatus::Failed,
                    message: Some(format!("session store failed: {}", e)),
                });
                continue;
            }

            info!(
                "Provisioned SSO user {} on {} (remote id {})",
                username, server.name, auth.user.id
            );
            results.push(ServerSyncResult {
                server_name: server.name.clone(),
                status: SyncStatus::Created,
                message: None,
            });
        }

        results
    }

    /// Lazily establish backend sessions for `device` from the user's existing
    /// mappings. Real Jellyfin clients (web/native) present their own device id,
    /// which won't match the synthetic device an SSO login was provisioned under;
    /// this re-auths to each mapped backend (decrypting the master-key mapping)
    /// and stores a session bound to the caller's device. Returns the count
    /// created. Only runs when the caller already holds a valid virtual_key (the
    /// user is resolved upstream), so possessing the bearer token is the auth.
    pub async fn ensure_device_sessions(
        &self,
        user: &crate::user_authorization_service::User,
        device: &crate::user_authorization_service::Device,
    ) -> usize {
        let mappings = match self.user_authorization.list_server_mappings(&user.id).await {
            Ok(m) => m,
            Err(e) => {
                error!("ensure_device_sessions: list mappings failed: {}", e);
                return 0;
            }
        };
        if mappings.is_empty() {
            return 0;
        }

        let servers = match self.server_storage.list_servers().await {
            Ok(s) => s,
            Err(_) => return 0,
        };
        let (admin_master, client_info) = {
            let config = self.config.read().await;
            (
                Into::<HashedPassword>::into(config.password.clone()),
                crate::config::CLIENT_INFO.clone(),
            )
        };

        let mut created = 0usize;
        for mapping in mappings {
            let Some(server) = servers.iter().find(|s| {
                s.url.as_str().trim_end_matches('/') == mapping.server_url.trim_end_matches('/')
            }) else {
                continue;
            };

            // Master-key encrypted for SSO users; user-password for legacy. The
            // decrypt helper tries the user hash then falls back to the master key.
            let decrypted = self.user_authorization.decrypt_server_mapping_password(
                &mapping,
                &user.original_password_hash,
                &admin_master,
                None,
                None,
            );

            let client = match JellyfinClient::new(server.url.as_str(), client_info.clone()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let auth: jellyfin_api::models::AuthResponse = match client
                .authenticate_by_name_typed(&mapping.mapped_username, decrypted.as_str())
                .await
            {
                Ok(a) => a,
                Err(e) => {
                    warn!(
                        "ensure_device_sessions: backend auth failed on {}: {}",
                        server.name, e
                    );
                    continue;
                }
            };

            let auth_record = crate::models::Authorization {
                client: device.client.clone(),
                device: device.device.clone(),
                device_id: device.device_id.clone(),
                version: device.version.clone(),
                token: None,
            };
            if let Err(e) = self
                .user_authorization
                .store_authorization_session(
                    &user.id,
                    &mapping.server_url,
                    &auth_record,
                    auth.access_token,
                    auth.user.id,
                    None,
                )
                .await
            {
                error!(
                    "ensure_device_sessions: store session failed on {}: {}",
                    server.name, e
                );
                continue;
            }
            created += 1;
        }
        if created > 0 {
            info!(
                "Lazily created {} device session(s) for user {} (device_id {})",
                created, user.id, device.device_id
            );
        }
        created
    }

    /// Refresh a user's existing backend session tokens in place: re-authenticate
    /// each backend ONCE (via the stored mapping) and update all of the user's
    /// sessions for that backend with the fresh token. Called on SSO login so
    /// stale tokens left over from prior logins heal automatically — without the
    /// re-auth storm that deleting sessions + lazy-recreating would cause against
    /// federated peers. Returns the number of sessions refreshed.
    pub async fn refresh_user_sessions(&self, user: &crate::user_authorization_service::User) -> usize {
        let sessions = match self
            .user_authorization
            .get_user_sessions(&user.id, None)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                error!("refresh_user_sessions: list sessions failed: {}", e);
                return 0;
            }
        };
        if sessions.is_empty() {
            return 0;
        }
        let mappings = match self.user_authorization.list_server_mappings(&user.id).await {
            Ok(m) => m,
            Err(_) => return 0,
        };
        let (master_key, client_info) = {
            let config = self.config.read().await;
            (
                Into::<HashedPassword>::into(config.password.clone()),
                crate::config::CLIENT_INFO.clone(),
            )
        };

        let norm = |u: &str| u.trim_end_matches('/').to_string();
        // Re-auth each backend at most once; cache (token, original_user_id).
        let mut fresh: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        let mut refreshed = 0usize;

        for (session, server) in &sessions {
            let key = norm(server.url.as_str());
            if !fresh.contains_key(&key) {
                let Some(mapping) = mappings.iter().find(|m| norm(&m.server_url) == key) else {
                    continue;
                };
                let decrypted = self.user_authorization.decrypt_server_mapping_password(
                    mapping,
                    &user.original_password_hash,
                    &master_key,
                    None,
                    None,
                );
                let Ok(client) = JellyfinClient::new(server.url.as_str(), client_info.clone()) else {
                    continue;
                };
                match client
                    .authenticate_by_name_typed::<jellyfin_api::models::AuthResponse>(
                        &mapping.mapped_username,
                        decrypted.as_str(),
                    )
                    .await
                {
                    Ok(auth) => {
                        fresh.insert(key.clone(), (auth.access_token, auth.user.id));
                    }
                    Err(e) => {
                        warn!(
                            "refresh_user_sessions: re-auth failed on {}: {}",
                            server.name, e
                        );
                        continue;
                    }
                }
            }
            if let Some((token, original_user_id)) = fresh.get(&key) {
                let auth_record = crate::models::Authorization {
                    client: session.device.client.clone(),
                    device: session.device.device.clone(),
                    device_id: session.device.device_id.clone(),
                    version: session.device.version.clone(),
                    token: None,
                };
                if self
                    .user_authorization
                    .store_authorization_session(
                        &user.id,
                        &session.server_url,
                        &auth_record,
                        token.clone(),
                        original_user_id.clone(),
                        None,
                    )
                    .await
                    .is_ok()
                {
                    refreshed += 1;
                }
            }
        }
        if refreshed > 0 {
            info!(
                "Refreshed {} backend session(s) for user {} on SSO login",
                refreshed, user.id
            );
        }
        refreshed
    }

    pub async fn delete_user_from_all_servers(&self, username: &str) -> Vec<ServerSyncResult> {
        let mut results = Vec::new();
        let servers = match self.server_storage.list_servers().await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to list servers for delete: {}", e);
                return results;
            }
        };

        let config = self.config.read().await;
        let admin_password = &config.password;

        for server in servers {
            if let Some(admin) = match self.server_storage.get_server_admin(server.id).await {
                Ok(a) => a,
                Err(e) => {
                    results.push(ServerSyncResult {
                        server_name: server.name.clone(),
                        status: SyncStatus::Failed,
                        message: Some(format!("Failed to get admin creds: {}", e)),
                    });
                    continue;
                }
            } {
                let decrypted_admin_password =
                    match decrypt_password(&admin.password, &admin_password.into()) {
                        Ok(p) => p,
                        Err(e) => {
                            error!(
                                "Failed to decrypt admin password for server {}: {}",
                                server.name, e
                            );
                            results.push(ServerSyncResult {
                                server_name: server.name.clone(),
                                status: SyncStatus::Failed,
                                message: Some("Failed to decrypt admin password".to_string()),
                            });
                            continue;
                        }
                    };

                let client_info = crate::config::CLIENT_INFO.clone();

                let client = match JellyfinClient::new(server.url.as_str(), client_info.clone()) {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to create jellyfin client: {}", e);
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status: SyncStatus::Failed,
                            message: Some(format!("Client error: {}", e)),
                        });
                        continue;
                    }
                };

                match client
                    .authenticate_by_name(&admin.username, decrypted_admin_password.as_str())
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        error!(
                            "Failed to authenticate as admin on server {}: {}",
                            server.name, e
                        );
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status: SyncStatus::Failed,
                            message: Some(format!("Admin auth failed: {}", e)),
                        });
                        continue;
                    }
                };

                // Find user ID
                let users = match client.get_users().await {
                    Ok(u) => u,
                    Err(e) => {
                        error!("Failed to list users on server {}: {}", server.name, e);
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status: SyncStatus::Failed,
                            message: Some(format!("Failed to list users: {}", e)),
                        });
                        continue;
                    }
                };

                let user_id = users
                    .iter()
                    .find(|u| u.name.eq_ignore_ascii_case(username))
                    .map(|u| u.id.clone());

                if let Some(id) = user_id {
                    match client.delete_user(&id).await {
                        Ok(_) => {
                            info!(
                                "Deleted user {} from server {} (Deleted: true)",
                                username, server.name
                            );
                            results.push(ServerSyncResult {
                                server_name: server.name.clone(),
                                status: SyncStatus::Deleted,
                                message: None,
                            });
                        }
                        Err(e) => {
                            warn!(
                                "Failed to delete user {} from server {}: {}",
                                username, server.name, e
                            );
                            results.push(ServerSyncResult {
                                server_name: server.name.clone(),
                                status: SyncStatus::Failed,
                                message: Some(format!("Delete failed: {}", e)),
                            });
                        }
                    }
                } else {
                    results.push(ServerSyncResult {
                        server_name: server.name.clone(),
                        status: SyncStatus::NotFound,
                        message: None,
                    });
                }
            } else {
                results.push(ServerSyncResult {
                    server_name: server.name.clone(),
                    status: SyncStatus::Skipped,
                    message: Some("No admin credentials".to_string()),
                });
            }
        }

        results
    }
}
