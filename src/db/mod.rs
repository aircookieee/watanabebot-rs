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

        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        if existed {
            // Back up through SQLite (not a file copy) so data still in the -wal file from the
            // previous run is included, and the backup is a single self-contained file.
            create_startup_backup(&conn, path)?;
        }
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

/// Startup backups kept in `data/backups/`; older ones are deleted.
const KEEP_BACKUPS: usize = 30;
const BACKUP_PREFIX: &str = "watanabebot-rs.";

fn create_startup_backup(conn: &Connection, path: &Path) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or(Path::new(".")).join("backups");
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ");
    // Own prefix: the TypeScript bot wrote `watanabebot.<stamp>.db` into the same folder, and
    // pruning must never touch those.
    let backup = dir.join(format!("{BACKUP_PREFIX}{stamp}.db"));
    conn.execute("VACUUM INTO ?", [backup.to_string_lossy()])
        .with_context(|| format!("backing up database to {}", backup.display()))?;
    tracing::info!("Created database startup backup: {}", backup.display());

    let mut backups: Vec<_> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(BACKUP_PREFIX) && n.ends_with(".db"))
        })
        .collect();
    // Timestamped names sort chronologically.
    backups.sort();
    let excess = backups.len().saturating_sub(KEEP_BACKUPS);
    for old in backups.into_iter().take(excess) {
        if let Err(err) = std::fs::remove_file(&old) {
            tracing::warn!("could not delete old backup {}: {err}", old.display());
        }
    }
    Ok(())
}

/// Current UTC time in the `YYYY-MM-DD HH:MM:SS` format SQLite's CURRENT_TIMESTAMP uses.
pub fn now_sql() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_backup_includes_wal_data() {
        let dir = std::env::temp_dir().join(format!("watanabebot-db-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = Config::from_env();
        cfg.paths.database_path = dir.join("test.db");
        cfg.production = false;
        cfg.legacy_anilist_map_path = None;

        // First boot: write a row that may still sit in the -wal file when the process "dies".
        let db = Db::open(&cfg).unwrap();
        db.set_guild_config(42, Some(7), ReactMode::Here).unwrap();
        std::mem::forget(db); // simulate a kill: no clean close, no checkpoint

        // A TypeScript-era backup in the same folder must survive pruning.
        std::fs::create_dir_all(dir.join("backups")).unwrap();
        let legacy = dir.join("backups").join("watanabebot.2020-01-01T00-00-00-000Z.db");
        std::fs::write(&legacy, b"legacy").unwrap();

        // Second boot backs up through SQLite; the backup must contain the row.
        let _db = Db::open(&cfg).unwrap();
        assert!(legacy.exists());
        let backups: Vec<_> = std::fs::read_dir(dir.join("backups"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(BACKUP_PREFIX))
            .collect();
        assert_eq!(backups.len(), 1);
        let backup = Connection::open(backups[0].path()).unwrap();
        let mode: String = backup
            .query_row("SELECT mode FROM guild_config WHERE guild_id = '42'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "here");
    }
}
