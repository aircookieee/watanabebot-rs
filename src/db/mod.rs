//! SQLite storage. Uses the same schema as the TypeScript bot (sql.js wrote a regular
//! SQLite file), so an existing `data/watanabebot.db` is opened in place. New features
//! only add tables/columns, never drop or rewrite existing data.
//!
//! Each feature owns a submodule that adds `impl Db` methods and a `migrate` function.

pub mod anilist;
pub mod economy;
pub mod linkfix;
pub mod tournament;
pub mod web;

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{bail, Context as _};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::config::Config;

/// Cheap-to-clone handle to the database. All access is serialized through one connection;
/// every operation is short, so a std mutex is fine (never hold it across `.await`).
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(cfg: &Config) -> anyhow::Result<Self> {
        let path = &cfg.paths.database_path;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }

        let existed = path.exists();
        if !existed && cfg.production && !cfg.allow_database_bootstrap {
            bail!(
                "Refusing to create a new production database at {}. This prevents wallet, bet, and \
                 tournament resets when the data volume is missing or empty. Restore/mount the existing \
                 database, or set ALLOW_DATABASE_BOOTSTRAP=true only for an intentional first boot.",
                path.display()
            );
        }

        if existed {
            create_startup_backup(path)?;
        }

        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        migrate(&conn)?;
        anilist::import_legacy_map(&conn, cfg)?;

        Ok(Db { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Locks the connection. Recovers from a poisoned lock (a panic mid-query leaves
    /// SQLite itself consistent).
    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `f` inside a transaction; commits on `Ok`, rolls back on `Err`.
    pub fn tx<T>(&self, f: impl FnOnce(&Transaction) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    // ---- settings (global key/value) ----

    pub fn get_setting(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT value FROM settings WHERE key = ?", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.conn()
            .execute("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)", [key, value])?;
        Ok(())
    }

    // ---- guild_config (reaction channel mode) ----

    pub fn get_guild_config(&self, guild_id: u64) -> anyhow::Result<Option<GuildConfig>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT channel_id, mode FROM guild_config WHERE guild_id = ?",
                [guild_id.to_string()],
                |r| {
                    let channel: String = r.get(0)?;
                    let mode: String = r.get(1)?;
                    Ok(GuildConfig {
                        channel_id: channel.parse().ok(),
                        mode: if mode == "here" { ReactMode::Here } else { ReactMode::Everywhere },
                    })
                },
            )
            .optional()?)
    }

    pub fn set_guild_config(&self, guild_id: u64, channel_id: Option<u64>, mode: ReactMode) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO guild_config (guild_id, channel_id, mode) VALUES (?, ?, ?)",
            params![
                guild_id.to_string(),
                channel_id.map(|c| c.to_string()).unwrap_or_default(),
                mode.as_str()
            ],
        )?;
        Ok(())
    }

    pub fn ensure_guild_config(&self, guild_id: u64) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO guild_config (guild_id, channel_id, mode) VALUES (?, '', 'everywhere')",
            [guild_id.to_string()],
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactMode {
    Here,
    Everywhere,
}

impl ReactMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ReactMode::Here => "here",
            ReactMode::Everywhere => "everywhere",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GuildConfig {
    pub channel_id: Option<u64>,
    pub mode: ReactMode,
}

fn migrate(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS guild_config (
            guild_id TEXT PRIMARY KEY,
            channel_id TEXT DEFAULT '',
            mode TEXT DEFAULT 'everywhere'
        );
        CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        ",
    )?;
    anilist::migrate(conn)?;
    economy::migrate(conn)?;
    tournament::migrate(conn)?;
    web::migrate(conn)?;
    linkfix::migrate(conn)?;
    Ok(())
}

/// Adds a column if it does not exist yet (SQLite has no `ADD COLUMN IF NOT EXISTS`).
pub fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl: &str) -> anyhow::Result<()> {
    let exists: bool = conn
        .prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?"))?
        .exists([column])?;
    if !exists {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
    }
    Ok(())
}

fn create_startup_backup(path: &Path) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or(Path::new(".")).join("backups");
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ");
    let backup = dir.join(format!("watanabebot.{stamp}.db"));
    std::fs::copy(path, &backup).with_context(|| format!("backing up database to {}", backup.display()))?;
    tracing::info!("Created database startup backup: {}", backup.display());
    Ok(())
}

/// Current UTC time in the `YYYY-MM-DD HH:MM:SS` format SQLite's CURRENT_TIMESTAMP uses.
pub fn now_sql() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}
