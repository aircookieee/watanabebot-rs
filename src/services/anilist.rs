//! AniList GraphQL client, rate limiting, list caching and score aggregation.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;

use crate::config;
use crate::db::anilist::RefreshKind;
use crate::db::Db;

const API_URL: &str = "https://graphql.anilist.co";
const REFRESH_INTERVAL: Duration = Duration::from_secs(3 * 60 * 60);
const COMMAND_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const MEDIA_CACHE_TTL_SECS: i64 = 24 * 60 * 60;
const TARGET_REQUESTS_PER_MINUTE: u64 = 25;
/// ceil(60000 / 25)
const BASE_DELAY_MS: u64 = (60_000 + TARGET_REQUESTS_PER_MINUTE - 1) / TARGET_REQUESTS_PER_MINUTE;
const DELAY_PER_USER_MS: u64 = 100;
const MAX_DELAY_MS: u64 = 10_000;
const PREWARM_DELAY: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Anime,
    Manga,
}

impl MediaType {
    /// AniList `MediaType` enum value, also what is stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            MediaType::Anime => "ANIME",
            MediaType::Manga => "MANGA",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MediaType::Anime => "Anime",
            MediaType::Manga => "Manga",
        }
    }

    pub fn lower(self) -> &'static str {
        match self {
            MediaType::Anime => "anime",
            MediaType::Manga => "manga",
        }
    }
}

/// Minimal per-user list entry as stored in `user_media_lists.entry_data`
/// (same JSON shape the TypeScript bot wrote).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserEntry {
    pub media_id: i64,
    pub media_type: String,
    pub status: String,
    #[serde(default)]
    pub score: i64,
    #[serde(default)]
    pub progress: i64,
    #[serde(default)]
    pub repeat: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaTitle {
    pub romaji: Option<String>,
    pub english: Option<String>,
    pub native: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CoverImage {
    pub large: Option<String>,
    pub extra_large: Option<String>,
}

/// Result of an AniList media search.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Media {
    pub id: i64,
    pub site_url: String,
    pub title: MediaTitle,
    pub description: Option<String>,
    pub mean_score: Option<i64>,
    pub cover_image: CoverImage,
}

/// One registered user's relation to a media item.
#[derive(Debug, Clone)]
pub struct AnimeMatch {
    pub ani_username: String,
    pub score: i64,
    pub progress: i64,
    pub status: String,
    pub repeat: i64,
    pub is_favorite: bool,
}

#[derive(Debug, Clone)]
pub struct AnimeInfo {
    pub resolved_title: String,
    pub description: String,
    pub anilist_url: String,
    pub score: i64,
    pub cover_image: String,
    pub matches: Vec<AnimeMatch>,
}

/// Raw HTTP reply, kept whole so failures can be dumped for debugging.
struct Raw {
    status: u16,
    status_text: String,
    headers: Map<String, Value>,
    url: String,
    body: String,
}

/// While alive, `Anilist::update_in_progress` is true.
pub struct UpdateGuard<'a>(&'a AtomicUsize);

impl Drop for UpdateGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct Anilist {
    db: Db,
    client: reqwest::Client,
    /// Start time of the previous API request; the lock also serializes requests.
    last_request: Mutex<Option<Instant>>,
    updates_running: AtomicUsize,
}

impl Anilist {
    pub fn new(db: Db, client: reqwest::Client) -> Self {
        Anilist { db, client, last_request: Mutex::new(None), updates_running: AtomicUsize::new(0) }
    }

    /// True while a bulk refresh (pre-warm, scheduled or manual update) is running.
    pub fn update_in_progress(&self) -> bool {
        self.updates_running.load(Ordering::SeqCst) > 0
    }

