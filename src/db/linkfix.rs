//! Per-guild link replacement configuration and the table of reposted messages.
//!
//! The legacy global `twitter_enabled` setting in the `settings` table is intentionally left
//! untouched; the per-guild defaults below apply regardless of its value.

use std::collections::{HashMap, HashSet};

use rusqlite::{params, Connection, OptionalExtension};

use super::Db;

/// How a fixed link is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixMode {
    /// Delete the original and repost it as the author through a webhook.
    Webhook,
    /// Keep the original (embeds suppressed) and reply with the fixed links.
    Reply,
}

impl FixMode {
    pub fn as_str(self) -> &'static str {
        match self {
            FixMode::Webhook => "webhook",
            FixMode::Reply => "reply",
        }
    }

    fn from_db(value: &str) -> Self {
        if value == "reply" {
            FixMode::Reply
        } else {
            FixMode::Webhook
        }
    }
}

/// Per-site override chosen by a server admin. `None` means "use the built-in default".
#[derive(Debug, Clone, Default)]
pub struct SiteOverride {
    pub enabled: Option<bool>,
    pub domain: Option<String>,
}

/// Everything the fixer needs to know about one guild.
#[derive(Debug, Clone)]
pub struct GuildLinkConfig {
    /// Server-wide switch (default on).
    pub enabled: bool,
    pub mode: FixMode,
    pub sites: HashMap<String, SiteOverride>,
    /// Custom rules: (source domain, replacement domain), sorted by source.
    pub rules: Vec<(String, String)>,
    pub ignored: HashSet<u64>,
}

impl Default for GuildLinkConfig {
    fn default() -> Self {
        GuildLinkConfig {
            enabled: true,
            mode: FixMode::Webhook,
            sites: HashMap::new(),
            rules: Vec::new(),
            ignored: HashSet::new(),
        }
    }
}

/// A message the fixer posted (webhook repost or reply).
#[derive(Debug, Clone, Copy)]
pub struct RepostRecord {
    pub author_id: u64,
    pub guild_id: u64,
}

pub(super) fn migrate(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS linkfix_guild (
            guild_id TEXT PRIMARY KEY,
            enabled INTEGER NOT NULL DEFAULT 1,
            mode TEXT NOT NULL DEFAULT 'webhook'
        );
        CREATE TABLE IF NOT EXISTS linkfix_site (
            guild_id TEXT NOT NULL,
            site_id TEXT NOT NULL,
            enabled INTEGER,
            domain TEXT,
            PRIMARY KEY (guild_id, site_id)
        );
        CREATE TABLE IF NOT EXISTS linkfix_ignored (
            guild_id TEXT NOT NULL,
            channel_id TEXT NOT NULL,
            PRIMARY KEY (guild_id, channel_id)
        );
        CREATE TABLE IF NOT EXISTS linkfix_rule (
            guild_id TEXT NOT NULL,
            from_domain TEXT NOT NULL,
            to_domain TEXT NOT NULL,
            PRIMARY KEY (guild_id, from_domain)
        );
        CREATE TABLE IF NOT EXISTS linkfix_repost (
            message_id TEXT PRIMARY KEY,
            author_id TEXT NOT NULL,
            channel_id TEXT NOT NULL,
            guild_id TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        DELETE FROM linkfix_repost WHERE created_at < datetime('now', '-60 days');
        ",
    )?;
    Ok(())
}

