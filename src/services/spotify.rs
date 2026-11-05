//! Spotify client-credentials client for the daily Love Live song.

use std::time::{Duration, Instant};

use anyhow::{bail, Context as _};
use rand::seq::SliceRandom;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::config;

const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const PAGE_SIZE: usize = 100;

#[derive(Debug, Clone)]
pub struct SpotifyTrack {
    pub title: String,
    pub link: String,
    #[allow(dead_code)]
    pub uri: String,
    pub album_cover: String,
    pub artists: String,
}

pub struct Spotify {
    client: reqwest::Client,
    /// Access token and the moment it must be renewed.
    token: Mutex<Option<(String, Instant)>>,
}

impl Spotify {
    pub fn new(client: reqwest::Client) -> Self {
        Spotify { client, token: Mutex::new(None) }
    }

    /// True if client credentials and a playlist id are configured.
    pub fn is_configured(&self) -> bool {
        let cfg = &config::get().spotify;
        !cfg.client_id.is_empty() && !cfg.client_secret.is_empty() && !cfg.playlist_id.is_empty()
    }

    async fn access_token(&self) -> anyhow::Result<String> {
        let mut guard = self.token.lock().await;
        if let Some((token, expiry)) = guard.as_ref() {
            if Instant::now() < *expiry {
                return Ok(token.clone());
            }
        }

        let cfg = &config::get().spotify;
        let result: anyhow::Result<(String, u64)> = async {
            let response = self
                .client
                .post(TOKEN_URL)
                .basic_auth(&cfg.client_id, Some(&cfg.client_secret))
                .form(&[("grant_type", "client_credentials")])
                .send()
                .await?;
            if !response.status().is_success() {
                bail!("token request failed with status {}", response.status().as_u16());
            }
            let body: Value = response.json().await?;
            let token = body["access_token"].as_str().context("token response has no access_token")?.to_string();
            let expires_in = body["expires_in"].as_u64().unwrap_or(3600);
            Ok((token, expires_in))
        }
        .await;

        match result {
            Ok((token, expires_in)) => {
                tracing::info!("Spotify access token retrieved");
                let expiry = Instant::now() + Duration::from_secs(expires_in.saturating_sub(60));
                *guard = Some((token.clone(), expiry));
                Ok(token)
            }
            Err(err) => {
                tracing::error!("Failed to authenticate with Spotify: {err:#}");
                Err(err)
            }
        }
    }

    /// All tracks of a playlist (defaults to the configured one). Authentication failures
    /// are errors; a failure while paging is logged and the tracks fetched so far are returned.
    pub async fn get_playlist_tracks(&self, playlist_id: Option<&str>) -> anyhow::Result<Vec<SpotifyTrack>> {
        let token = self.access_token().await?;
        let id = playlist_id.unwrap_or(config::get().spotify.playlist_id.as_str());

        let mut tracks = Vec::new();
        if let Err(err) = self.fetch_pages(&token, id, &mut tracks).await {
            tracing::error!("Failed to fetch tracks from Spotify: {err:#}");
        }
        Ok(tracks)
    }

    async fn fetch_pages(&self, token: &str, playlist_id: &str, out: &mut Vec<SpotifyTrack>) -> anyhow::Result<()> {
        let mut offset = 0usize;
        loop {
            let response = self
                .client
                .get(format!("https://api.spotify.com/v1/playlists/{playlist_id}/tracks"))
                .bearer_auth(token)
                .query(&[
                    ("offset", offset.to_string()),
                    ("limit", PAGE_SIZE.to_string()),
                    (
                        "fields",
                        "items.track.name,items.track.uri,items.track.external_urls.spotify,items.track.album.images,items.track.artists,total"
                            .to_string(),
                    ),
                ])
                .send()
                .await?;
            if !response.status().is_success() {
                bail!("playlist request failed with status {}", response.status().as_u16());
            }
            let body: Value = response.json().await?;
            let items = body["items"].as_array().cloned().unwrap_or_default();
            let total = body["total"].as_u64().unwrap_or(0) as usize;

            for item in &items {
                let track = &item["track"];
                if track.is_null() {
                    continue;
                }
                let artists = track["artists"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default();
                out.push(SpotifyTrack {
                    title: track["name"].as_str().unwrap_or_default().to_string(),
                    link: track["external_urls"]["spotify"].as_str().unwrap_or_default().to_string(),
                    uri: track["uri"].as_str().unwrap_or_default().to_string(),
                    album_cover: track["album"]["images"][0]["url"].as_str().unwrap_or_default().to_string(),
                    artists,
                });
            }

            offset += PAGE_SIZE;
            // Stop at the end of the playlist (null entries make `out.len()` fall short of `total`).
            if items.is_empty() || offset >= total {
                return Ok(());
            }
        }
    }
}

pub fn get_random_track(tracks: &[SpotifyTrack]) -> Option<&SpotifyTrack> {
    tracks.choose(&mut rand::thread_rng())
}

pub struct TrackLinks {
    pub full_name: String,
    pub apple_music_url: String,
    pub youtube_music_url: String,
}

pub fn format_track_for_embed(track: &SpotifyTrack) -> TrackLinks {
    let full_name = format!("{} - {}", track.artists, track.title);
    TrackLinks {
        apple_music_url: full_name.replace(' ', "%20"),
        youtube_music_url: full_name.replace(' ', "+"),
        full_name,
    }
}