    pub fn begin_update(&self) -> UpdateGuard<'_> {
        self.updates_running.fetch_add(1, Ordering::SeqCst);
        UpdateGuard(&self.updates_running)
    }

    /// Minimum gap between requests: 25 req/min plus 100ms per registered user, capped at 10s.
    fn rate_limit_delay(&self) -> Duration {
        let users = self.db.anilist_user_count().unwrap_or(0).max(0) as u64;
        Duration::from_millis((BASE_DELAY_MS + users * DELAY_PER_USER_MS).min(MAX_DELAY_MS))
    }

    // ---- HTTP ----

    async fn post(&self, query: &str, variables: Value) -> anyhow::Result<Raw> {
        let resp = {
            let mut last = self.last_request.lock().await;
            let delay = self.rate_limit_delay();
            if let Some(prev) = *last {
                let elapsed = prev.elapsed();
                if elapsed < delay {
                    tokio::time::sleep(delay - elapsed).await;
                }
            }
            *last = Some(Instant::now());
            self.client
                .post(API_URL)
                .header("Accept", "application/json")
                .json(&json!({ "query": query, "variables": variables }))
                .send()
                .await
        }?;

        let status = resp.status();
        let mut headers = Map::new();
        for (name, value) in resp.headers() {
            headers.insert(name.to_string(), Value::String(value.to_str().unwrap_or_default().to_string()));
        }
        let url = resp.url().to_string();
        let body = resp.text().await.unwrap_or_else(|e| format!("Failed to read body: {e}"));
        Ok(Raw {
            status: status.as_u16(),
            status_text: status.canonical_reason().unwrap_or("").to_string(),
            headers,
            url,
            body,
        })
    }

    /// Writes the failed exchange to `<data_dir>/anilist_failures/<prefix>_<timestamp>_<n>.json`.
    fn dump_failure(&self, raw: &Raw, context: &Value, prefix: &str, body: Value) {
        let result = (|| -> anyhow::Result<()> {
            let dir = config::get().paths.data_dir.join("anilist_failures");
            std::fs::create_dir_all(&dir)?;
            let stamp = Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ");
            let file = dir.join(format!("{prefix}_{stamp}_{}.json", rand::random::<u32>() % 100_000));
            let dump = json!({
                "status": raw.status,
                "statusText": raw.status_text,
                "headers": raw.headers,
                "url": raw.url,
                "request": context,
                "body": body,
            });
            std::fs::write(file, serde_json::to_string_pretty(&dump)?)?;
            Ok(())
        })();
        if let Err(err) = result {
            tracing::error!("Failed to dump Anilist response ({prefix}): {err}");
        }
    }

    /// Validates an HTTP + GraphQL reply, dumping the exchange on failure.
    fn parse_checked(&self, raw: &Raw, context: Value, prefix: &str, graphql_prefix: &str) -> anyhow::Result<Value> {
        if !(200..300).contains(&raw.status) {
            self.dump_failure(raw, &context, prefix, Value::String(raw.body.clone()));
            bail!("Anilist API error: {}", raw.status);
        }
        let json: Value = serde_json::from_str(&raw.body)?;
        if json.get("errors").is_some_and(|e| !e.is_null()) {
            self.dump_failure(raw, &context, graphql_prefix, json.clone());
            let message = json["errors"][0]["message"].as_str().unwrap_or("unknown error");
            bail!("GraphQL error: {message}");
        }
        Ok(json)
    }

    async fn fetch_lists(&self, username: &str, media_type: MediaType) -> anyhow::Result<Vec<UserEntry>> {
        const QUERY: &str = "
        query ($username: String, $type: MediaType) {
            MediaListCollection(userName: $username, type: $type) {
                lists {
                    name
                    isCustomList
                    isSplitCompletedList
                    entries {
                        media {
                            id
                            type
                            title {
                                romaji
                                english
                                native
                            }
                            status
                            description(asHtml: false)
                            coverImage {
                                large
                                extraLarge
                            }
                        }
                        status
                        score(format: POINT_100)
                        progress
                        repeat
                    }
                }
            }
        }
    ";
        let raw = self.post(QUERY, json!({ "username": username, "type": media_type.as_str() })).await?;
        let context = json!({ "username": username, "mediaType": media_type.as_str(), "query": QUERY });
        let json = self.parse_checked(&raw, context, "fail", "graphql_fail")?;

        let lists = json["data"]["MediaListCollection"]["lists"]
            .as_array()
            .ok_or_else(|| anyhow!("Unexpected Anilist response: missing MediaListCollection.lists"))?;
        let mut entries = Vec::new();
        for list in lists {
            for entry in list["entries"].as_array().into_iter().flatten() {
                let Some(media_id) = entry["media"]["id"].as_i64() else { continue };
                entries.push(UserEntry {
                    media_id,
                    media_type: media_type.as_str().to_string(),
                    status: entry["status"].as_str().unwrap_or_default().to_string(),
                    score: json_int(&entry["score"]),
                    progress: json_int(&entry["progress"]),
                    repeat: json_int(&entry["repeat"]),
                });
            }
        }
        Ok(entries)
    }

    async fn fetch_favorites(&self, username: &str) -> anyhow::Result<Vec<i64>> {
        const QUERY: &str = "
        query ($username: String) {
            User(name: $username) {
                favourites {
                    anime {
                        nodes {
                            id
                        }
                    }
                    manga {
                        nodes {
                            id
                        }
                    }
                }
            }
        }
    ";
        let raw = self.post(QUERY, json!({ "username": username })).await?;
        let context = json!({ "username": username, "query": QUERY });
        let json = self.parse_checked(&raw, context, "fail_favs", "graphql_fail_favs")?;

        let mut ids = Vec::new();
        for kind in ["anime", "manga"] {
            for node in json["data"]["User"]["favourites"][kind]["nodes"].as_array().into_iter().flatten() {
                if let Some(id) = node["id"].as_i64() {
                    ids.push(id);
                }
            }
        }
        Ok(ids)
    }

    // ---- media search ----

    /// Looks up a media item by AniList id (all digits) or by title search.
    /// Results are cached for 24 hours. `Ok(None)` means nothing was found.
    pub async fn search_media(&self, input: &str, media_type: MediaType) -> anyhow::Result<Option<Media>> {
        if let Ok(Some(cached)) = self.db.anilist_get_cached_media(input, media_type.as_str(), MEDIA_CACHE_TTL_SECS) {
            if let Ok(media) = serde_json::from_str::<Media>(&cached) {
                return Ok(Some(media));
            }
        }

        const FIELDS: &str = "
                    id
                    type
                    siteUrl
                    title {
                        romaji
                        english
                        native
                    }
                    description(asHtml: false)
                    meanScore
                    coverImage {
                        large
                        extraLarge
                    }
    ";
        let is_id = !input.is_empty() && input.chars().all(|c| c.is_ascii_digit());
        let (query, variables) = if is_id {
            (
                format!("query ($id: Int, $type: MediaType) {{ Media(id: $id, type: $type) {{ {FIELDS} }} }}"),
                json!({ "id": input.parse::<i64>().map_err(|_| anyhow!("Invalid Anilist id"))?, "type": media_type.as_str() }),
            )
        } else {
            (
                format!("query ($search: String, $type: MediaType) {{ Media(search: $search, type: $type) {{ {FIELDS} }} }}"),
                json!({ "search": input, "type": media_type.as_str() }),
            )
        };

        let raw = self.post(&query, variables).await?;
        // AniList answers "Not Found." with HTTP 404 when a search matches nothing.
        if raw.status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&raw.status) {
            bail!("Anilist API error: {}", raw.status);
        }
        let json: Value = serde_json::from_str(&raw.body)?;
        let media = &json["data"]["Media"];
        if media.is_null() {
            return Ok(None);
        }
        let parsed: Media = serde_json::from_value(media.clone())?;
        if let Err(err) =
            self.db.anilist_set_cached_media(input, media_type.as_str(), &media.to_string(), MEDIA_CACHE_TTL_SECS)
        {
            tracing::warn!("Failed to cache Anilist media: {err:#}");
        }
        Ok(Some(parsed))
    }

    // ---- registration ----

    /// Links `anilist_username` to the Discord user and loads their lists.
    /// Returns false if the username already belongs to another Discord user.
    pub async fn register_user(&self, discord_id: &str, anilist_username: &str) -> anyhow::Result<bool> {
        if !self.db.anilist_register_user(discord_id, anilist_username)? {
            return Ok(false);
        }
        self.get_user_lists(discord_id, true, None).await;
        Ok(true)
    }

    pub fn unregister_user(&self, discord_id: &str) -> anyhow::Result<bool> {
        self.db.anilist_unregister_user(discord_id)
    }

    // ---- user data ----

    fn stored_entries(&self, discord_id: &str, media_type: Option<MediaType>) -> Vec<UserEntry> {
        match self.db.anilist_user_media_list(discord_id, media_type.map(MediaType::as_str)) {
            Ok(rows) => rows.iter().filter_map(|r| serde_json::from_str(&r.entry_data).ok()).collect(),
            Err(err) => {
                tracing::error!("Failed to read stored Anilist lists: {err:#}");
                Vec::new()
            }
        }
    }

    fn stored_favorites(&self, discord_id: &str) -> Vec<i64> {
        self.db.anilist_user_favorites(discord_id).unwrap_or_else(|err| {
            tracing::error!("Failed to read stored Anilist favorites: {err:#}");
            Vec::new()
        })
    }

    fn last_refresh(&self, discord_id: &str) -> crate::db::anilist::RefreshTimes {
        self.db.anilist_last_refresh(discord_id).ok().flatten().unwrap_or_default()
    }

    /// A user's list entries. Uses stored data unless it is older than 3 hours (or
    /// `force_refresh`); falls back to stored data if AniList cannot be reached.
    pub async fn get_user_lists(
        &self,
        discord_id: &str,
        force_refresh: bool,
        media_type: Option<MediaType>,
    ) -> Vec<UserEntry> {
        let Ok(Some(username)) = self.db.anilist_username(discord_id) else {
            return Vec::new();
        };

        let stale = is_stale(self.last_refresh(discord_id).lists.as_deref(), REFRESH_INTERVAL);
        if !force_refresh && !stale {
            let stored = self.stored_entries(discord_id, media_type);
            if !stored.is_empty() {
                return stored;
            }
        }

        match self.refresh_lists(discord_id, &username, media_type).await {
            Ok(entries) => entries,
            Err(err) => {
                tracing::error!("Failed to fetch lists for {username}: {err:#}");
                self.stored_entries(discord_id, media_type)
            }
        }
    }

    async fn refresh_lists(
        &self,
        discord_id: &str,
        username: &str,
        media_type: Option<MediaType>,
    ) -> anyhow::Result<Vec<UserEntry>> {
        let types: &[MediaType] = match media_type {
            Some(MediaType::Anime) => &[MediaType::Anime],
            Some(MediaType::Manga) => &[MediaType::Manga],
            None => &[MediaType::Anime, MediaType::Manga],
        };
        let mut all = Vec::new();
        for &t in types {
            let entries = self.fetch_lists(username, t).await?;
            tracing::info!("Fetched {} lists for {username}, {} entries", t.lower(), entries.len());
            let rows = entries
                .iter()
                .map(|e| Ok((e.media_id, e.media_type.clone(), serde_json::to_string(e)?)))
                .collect::<anyhow::Result<Vec<_>>>()?;
            self.db.anilist_save_user_media_batch(discord_id, &rows)?;
            all.extend(entries);
        }
        self.db.anilist_update_refresh_log(discord_id, RefreshKind::Lists, &now_iso())?;
        Ok(all)
    }

    /// A user's favorite media ids (anime and manga). Same staleness rules as lists.
    pub async fn get_user_favorites(&self, discord_id: &str, force_refresh: bool) -> Vec<i64> {
        let Ok(Some(username)) = self.db.anilist_username(discord_id) else {
            return Vec::new();
        };

        let stale = is_stale(self.last_refresh(discord_id).favorites.as_deref(), REFRESH_INTERVAL);
        if !force_refresh && !stale {
            let stored = self.stored_favorites(discord_id);
            if !stored.is_empty() {
                return stored;
            }
        }

        let result: anyhow::Result<Vec<i64>> = async {
            let favs = self.fetch_favorites(&username).await?;
            tracing::info!("Fetched favorites for {username}, {} entries", favs.len());
            self.db.anilist_save_user_favorites(discord_id, &favs)?;
            self.db.anilist_update_refresh_log(discord_id, RefreshKind::Favorites, &now_iso())?;
            Ok(favs)
        }
        .await;
        match result {
            Ok(favs) => favs,
            Err(err) => {
                tracing::error!("Failed to fetch favorites for {username}: {err:#}");
                self.stored_favorites(discord_id)
            }
        }
    }

    /// Looks up a media item and how every registered user relates to it.
    /// If `requester_discord_id` is given, their lists are refreshed first when older than 5 minutes.
    pub async fn get_anime_info_with_scores(
        &self,
        search_input: &str,
        media_type: MediaType,
        requester_discord_id: Option<&str>,
    ) -> anyhow::Result<Option<AnimeInfo>> {
        let Some(media) = self.search_media(search_input, media_type).await? else {
            return Ok(None);
        };

        let resolved_title = first_non_empty([
            media.title.english.as_deref(),
            media.title.romaji.as_deref(),
            media.title.native.as_deref(),
        ])
        .unwrap_or(search_input)
        .to_string();
        let description = clean_description(media.description.as_deref());
        let cover_image =
            first_non_empty([media.cover_image.extra_large.as_deref(), media.cover_image.large.as_deref()])
                .unwrap_or_default()
                .to_string();

        if let Some(requester) = requester_discord_id {
            if is_stale(self.last_refresh(requester).lists.as_deref(), COMMAND_REFRESH_INTERVAL) {
                self.get_user_lists(requester, true, Some(media_type)).await;
            }
        }

        let mut matches = Vec::new();
        for (discord_id, ani_username) in self.db.all_anilist_mappings()? {
            let found = self
                .stored_entries(&discord_id, Some(media_type))
                .into_iter()
                .find(|e| e.media_id == media.id && e.media_type == media_type.as_str());
            matches.push(match found {
                Some(entry) => AnimeMatch {
                    is_favorite: self.stored_favorites(&discord_id).contains(&media.id),
                    ani_username,
                    score: entry.score,
                    progress: entry.progress,
                    status: entry.status,
                    repeat: entry.repeat,
                },
                None => AnimeMatch {
                    ani_username,
                    score: 0,
                    progress: 0,
                    status: "NOT_ON_LIST".to_string(),
                    repeat: 0,
                    is_favorite: false,
                },
            });
        }

        Ok(Some(AnimeInfo {
            resolved_title,
            description,
            anilist_url: media.site_url,
            score: media.mean_score.unwrap_or(0),
            cover_image,
            matches,
        }))
    }

    /// Force-refreshes lists and favorites of every registered user, one user at a time.
    pub async fn update_all_user_data(&self) -> anyhow::Result<()> {
        let _guard = self.begin_update();
        let mappings = self.db.all_anilist_mappings()?;
        let delay = self.rate_limit_delay();
        for (discord_id, _) in mappings {
            self.get_user_lists(&discord_id, true, None).await;
            self.get_user_favorites(&discord_id, true).await;
            tokio::time::sleep(delay).await;
        }
        Ok(())
    }

    /// Startup warm-up: refreshes every user's data, waiting 4 seconds between users.
    pub async fn prewarm(&self) {
        let mappings = match self.db.all_anilist_mappings() {
            Ok(m) => m,
            Err(err) => {
                tracing::error!("Anilist pre-warm failed to read users: {err:#}");
                return;
            }
        };
        if mappings.is_empty() {
            return;
        }
        let _guard = self.begin_update();
        tracing::info!("Pre-warming Anilist cache for {} users...", mappings.len());
        for (discord_id, _) in mappings {
            self.get_user_lists(&discord_id, true, None).await;
            self.get_user_favorites(&discord_id, true).await;
            tokio::time::sleep(PREWARM_DELAY).await;
        }
        tracing::info!("Anilist cache pre-warmed");
    }
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// True if there is no (parseable) timestamp or it is older than `max_age`.
fn is_stale(timestamp: Option<&str>, max_age: Duration) -> bool {
    let Some(ts) = timestamp else { return true };
    let Ok(parsed) = DateTime::parse_from_rfc3339(ts) else { return true };
    Utc::now().signed_duration_since(parsed).num_milliseconds() > max_age.as_millis() as i64
}

fn json_int(v: &Value) -> i64 {
    v.as_i64().or_else(|| v.as_f64().map(|f| f.round() as i64)).unwrap_or(0)
}

fn first_non_empty<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> Option<&'a str> {
    values.into_iter().flatten().find(|s| !s.is_empty())
}

static RE_BREAKS: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?:<br>|\n)+").unwrap());
static RE_BOLD: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)<b>(.*?)</b>").unwrap());
static RE_ITALIC: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)<i>(.*?)</i>").unwrap());
static RE_B_TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)</?b>").unwrap());
static RE_I_TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)</?i>").unwrap());

/// Converts AniList's HTML-ish description to Discord markdown.
fn clean_description(description: Option<&str>) -> String {
    let Some(text) = description else {
        return "No description available.".to_string();
    };
    let text = RE_BREAKS.replace_all(text, "\n");
    let text = RE_BOLD.replace_all(&text, "**${1}**");
    let text = RE_ITALIC.replace_all(&text, "*${1}*");
    let text = RE_B_TAG.replace_all(&text, "");
    let text = RE_I_TAG.replace_all(&text, "");
    if text.is_empty() {
        "No description available.".to_string()
    } else {
        text.into_owned()
    }
}
