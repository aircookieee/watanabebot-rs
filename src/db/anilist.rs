//! AniList account links and cached list data.

use rusqlite::{params, Connection, OptionalExtension};

use crate::config::Config;

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
        CREATE TABLE IF NOT EXISTS anilist_media_cache (
            query_lower TEXT NOT NULL,
            media_type TEXT NOT NULL,
            data TEXT NOT NULL,
            cached_at INTEGER NOT NULL,
            PRIMARY KEY (query_lower, media_type)
        );
        ",
    )?;
    Ok(())
}

/// One row of `user_media_lists`.
#[derive(Debug, Clone)]
pub struct MediaListRow {
    pub entry_data: String,
}

/// Last time a user's lists / favorites were refreshed (RFC 3339 strings).
#[derive(Debug, Clone, Default)]
pub struct RefreshTimes {
    pub lists: Option<String>,
    pub favorites: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshKind {
    Lists,
    Favorites,
}

impl Db {
    /// Links an AniList username to a Discord user. Returns false if the username is
    /// already linked to a different Discord user.
    pub fn anilist_register_user(&self, discord_id: &str, anilist_username: &str) -> anyhow::Result<bool> {
        self.tx(|tx| {
            let owner: Option<String> = tx
                .query_row(
                    "SELECT discord_id FROM discord_anilist_map WHERE anilist_username = ?",
                    [anilist_username],
                    |r| r.get(0),
                )
                .optional()?;
            if owner.is_some_and(|o| o != discord_id) {
                return Ok(false);
            }
            let exists = tx
                .prepare("SELECT 1 FROM discord_anilist_map WHERE discord_id = ?")?
                .exists([discord_id])?;
            if exists {
                tx.execute(
                    "UPDATE discord_anilist_map SET anilist_username = ?, updated_at = CURRENT_TIMESTAMP WHERE discord_id = ?",
                    [anilist_username, discord_id],
                )?;
            } else {
                tx.execute(
                    "INSERT INTO discord_anilist_map (discord_id, anilist_username) VALUES (?, ?)",
                    [discord_id, anilist_username],
                )?;
            }
            Ok(true)
        })
    }

    /// Removes a link. Returns false if the user was not registered.
    pub fn anilist_unregister_user(&self, discord_id: &str) -> anyhow::Result<bool> {
        let deleted = self
            .conn()
            .execute("DELETE FROM discord_anilist_map WHERE discord_id = ?", [discord_id])?;
        Ok(deleted > 0)
    }

    pub fn anilist_username(&self, discord_id: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT anilist_username FROM discord_anilist_map WHERE discord_id = ?",
                [discord_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// All `(discord_id, anilist_username)` pairs in registration order.
    pub fn all_anilist_mappings(&self) -> anyhow::Result<Vec<(String, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT discord_id, anilist_username FROM discord_anilist_map ORDER BY rowid")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn anilist_user_count(&self) -> anyhow::Result<i64> {
        Ok(self
            .conn()
            .query_row("SELECT COUNT(*) FROM discord_anilist_map", [], |r| r.get(0))?)
    }

    /// Replaces a user's stored list of one media type with `entries`
    /// (`(media_id, media_type, entry_data)`). An empty batch changes nothing.
    pub fn anilist_save_user_media_batch(
        &self,
        discord_id: &str,
        entries: &[(i64, String, String)],
    ) -> anyhow::Result<()> {
        let Some(first) = entries.first() else {
            return Ok(());
        };
        let list_type = first.1.clone();
        self.tx(|tx| {
            tx.execute(
                "DELETE FROM user_media_lists WHERE discord_id = ? AND media_type = ?",
                params![discord_id, list_type],
            )?;
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO user_media_lists (discord_id, media_id, media_type, entry_data, updated_at)
                 VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP)",
            )?;
            for (media_id, media_type, entry_data) in entries {
                stmt.execute(params![discord_id, media_id, media_type, entry_data])?;
            }
            Ok(())
        })
    }

