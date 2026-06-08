use crate::error::Error;
use crate::models::{
    AuthResponse, IncludeBaseItemFields, IncludeItemTypes, MediaFoldersResponse, User,
};
use reqwest::{header, Client, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::sync::RwLock;
use tracing::info;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientInfo {
    pub client: String,
    pub device: String,
    pub device_id: String,
    pub version: String,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            client: "Jellyfin API Client".to_string(),
            device: "Unknown".to_string(),
            device_id: "unknown-device-id".to_string(),
            version: "0.0.0".to_string(),
        }
    }
}

pub struct JellyfinClient {
    base_url: Url,
    client_info: ClientInfo,
    http_client: Client,
    auth_token: RwLock<Option<String>>,
}

impl PartialEq for JellyfinClient {
    fn eq(&self, other: &Self) -> bool {
        self.base_url == other.base_url && self.client_info == other.client_info
    }
}

impl Eq for JellyfinClient {}

impl std::hash::Hash for JellyfinClient {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.base_url.hash(state);
        self.client_info.hash(state);
    }
}

impl JellyfinClient {
    pub fn new(base_url: &str, client_info: ClientInfo) -> Result<Self, Error> {
        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        Self::new_with_client(base_url, client_info, http_client)
    }

    pub fn new_with_client(
        base_url: &str,
        client_info: ClientInfo,
        http_client: Client,
    ) -> Result<Self, Error> {
        let mut url = Url::parse(base_url)?;
        // Ensure trailing slash for consistent joining
        if !url.path().ends_with('/') {
            url.path_segments_mut()
                .map_err(|_| Error::UrlParse(url::ParseError::EmptyHost))?
                .push("");
        }

        Ok(Self {
            base_url: url,
            client_info,
            http_client,
            auth_token: RwLock::new(None),
        })
    }

    pub async fn with_token(&self, token: String) -> &Self {
        *self.auth_token.write().await = Some(token);
        self
    }

    pub async fn get_token(&self) -> Option<String> {
        self.auth_token.read().await.clone()
    }

