//! Wallets, wallet transactions, audit log, daily claims, poll vote rewards, bettor stats.
//!
//! The money primitives are free functions taking a `&Connection` so callers can compose
//! several of them (and bet / market writes) inside one `Db::tx` transaction. `&Transaction`
//! coerces to `&Connection`, so the same functions work inside and outside transactions.

// Some of these are only used by the web dashboard / tests.
#![allow(dead_code)]

use anyhow::bail;
use chrono::NaiveDate;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use super::Db;
use crate::config;

// ---------------------------------------------------------------------------------------------
// Row types (serialized with the same JSON field names the TypeScript API returned)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wallet {
    pub balance: i64,
    pub total_earned: i64,
    pub total_spent: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaderboardRow {
    pub user_id: String,
    pub balance: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletTransactionRow {
    pub id: i64,
    pub user_id: String,
    pub amount: i64,
    pub reason: String,
    pub reference_id: Option<String>,
    pub balance_after: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditLogRow {
    pub id: i64,
    pub actor_user_id: String,
    pub action: String,
    pub target_type: String,
    pub target_id: Option<String>,
    pub metadata: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfitLeaderboardRow {
    pub user_id: String,
    pub net_profit: i64,
    pub won: i64,
    pub lost: i64,
}

/// Aggregate of one kind of bet (match bets or special bets) for a user.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BetAggregate {
    pub placed: i64,
    pub won: i64,
    pub lost: i64,
    pub pending: i64,
    pub refunded: i64,
    /// Sum of stakes over every non-refunded bet (pending stakes included).
    pub total_wagered: i64,
    /// Sum of stakes over won and lost bets only.
    pub resolved_wagered: i64,
    /// Sum of payouts over won bets.
    pub total_paid_out: i64,
    pub biggest_payout: i64,
}

impl BetAggregate {
    pub fn net_profit(&self) -> i64 {
        self.total_paid_out - self.resolved_wagered
    }

    pub fn combine(&self, other: &BetAggregate) -> BetAggregate {
        BetAggregate {
            placed: self.placed + other.placed,
            won: self.won + other.won,
            lost: self.lost + other.lost,
            pending: self.pending + other.pending,
            refunded: self.refunded + other.refunded,
            total_wagered: self.total_wagered + other.total_wagered,
            resolved_wagered: self.resolved_wagered + other.resolved_wagered,
            total_paid_out: self.total_paid_out + other.total_paid_out,
            biggest_payout: self.biggest_payout.max(other.biggest_payout),
        }
    }
}

/// Stored daily-claim state for a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyClaimRow {
    pub last_claim_date: NaiveDate,
    pub streak: i64,
    pub best_streak: i64,
}

// ---------------------------------------------------------------------------------------------
// Connection-level wallet primitives
// ---------------------------------------------------------------------------------------------

/// Largest single credit/debit/bet. Keeps sums of many amounts far from i64 overflow.
pub const MAX_AMOUNT: i64 = 1_000_000_000_000;
/// Largest balance a wallet may hold.
pub const MAX_BALANCE: i64 = 1_000_000_000_000_000;

fn check_amount(amount: i64) -> anyhow::Result<()> {
    if amount > MAX_AMOUNT {
        bail!("Amount cannot exceed {MAX_AMOUNT} MugCoins.");
    }
    Ok(())
}

/// Returns the wallet, creating it with the configured starting balance if missing.
pub fn get_or_create_wallet(conn: &Connection, user_id: &str, guild_id: &str) -> anyhow::Result<Wallet> {
    let existing = conn
        .query_row(
            "SELECT balance, total_earned, total_spent FROM wallets WHERE user_id = ? AND guild_id = ?",
            params![user_id, guild_id],
            |r| Ok(Wallet { balance: r.get(0)?, total_earned: r.get(1)?, total_spent: r.get(2)? }),
        )
        .optional()?;
    if let Some(wallet) = existing {
        return Ok(wallet);
    }
    let starting = config::get().currency.starting_balance;
    conn.execute(
        "INSERT INTO wallets (user_id, guild_id, balance) VALUES (?, ?, ?)",
        params![user_id, guild_id, starting],
    )?;
    Ok(Wallet { balance: starting, total_earned: 0, total_spent: 0 })
}

pub fn get_balance(conn: &Connection, user_id: &str, guild_id: &str) -> anyhow::Result<i64> {
    Ok(get_or_create_wallet(conn, user_id, guild_id)?.balance)
}

/// Credits `amount`. A non-positive amount is a no-op (as in the TS bot) that returns the
/// current balance. Returns the new balance.
pub fn add_currency(
    conn: &Connection,
    user_id: &str,
    guild_id: &str,
    amount: i64,
    reason: &str,
    reference_id: Option<&str>,
) -> anyhow::Result<i64> {
    check_amount(amount)?;
    let wallet = get_or_create_wallet(conn, user_id, guild_id)?;
    if amount <= 0 {
        return Ok(wallet.balance);
    }
    if wallet.balance > MAX_BALANCE - amount {
        bail!("That would exceed the maximum balance of {MAX_BALANCE} MugCoins.");
    }
    let new_balance = wallet.balance + amount;
    let new_earned = wallet.total_earned + amount;
    conn.execute(
        "UPDATE wallets SET balance = ?, total_earned = ?, updated_at = CURRENT_TIMESTAMP WHERE user_id = ? AND guild_id = ?",
        params![new_balance, new_earned, user_id, guild_id],
    )?;
    conn.execute(
        "INSERT INTO wallet_transactions (user_id, guild_id, amount, reason, reference_id, balance_after) VALUES (?, ?, ?, ?, ?, ?)",
        params![user_id, guild_id, amount, reason, reference_id, new_balance],
    )?;
    Ok(new_balance)
}

/// Debits `amount`; returns false (and writes nothing but the lazily created wallet) when the
/// amount is not positive or the balance is too low.
pub fn spend_currency(
    conn: &Connection,
    user_id: &str,
    guild_id: &str,
    amount: i64,
    reason: &str,
    reference_id: Option<&str>,
) -> anyhow::Result<bool> {
    check_amount(amount)?;
    if amount <= 0 {
        return Ok(false);
    }
    let wallet = get_or_create_wallet(conn, user_id, guild_id)?;
    if wallet.balance < amount {
        return Ok(false);
    }
    let new_balance = wallet.balance - amount;
    let new_spent = wallet.total_spent + amount;
    conn.execute(
        "UPDATE wallets SET balance = ?, total_spent = ?, updated_at = CURRENT_TIMESTAMP WHERE user_id = ? AND guild_id = ?",
        params![new_balance, new_spent, user_id, guild_id],
    )?;
    conn.execute(
        "INSERT INTO wallet_transactions (user_id, guild_id, amount, reason, reference_id, balance_after) VALUES (?, ?, ?, ?, ?, ?)",
        params![user_id, guild_id, -amount, reason, reference_id, new_balance],
    )?;
    Ok(true)
}

/// Sets the balance to `amount` (>= 0), logging the difference. No-op when unchanged.
pub fn set_balance(conn: &Connection, user_id: &str, guild_id: &str, amount: i64, reason: &str) -> anyhow::Result<()> {
    if amount < 0 {
        return Ok(());
    }
    if amount > MAX_BALANCE {
        bail!("Balance cannot exceed {MAX_BALANCE} MugCoins.");
    }
    let wallet = get_or_create_wallet(conn, user_id, guild_id)?;
    let diff = amount - wallet.balance;
    if diff == 0 {
        return Ok(());
    }
    conn.execute(
        "UPDATE wallets SET balance = ?, updated_at = CURRENT_TIMESTAMP WHERE user_id = ? AND guild_id = ?",
        params![amount, user_id, guild_id],
    )?;
    conn.execute(
        "INSERT INTO wallet_transactions (user_id, guild_id, amount, reason, balance_after) VALUES (?, ?, ?, ?, ?)",
        params![user_id, guild_id, diff, reason, amount],
    )?;
    Ok(())
}

/// Moves `amount` from sender to receiver. Returns false for a non-positive amount, a self
/// transfer or insufficient funds.
pub fn transfer_currency(
    conn: &Connection,
    sender_id: &str,
    receiver_id: &str,
    guild_id: &str,
    amount: i64,
) -> anyhow::Result<bool> {
    check_amount(amount)?;
    if amount <= 0 || sender_id == receiver_id {
        return Ok(false);
    }
    let sender = get_or_create_wallet(conn, sender_id, guild_id)?;
    if sender.balance < amount {
        return Ok(false);
    }
    if get_or_create_wallet(conn, receiver_id, guild_id)?.balance > MAX_BALANCE - amount {
        bail!("That would exceed the recipient's maximum balance of {MAX_BALANCE} MugCoins.");
    }
    let receiver = get_or_create_wallet(conn, receiver_id, guild_id)?;

    let sender_balance = sender.balance - amount;
    let receiver_balance = receiver.balance + amount;

    conn.execute(
        "UPDATE wallets SET balance = ?, total_spent = ?, updated_at = CURRENT_TIMESTAMP WHERE user_id = ? AND guild_id = ?",
        params![sender_balance, sender.total_spent + amount, sender_id, guild_id],
    )?;
    conn.execute(
        "INSERT INTO wallet_transactions (user_id, guild_id, amount, reason, balance_after) VALUES (?, ?, ?, ?, ?)",
        params![sender_id, guild_id, -amount, "user_transfer", sender_balance],
    )?;
    conn.execute(
        "UPDATE wallets SET balance = ?, total_earned = ?, updated_at = CURRENT_TIMESTAMP WHERE user_id = ? AND guild_id = ?",
        params![receiver_balance, receiver.total_earned + amount, receiver_id, guild_id],
    )?;
    conn.execute(
        "INSERT INTO wallet_transactions (user_id, guild_id, amount, reason, balance_after) VALUES (?, ?, ?, ?, ?)",
        params![receiver_id, guild_id, amount, "user_transfer", receiver_balance],
    )?;
    Ok(true)
}

pub fn get_leaderboard(conn: &Connection, guild_id: &str, limit: i64, offset: i64) -> anyhow::Result<Vec<LeaderboardRow>> {
    let mut stmt = conn.prepare(
        "SELECT user_id, balance FROM wallets WHERE guild_id = ? ORDER BY balance DESC, user_id ASC LIMIT ? OFFSET ?",
    )?;
    let rows = stmt
        .query_map(params![guild_id, limit, offset], |r| {
            Ok(LeaderboardRow { user_id: r.get(0)?, balance: r.get(1)? })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn map_transaction(r: &rusqlite::Row) -> rusqlite::Result<WalletTransactionRow> {
    Ok(WalletTransactionRow {
        id: r.get(0)?,
        user_id: r.get(1)?,
        amount: r.get(2)?,
        reason: r.get(3)?,
        reference_id: r.get(4)?,
        balance_after: r.get(5)?,
        created_at: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
    })
}

pub fn get_wallet_transactions(
    conn: &Connection,
    guild_id: &str,
    user_id: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<WalletTransactionRow>> {
    let rows = match user_id {
        Some(user) => {
            let mut stmt = conn.prepare(
                "SELECT id, user_id, amount, reason, reference_id, balance_after, created_at FROM wallet_transactions WHERE guild_id = ? AND user_id = ? ORDER BY id DESC LIMIT ?",
            )?;
            let rows = stmt.query_map(params![guild_id, user, limit], map_transaction)?.collect::<Result<Vec<_>, _>>()?;
            rows
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT id, user_id, amount, reason, reference_id, balance_after, created_at FROM wallet_transactions WHERE guild_id = ? ORDER BY id DESC LIMIT ?",
            )?;
            let rows = stmt.query_map(params![guild_id, limit], map_transaction)?.collect::<Result<Vec<_>, _>>()?;
            rows
        }
    };
    Ok(rows)
}

pub fn add_audit_log(
    conn: &Connection,
    actor_user_id: &str,
    action: &str,
    target_type: &str,
    target_id: Option<&str>,
    metadata: &serde_json::Value,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO audit_log (actor_user_id, action, target_type, target_id, metadata) VALUES (?, ?, ?, ?, ?)",
        params![actor_user_id, action, target_type, target_id, metadata.to_string()],
    )?;
    Ok(())
}

pub fn get_audit_logs(conn: &Connection, limit: i64) -> anyhow::Result<Vec<AuditLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, actor_user_id, action, target_type, target_id, metadata, created_at FROM audit_log ORDER BY id DESC LIMIT ?",
    )?;
    let rows = stmt
        .query_map([limit], |r| {
            Ok(AuditLogRow {
                id: r.get(0)?,
                actor_user_id: r.get(1)?,
                action: r.get(2)?,
                target_type: r.get(3)?,
                target_id: r.get(4)?,
                metadata: r.get(5)?,
                created_at: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------------------------
// Daily claims
// ---------------------------------------------------------------------------------------------

pub fn get_daily_claim(conn: &Connection, user_id: &str, guild_id: &str) -> anyhow::Result<Option<DailyClaimRow>> {
    let row = conn
        .query_row(
            "SELECT last_claim_date, streak, best_streak FROM daily_claims WHERE user_id = ? AND guild_id = ?",
            params![user_id, guild_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
        )
        .optional()?;
    let Some((date, streak, best_streak)) = row else {
        return Ok(None);
    };
    let Ok(last_claim_date) = NaiveDate::parse_from_str(&date, "%Y-%m-%d") else {
        bail!("corrupt daily claim date '{date}' for user {user_id}");
    };
    Ok(Some(DailyClaimRow { last_claim_date, streak, best_streak }))
}

pub fn save_daily_claim(
    conn: &Connection,
    user_id: &str,
    guild_id: &str,
    date: NaiveDate,
    streak: i64,
    best_streak: i64,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO daily_claims (user_id, guild_id, last_claim_date, streak, best_streak, updated_at)
         VALUES (?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
         ON CONFLICT(user_id, guild_id) DO UPDATE SET
             last_claim_date = excluded.last_claim_date,
             streak = excluded.streak,
             best_streak = excluded.best_streak,
             updated_at = CURRENT_TIMESTAMP",
        params![user_id, guild_id, date.format("%Y-%m-%d").to_string(), streak, best_streak],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Poll vote rewards
// ---------------------------------------------------------------------------------------------

/// Credits `amount` once per (poll message, user). Returns the new balance when paid, or
/// `None` if this user already received the reward for that poll (or `amount` is not positive).
pub fn claim_poll_vote_reward(
    conn: &Connection,
    message_id: &str,
    user_id: &str,
    guild_id: &str,
    amount: i64,
) -> anyhow::Result<Option<i64>> {
    if amount <= 0 {
        return Ok(None);
    }
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO poll_vote_rewards (message_id, user_id, guild_id, amount) VALUES (?, ?, ?, ?)",
        params![message_id, user_id, guild_id, amount],
    )?;
    if inserted == 0 {
        return Ok(None);
    }
    Ok(Some(add_currency(conn, user_id, guild_id, amount, "poll_vote", Some(message_id))?))
}

// ---------------------------------------------------------------------------------------------
// Bettor stats
// ---------------------------------------------------------------------------------------------

/// Aggregates one bets table (`bets` or `special_bets`) for a user in a guild.
pub fn bet_aggregate(conn: &Connection, table: &str, guild_id: &str, user_id: &str) -> anyhow::Result<BetAggregate> {
    if table != "bets" && table != "special_bets" {
        bail!("unknown bets table {table}");
    }
    let sql = format!(
        "SELECT COUNT(*),
                COALESCE(SUM(status = 'won'), 0),
                COALESCE(SUM(status = 'lost'), 0),
                COALESCE(SUM(status = 'pending'), 0),
                COALESCE(SUM(status = 'refunded'), 0),
                COALESCE(SUM(CASE WHEN status != 'refunded' THEN amount END), 0),
                COALESCE(SUM(CASE WHEN status IN ('won', 'lost') THEN amount END), 0),
                COALESCE(SUM(CASE WHEN status = 'won' THEN payout END), 0),
                COALESCE(MAX(CASE WHEN status = 'won' THEN payout END), 0)
         FROM {table} WHERE guild_id = ? AND user_id = ?"
    );
    Ok(conn.query_row(&sql, params![guild_id, user_id], |r| {
        Ok(BetAggregate {
            placed: r.get(0)?,
            won: r.get(1)?,
            lost: r.get(2)?,
            pending: r.get(3)?,
            refunded: r.get(4)?,
            total_wagered: r.get(5)?,
            resolved_wagered: r.get(6)?,
            total_paid_out: r.get(7)?,
            biggest_payout: r.get(8)?,
        })
    })?)
}

/// Net profit leaderboard over match bets and special bets (won/lost bets only).
pub fn get_profit_leaderboard(
    conn: &Connection,
    guild_id: &str,
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<ProfitLeaderboardRow>> {
    let mut stmt = conn.prepare(
        "SELECT user_id, SUM(profit) AS net, SUM(won) AS w, SUM(lost) AS l FROM (
             SELECT user_id,
                    CASE WHEN status = 'won' THEN payout - amount WHEN status = 'lost' THEN -amount ELSE 0 END AS profit,
                    (status = 'won') AS won, (status = 'lost') AS lost
             FROM bets WHERE guild_id = ?1
             UNION ALL
             SELECT user_id,
                    CASE WHEN status = 'won' THEN payout - amount WHEN status = 'lost' THEN -amount ELSE 0 END AS profit,
                    (status = 'won') AS won, (status = 'lost') AS lost
             FROM special_bets WHERE guild_id = ?1
         )
         GROUP BY user_id
         HAVING SUM(won) + SUM(lost) > 0
         ORDER BY net DESC, user_id ASC
         LIMIT ?2 OFFSET ?3",
    )?;
    let rows = stmt
        .query_map(params![guild_id, limit, offset], |r| {
            Ok(ProfitLeaderboardRow { user_id: r.get(0)?, net_profit: r.get(1)?, won: r.get(2)?, lost: r.get(3)? })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------------------------
// `Db` convenience methods
// ---------------------------------------------------------------------------------------------

impl Db {
    /// Credits `amount` (> 0) to the wallet (created with the starting balance if missing),
    /// records a wallet_transactions row, and returns the new balance.
    pub fn add_currency(
        &self,
        user_id: u64,
        guild_id: u64,
        amount: i64,
        reason: &str,
        reference_id: Option<&str>,
    ) -> anyhow::Result<i64> {
        self.tx(|tx| add_currency(tx, &user_id.to_string(), &guild_id.to_string(), amount, reason, reference_id))
    }

    pub fn get_balance(&self, user_id: &str, guild_id: &str) -> anyhow::Result<i64> {
        self.tx(|tx| get_balance(tx, user_id, guild_id))
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

        -- New: daily claim streaks (one row per user and guild).
        CREATE TABLE IF NOT EXISTS daily_claims (
            user_id TEXT NOT NULL,
            guild_id TEXT NOT NULL,
            last_claim_date TEXT NOT NULL,
            streak INTEGER NOT NULL DEFAULT 1,
            best_streak INTEGER NOT NULL DEFAULT 1,
            updated_at TEXT DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (user_id, guild_id)
        );

        -- New: one reward per (poll message, user), so removing and re-adding a vote never pays twice.
        CREATE TABLE IF NOT EXISTS poll_vote_rewards (
            message_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            guild_id TEXT NOT NULL,
            amount INTEGER NOT NULL,
            created_at TEXT DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (message_id, user_id)
        );
        ",
    )?;
    Ok(())
}

/// Creates the economy tables on an in-memory connection (for tests in other modules).
#[cfg(test)]
pub fn migrate_for_tests(conn: &Connection) {
    migrate(conn).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // The bets tables are owned by the tournament module; the stats queries only need these columns.
        conn.execute_batch(
            "CREATE TABLE bets (id INTEGER PRIMARY KEY AUTOINCREMENT, user_id TEXT, guild_id TEXT, amount INTEGER, status TEXT, payout INTEGER);
             CREATE TABLE special_bets (id INTEGER PRIMARY KEY AUTOINCREMENT, user_id TEXT, guild_id TEXT, amount INTEGER, status TEXT, payout INTEGER);",
        )
        .unwrap();
        conn
    }

    // Wallet creation reads the global config; tests seed wallets by hand instead.
    fn seed_wallet(conn: &Connection, user: &str, guild: &str, balance: i64) {
        conn.execute(
            "INSERT INTO wallets (user_id, guild_id, balance) VALUES (?, ?, ?)",
            params![user, guild, balance],
        )
        .unwrap();
    }

    #[test]
    fn spend_and_add_update_wallet_and_log() {
        let conn = conn();
        seed_wallet(&conn, "u1", "g", 100);
        assert!(!spend_currency(&conn, "u1", "g", 101, "x", None).unwrap());
        assert!(spend_currency(&conn, "u1", "g", 40, "bet_place", Some("7")).unwrap());
        assert_eq!(add_currency(&conn, "u1", "g", 10, "bet_win", None).unwrap(), 70);
        let w = get_or_create_wallet(&conn, "u1", "g").unwrap();
        assert_eq!(w, Wallet { balance: 70, total_earned: 10, total_spent: 40 });
        let txs = get_wallet_transactions(&conn, "g", Some("u1"), 10).unwrap();
        assert_eq!(txs.len(), 2);
        assert_eq!(txs[0].amount, 10);
        assert_eq!(txs[1].amount, -40);
        assert_eq!(txs[1].reference_id.as_deref(), Some("7"));
    }

    #[test]
    fn add_zero_is_noop() {
        let conn = conn();
        seed_wallet(&conn, "u1", "g", 5);
        assert_eq!(add_currency(&conn, "u1", "g", 0, "x", None).unwrap(), 5);
        assert!(get_wallet_transactions(&conn, "g", None, 10).unwrap().is_empty());
    }

    #[test]
    fn transfer_checks_funds_and_self() {
        let conn = conn();
        seed_wallet(&conn, "a", "g", 50);
        seed_wallet(&conn, "b", "g", 0);
        assert!(!transfer_currency(&conn, "a", "a", "g", 5).unwrap());
        assert!(!transfer_currency(&conn, "a", "b", "g", 51).unwrap());
        assert!(transfer_currency(&conn, "a", "b", "g", 20).unwrap());
        assert_eq!(get_balance(&conn, "a", "g").unwrap(), 30);
        assert_eq!(get_balance(&conn, "b", "g").unwrap(), 20);
    }

    #[test]
    fn set_balance_logs_difference() {
        let conn = conn();
        seed_wallet(&conn, "a", "g", 50);
        set_balance(&conn, "a", "g", 80, "admin_set").unwrap();
        set_balance(&conn, "a", "g", 80, "admin_set").unwrap();
        set_balance(&conn, "a", "g", -5, "admin_set").unwrap();
        assert_eq!(get_balance(&conn, "a", "g").unwrap(), 80);
        let txs = get_wallet_transactions(&conn, "g", Some("a"), 10).unwrap();
        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].amount, 30);
    }

    #[test]
    fn poll_vote_reward_is_paid_once() {
        let conn = conn();
        seed_wallet(&conn, "u", "g", 0);
        assert_eq!(claim_poll_vote_reward(&conn, "m1", "u", "g", 5).unwrap(), Some(5));
        assert_eq!(claim_poll_vote_reward(&conn, "m1", "u", "g", 5).unwrap(), None);
        assert_eq!(claim_poll_vote_reward(&conn, "m2", "u", "g", 5).unwrap(), Some(10));
        assert_eq!(claim_poll_vote_reward(&conn, "m3", "u", "g", 0).unwrap(), None);
    }

    #[test]
    fn daily_claim_roundtrip() {
        let conn = conn();
        assert!(get_daily_claim(&conn, "u", "g").unwrap().is_none());
        let d = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        save_daily_claim(&conn, "u", "g", d, 3, 5).unwrap();
        save_daily_claim(&conn, "u", "g", d.succ_opt().unwrap(), 4, 5).unwrap();
        let row = get_daily_claim(&conn, "u", "g").unwrap().unwrap();
        assert_eq!(row.streak, 4);
        assert_eq!(row.best_streak, 5);
        assert_eq!(row.last_claim_date, NaiveDate::from_ymd_opt(2026, 1, 3).unwrap());
    }

    #[test]
    fn aggregates_and_profit_board() {
        let conn = conn();
        conn.execute_batch(
            "INSERT INTO bets (user_id, guild_id, amount, status, payout) VALUES
                ('x','g',100,'won',250), ('x','g',50,'lost',0), ('x','g',30,'pending',0), ('x','g',20,'refunded',0),
                ('y','g',10,'lost',0), ('x','other',999,'won',999);
             INSERT INTO special_bets (user_id, guild_id, amount, status, payout) VALUES
                ('x','g',40,'won',90), ('y','g',5,'won',60);",
        )
        .unwrap();
        let m = bet_aggregate(&conn, "bets", "g", "x").unwrap();
        assert_eq!((m.placed, m.won, m.lost, m.pending, m.refunded), (4, 1, 1, 1, 1));
        assert_eq!(m.total_wagered, 180);
        assert_eq!(m.resolved_wagered, 150);
        assert_eq!(m.total_paid_out, 250);
        assert_eq!(m.biggest_payout, 250);
        assert_eq!(m.net_profit(), 100);

        let s = bet_aggregate(&conn, "special_bets", "g", "x").unwrap();
        let total = m.combine(&s);
        assert_eq!(total.placed, 5);
        assert_eq!(total.net_profit(), 100 + 50);
        assert_eq!(total.biggest_payout, 250);

        let empty = bet_aggregate(&conn, "bets", "g", "nobody").unwrap();
        assert_eq!(empty, BetAggregate::default());

        let board = get_profit_leaderboard(&conn, "g", 10, 0).unwrap();
        assert_eq!(board.len(), 2);
        // x: (250-100) + (-50) + (90-40) = 150 ; y: -10 + (60-5) = 45
        assert_eq!((board[0].user_id.as_str(), board[0].net_profit), ("x", 150));
        assert_eq!((board[1].user_id.as_str(), board[1].net_profit), ("y", 45));
        assert_eq!((board[0].won, board[0].lost), (2, 1));
        assert_eq!(get_profit_leaderboard(&conn, "g", 10, 1).unwrap().len(), 1);
    }
}