    pub fn anilist_user_media_list(
        &self,
        discord_id: &str,
        media_type: Option<&str>,
    ) -> anyhow::Result<Vec<MediaListRow>> {
        let conn = self.conn();
        let map_row = |r: &rusqlite::Row| Ok(MediaListRow { entry_data: r.get(0)? });
        let rows = match media_type {
            Some(t) => conn
                .prepare("SELECT entry_data FROM user_media_lists WHERE discord_id = ? AND media_type = ?")?
                .query_map(params![discord_id, t], map_row)?
                .collect::<Result<Vec<_>, _>>()?,
            None => conn
                .prepare("SELECT entry_data FROM user_media_lists WHERE discord_id = ?")?
                .query_map([discord_id], map_row)?
                .collect::<Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }

    /// Replaces a user's favorites. An empty list changes nothing (a failed or empty
    /// fetch must not wipe what is stored).
    pub fn anilist_save_user_favorites(&self, discord_id: &str, media_ids: &[i64]) -> anyhow::Result<()> {
        if media_ids.is_empty() {
            return Ok(());
        }
        self.tx(|tx| {
            tx.execute("DELETE FROM user_favorites WHERE discord_id = ?", [discord_id])?;
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO user_favorites (discord_id, media_id, updated_at) VALUES (?, ?, CURRENT_TIMESTAMP)",
            )?;
            for id in media_ids {
                stmt.execute(params![discord_id, id])?;
            }
            Ok(())
        })
    }

    pub fn anilist_user_favorites(&self, discord_id: &str) -> anyhow::Result<Vec<i64>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT media_id FROM user_favorites WHERE discord_id = ?")?;
        let ids = stmt
            .query_map([discord_id], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    pub fn anilist_last_refresh(&self, discord_id: &str) -> anyhow::Result<Option<RefreshTimes>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT last_lists_refresh, last_favorites_refresh FROM anilist_refresh_log WHERE discord_id = ?",
                [discord_id],
                |r| Ok(RefreshTimes { lists: r.get(0)?, favorites: r.get(1)? }),
            )
            .optional()?)
    }

    pub fn anilist_update_refresh_log(&self, discord_id: &str, kind: RefreshKind, value: &str) -> anyhow::Result<()> {
        let column = match kind {
            RefreshKind::Lists => "last_lists_refresh",
            RefreshKind::Favorites => "last_favorites_refresh",
        };
        self.tx(|tx| {
            tx.execute("INSERT OR IGNORE INTO anilist_refresh_log (discord_id) VALUES (?)", [discord_id])?;
            tx.execute(
                &format!("UPDATE anilist_refresh_log SET {column} = ? WHERE discord_id = ?"),
                [value, discord_id],
            )?;
            Ok(())
        })
    }

    /// Cached AniList media JSON for a search string, if younger than `ttl_secs`.
    pub fn anilist_get_cached_media(
        &self,
        query: &str,
        media_type: &str,
        ttl_secs: i64,
    ) -> anyhow::Result<Option<String>> {
        let min_time = chrono::Utc::now().timestamp() - ttl_secs;
        Ok(self
            .conn()
            .query_row(
                "SELECT data FROM anilist_media_cache WHERE query_lower = ? AND media_type = ? AND cached_at > ?",
                params![query.to_lowercase(), media_type, min_time],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Stores a media search result and drops entries older than `ttl_secs`.
    pub fn anilist_set_cached_media(
        &self,
        query: &str,
        media_type: &str,
        data: &str,
        ttl_secs: i64,
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.tx(|tx| {
            tx.execute("DELETE FROM anilist_media_cache WHERE cached_at <= ?", [now - ttl_secs])?;
            tx.execute(
                "INSERT OR REPLACE INTO anilist_media_cache (query_lower, media_type, data, cached_at) VALUES (?, ?, ?, ?)",
                params![query.to_lowercase(), media_type, data, now],
            )?;
            Ok(())
        })
    }
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