    async fn build_auth_header(&self) -> String {
        let mut header = format!(
            "MediaBrowser Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
            self.client_info.client,
            self.client_info.device,
            self.client_info.device_id,
            self.client_info.version
        );

        if let Some(token) = self.auth_token.read().await.as_ref() {
            header.push_str(&format!(", Token=\"{}\"", token));
        }

        // println!("DEBUG HEADER: {}", header);
        header
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T, Error> {
        let url = self.base_url.join(path)?;
        let auth_header = self.build_auth_header().await;

        let mut request = self
            .http_client
            .request(method, url)
            .header(header::AUTHORIZATION, auth_header);

        if let Some(b) = body {
            request = request.json(b);
        }

        let response = request.send().await?;
        let status = response.status();

        if status.is_success() {
            let data = response.json::<T>().await?;
            Ok(data)
        } else {
            match status {
                StatusCode::UNAUTHORIZED => Err(Error::Unauthorized),
                StatusCode::FORBIDDEN => Err(Error::Forbidden),
                StatusCode::NOT_FOUND => Err(Error::NotFound),
                _ => {
                    let text = response.text().await.unwrap_or_default();
                    Err(Error::ServerError(format!("{} - {}", status, text)))
                }
            }
        }
    }

    async fn request_no_content(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(), Error> {
        let url = self.base_url.join(path)?;
        let auth_header = self.build_auth_header().await;

        let mut request = self
            .http_client
            .request(method, url)
            .header(header::AUTHORIZATION, auth_header);

        if let Some(b) = body {
            request = request.json(b);
        }

        let response = request.send().await?;
        let status = response.status();

        if status.is_success() {
            Ok(())
        } else {
            match status {
                StatusCode::UNAUTHORIZED => Err(Error::Unauthorized),
                StatusCode::FORBIDDEN => Err(Error::Forbidden),
                StatusCode::NOT_FOUND => Err(Error::NotFound),
                _ => {
                    let text = response.text().await.unwrap_or_default();
                    Err(Error::ServerError(format!("{} - {}", status, text)))
                }
            }
        }
    }

    pub async fn authenticate_by_name_typed<T: DeserializeOwned>(
        &self,
        username: &str,
        password: &str,
    ) -> Result<T, Error> {
        let body = json!({
            "Username": username,
            "Pw": password
        });

        self.request(
            reqwest::Method::POST,
            "Users/AuthenticateByName",
            Some(&body),
        )
        .await
        .map_err(|e| match e {
            Error::Unauthorized => Error::AuthenticationFailed("Invalid credentials".to_string()),
            _ => e,
        })
    }

    pub async fn authenticate_by_name(
        &self,
        username: &str,
        password: &str,
    ) -> Result<User, Error> {
        let response: AuthResponse = self.authenticate_by_name_typed(username, password).await?;

        let mut write_guard = self.auth_token.write().await;
        *write_guard = Some(response.access_token);
        info!("Authenticated user: {}", response.user.name);
        Ok(response.user)
    }

    pub async fn logout(&self) -> Result<(), Error> {
        self.request_no_content(reqwest::Method::POST, "Sessions/Logout", None)
            .await?;
        *self.auth_token.write().await = None;
        Ok(())
    }

    pub async fn get_me(&self) -> Result<User, Error> {
        self.request(reqwest::Method::GET, "Users/Me", None).await
    }

    // ----- watched-state sync helpers (cross-server played/unplayed) -----

    /// Fetch an item's `ProviderIds` (to match the same title across backends)
    /// AND its current `Played` state for this user, in one call. Re-reading the
    /// authoritative source state lets the sync converge correctly under rapid
    /// play/unplay toggles, regardless of fan-out task ordering.
    pub async fn get_item_match_info(
        &self,
        user_id: &str,
        item_id: &str,
    ) -> Result<(std::collections::HashMap<String, String>, Option<bool>), Error> {
        let path = format!("Users/{user_id}/Items/{item_id}?Fields=ProviderIds");
        let v: serde_json::Value = self.request(reqwest::Method::GET, &path, None).await?;
        let providers = v
            .get("ProviderIds")
            .and_then(|p| p.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let played = v
            .get("UserData")
            .and_then(|u| u.get("Played"))
            .and_then(|p| p.as_bool());
        Ok((providers, played))
    }

    /// Find a movie on this backend matching ANY of the given provider ids.
    /// `provider_ids` are (key, value) pairs, e.g. `("Imdb","tt0063350")`. Returns
    /// the backend's item id, or `None` if nothing matches.
    ///
    /// Jellyfin's server-side `AnyProviderIdEquals` filter is unreliable across
    /// versions (silently ignored on current builds), so we list the user's
    /// movies with their `ProviderIds` and match client-side. The caller memoises
    /// the result, so this lists a backend's movies at most once per title/hour.
    pub async fn find_item_id_by_provider_ids(
        &self,
        user_id: &str,
        provider_ids: &[(String, String)],
    ) -> Result<Option<String>, Error> {
        // Match only on *identifying* keys — collection-type keys are shared
        // across a franchise and would false-match a sibling movie. If the source
        // had only collection ids, refuse to match rather than guess wrong.
        let want: std::collections::HashSet<(String, String)> = provider_ids
            .iter()
            .filter(|(k, _)| is_identifying_provider_key(k))
            .map(|(k, v)| (k.to_lowercase(), v.clone()))
            .collect();
        if want.is_empty() {
            return Ok(None);
        }
        let path = format!(
            "Users/{user_id}/Items?Recursive=true&IncludeItemTypes=Movie\
             &Fields=ProviderIds&EnableImages=false&EnableUserData=false"
        );
        let v: serde_json::Value = self.request(reqwest::Method::GET, &path, None).await?;
        let Some(items) = v.get("Items").and_then(|i| i.as_array()) else {
            return Ok(None);
        };
        Ok(first_provider_match(items, &want))
    }

    /// Read an item's cross-backend match key (provider id, or series+S/E for an
    /// episode) and its current synced UserData. For an episode this also fetches
    /// the parent series to get its provider ids.
    pub async fn get_item_sync_info(
        &self,
        user_id: &str,
        item_id: &str,
    ) -> Result<(MatchKey, ItemUserData), Error> {
        let path = format!("Users/{user_id}/Items/{item_id}?Fields=ProviderIds");
        let v: serde_json::Value = self.request(reqwest::Method::GET, &path, None).await?;
        let user_data = ItemUserData::from_value(v.get("UserData"));
        let typ = v.get("Type").and_then(|t| t.as_str()).unwrap_or("");

        if typ == "Episode" {
            let season = v.get("ParentIndexNumber").and_then(|n| n.as_i64());
            let episode = v.get("IndexNumber").and_then(|n| n.as_i64());
            let series_id = v.get("SeriesId").and_then(|s| s.as_str());
            if let (Some(season), Some(episode), Some(series_id)) = (season, episode, series_id) {
                let sp = format!("Users/{user_id}/Items/{series_id}?Fields=ProviderIds");
                let sv: serde_json::Value =
                    self.request(reqwest::Method::GET, &sp, None).await?;
                return Ok((
                    MatchKey::Episode {
                        series: providers_from(sv.get("ProviderIds")),
                        season,
                        episode,
                    },
                    user_data,
                ));
            }
        }
        Ok((MatchKey::Provider(providers_from(v.get("ProviderIds"))), user_data))
    }

    /// Resolve "the same title" on this backend for a [`MatchKey`] → its item id,
    /// or `None` if this backend doesn't have it. Movies/series match by provider
    /// id; episodes resolve the series by provider id then match season+episode.
    pub async fn find_item_by_match(
        &self,
        user_id: &str,
        key: &MatchKey,
    ) -> Result<Option<String>, Error> {
        match key {
            MatchKey::Provider(pairs) => {
                let want = identifying_want(pairs);
                if want.is_empty() {
                    return Ok(None);
                }
                let path = format!(
                    "Users/{user_id}/Items?Recursive=true&IncludeItemTypes=Movie,Series\
                     &Fields=ProviderIds&EnableImages=false&EnableUserData=false"
                );
                let v: serde_json::Value = self.request(reqwest::Method::GET, &path, None).await?;
                let Some(items) = v.get("Items").and_then(|i| i.as_array()) else {
                    return Ok(None);
                };
                Ok(first_provider_match(items, &want))
            }
            MatchKey::Episode {
                series,
                season,
                episode,
            } => {
                let want = identifying_want(series);
                if want.is_empty() {
                    return Ok(None);
                }
                // Find the peer's matching series first…
                let path = format!(
                    "Users/{user_id}/Items?Recursive=true&IncludeItemTypes=Series\
                     &Fields=ProviderIds&EnableImages=false&EnableUserData=false"
                );
                let v: serde_json::Value = self.request(reqwest::Method::GET, &path, None).await?;
                let Some(items) = v.get("Items").and_then(|i| i.as_array()) else {
                    return Ok(None);
                };
                let Some(series_id) = first_provider_match(items, &want) else {
                    return Ok(None);
                };
                // …then the episode within it by season+episode number.
                let path = format!(
                    "Users/{user_id}/Items?ParentId={series_id}&Recursive=true\
                     &IncludeItemTypes=Episode&EnableImages=false&EnableUserData=false"
                );
                let v: serde_json::Value = self.request(reqwest::Method::GET, &path, None).await?;
                let Some(eps) = v.get("Items").and_then(|i| i.as_array()) else {
                    return Ok(None);
                };
                Ok(first_episode_match(eps, *season, *episode))
            }
        }
    }

    /// Write the synced UserData fields onto an item for a user.
    pub async fn apply_user_data(
        &self,
        user_id: &str,
        item_id: &str,
        ud: &ItemUserData,
    ) -> Result<(), Error> {
        let mut body = serde_json::json!({
            "Played": ud.played,
            "PlaybackPositionTicks": ud.playback_position_ticks,
            "PlayCount": ud.play_count,
            "IsFavorite": ud.is_favorite,
        });
        if let Some(d) = &ud.last_played_date {
            body["LastPlayedDate"] = serde_json::json!(d);
        }
        let path = format!("Users/{user_id}/Items/{item_id}/UserData");
        self.request_no_content(reqwest::Method::POST, &path, Some(&body))
            .await
    }

    /// Fetch a backend item's `MediaSources` (raw JSON), used to merge a peer
    /// backend's copy of a movie in as a selectable "version".
    pub async fn get_item_media_sources(
        &self,
        user_id: &str,
        item_id: &str,
    ) -> Result<Vec<serde_json::Value>, Error> {
        let path = format!("Users/{user_id}/Items/{item_id}?Fields=MediaSources");
        let v: serde_json::Value = self.request(reqwest::Method::GET, &path, None).await?;
        Ok(v.get("MediaSources")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// Mark an item played (`POST`) or unplayed (`DELETE`) for a user.
    pub async fn set_played(
        &self,
        user_id: &str,
        item_id: &str,
        played: bool,
    ) -> Result<(), Error> {
        let method = if played {
            reqwest::Method::POST
        } else {
            reqwest::Method::DELETE
        };
        let path = format!("Users/{user_id}/PlayedItems/{item_id}");
        self.request_no_content(method, &path, None).await
    }

    pub async fn get_media_folders(
        &self,
        user_id: Option<&str>,
    ) -> Result<Vec<crate::models::MediaFolder>, Error> {
        let path = if let Some(uid) = user_id {
            format!("Users/{}/Views", uid)
        } else {
            "Library/MediaFolders".to_string()
        };

        let response: MediaFoldersResponse =
            self.request(reqwest::Method::GET, &path, None).await?;
        Ok(response.items)
    }

    pub async fn get_public_system_info(&self) -> Result<crate::models::PublicSystemInfo, Error> {
        self.request(reqwest::Method::GET, "System/Info/Public", None)
            .await
    }

    pub async fn get_branding_configuration(
        &self,
    ) -> Result<crate::models::BrandingConfiguration, Error> {
        self.request(reqwest::Method::GET, "Branding/Configuration", None)
            .await
    }

    // Admin methods

    pub async fn get_users(&self) -> Result<Vec<User>, Error> {
        self.request(reqwest::Method::GET, "Users", None).await
    }

    pub async fn create_user(&self, username: &str, password: Option<&str>) -> Result<User, Error> {
        let body = json!({
            "Name": username,
            "Password": password
        });

        let user: User = self
            .request(reqwest::Method::POST, "Users/New", Some(&body))
            .await?;

        Ok(user)
    }

    pub async fn delete_user(&self, user_id: &str) -> Result<(), Error> {
        let path = format!("Users/{}", user_id);
        self.request_no_content(reqwest::Method::DELETE, &path, None)
            .await
    }

    /// Admin: set (reset) a user's password. The calling client must hold an
    /// admin token. Verified contract: `POST /Users/{id}/Password` with an empty
    /// `CurrentPw` succeeds for an administrator changing another user.
    pub async fn set_user_password(&self, user_id: &str, new_password: &str) -> Result<(), Error> {
        let body = json!({
            "CurrentPw": "",
            "NewPw": new_password,
            "ResetPassword": false
        });
        let path = format!("Users/{}/Password", user_id);
        self.request_no_content(reqwest::Method::POST, &path, Some(&body))
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn get_items(
        &self,
        user_id: &str,
        parent_id: Option<&str>,
        recursive: bool,
        include_item_types: Option<Vec<IncludeItemTypes>>,
        limit: Option<i32>,
        start_index: Option<i32>,
        sort_by: Option<String>,
        sort_order: Option<String>,
        include_fields: Option<Vec<IncludeBaseItemFields>>,
    ) -> Result<crate::models::ItemsResponse, Error> {
        let mut query = vec![
            ("Recursive", recursive.to_string()),
            //("Fields", "PrimaryImageAspectRatio,CanDelete,BasicSyncInfo,ProductionYear,RunTimeTicks,CommunityRating".to_string()),
        ];

        if let Some(include_fields) = include_fields {
            let fields_str = include_fields
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<String>>()
                .join(",");
            query.push(("Fields", fields_str));
        }

        if let Some(pid) = parent_id {
            query.push(("ParentId", pid.to_string()));
        }

        if let Some(types) = include_item_types {
            query.push((
                "IncludeItemTypes",
                types
                    .iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<String>>()
                    .join(","),
            ));
        }

        if let Some(l) = limit {
            query.push(("Limit", l.to_string()));
        }

        if let Some(si) = start_index {
            query.push(("StartIndex", si.to_string()));
        }

        if let Some(s) = sort_by {
            query.push(("SortBy", s));
        }

        if let Some(o) = sort_order {
            query.push(("SortOrder", o));
        }

        let path = format!("Users/{}/Items", user_id);
        let url = self.base_url.join(&path)?;

        let auth_header = self.build_auth_header().await;

        let response = self
            .http_client
            .get(url)
            .header(header::AUTHORIZATION, auth_header)
            .query(&query)
            .send()
            .await?;

        let status = response.status();

        if status.is_success() {
            let data = response.json::<crate::models::ItemsResponse>().await?;
            Ok(data)
        } else {
            match status {
                StatusCode::UNAUTHORIZED => Err(Error::Unauthorized),
                StatusCode::FORBIDDEN => Err(Error::Forbidden),
                StatusCode::NOT_FOUND => Err(Error::NotFound),
                _ => {
                    let text = response.text().await.unwrap_or_default();
                    Err(Error::ServerError(format!("{} - {}", status, text)))
                }
            }
        }
    }
}

/// Whether a provider key uniquely identifies a single item. Collection-type
/// keys (e.g. `TmdbCollection`) are shared by every movie in a franchise, so
/// matching on them would mark the wrong sibling — they're excluded.
fn is_identifying_provider_key(key: &str) -> bool {
    !key.to_lowercase().contains("collection")
}

/// First item in `items` whose `ProviderIds` share an *identifying* (key,value)
/// pair with `want` (keys compared case-insensitively, values exactly). Pure,
/// testable core of [`JellyfinClient::find_item_id_by_provider_ids`].
fn first_provider_match(
    items: &[serde_json::Value],
    want: &std::collections::HashSet<(String, String)>,
) -> Option<String> {
    for item in items {
        let Some(pids) = item.get("ProviderIds").and_then(|p| p.as_object()) else {
            continue;
        };
        let hit = pids.iter().any(|(k, val)| {
            is_identifying_provider_key(k)
                && val
                    .as_str()
                    .is_some_and(|v| want.contains(&(k.to_lowercase(), v.to_string())))
        });
        if hit {
            if let Some(id) = item.get("Id").and_then(|i| i.as_str()) {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// The subset of a user's per-item state synced across backends.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ItemUserData {
    pub played: bool,
    pub playback_position_ticks: i64,
    pub play_count: i64,
    pub is_favorite: bool,
    pub last_played_date: Option<String>,
}

impl ItemUserData {
    /// Parse from a Jellyfin item's `UserData` object.
    fn from_value(ud: Option<&serde_json::Value>) -> Self {
        let g = |k: &str| ud.and_then(|u| u.get(k));
        ItemUserData {
            played: g("Played").and_then(|v| v.as_bool()).unwrap_or(false),
            playback_position_ticks: g("PlaybackPositionTicks")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            play_count: g("PlayCount").and_then(|v| v.as_i64()).unwrap_or(0),
            is_favorite: g("IsFavorite").and_then(|v| v.as_bool()).unwrap_or(false),
            last_played_date: g("LastPlayedDate")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }
    }
}

/// How to find "the same title" on a peer backend. Movies and series match on
/// their identifying provider id; episodes match on their *series'* provider id
/// plus season+episode number (episode-level ids are too spotty to rely on).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MatchKey {
    Provider(Vec<(String, String)>),
    Episode {
        series: Vec<(String, String)>,
        season: i64,
        episode: i64,
    },
}

impl MatchKey {
    /// A stable string key (for caching / queue dedup), order-independent.
    pub fn cache_key(&self) -> String {
        match self {
            MatchKey::Provider(p) => provider_key_str(p),
            MatchKey::Episode {
                series,
                season,
                episode,
            } => format!("{}|s{season}e{episode}", provider_key_str(series)),
        }
    }
    /// No identifying ids at all → can't match anything cross-backend.
    pub fn is_empty(&self) -> bool {
        let p = match self {
            MatchKey::Provider(p) | MatchKey::Episode { series: p, .. } => p,
        };
        !p.iter().any(|(k, _)| is_identifying_provider_key(k))
    }
}

fn provider_key_str(p: &[(String, String)]) -> String {
    let mut v: Vec<String> = p.iter().map(|(k, val)| format!("{}.{val}", k.to_lowercase())).collect();
    v.sort();
    v.join(",")
}

fn providers_from(p: Option<&serde_json::Value>) -> Vec<(String, String)> {
    p.and_then(|p| p.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn identifying_want(p: &[(String, String)]) -> std::collections::HashSet<(String, String)> {
    p.iter()
        .filter(|(k, _)| is_identifying_provider_key(k))
        .map(|(k, v)| (k.to_lowercase(), v.clone()))
        .collect()
}

/// First episode in `items` matching `season`/`episode` number → its `Id`.
fn first_episode_match(items: &[serde_json::Value], season: i64, episode: i64) -> Option<String> {
    for item in items {
        let s = item.get("ParentIndexNumber").and_then(|n| n.as_i64());
        let e = item.get("IndexNumber").and_then(|n| n.as_i64());
        if s == Some(season) && e == Some(episode) {
            if let Some(id) = item.get("Id").and_then(|i| i.as_str()) {
                return Some(id.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;
    use wiremock::matchers::{method, path};

    fn want(pairs: &[(&str, &str)]) -> HashSet<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.to_string()))
            .collect()
    }

    #[test]
    fn identifying_key_excludes_collections() {
        assert!(is_identifying_provider_key("Imdb"));
        assert!(is_identifying_provider_key("Tmdb"));
        assert!(is_identifying_provider_key("Tvdb"));
        assert!(!is_identifying_provider_key("TmdbCollection"));
        assert!(!is_identifying_provider_key("tvdbcollection"));
    }

    #[test]
    fn matches_strong_id_not_collection_sibling() {
        // Source = "Iron Man". The peer lists the SIBLING "Iron Man 2" FIRST
        // (same TmdbCollection, different Imdb), then the correct "Iron Man".
        // Must pick the correct movie, never the collection sibling.
        let items = vec![
            json!({"Id":"sibling","ProviderIds":{"Imdb":"tt1228705","Tmdb":"10138","TmdbCollection":"131292"}}),
            json!({"Id":"correct","ProviderIds":{"Imdb":"tt0371746","Tmdb":"1726","TmdbCollection":"131292"}}),
        ];
        let w = want(&[
            ("Imdb", "tt0371746"),
            ("Tmdb", "1726"),
            ("TmdbCollection", "131292"),
        ]);
        assert_eq!(first_provider_match(&items, &w).as_deref(), Some("correct"));
    }

    #[test]
    fn collection_only_overlap_does_not_match() {
        // The only shared id is a collection -> refuse (would guess a sibling).
        let items = vec![
            json!({"Id":"sibling","ProviderIds":{"Imdb":"tt9999999","TmdbCollection":"131292"}}),
        ];
        assert_eq!(first_provider_match(&items, &want(&[("TmdbCollection", "131292")])), None);
        assert_eq!(
            first_provider_match(&items, &want(&[("Imdb", "tt0371746"), ("TmdbCollection", "131292")])),
            None
        );
    }

    #[test]
    fn exact_match_and_miss() {
        let items = vec![
            json!({"Id":"a","ProviderIds":{"Imdb":"tt1"}}),
            json!({"Id":"b","ProviderIds":{"Tmdb":"22"}}),
        ];
        assert_eq!(first_provider_match(&items, &want(&[("Tmdb", "22")])).as_deref(), Some("b"));
        // case-insensitive key
        assert_eq!(first_provider_match(&items, &want(&[("tmdb", "22")])).as_deref(), Some("b"));
        assert_eq!(first_provider_match(&items, &want(&[("Imdb", "nope")])), None);
    }

    #[test]
    fn tolerates_malformed_items() {
        let items = vec![
            json!({"Id":"noproviders"}),
            json!({"ProviderIds":{"Imdb":"tt1"}}),            // matches but has no Id
            json!({"Id":"nullval","ProviderIds":{"Imdb":null,"Tmdb":5}}), // non-string values
            json!("not even an object"),
            json!({"Id":"good","ProviderIds":{"Imdb":"tt7"}}),
        ];
        assert_eq!(first_provider_match(&items, &want(&[("Imdb", "tt7")])).as_deref(), Some("good"));
        // a matching item with no Id is skipped, nothing else matches tt1 -> None
        assert_eq!(first_provider_match(&items, &want(&[("Imdb", "tt1")])), None);
        // empty input
        assert_eq!(first_provider_match(&[], &want(&[("Imdb", "tt7")])), None);
    }
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_authenticate_success() {
        let mock_server = MockServer::start().await;

        let auth_response = json!({
            "AccessToken": "test_token",
            "User": {
                "Id": "user_id",
                "Name": "test_user",
                "ServerId": "server_id"
            }
        });

        Mock::given(method("POST"))
            .and(path("/Users/AuthenticateByName"))
            .respond_with(ResponseTemplate::new(200).set_body_json(auth_response))
            .mount(&mock_server)
            .await;

        let client_info = ClientInfo::default();
        let client = JellyfinClient::new(&mock_server.uri(), client_info).unwrap();

        let user = client
            .authenticate_by_name("test_user", "password")
            .await
            .unwrap();

        assert_eq!(user.name, "test_user");
        assert_eq!(client.get_token().await.as_deref(), Some("test_token"));
    }

    #[tokio::test]
    async fn test_get_media_folders() {
        let mock_server = MockServer::start().await;

        let folders_response = json!({
            "Items": [
                {
                    "Name": "Movies",
                    "CollectionType": "movies",
                    "Id": "folder_1"
                }
            ]
        });

        Mock::given(method("GET"))
            .and(path("/Library/MediaFolders"))
            //.and(header("Authorization", "MediaBrowser Client=\"Jellyfin API Client\", Device=\"Unknown\", DeviceId=\"unknown-device-id\", Version=\"0.0.0\", Token=\"test_token\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(folders_response))
            .mount(&mock_server)
            .await;

        let client_info = ClientInfo::default();
        let client = JellyfinClient::new(&mock_server.uri(), client_info).unwrap();
        let client = client.with_token("test_token".to_string()).await;

        let folders = client.get_media_folders(None).await.unwrap();

        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "Movies");
    }

    #[tokio::test]
    async fn test_get_branding_configuration() {
        let mock_server = MockServer::start().await;

        let branding_response = json!({
            "LoginDisclaimer": "Welcome to Jellyfin",
            "CustomCss": "body { background: black; }",
            "SplashscreenEnabled": true
        });

        Mock::given(method("GET"))
            .and(path("/Branding/Configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(branding_response))
            .mount(&mock_server)
            .await;

        let client_info = ClientInfo::default();
        let client = JellyfinClient::new(&mock_server.uri(), client_info).unwrap();

        let config = client.get_branding_configuration().await.unwrap();

        assert_eq!(
            config.login_disclaimer,
            Some("Welcome to Jellyfin".to_string())
        );
        assert_eq!(
            config.custom_css,
            Some("body { background: black; }".to_string())
        );
        assert_eq!(config.splashscreen_enabled, Some(true));
    }
}
