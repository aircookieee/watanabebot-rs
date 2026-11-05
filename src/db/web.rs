//! Web dashboard sessions.

use rusqlite::{params, Connection, OptionalExtension};

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

/// A persisted dashboard login.
#[derive(Debug, Clone)]
pub struct WebSessionRow {
    pub user_id: String,
    pub username: String,
    pub guild_member: bool,
    pub csrf_token: String,
    #[allow(dead_code)]
    pub bracket_preview: Option<String>,
    /// UTC `YYYY-MM-DD HH:MM:SS` (SQLite CURRENT_TIMESTAMP); `None` for legacy rows without it.
    pub created_at: Option<String>,
}

impl Db {
    /// Inserts or updates a session. `created_at` is kept when the session already exists.
    pub fn save_web_session(
        &self,
        sid: &str,
        user_id: &str,
        username: &str,
        guild_member: bool,
        csrf_token: &str,
        bracket_preview: Option<&str>,
    ) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO web_sessions (sid, user_id, username, guild_member, csrf_token, bracket_preview)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(sid) DO UPDATE SET
                user_id = excluded.user_id,
                username = excluded.username,
                guild_member = excluded.guild_member,
                csrf_token = excluded.csrf_token,
                bracket_preview = excluded.bracket_preview,
                updated_at = CURRENT_TIMESTAMP",
            params![sid, user_id, username, guild_member as i64, csrf_token, bracket_preview],
        )?;
        Ok(())
    }

    pub fn get_web_session(&self, sid: &str) -> anyhow::Result<Option<WebSessionRow>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT user_id, username, guild_member, csrf_token, bracket_preview, created_at
                 FROM web_sessions WHERE sid = ?",
                [sid],
                |r| {
                    Ok(WebSessionRow {
                        user_id: r.get(0)?,
                        username: r.get(1)?,
                        guild_member: r.get::<_, i64>(2)? != 0,
                        csrf_token: r.get(3)?,
                        bracket_preview: r.get(4)?,
                        created_at: r.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn delete_web_session(&self, sid: &str) -> anyhow::Result<()> {
        self.conn()
            .execute("DELETE FROM web_sessions WHERE sid = ?", [sid])?;
        Ok(())
    }

    /// Deletes sessions created more than `max_age_secs` ago. Returns how many were removed.
    pub fn purge_expired_web_sessions(&self, max_age_secs: i64) -> anyhow::Result<usize> {
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(max_age_secs);
        let cutoff = cutoff.format("%Y-%m-%d %H:%M:%S").to_string();
        Ok(self.conn().execute(
            "DELETE FROM web_sessions WHERE created_at IS NOT NULL AND created_at < ?",
            [cutoff],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn upsert_keeps_created_at_and_updates_fields() {
        let conn = conn();
        conn.execute(
            "INSERT INTO web_sessions (sid, user_id, username, guild_member, csrf_token, created_at) \
             VALUES ('s', '1', 'a', 1, 'c1', '2020-01-01 00:00:00')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO web_sessions (sid, user_id, username, guild_member, csrf_token, bracket_preview)
             VALUES ('s', '1', 'b', 1, 'c2', NULL)
             ON CONFLICT(sid) DO UPDATE SET username = excluded.username, csrf_token = excluded.csrf_token",
            [],
        )
        .unwrap();
        let (name, created): (String, String) = conn
            .query_row(
                "SELECT username, created_at FROM web_sessions WHERE sid = 's'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(name, "b");
        assert_eq!(created, "2020-01-01 00:00:00");
    }
}
