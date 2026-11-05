//! MugCoin wallet operations (pay, admin set/give/take, daily claim, stats, leaderboards).
//!
//! All functions take the user/guild ids as strings (Discord snowflakes, as stored in SQLite and
//! as exposed in the JSON API) and are synchronous: every call is a short SQLite operation.

// This is a service API: parts of it are consumed by the web dashboard only.
#![allow(dead_code)]

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::json;

use super::authz;
use crate::config;
use crate::db::economy::{
    self, AuditLogRow, BetAggregate, LeaderboardRow, ProfitLeaderboardRow, WalletTransactionRow,
};
use crate::db::Db;

fn can_use_admin_tools(user_id: &str) -> bool {
    user_id.parse::<u64>().map(authz::can_use_admin_tools).unwrap_or(false)
}

// ---------------------------------------------------------------------------------------------
// Basic wallet reads
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletSummary {
    pub user_id: String,
    pub guild_id: String,
    pub balance: i64,
}

pub fn get_wallet_summary(db: &Db, user_id: &str, guild_id: &str) -> anyhow::Result<WalletSummary> {
    let balance = db.tx(|tx| economy::get_balance(tx, user_id, guild_id))?;
    Ok(WalletSummary { user_id: user_id.to_string(), guild_id: guild_id.to_string(), balance })
}

pub fn get_balance(db: &Db, user_id: &str, guild_id: &str) -> anyhow::Result<i64> {
    db.tx(|tx| economy::get_balance(tx, user_id, guild_id))
}

pub fn get_wallet_leaderboard(db: &Db, guild_id: &str, limit: i64) -> anyhow::Result<Vec<LeaderboardRow>> {
    get_wallet_leaderboard_page(db, guild_id, limit, 0)
}

