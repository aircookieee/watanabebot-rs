//! Wallets, wallet transactions, audit log, daily claims, bettor stats.
//! OWNER: Economy/tournament agent. Add `impl Db` query methods here.

use rusqlite::Connection;

use super::Db;

impl Db {
    /// Credits `amount` (> 0) to the wallet (created with the starting balance if missing),
    /// records a wallet_transactions row, and returns the new balance.
    /// STUB: Economy agent implements this; the signature is used by `bot::events`.
    pub fn add_currency(
        &self,
        _user_id: u64,
        _guild_id: u64,
        _amount: i64,
        _reason: &str,
        _reference_id: Option<&str>,
    ) -> anyhow::Result<i64> {
        anyhow::bail!("add_currency not implemented yet")
    }
}

pub(super) fn migrate(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS wallets (
            user_id TEXT NOT NULL,
            guild_id TEXT NOT NULL,
            balance INTEGER NOT NULL DEFAULT 500,
            total_earned INTEGER NOT NULL DEFAULT 0,
            total_spent INTEGER NOT NULL DEFAULT 0,
            updated_at TEXT DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (user_id, guild_id)
        );
        CREATE TABLE IF NOT EXISTS wallet_transactions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id TEXT NOT NULL,
            guild_id TEXT NOT NULL,
            amount INTEGER NOT NULL,
            reason TEXT NOT NULL,
            reference_id TEXT,
            balance_after INTEGER NOT NULL,
            created_at TEXT DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE IF NOT EXISTS audit_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            actor_user_id TEXT NOT NULL,
            action TEXT NOT NULL,
            target_type TEXT NOT NULL,
            target_id TEXT,
            metadata TEXT,
            created_at TEXT DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_wallet_tx_guild_user ON wallet_transactions (guild_id, user_id, id);
        ",
    )?;
    Ok(())
}