impl Db {
    pub fn get_linkfix_config(&self, guild_id: u64) -> anyhow::Result<GuildLinkConfig> {
        let conn = self.conn();
        let gid = guild_id.to_string();
        let mut cfg = GuildLinkConfig::default();

        let guild_row: Option<(bool, String)> = conn
            .query_row(
                "SELECT enabled, mode FROM linkfix_guild WHERE guild_id = ?",
                [&gid],
                |r| Ok((r.get::<_, i64>(0)? != 0, r.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((enabled, mode)) = guild_row {
            cfg.enabled = enabled;
            cfg.mode = FixMode::from_db(&mode);
        }

        let mut stmt =
            conn.prepare("SELECT site_id, enabled, domain FROM linkfix_site WHERE guild_id = ?")?;
        let rows = stmt.query_map([&gid], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (site, enabled, domain) = row?;
            cfg.sites.insert(
                site,
                SiteOverride {
                    enabled: enabled.map(|e| e != 0),
                    domain,
                },
            );
        }

        let mut stmt = conn.prepare("SELECT channel_id FROM linkfix_ignored WHERE guild_id = ?")?;
        let rows = stmt.query_map([&gid], |r| r.get::<_, String>(0))?;
        for row in rows {
            if let Ok(id) = row?.parse::<u64>() {
                cfg.ignored.insert(id);
            }
        }

        let mut stmt = conn
            .prepare("SELECT from_domain, to_domain FROM linkfix_rule WHERE guild_id = ? ORDER BY from_domain")?;
        let rows = stmt.query_map([&gid], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            cfg.rules.push(row?);
        }
        Ok(cfg)
    }

    pub fn set_linkfix_enabled(&self, guild_id: u64, enabled: bool) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO linkfix_guild (guild_id, enabled) VALUES (?, ?)
             ON CONFLICT(guild_id) DO UPDATE SET enabled = excluded.enabled",
            params![guild_id.to_string(), enabled as i64],
        )?;
        Ok(())
    }

    pub fn set_linkfix_mode(&self, guild_id: u64, mode: FixMode) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO linkfix_guild (guild_id, mode) VALUES (?, ?)
             ON CONFLICT(guild_id) DO UPDATE SET mode = excluded.mode",
            params![guild_id.to_string(), mode.as_str()],
        )?;
        Ok(())
    }

    pub fn set_linkfix_site_enabled(
        &self,
        guild_id: u64,
        site_id: &str,
        enabled: bool,
    ) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO linkfix_site (guild_id, site_id, enabled) VALUES (?, ?, ?)
             ON CONFLICT(guild_id, site_id) DO UPDATE SET enabled = excluded.enabled",
            params![guild_id.to_string(), site_id, enabled as i64],
        )?;
        Ok(())
    }

    /// Sets (or with `None` clears) the replacement domain override for a site.
    pub fn set_linkfix_site_domain(
        &self,
        guild_id: u64,
        site_id: &str,
        domain: Option<&str>,
    ) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO linkfix_site (guild_id, site_id, domain) VALUES (?, ?, ?)
             ON CONFLICT(guild_id, site_id) DO UPDATE SET domain = excluded.domain",
            params![guild_id.to_string(), site_id, domain],
        )?;
        Ok(())
    }

    /// Returns true if the channel was newly added.
    pub fn add_linkfix_ignored(&self, guild_id: u64, channel_id: u64) -> anyhow::Result<bool> {
        let n = self.conn().execute(
            "INSERT OR IGNORE INTO linkfix_ignored (guild_id, channel_id) VALUES (?, ?)",
            params![guild_id.to_string(), channel_id.to_string()],
        )?;
        Ok(n > 0)
    }

    /// Returns true if the channel was ignored before.
    pub fn remove_linkfix_ignored(&self, guild_id: u64, channel_id: u64) -> anyhow::Result<bool> {
        let n = self.conn().execute(
            "DELETE FROM linkfix_ignored WHERE guild_id = ? AND channel_id = ?",
            params![guild_id.to_string(), channel_id.to_string()],
        )?;
        Ok(n > 0)
    }

    pub fn set_linkfix_rule(&self, guild_id: u64, from: &str, to: &str) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO linkfix_rule (guild_id, from_domain, to_domain) VALUES (?, ?, ?)
             ON CONFLICT(guild_id, from_domain) DO UPDATE SET to_domain = excluded.to_domain",
            params![guild_id.to_string(), from, to],
        )?;
        Ok(())
    }

    /// Returns true if a rule was removed.
    pub fn remove_linkfix_rule(&self, guild_id: u64, from: &str) -> anyhow::Result<bool> {
        let n = self.conn().execute(
            "DELETE FROM linkfix_rule WHERE guild_id = ? AND from_domain = ?",
            params![guild_id.to_string(), from],
        )?;
        Ok(n > 0)
    }

    pub fn record_linkfix_repost(
        &self,
        message_id: u64,
        author_id: u64,
        channel_id: u64,
        guild_id: u64,
    ) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO linkfix_repost (message_id, author_id, channel_id, guild_id) VALUES (?, ?, ?, ?)",
            params![message_id.to_string(), author_id.to_string(), channel_id.to_string(), guild_id.to_string()],
        )?;
        Ok(())
    }

    pub fn get_linkfix_repost(&self, message_id: u64) -> anyhow::Result<Option<RepostRecord>> {
        let row: Option<(String, String)> = self
            .conn()
            .query_row(
                "SELECT author_id, guild_id FROM linkfix_repost WHERE message_id = ?",
                [message_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(author, guild)| {
            Some(RepostRecord {
                author_id: author.parse().ok()?,
                guild_id: guild.parse().ok()?,
            })
        }))
    }

    pub fn delete_linkfix_repost(&self, message_id: u64) -> anyhow::Result<()> {
        self.conn().execute(
            "DELETE FROM linkfix_repost WHERE message_id = ?",
            [message_id.to_string()],
        )?;
        Ok(())
    }
}