pub fn get_wallet_leaderboard_page(
    db: &Db,
    guild_id: &str,
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<LeaderboardRow>> {
    economy::get_leaderboard(&db.conn(), guild_id, limit, offset)
}

pub fn get_wallet_transactions(
    db: &Db,
    guild_id: &str,
    user_id: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<WalletTransactionRow>> {
    economy::get_wallet_transactions(&db.conn(), guild_id, user_id, limit)
}

pub fn get_audit_logs(db: &Db, limit: i64) -> anyhow::Result<Vec<AuditLogRow>> {
    economy::get_audit_logs(&db.conn(), limit)
}

// ---------------------------------------------------------------------------------------------
// Pay and admin operations
// ---------------------------------------------------------------------------------------------

/// Returns false for a non-positive amount, a self payment or insufficient funds.
pub fn pay_user(db: &Db, sender_id: &str, receiver_id: &str, guild_id: &str, amount: i64) -> anyhow::Result<bool> {
    if amount <= 0 {
        return Ok(false);
    }
    db.tx(|tx| economy::transfer_currency(tx, sender_id, receiver_id, guild_id, amount))
}

pub fn admin_set_user_balance(
    db: &Db,
    actor_user_id: &str,
    target_user_id: &str,
    guild_id: &str,
    amount: i64,
) -> anyhow::Result<bool> {
    if !can_use_admin_tools(actor_user_id) || amount < 0 {
        return Ok(false);
    }
    db.tx(|tx| {
        economy::set_balance(tx, target_user_id, guild_id, amount, "admin_set")?;
        economy::add_audit_log(
            tx,
            actor_user_id,
            "wallet_set",
            "wallet",
            Some(target_user_id),
            &json!({ "guildId": guild_id, "amount": amount }),
        )?;
        Ok(true)
    })
}

pub fn admin_give_user_balance(
    db: &Db,
    actor_user_id: &str,
    target_user_id: &str,
    guild_id: &str,
    amount: i64,
) -> anyhow::Result<bool> {
    if !can_use_admin_tools(actor_user_id) || amount <= 0 {
        return Ok(false);
    }
    db.tx(|tx| {
        economy::add_currency(tx, target_user_id, guild_id, amount, "admin_give", None)?;
        economy::add_audit_log(
            tx,
            actor_user_id,
            "wallet_give",
            "wallet",
            Some(target_user_id),
            &json!({ "guildId": guild_id, "amount": amount }),
        )?;
        Ok(true)
    })
}

/// Returns false when the target cannot afford the amount.
pub fn admin_take_user_balance(
    db: &Db,
    actor_user_id: &str,
    target_user_id: &str,
    guild_id: &str,
    amount: i64,
) -> anyhow::Result<bool> {
    if !can_use_admin_tools(actor_user_id) || amount <= 0 {
        return Ok(false);
    }
    db.tx(|tx| {
        let ok = economy::spend_currency(tx, target_user_id, guild_id, amount, "admin_take", None)?;
        if ok {
            economy::add_audit_log(
                tx,
                actor_user_id,
                "wallet_take",
                "wallet",
                Some(target_user_id),
                &json!({ "guildId": guild_id, "amount": amount }),
            )?;
        }
        Ok(ok)
    })
}

// ---------------------------------------------------------------------------------------------
// Daily claim
// ---------------------------------------------------------------------------------------------

/// `base + bonus * min(streak - 1, cap)`; `streak` is the streak including today's claim.
pub fn daily_reward(base: i64, bonus: i64, cap: i64, streak: i64) -> i64 {
    base + bonus * (streak - 1).clamp(0, cap.max(0))
}

/// The streak after claiming on `today`, or `None` if `today` was already claimed.
/// Consecutive UTC days extend the streak; a missed day resets it to 1.
pub fn next_streak(last_claim: Option<NaiveDate>, streak: i64, today: NaiveDate) -> Option<i64> {
    match last_claim {
        None => Some(1),
        Some(last) if last >= today => None,
        Some(last) if last.succ_opt() == Some(today) => Some(streak.max(0) + 1),
        Some(_) => Some(1),
    }
}

/// Start of the next UTC day after `today`.
pub fn next_claim_time(today: NaiveDate) -> DateTime<Utc> {
    let next = today.succ_opt().unwrap_or(today).and_hms_opt(0, 0, 0).expect("midnight is valid");
    Utc.from_utc_datetime(&next)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyClaimResult {
    /// False if the user had already claimed today (nothing was paid).
    pub claimed: bool,
    /// Coins paid by this call (0 when `claimed` is false).
    pub reward: i64,
    pub streak: i64,
    pub best_streak: i64,
    pub balance: i64,
    /// RFC 3339 time at which the next claim becomes available (next UTC midnight).
    pub next_claim_at: String,
    /// Unix seconds of `next_claim_at`, for Discord `<t:..:R>` timestamps.
    pub next_claim_unix: i64,
}

/// Daily claim on a given connection (call inside a transaction). Pure apart from the DB.
#[allow(clippy::too_many_arguments)]
pub fn claim_daily_conn(
    conn: &Connection,
    user_id: &str,
    guild_id: &str,
    today: NaiveDate,
    base: i64,
    bonus: i64,
    cap: i64,
) -> anyhow::Result<DailyClaimResult> {
    let existing = economy::get_daily_claim(conn, user_id, guild_id)?;
    let next_at = next_claim_time(today);
    let next_fields = |claimed: bool, reward: i64, streak: i64, best: i64, balance: i64| DailyClaimResult {
        claimed,
        reward,
        streak,
        best_streak: best,
        balance,
        next_claim_at: next_at.to_rfc3339(),
        next_claim_unix: next_at.timestamp(),
    };

    let (last, prev_streak, prev_best) = match &existing {
        Some(row) => (Some(row.last_claim_date), row.streak, row.best_streak),
        None => (None, 0, 0),
    };

    let Some(streak) = next_streak(last, prev_streak, today) else {
        let balance = economy::get_balance(conn, user_id, guild_id)?;
        return Ok(next_fields(false, 0, prev_streak, prev_best, balance));
    };

    let reward = daily_reward(base, bonus, cap, streak);
    let best = prev_best.max(streak);
    let balance = economy::add_currency(conn, user_id, guild_id, reward, "daily", None)?;
    economy::save_daily_claim(conn, user_id, guild_id, today, streak, best)?;
    Ok(next_fields(true, reward, streak, best, balance))
}

/// `/mugcoin daily`: once per UTC day, using the configured reward values.
pub fn claim_daily(db: &Db, user_id: &str, guild_id: &str) -> anyhow::Result<DailyClaimResult> {
    let cfg = &config::get().currency;
    let today = Utc::now().date_naive();
    db.tx(|tx| {
        claim_daily_conn(tx, user_id, guild_id, today, cfg.daily_base, cfg.daily_streak_bonus, cfg.daily_streak_cap)
    })
}

// ---------------------------------------------------------------------------------------------
// Poll vote reward
// ---------------------------------------------------------------------------------------------

/// Pays `currency.poll_vote_reward` once per (poll message, user). Returns the amount paid.
pub fn reward_poll_vote(db: &Db, message_id: &str, user_id: &str, guild_id: &str) -> anyhow::Result<Option<i64>> {
    let amount = config::get().currency.poll_vote_reward;
    let paid = db.tx(|tx| economy::claim_poll_vote_reward(tx, message_id, user_id, guild_id, amount))?;
    Ok(paid.map(|_| amount))
}

// ---------------------------------------------------------------------------------------------
// Bettor stats and the profit board
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BettorStats {
    pub user_id: String,
    pub guild_id: String,
    /// Match bets plus special bets.
    #[serde(flatten)]
    pub totals: BetAggregate,
    /// `total_paid_out - resolved_wagered`, i.e. payouts minus stakes over won/lost bets.
    pub net_profit: i64,
    /// `won / (won + lost)` as a fraction in 0..=1; null when nothing has resolved yet.
    pub win_rate: Option<f64>,
    pub match_bets: BetAggregate,
    pub special_bets: BetAggregate,
}

impl BettorStats {
    pub fn from_aggregates(user_id: &str, guild_id: &str, match_bets: BetAggregate, special_bets: BetAggregate) -> Self {
        let totals = match_bets.combine(&special_bets);
        BettorStats {
            user_id: user_id.to_string(),
            guild_id: guild_id.to_string(),
            net_profit: totals.net_profit(),
            win_rate: win_rate(totals.won, totals.lost),
            totals,
            match_bets,
            special_bets,
        }
    }
}

pub fn win_rate(won: i64, lost: i64) -> Option<f64> {
    let resolved = won + lost;
    (resolved > 0).then(|| won as f64 / resolved as f64)
}

pub fn get_bettor_stats(db: &Db, user_id: &str, guild_id: &str) -> anyhow::Result<BettorStats> {
    let conn = db.conn();
    let match_bets = economy::bet_aggregate(&conn, "bets", guild_id, user_id)?;
    let special_bets = economy::bet_aggregate(&conn, "special_bets", guild_id, user_id)?;
    Ok(BettorStats::from_aggregates(user_id, guild_id, match_bets, special_bets))
}

pub fn get_profit_leaderboard(
    db: &Db,
    guild_id: &str,
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<ProfitLeaderboardRow>> {
    economy::get_profit_leaderboard(&db.conn(), guild_id, limit, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn reward_scales_with_streak_up_to_the_cap() {
        // base 50, +10 per extra streak day, capped at 7 extra days
        assert_eq!(daily_reward(50, 10, 7, 1), 50);
        assert_eq!(daily_reward(50, 10, 7, 2), 60);
        assert_eq!(daily_reward(50, 10, 7, 8), 120);
        assert_eq!(daily_reward(50, 10, 7, 9), 120);
        assert_eq!(daily_reward(50, 10, 7, 100), 120);
        // degenerate inputs never go below the base
        assert_eq!(daily_reward(50, 10, 7, 0), 50);
        assert_eq!(daily_reward(50, 10, -3, 5), 50);
    }

    #[test]
    fn streak_rules() {
        let today = d(2026, 3, 10);
        assert_eq!(next_streak(None, 0, today), Some(1));
        assert_eq!(next_streak(Some(d(2026, 3, 9)), 4, today), Some(5));
        assert_eq!(next_streak(Some(d(2026, 3, 8)), 4, today), Some(1));
        assert_eq!(next_streak(Some(d(2025, 1, 1)), 9, today), Some(1));
        assert_eq!(next_streak(Some(today), 4, today), None);
        // a clock that went backwards still counts as already claimed
        assert_eq!(next_streak(Some(d(2026, 3, 11)), 4, today), None);
        // month and year boundaries are consecutive days
        assert_eq!(next_streak(Some(d(2026, 2, 28)), 2, d(2026, 3, 1)), Some(3));
        assert_eq!(next_streak(Some(d(2025, 12, 31)), 2, d(2026, 1, 1)), Some(3));
    }

    #[test]
    fn next_claim_is_next_utc_midnight() {
        let t = next_claim_time(d(2026, 3, 10));
        assert_eq!(t.to_rfc3339(), "2026-03-11T00:00:00+00:00");
        assert_eq!(t.timestamp() % 86_400, 0);
    }

    fn conn_with_wallet(balance: i64) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        economy::migrate_for_tests(&conn);
        conn.execute(
            "INSERT INTO wallets (user_id, guild_id, balance) VALUES ('u', 'g', ?)",
            [balance],
        )
        .unwrap();
        conn
    }

    #[test]
    fn claim_daily_pays_once_per_day_and_tracks_streaks() {
        let conn = conn_with_wallet(0);
        let (base, bonus, cap) = (50, 10, 2);

        let r = claim_daily_conn(&conn, "u", "g", d(2026, 3, 10), base, bonus, cap).unwrap();
        assert!(r.claimed);
        assert_eq!((r.reward, r.streak, r.best_streak, r.balance), (50, 1, 1, 50));
        assert_eq!(r.next_claim_at, "2026-03-11T00:00:00+00:00");

        // same UTC day: nothing paid, streak untouched
        let again = claim_daily_conn(&conn, "u", "g", d(2026, 3, 10), base, bonus, cap).unwrap();
        assert!(!again.claimed);
        assert_eq!((again.reward, again.streak, again.balance), (0, 1, 50));

        // consecutive days build the streak up to the cap
        let r2 = claim_daily_conn(&conn, "u", "g", d(2026, 3, 11), base, bonus, cap).unwrap();
        assert_eq!((r2.reward, r2.streak), (60, 2));
        let r3 = claim_daily_conn(&conn, "u", "g", d(2026, 3, 12), base, bonus, cap).unwrap();
        assert_eq!((r3.reward, r3.streak), (70, 3));
        let r4 = claim_daily_conn(&conn, "u", "g", d(2026, 3, 13), base, bonus, cap).unwrap();
        assert_eq!((r4.reward, r4.streak, r4.best_streak), (70, 4, 4));

        // a missed day resets the streak but keeps the best one
        let r5 = claim_daily_conn(&conn, "u", "g", d(2026, 3, 15), base, bonus, cap).unwrap();
        assert_eq!((r5.reward, r5.streak, r5.best_streak), (50, 1, 4));
        assert_eq!(r5.balance, 50 + 60 + 70 + 70 + 50);

        let txs = economy::get_wallet_transactions(&conn, "g", Some("u"), 20).unwrap();
        assert_eq!(txs.len(), 5);
        assert!(txs.iter().all(|t| t.reason == "daily"));
    }

    #[test]
    fn bettor_stats_math() {
        let m = BetAggregate {
            placed: 4,
            won: 2,
            lost: 1,
            pending: 1,
            refunded: 0,
            total_wagered: 400,
            resolved_wagered: 300,
            total_paid_out: 500,
            biggest_payout: 350,
        };
        let s = BetAggregate {
            placed: 1,
            won: 0,
            lost: 1,
            pending: 0,
            refunded: 0,
            total_wagered: 50,
            resolved_wagered: 50,
            total_paid_out: 0,
            biggest_payout: 0,
        };
        let stats = BettorStats::from_aggregates("u", "g", m, s);
        assert_eq!(stats.totals.placed, 5);
        assert_eq!(stats.net_profit, 500 - 350);
        assert_eq!(stats.win_rate, Some(2.0 / 4.0));
        assert_eq!(stats.totals.biggest_payout, 350);

        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["userId"], "u");
        assert_eq!(json["netProfit"], 150);
        assert_eq!(json["totalWagered"], 450);
        assert_eq!(json["matchBets"]["placed"], 4);
        assert_eq!(json["specialBets"]["lost"], 1);

        let empty = BettorStats::from_aggregates("u", "g", BetAggregate::default(), BetAggregate::default());
        assert_eq!(empty.win_rate, None);
        assert_eq!(empty.net_profit, 0);
        assert_eq!(win_rate(3, 1), Some(0.75));
    }
}
