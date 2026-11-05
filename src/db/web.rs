//! Web dashboard sessions.
//! OWNER: Web agent. Add `impl Db` query methods here.

use rusqlite::Connection;

#[allow(unused_imports)]
use super::Db;

pub(super) fn migrate(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS web_sessions (
            sid TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            username TEXT NOT NULL,
            guild_member INTEGER NOT NULL DEFAULT 1,
            csrf_token TEXT NOT NULL,
            bracket_preview TEXT,
            created_at TEXT DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT DEFAULT CURRENT_TIMESTAMP
        );
        ",
    )?;
    Ok(())
}
