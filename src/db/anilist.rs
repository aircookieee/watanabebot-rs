//! AniList account links and cached list data.
//! OWNER: AniList agent. Add `impl Db` query methods here.

use rusqlite::Connection;

use crate::config::Config;

#[allow(unused_imports)]
use super::Db;

pub(super) fn migrate(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS discord_anilist_map (
            discord_id TEXT PRIMARY KEY,
            anilist_username TEXT NOT NULL,
            created_at TEXT DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE IF NOT EXISTS user_media_lists (
            discord_id TEXT NOT NULL,
            media_id INTEGER NOT NULL,
            media_type TEXT NOT NULL,
            entry_data TEXT NOT NULL,
            updated_at TEXT DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (discord_id, media_id)
        );
        CREATE TABLE IF NOT EXISTS user_favorites (
            discord_id TEXT NOT NULL,
            media_id INTEGER NOT NULL,
            updated_at TEXT DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (discord_id, media_id)
        );
        CREATE TABLE IF NOT EXISTS anilist_refresh_log (
            discord_id TEXT PRIMARY KEY,
            last_lists_refresh TEXT,
            last_favorites_refresh TEXT
        );
        ",
    )?;
    Ok(())
}

/// Imports `discordAniListMap.json` (legacy `{ discordId: anilistUsername }`) with INSERT OR IGNORE.
/// Looks at `LEGACY_ANILIST_MAP_PATH`, then `./discordAniListMap.json`.
pub(super) fn import_legacy_map(conn: &Connection, cfg: &Config) -> anyhow::Result<()> {
    let candidates = cfg
        .legacy_anilist_map_path
        .iter()
        .cloned()
        .chain(std::iter::once(std::path::PathBuf::from("discordAniListMap.json")));
    for path in candidates {
        if !path.exists() {
            continue;
        }
        let raw = std::fs::read_to_string(&path)?;
        let map: std::collections::HashMap<String, serde_json::Value> = match serde_json::from_str(&raw) {
            Ok(m) => m,
            Err(err) => {
                tracing::error!("Failed to parse legacy AniList map {}: {err}", path.display());
                return Ok(());
            }
        };
        let mut stmt =
            conn.prepare("INSERT OR IGNORE INTO discord_anilist_map (discord_id, anilist_username) VALUES (?, ?)")?;
        let mut count = 0;
        for (discord_id, username) in map {
            if let Some(username) = username.as_str() {
                count += stmt.execute([discord_id.as_str(), username])?;
            }
        }
        tracing::info!("Imported {count} new legacy AniList mappings from {}", path.display());
        return Ok(());
    }
    Ok(())
}
