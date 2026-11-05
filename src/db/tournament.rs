//! Tournaments, rounds, matches, bets, special markets, match polls.
//!
//! Like `economy`, everything here is a free function over `&Connection` so the service layer
//! can run multi-step operations (bet placement, match resolution, tournament end) inside one
//! `Db::tx` transaction.

// Some of these are only used by the web dashboard / tests.
#![allow(dead_code)]

use anyhow::bail;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use super::economy::{add_currency, spend_currency};

pub const MARKET_TOURNAMENT_WINNER: &str = "tournament_winner";
pub const MARKET_FINAL_RUNNER_UP: &str = "final_runner_up";

pub const POLL_ACTIVE: &str = "active";
pub const POLL_RESOLVED: &str = "resolved";
pub const POLL_TIED: &str = "tied";
pub const POLL_CANCELLED: &str = "cancelled";

// ---------------------------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TournamentSummary {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchRecord {
    pub id: i64,
    pub tournament_id: i64,
    pub match_number: i64,
    pub round_number: i64,
    pub contestant_a: String,
    pub contestant_b: String,
    pub winner: Option<String>,
    pub betting_open: bool,
}

impl MatchRecord {
    /// "Round R, Match M: A vs B"
    pub fn label(&self) -> String {
        format!(
            "Round {}, Match {}: {} vs {}",
            self.round_number, self.match_number, self.contestant_a, self.contestant_b
        )
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpecialMarketRecord {
    pub id: i64,
    pub tournament_id: i64,
    #[serde(rename = "type")]
    pub market_type: String,
    pub round_number: Option<i64>,
    pub match_id: Option<i64>,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpecialBetRecord {
    pub id: i64,
    pub market_id: i64,
    pub tournament_id: i64,
    pub user_id: String,
    pub guild_id: String,
    pub picked: String,
    pub amount: i64,
    pub status: String,
    pub payout: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserSpecialBet {
    #[serde(flatten)]
    pub bet: SpecialBetRecord,
    pub market_type: String,
    pub round_number: Option<i64>,
    pub match_id: Option<i64>,
    pub market_status: String,
}

/// A bet joined with its match (the TS `getUserBets` row).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMatchBet {
    pub match_id: i64,
    pub picked: String,
    pub amount: i64,
    pub status: String,
    pub payout: i64,
    pub match_number: i64,
    pub round_number: i64,
    pub contestant_a: String,
    pub contestant_b: String,
    pub winner: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchBetRow {
    pub user_id: String,
    pub picked: String,
    pub amount: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PayoutRow {
    pub user_id: String,
    pub bet_amount: i64,
    pub payout: i64,
}

/// Result of resolving a match or a special market.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolutionSummary {
    pub total_pool: i64,
    pub winner_pool: i64,
    pub payouts: Vec<PayoutRow>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchPollRecord {
    pub id: i64,
    pub match_id: i64,
    pub tournament_id: i64,
    pub guild_id: String,
    pub channel_id: String,
    pub message_id: String,
    pub expires_at: String,
    pub status: String,
    pub created_at: String,
}

// ---------------------------------------------------------------------------------------------
// Pure math
// ---------------------------------------------------------------------------------------------

/// Parimutuel payout: `floor(amount / winner_pool * total_pool)`, computed exactly in integers.
pub fn parimutuel_payout(amount: i64, winner_pool: i64, total_pool: i64) -> i64 {
    if winner_pool <= 0 {
        return 0;
    }
    ((amount as i128 * total_pool as i128) / winner_pool as i128) as i64
}

// ---------------------------------------------------------------------------------------------
// Tournaments and matches
// ---------------------------------------------------------------------------------------------

pub fn get_tournament_guild_id(conn: &Connection, tournament_id: i64) -> anyhow::Result<String> {
    Ok(conn
        .query_row("SELECT guild_id FROM tournaments WHERE id = ?", [tournament_id], |r| r.get::<_, String>(0))
        .optional()?
        .unwrap_or_default())
}

#[derive(Debug, Clone)]
pub struct TournamentRecord {
    pub id: i64,
    pub guild_id: String,
    pub name: String,
    pub status: String,
}

pub fn get_tournament(conn: &Connection, tournament_id: i64) -> anyhow::Result<Option<TournamentRecord>> {
    Ok(conn
        .query_row(
            "SELECT id, guild_id, name, status FROM tournaments WHERE id = ?",
            [tournament_id],
            |r| Ok(TournamentRecord { id: r.get(0)?, guild_id: r.get(1)?, name: r.get(2)?, status: r.get(3)? }),
        )
        .optional()?)
}

pub fn create_tournament(conn: &Connection, guild_id: &str, name: &str) -> anyhow::Result<i64> {
    conn.execute("INSERT INTO tournaments (guild_id, name) VALUES (?, ?)", params![guild_id, name])?;
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO tournament_rounds (tournament_id, round_number, status) VALUES (?, 1, 'active')",
        [id],
    )?;
    Ok(id)
}

pub fn add_tournament_match(
    conn: &Connection,
    tournament_id: i64,
    match_number: i64,
    contestant_a: &str,
    contestant_b: &str,
    round_number: i64,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO tournament_matches (tournament_id, match_number, round_number, contestant_a, contestant_b) VALUES (?, ?, ?, ?, ?)",
        params![tournament_id, match_number, round_number, contestant_a, contestant_b],
    )?;
    Ok(())
}

pub fn get_active_tournament(conn: &Connection, guild_id: &str) -> anyhow::Result<Option<TournamentSummary>> {
    Ok(conn
        .query_row(
            "SELECT id, name FROM tournaments WHERE guild_id = ? AND status = 'active' ORDER BY id DESC LIMIT 1",
            [guild_id],
            |r| Ok(TournamentSummary { id: r.get(0)?, name: r.get(1)? }),
        )
        .optional()?)
}

fn map_match(r: &rusqlite::Row) -> rusqlite::Result<MatchRecord> {
    Ok(MatchRecord {
        id: r.get(0)?,
        tournament_id: r.get(1)?,
        match_number: r.get(2)?,
        round_number: r.get(3)?,
        contestant_a: r.get(4)?,
        contestant_b: r.get(5)?,
        winner: r.get(6)?,
        betting_open: r.get::<_, i64>(7)? == 1,
    })
}

const MATCH_COLUMNS: &str =
    "id, tournament_id, match_number, round_number, contestant_a, contestant_b, winner, betting_open";

/// Matches of a tournament ordered by round then match number; optionally one round only.
pub fn get_tournament_matches(
    conn: &Connection,
    tournament_id: i64,
    round_number: Option<i64>,
) -> anyhow::Result<Vec<MatchRecord>> {
    let rows = match round_number {
        None => {
            let mut stmt = conn.prepare(&format!(
                "SELECT {MATCH_COLUMNS} FROM tournament_matches WHERE tournament_id = ? ORDER BY round_number ASC, match_number ASC"
            ))?;
            let rows = stmt.query_map([tournament_id], map_match)?.collect::<Result<Vec<_>, _>>()?;
            rows
        }
        Some(round) => {
            let mut stmt = conn.prepare(&format!(
                "SELECT {MATCH_COLUMNS} FROM tournament_matches WHERE tournament_id = ? AND round_number = ? ORDER BY match_number ASC"
            ))?;
            let rows = stmt.query_map(params![tournament_id, round], map_match)?.collect::<Result<Vec<_>, _>>()?;
            rows
        }
    };
    Ok(rows)
}

pub fn get_match(conn: &Connection, match_id: i64) -> anyhow::Result<Option<MatchRecord>> {
    Ok(conn
        .query_row(
            &format!("SELECT {MATCH_COLUMNS} FROM tournament_matches WHERE id = ?"),
            [match_id],
            map_match,
        )
        .optional()?)
}

/// Match numbers repeat every round; like the TS bot this returns the one in the highest round.
pub fn get_match_by_number(conn: &Connection, tournament_id: i64, match_number: i64) -> anyhow::Result<Option<MatchRecord>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {MATCH_COLUMNS} FROM tournament_matches WHERE tournament_id = ? AND match_number = ? ORDER BY round_number DESC LIMIT 1"
            ),
            params![tournament_id, match_number],
            map_match,
        )
        .optional()?)
}

pub fn get_current_round_number(conn: &Connection, tournament_id: i64) -> anyhow::Result<i64> {
    let round: i64 = conn.query_row(
        "SELECT COALESCE(MAX(round_number), 1) FROM tournament_matches WHERE tournament_id = ?",
        [tournament_id],
        |r| r.get(0),
    )?;
    Ok(round.max(1))
}

pub fn create_next_round_from_winners(conn: &Connection, tournament_id: i64) -> anyhow::Result<(i64, i64)> {
    let current_round = get_current_round_number(conn, tournament_id)?;
    let current_matches = get_tournament_matches(conn, tournament_id, Some(current_round))?;
    if current_matches.is_empty() {
        bail!("No matches found in the current round.");
    }
    if !current_matches.iter().all(|m| m.winner.is_some()) {
        bail!("All matches in the current round must be resolved before advancing.");
    }
    if current_matches.len() < 2 {
        bail!("No further round can be created from a single resolved match.");
    }

    let next_round = current_round + 1;
    if !get_tournament_matches(conn, tournament_id, Some(next_round))?.is_empty() {
        bail!("The next round has already been created.");
    }

    let winners: Vec<String> = current_matches.iter().filter_map(|m| m.winner.clone()).collect();
    if !winners.len().is_multiple_of(2) {
        bail!("Winner count must be even to advance the round.");
    }

    conn.execute(
        "INSERT OR REPLACE INTO tournament_rounds (tournament_id, round_number, status) VALUES (?, ?, 'completed')",
        params![tournament_id, current_round],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO tournament_rounds (tournament_id, round_number, status) VALUES (?, ?, 'active')",
        params![tournament_id, next_round],
    )?;

    let mut next_match_number = 1;
    for pair in winners.chunks(2) {
        add_tournament_match(conn, tournament_id, next_match_number, &pair[0], &pair[1], next_round)?;
        next_match_number += 1;
    }
    Ok((next_round, (winners.len() / 2) as i64))
}

pub fn close_betting_for_match(conn: &Connection, match_id: i64) -> anyhow::Result<()> {
    conn.execute("UPDATE tournament_matches SET betting_open = 0 WHERE id = ?", [match_id])?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Match bets
// ---------------------------------------------------------------------------------------------

/// Spends the stake and records the bet. Returns false (writing nothing) when the amount is
/// invalid, the user already bet on this match or cannot afford it. Must run in a transaction.
pub fn place_bet(
    conn: &Connection,
    tournament_id: i64,
    match_id: i64,
    user_id: &str,
    guild_id: &str,
    picked: &str,
    amount: i64,
) -> anyhow::Result<bool> {
    if amount <= 0 {
        return Ok(false);
    }
    let duplicate = conn
        .query_row(
            "SELECT 1 FROM bets WHERE tournament_id = ? AND match_id = ? AND user_id = ?",
            params![tournament_id, match_id, user_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if duplicate {
        return Ok(false);
    }
    if !spend_currency(conn, user_id, guild_id, amount, "bet_place", Some(&match_id.to_string()))? {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO bets (tournament_id, match_id, user_id, guild_id, picked, amount) VALUES (?, ?, ?, ?, ?, ?)",
        params![tournament_id, match_id, user_id, guild_id, picked, amount],
    )?;
    Ok(true)
}

pub fn get_bets_for_match(conn: &Connection, tournament_id: i64, match_id: i64) -> anyhow::Result<Vec<MatchBetRow>> {
    let mut stmt = conn.prepare("SELECT user_id, picked, amount FROM bets WHERE tournament_id = ? AND match_id = ?")?;
    let rows = stmt
        .query_map(params![tournament_id, match_id], |r| {
            Ok(MatchBetRow { user_id: r.get(0)?, picked: r.get(1)?, amount: r.get(2)? })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Number of bets per match id for a tournament.
pub fn get_bet_counts(conn: &Connection, tournament_id: i64) -> anyhow::Result<Vec<(i64, i64)>> {
    let mut stmt =
        conn.prepare("SELECT match_id, COUNT(*) FROM bets WHERE tournament_id = ? GROUP BY match_id")?;
    let rows = stmt
        .query_map([tournament_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn get_user_bets(conn: &Connection, tournament_id: i64, user_id: &str) -> anyhow::Result<Vec<UserMatchBet>> {
    let mut stmt = conn.prepare(
        "SELECT b.match_id, b.picked, b.amount, b.status, b.payout, m.match_number, m.round_number, m.contestant_a, m.contestant_b, m.winner
         FROM bets b
         JOIN tournament_matches m ON b.match_id = m.id
         WHERE b.tournament_id = ? AND b.user_id = ?
         ORDER BY m.round_number ASC, m.match_number ASC",
    )?;
    let rows = stmt
        .query_map(params![tournament_id, user_id], |r| {
            Ok(UserMatchBet {
                match_id: r.get(0)?,
                picked: r.get(1)?,
                amount: r.get(2)?,
                status: r.get(3)?,
                payout: r.get(4)?,
                match_number: r.get(5)?,
                round_number: r.get(6)?,
                contestant_a: r.get(7)?,
                contestant_b: r.get(8)?,
                winner: r.get(9)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Marks the match resolved and pays the parimutuel winners. Does not check that the match is
/// unresolved; callers do that inside the same transaction.
pub fn resolve_match_payouts(
    conn: &Connection,
    tournament_id: i64,
    match_id: i64,
    winner: &str,
) -> anyhow::Result<ResolutionSummary> {
    close_betting_for_match(conn, match_id)?;
    conn.execute("UPDATE tournament_matches SET winner = ? WHERE id = ?", params![winner, match_id])?;

    let bets = get_bets_for_match(conn, tournament_id, match_id)?;
    let total_pool: i64 = bets.iter().map(|b| b.amount).sum();
    let winner_pool: i64 = bets.iter().filter(|b| b.picked == winner).map(|b| b.amount).sum();
    let guild_id = get_tournament_guild_id(conn, tournament_id)?;
    let match_ref = match_id.to_string();

    let mut payouts = Vec::new();
    for bet in &bets {
        if bet.picked == winner {
            let payout = parimutuel_payout(bet.amount, winner_pool, total_pool);
            conn.execute(
                "UPDATE bets SET status = 'won', payout = ? WHERE tournament_id = ? AND match_id = ? AND user_id = ?",
                params![payout, tournament_id, match_id, bet.user_id],
            )?;
            if !guild_id.is_empty() {
                add_currency(conn, &bet.user_id, &guild_id, payout, "bet_win", Some(&match_ref))?;
            }
            payouts.push(PayoutRow { user_id: bet.user_id.clone(), bet_amount: bet.amount, payout });
        } else {
            conn.execute(
                "UPDATE bets SET status = 'lost' WHERE tournament_id = ? AND match_id = ? AND user_id = ?",
                params![tournament_id, match_id, bet.user_id],
            )?;
        }
    }
    Ok(ResolutionSummary { total_pool, winner_pool, payouts })
}

/// Completes the tournament, refunds every pending match and special bet, cancels open
/// special markets and any still-active match polls.
pub fn end_tournament(conn: &Connection, tournament_id: i64) -> anyhow::Result<()> {
    conn.execute("UPDATE tournaments SET status = 'completed' WHERE id = ?", [tournament_id])?;

    let pending: Vec<(i64, String, String, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT match_id, user_id, guild_id, amount FROM bets WHERE tournament_id = ? AND status = 'pending'",
        )?;
        let rows = stmt
            .query_map([tournament_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    for (match_id, user_id, guild_id, amount) in pending {
        add_currency(conn, &user_id, &guild_id, amount, "bet_refund_ended", Some(&match_id.to_string()))?;
        conn.execute(
            "UPDATE bets SET status = 'refunded' WHERE tournament_id = ? AND match_id = ? AND user_id = ?",
            params![tournament_id, match_id, user_id],
        )?;
    }

    refund_pending_special_bets(conn, tournament_id)?;
    cancel_polls_for_tournament(conn, tournament_id)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Special markets
// ---------------------------------------------------------------------------------------------

fn map_market(r: &rusqlite::Row) -> rusqlite::Result<SpecialMarketRecord> {
    Ok(SpecialMarketRecord {
        id: r.get(0)?,
        tournament_id: r.get(1)?,
        market_type: r.get(2)?,
        round_number: r.get(3)?,
        match_id: r.get(4)?,
        status: r.get(5)?,
        created_at: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
    })
}

const MARKET_COLUMNS: &str = "id, tournament_id, type, round_number, match_id, status, created_at";

/// Creates the market if it does not exist yet (one per tournament and type) and returns its id.
pub fn create_special_market(
    conn: &Connection,
    tournament_id: i64,
    market_type: &str,
    round_number: Option<i64>,
    match_id: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    conn.execute(
        "INSERT OR IGNORE INTO tournament_special_markets (tournament_id, type, round_number, match_id) VALUES (?, ?, ?, ?)",
        params![tournament_id, market_type, round_number, match_id],
    )?;
    let market_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM tournament_special_markets WHERE tournament_id = ? AND type = ? LIMIT 1",
            params![tournament_id, market_type],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = market_id {
        conn.execute(
            "UPDATE tournament_special_markets SET round_number = COALESCE(?, round_number), match_id = COALESCE(?, match_id) WHERE id = ?",
            params![round_number, match_id, id],
        )?;
    }
    Ok(market_id)
}

pub fn replace_special_market_options(conn: &Connection, market_id: i64, options: &[String]) -> anyhow::Result<()> {
    conn.execute("DELETE FROM tournament_special_market_options WHERE market_id = ?", [market_id])?;
    let mut seen: Vec<String> = Vec::new();
    for option in options {
        let trimmed = option.trim();
        if trimmed.is_empty() || seen.iter().any(|s| s == trimmed) {
            continue;
        }
        seen.push(trimmed.to_string());
        conn.execute(
            "INSERT OR IGNORE INTO tournament_special_market_options (market_id, option_name) VALUES (?, ?)",
            params![market_id, trimmed],
        )?;
    }
    Ok(())
}

pub fn get_special_market_options(conn: &Connection, market_id: i64) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT option_name FROM tournament_special_market_options WHERE market_id = ? ORDER BY id ASC")?;
    let rows = stmt.query_map([market_id], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn get_special_markets(conn: &Connection, tournament_id: i64) -> anyhow::Result<Vec<SpecialMarketRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MARKET_COLUMNS} FROM tournament_special_markets WHERE tournament_id = ? ORDER BY id ASC"
    ))?;
    let rows = stmt.query_map([tournament_id], map_market)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn get_special_market(conn: &Connection, market_id: i64) -> anyhow::Result<Option<SpecialMarketRecord>> {
    Ok(conn
        .query_row(
            &format!("SELECT {MARKET_COLUMNS} FROM tournament_special_markets WHERE id = ?"),
            [market_id],
            map_market,
        )
        .optional()?)
}

pub fn get_special_market_by_type(
    conn: &Connection,
    tournament_id: i64,
    market_type: &str,
) -> anyhow::Result<Option<SpecialMarketRecord>> {
    Ok(conn
        .query_row(
            &format!("SELECT {MARKET_COLUMNS} FROM tournament_special_markets WHERE tournament_id = ? AND type = ? LIMIT 1"),
            params![tournament_id, market_type],
            map_market,
        )
        .optional()?)
}

/// Sets the market's round number and status (the only two fields the TS bot ever updated).
pub fn update_special_market(
    conn: &Connection,
    market_id: i64,
    round_number: Option<i64>,
    status: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE tournament_special_markets SET round_number = ?, status = ? WHERE id = ?",
        params![round_number, status, market_id],
    )?;
    Ok(())
}

pub fn close_special_market(conn: &Connection, market_id: i64) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE tournament_special_markets SET status = 'closed' WHERE id = ? AND status = 'open'",
        [market_id],
    )?;
    Ok(())
}

pub fn close_special_market_by_type(conn: &Connection, tournament_id: i64, market_type: &str) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE tournament_special_markets SET status = 'closed' WHERE tournament_id = ? AND type = ? AND status = 'open'",
        params![tournament_id, market_type],
    )?;
    Ok(())
}

/// Same contract as `place_bet`, for special markets.
pub fn place_special_bet(
    conn: &Connection,
    market_id: i64,
    tournament_id: i64,
    user_id: &str,
    guild_id: &str,
    picked: &str,
    amount: i64,
) -> anyhow::Result<bool> {
    if amount <= 0 {
        return Ok(false);
    }
    let duplicate = conn
        .query_row(
            "SELECT 1 FROM special_bets WHERE market_id = ? AND user_id = ?",
            params![market_id, user_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if duplicate {
        return Ok(false);
    }
    if !spend_currency(conn, user_id, guild_id, amount, "special_bet_place", Some(&market_id.to_string()))? {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO special_bets (market_id, tournament_id, user_id, guild_id, picked, amount) VALUES (?, ?, ?, ?, ?, ?)",
        params![market_id, tournament_id, user_id, guild_id, picked, amount],
    )?;
    Ok(true)
}

fn map_special_bet(r: &rusqlite::Row, offset: usize) -> rusqlite::Result<SpecialBetRecord> {
    Ok(SpecialBetRecord {
        id: r.get(offset)?,
        market_id: r.get(offset + 1)?,
        tournament_id: r.get(offset + 2)?,
        user_id: r.get(offset + 3)?,
        guild_id: r.get(offset + 4)?,
        picked: r.get(offset + 5)?,
        amount: r.get(offset + 6)?,
        status: r.get(offset + 7)?,
        payout: r.get(offset + 8)?,
        created_at: r.get::<_, Option<String>>(offset + 9)?.unwrap_or_default(),
    })
}

pub fn get_special_bets_for_market(conn: &Connection, market_id: i64) -> anyhow::Result<Vec<SpecialBetRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, market_id, tournament_id, user_id, guild_id, picked, amount, status, payout, created_at
         FROM special_bets WHERE market_id = ?",
    )?;
    let rows = stmt.query_map([market_id], |r| map_special_bet(r, 0))?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn get_user_special_bets(conn: &Connection, tournament_id: i64, user_id: &str) -> anyhow::Result<Vec<UserSpecialBet>> {
    let mut stmt = conn.prepare(
        "SELECT b.id, b.market_id, b.tournament_id, b.user_id, b.guild_id, b.picked, b.amount, b.status, b.payout, b.created_at,
                m.type, m.round_number, m.match_id, m.status
         FROM special_bets b
         JOIN tournament_special_markets m ON b.market_id = m.id
         WHERE b.tournament_id = ? AND b.user_id = ?
         ORDER BY b.id ASC",
    )?;
    let rows = stmt
        .query_map(params![tournament_id, user_id], |r| {
            Ok(UserSpecialBet {
                bet: map_special_bet(r, 0)?,
                market_type: r.get(10)?,
                round_number: r.get(11)?,
                match_id: r.get(12)?,
                market_status: r.get(13)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Closes and resolves a special market, paying parimutuel winners. Returns `None` when the
/// market does not exist.
pub fn resolve_special_market(
    conn: &Connection,
    market_id: i64,
    winner: &str,
) -> anyhow::Result<Option<ResolutionSummary>> {
    let Some(market) = get_special_market(conn, market_id)? else {
        return Ok(None);
    };
    close_special_market(conn, market_id)?;

    let bets = get_special_bets_for_market(conn, market_id)?;
    let guild_id = get_tournament_guild_id(conn, market.tournament_id)?;
    let total_pool: i64 = bets.iter().map(|b| b.amount).sum();
    let winner_pool: i64 = bets.iter().filter(|b| b.picked == winner).map(|b| b.amount).sum();
    let market_ref = market_id.to_string();

    let mut payouts = Vec::new();
    for bet in &bets {
        if bet.picked == winner {
            let payout = parimutuel_payout(bet.amount, winner_pool, total_pool);
            conn.execute("UPDATE special_bets SET status = 'won', payout = ? WHERE id = ?", params![payout, bet.id])?;
            if !guild_id.is_empty() {
                add_currency(conn, &bet.user_id, &guild_id, payout, "special_bet_win", Some(&market_ref))?;
            }
            payouts.push(PayoutRow { user_id: bet.user_id.clone(), bet_amount: bet.amount, payout });
        } else {
            conn.execute("UPDATE special_bets SET status = 'lost' WHERE id = ?", [bet.id])?;
        }
    }

    conn.execute("UPDATE tournament_special_markets SET status = 'resolved' WHERE id = ?", [market_id])?;
    Ok(Some(ResolutionSummary { total_pool, winner_pool, payouts }))
}

/// Refunds pending special bets and cancels the tournament's open/closed special markets.
pub fn refund_pending_special_bets(conn: &Connection, tournament_id: i64) -> anyhow::Result<()> {
    let pending: Vec<(i64, i64, String, String, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT id, market_id, user_id, guild_id, amount FROM special_bets WHERE tournament_id = ? AND status = 'pending'",
        )?;
        let rows = stmt
            .query_map([tournament_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    for (bet_id, market_id, user_id, guild_id, amount) in pending {
        add_currency(conn, &user_id, &guild_id, amount, "special_bet_refund_ended", Some(&market_id.to_string()))?;
        conn.execute("UPDATE special_bets SET status = 'refunded' WHERE id = ?", [bet_id])?;
    }
    conn.execute(
        "UPDATE tournament_special_markets SET status = 'cancelled' WHERE tournament_id = ? AND status IN ('open', 'closed')",
        [tournament_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Match polls
// ---------------------------------------------------------------------------------------------

const POLL_COLUMNS: &str = "id, match_id, tournament_id, guild_id, channel_id, message_id, expires_at, status, created_at";

fn map_poll(r: &rusqlite::Row) -> rusqlite::Result<MatchPollRecord> {
    Ok(MatchPollRecord {
        id: r.get(0)?,
        match_id: r.get(1)?,
        tournament_id: r.get(2)?,
        guild_id: r.get(3)?,
        channel_id: r.get(4)?,
        message_id: r.get(5)?,
        expires_at: r.get(6)?,
        status: r.get(7)?,
        created_at: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
    })
}

pub fn insert_match_poll(
    conn: &Connection,
    match_id: i64,
    tournament_id: i64,
    guild_id: &str,
    channel_id: &str,
    message_id: &str,
    expires_at: &str,
) -> anyhow::Result<i64> {
    conn.execute(
        "INSERT INTO tournament_match_polls (match_id, tournament_id, guild_id, channel_id, message_id, expires_at, status)
         VALUES (?, ?, ?, ?, ?, ?, 'active')",
        params![match_id, tournament_id, guild_id, channel_id, message_id, expires_at],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_match_poll_by_message(conn: &Connection, message_id: &str) -> anyhow::Result<Option<MatchPollRecord>> {
    Ok(conn
        .query_row(
            &format!("SELECT {POLL_COLUMNS} FROM tournament_match_polls WHERE message_id = ?"),
            [message_id],
            map_poll,
        )
        .optional()?)
}

pub fn get_active_poll_for_match(conn: &Connection, match_id: i64) -> anyhow::Result<Option<MatchPollRecord>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {POLL_COLUMNS} FROM tournament_match_polls WHERE match_id = ? AND status = 'active' ORDER BY id DESC LIMIT 1"
            ),
            [match_id],
            map_poll,
        )
        .optional()?)
}

/// All polls of a tournament, oldest first (so a later poll of the same match wins in a map).
pub fn get_polls_for_tournament(conn: &Connection, tournament_id: i64) -> anyhow::Result<Vec<MatchPollRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {POLL_COLUMNS} FROM tournament_match_polls WHERE tournament_id = ? ORDER BY id ASC"
    ))?;
    let rows = stmt.query_map([tournament_id], map_poll)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Active polls whose `expires_at` (UTC `YYYY-MM-DD HH:MM:SS`) is not after `now`.
pub fn list_due_active_polls(conn: &Connection, now: &str) -> anyhow::Result<Vec<MatchPollRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {POLL_COLUMNS} FROM tournament_match_polls WHERE status = 'active' AND expires_at <= ? ORDER BY id ASC"
    ))?;
    let rows = stmt.query_map([now], map_poll)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Moves a poll from one status to another; returns whether a row changed.
pub fn transition_poll_status(conn: &Connection, message_id: &str, from: &str, to: &str) -> anyhow::Result<bool> {
    let changed = conn.execute(
        "UPDATE tournament_match_polls SET status = ? WHERE message_id = ? AND status = ?",
        params![to, message_id, from],
    )?;
    Ok(changed > 0)
}

pub fn set_poll_status(conn: &Connection, message_id: &str, to: &str) -> anyhow::Result<()> {
    conn.execute("UPDATE tournament_match_polls SET status = ? WHERE message_id = ?", params![to, message_id])?;
    Ok(())
}

/// Cancels polls of a match that can no longer decide it (active or tied).
pub fn cancel_polls_for_match(conn: &Connection, match_id: i64) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE tournament_match_polls SET status = 'cancelled' WHERE match_id = ? AND status IN ('active', 'tied')",
        [match_id],
    )?;
    Ok(())
}

pub fn cancel_polls_for_tournament(conn: &Connection, tournament_id: i64) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE tournament_match_polls SET status = 'cancelled' WHERE tournament_id = ? AND status IN ('active', 'tied')",
        [tournament_id],
    )?;
    Ok(())
}

pub(super) fn migrate(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS tournaments (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            guild_id TEXT NOT NULL,
            name TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active',
            created_at TEXT DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE IF NOT EXISTS tournament_rounds (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tournament_id INTEGER NOT NULL,
            round_number INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'active',
            created_at TEXT DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(tournament_id, round_number),
            FOREIGN KEY (tournament_id) REFERENCES tournaments(id)
        );
        CREATE TABLE IF NOT EXISTS tournament_matches (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tournament_id INTEGER NOT NULL,
            match_number INTEGER NOT NULL,
            round_number INTEGER NOT NULL DEFAULT 1,
            contestant_a TEXT NOT NULL,
            contestant_b TEXT NOT NULL,
            winner TEXT,
            betting_open INTEGER NOT NULL DEFAULT 1,
            FOREIGN KEY (tournament_id) REFERENCES tournaments(id)
        );
        CREATE TABLE IF NOT EXISTS bets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tournament_id INTEGER NOT NULL,
            match_id INTEGER NOT NULL,
            user_id TEXT NOT NULL,
            guild_id TEXT NOT NULL,
            picked TEXT NOT NULL,
            amount INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            payout INTEGER NOT NULL DEFAULT 0,
            created_at TEXT DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(tournament_id, match_id, user_id),
            FOREIGN KEY (tournament_id) REFERENCES tournaments(id),
            FOREIGN KEY (match_id) REFERENCES tournament_matches(id)
        );
        CREATE TABLE IF NOT EXISTS tournament_special_markets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tournament_id INTEGER NOT NULL,
            type TEXT NOT NULL,
            round_number INTEGER,
            match_id INTEGER,
            status TEXT NOT NULL DEFAULT 'open',
            created_at TEXT DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(tournament_id, type),
            FOREIGN KEY (tournament_id) REFERENCES tournaments(id),
            FOREIGN KEY (match_id) REFERENCES tournament_matches(id)
        );
        CREATE TABLE IF NOT EXISTS special_bets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            market_id INTEGER NOT NULL,
            tournament_id INTEGER NOT NULL,
            user_id TEXT NOT NULL,
            guild_id TEXT NOT NULL,
            picked TEXT NOT NULL,
            amount INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            payout INTEGER NOT NULL DEFAULT 0,
            created_at TEXT DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(market_id, user_id),
            FOREIGN KEY (market_id) REFERENCES tournament_special_markets(id),
            FOREIGN KEY (tournament_id) REFERENCES tournaments(id)
        );
        CREATE TABLE IF NOT EXISTS tournament_special_market_options (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            market_id INTEGER NOT NULL,
            option_name TEXT NOT NULL,
            created_at TEXT DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(market_id, option_name),
            FOREIGN KEY (market_id) REFERENCES tournament_special_markets(id)
        );

        -- New: Discord polls that decide a match. status: active | resolved | tied | cancelled.
        -- expires_at is UTC 'YYYY-MM-DD HH:MM:SS'.
        CREATE TABLE IF NOT EXISTS tournament_match_polls (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            match_id INTEGER NOT NULL,
            tournament_id INTEGER NOT NULL,
            guild_id TEXT NOT NULL,
            channel_id TEXT NOT NULL,
            message_id TEXT NOT NULL UNIQUE,
            expires_at TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active',
            created_at TEXT DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (match_id) REFERENCES tournament_matches(id)
        );
        CREATE INDEX IF NOT EXISTS idx_match_polls_match ON tournament_match_polls (match_id);
        CREATE INDEX IF NOT EXISTS idx_match_polls_status ON tournament_match_polls (status, expires_at);
        ",
    )?;
    // Very old databases predate rounds.
    super::add_column_if_missing(conn, "tournament_matches", "round_number", "INTEGER NOT NULL DEFAULT 1")?;
    Ok(())
}

/// Creates the tournament tables on an in-memory connection (for tests in other modules).
#[cfg(test)]
pub fn migrate_for_tests(conn: &Connection) {
    migrate(conn).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::economy::{self, get_balance};

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        economy::migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        conn
    }

    fn wallet(conn: &Connection, user: &str, balance: i64) {
        conn.execute(
            "INSERT INTO wallets (user_id, guild_id, balance) VALUES (?, 'g', ?)",
            params![user, balance],
        )
        .unwrap();
    }

    fn bal(conn: &Connection, user: &str) -> i64 {
        get_balance(conn, user, "g").unwrap()
    }

    #[test]
    fn payout_is_exact_integer_math() {
        assert_eq!(parimutuel_payout(100, 100, 300), 300);
        assert_eq!(parimutuel_payout(50, 150, 400), 133);
        assert_eq!(parimutuel_payout(0, 100, 300), 0);
        assert_eq!(parimutuel_payout(10, 0, 300), 0);
        // (1 / 49) * 49 is 0.9999999999999999 in floating point; integer math pays the full 1.
        assert_eq!(parimutuel_payout(1, 49, 49), 1);
        assert_eq!(parimutuel_payout(1, 3, 3), 1);
    }

    #[test]
    fn match_flow_pays_parimutuel_and_ends() {
        let conn = setup();
        let tid = create_tournament(&conn, "g", "Cup").unwrap();
        add_tournament_match(&conn, tid, 1, "A", "B", 1).unwrap();
        let m = get_match_by_number(&conn, tid, 1).unwrap().unwrap();
        assert!(m.betting_open && m.winner.is_none());

        wallet(&conn, "u1", 1000);
        wallet(&conn, "u2", 1000);
        wallet(&conn, "u3", 1000);
        assert!(place_bet(&conn, tid, m.id, "u1", "g", "A", 100).unwrap());
        assert!(place_bet(&conn, tid, m.id, "u2", "g", "A", 200).unwrap());
        assert!(place_bet(&conn, tid, m.id, "u3", "g", "B", 300).unwrap());
        // duplicate bet and insufficient funds are rejected without side effects
        assert!(!place_bet(&conn, tid, m.id, "u1", "g", "B", 5).unwrap());
        assert_eq!(bal(&conn, "u1"), 900);
        wallet(&conn, "poor", 10);
        assert!(!place_bet(&conn, tid, m.id, "poor", "g", "A", 11).unwrap());
        assert_eq!(bal(&conn, "poor"), 10);

        let summary = resolve_match_payouts(&conn, tid, m.id, "A").unwrap();
        assert_eq!((summary.total_pool, summary.winner_pool), (600, 300));
        assert_eq!(summary.payouts.len(), 2);
        assert_eq!(bal(&conn, "u1"), 900 + 200);
        assert_eq!(bal(&conn, "u2"), 800 + 400);
        assert_eq!(bal(&conn, "u3"), 700);
        let bets = get_user_bets(&conn, tid, "u3").unwrap();
        assert_eq!(bets[0].status, "lost");
        let resolved = get_match(&conn, m.id).unwrap().unwrap();
        assert_eq!(resolved.winner.as_deref(), Some("A"));
        assert!(!resolved.betting_open);
    }

    #[test]
    fn nobody_on_winner_loses_everything_like_the_ts_bot() {
        let conn = setup();
        let tid = create_tournament(&conn, "g", "Cup").unwrap();
        add_tournament_match(&conn, tid, 1, "A", "B", 1).unwrap();
        let m = get_match_by_number(&conn, tid, 1).unwrap().unwrap();
        wallet(&conn, "u1", 100);
        assert!(place_bet(&conn, tid, m.id, "u1", "g", "B", 100).unwrap());
        let summary = resolve_match_payouts(&conn, tid, m.id, "A").unwrap();
        assert_eq!(summary.winner_pool, 0);
        assert!(summary.payouts.is_empty());
        assert_eq!(bal(&conn, "u1"), 0);
    }

    #[test]
    fn end_tournament_refunds_pending_and_cancels() {
        let conn = setup();
        let tid = create_tournament(&conn, "g", "Cup").unwrap();
        add_tournament_match(&conn, tid, 1, "A", "B", 1).unwrap();
        let m = get_match_by_number(&conn, tid, 1).unwrap().unwrap();
        let market = create_special_market(&conn, tid, MARKET_TOURNAMENT_WINNER, Some(1), None).unwrap().unwrap();
        wallet(&conn, "u1", 100);
        assert!(place_bet(&conn, tid, m.id, "u1", "g", "A", 40).unwrap());
        assert!(place_special_bet(&conn, market, tid, "u1", "g", "B", 30).unwrap());
        assert!(!place_special_bet(&conn, market, tid, "u1", "g", "A", 5).unwrap());
        insert_match_poll(&conn, m.id, tid, "g", "c", "msg", "2999-01-01 00:00:00").unwrap();
        assert_eq!(bal(&conn, "u1"), 30);

        end_tournament(&conn, tid).unwrap();
        assert_eq!(bal(&conn, "u1"), 100);
        assert!(get_active_tournament(&conn, "g").unwrap().is_none());
        assert_eq!(get_special_market(&conn, market).unwrap().unwrap().status, "cancelled");
        assert_eq!(get_match_poll_by_message(&conn, "msg").unwrap().unwrap().status, POLL_CANCELLED);
    }

    #[test]
    fn next_round_pairs_winners_in_order() {
        let conn = setup();
        let tid = create_tournament(&conn, "g", "Cup").unwrap();
        for (i, (a, b)) in [("A", "B"), ("C", "D")].iter().enumerate() {
            add_tournament_match(&conn, tid, i as i64 + 1, a, b, 1).unwrap();
        }
        assert!(create_next_round_from_winners(&conn, tid).is_err());
        let ms = get_tournament_matches(&conn, tid, Some(1)).unwrap();
        resolve_match_payouts(&conn, tid, ms[0].id, "B").unwrap();
        resolve_match_payouts(&conn, tid, ms[1].id, "C").unwrap();
        let (round, count) = create_next_round_from_winners(&conn, tid).unwrap();
        assert_eq!((round, count), (2, 1));
        let next = get_tournament_matches(&conn, tid, Some(2)).unwrap();
        assert_eq!((next[0].contestant_a.as_str(), next[0].contestant_b.as_str()), ("B", "C"));
        assert_eq!(get_current_round_number(&conn, tid).unwrap(), 2);
        // match numbers repeat across rounds: lookup returns the highest round
        assert_eq!(get_match_by_number(&conn, tid, 1).unwrap().unwrap().round_number, 2);
        // a single-match round cannot advance
        resolve_match_payouts(&conn, tid, next[0].id, "B").unwrap();
        assert!(create_next_round_from_winners(&conn, tid).is_err());
    }

    #[test]
    fn special_market_lifecycle() {
        let conn = setup();
        let tid = create_tournament(&conn, "g", "Cup").unwrap();
        let id = create_special_market(&conn, tid, MARKET_FINAL_RUNNER_UP, Some(3), None).unwrap().unwrap();
        // idempotent per (tournament, type)
        assert_eq!(create_special_market(&conn, tid, MARKET_FINAL_RUNNER_UP, Some(4), None).unwrap(), Some(id));
        assert_eq!(get_special_market(&conn, id).unwrap().unwrap().round_number, Some(4));
        replace_special_market_options(&conn, id, &["X".into(), " X ".into(), "Y".into(), "".into()]).unwrap();
        assert_eq!(get_special_market_options(&conn, id).unwrap(), vec!["X".to_string(), "Y".to_string()]);

        wallet(&conn, "u1", 100);
        wallet(&conn, "u2", 100);
        assert!(place_special_bet(&conn, id, tid, "u1", "g", "X", 60).unwrap());
        assert!(place_special_bet(&conn, id, tid, "u2", "g", "Y", 20).unwrap());
        let r = resolve_special_market(&conn, id, "Y").unwrap().unwrap();
        assert_eq!((r.total_pool, r.winner_pool), (80, 20));
        assert_eq!(bal(&conn, "u2"), 80 + 80);
        assert_eq!(bal(&conn, "u1"), 40);
        assert_eq!(get_special_market(&conn, id).unwrap().unwrap().status, "resolved");
        assert!(resolve_special_market(&conn, 9999, "Y").unwrap().is_none());
    }

    #[test]
    fn poll_lifecycle() {
        let conn = setup();
        let tid = create_tournament(&conn, "g", "Cup").unwrap();
        add_tournament_match(&conn, tid, 1, "A", "B", 1).unwrap();
        let m = get_match_by_number(&conn, tid, 1).unwrap().unwrap();
        insert_match_poll(&conn, m.id, tid, "g", "c", "m1", "2000-01-01 00:00:00").unwrap();
        insert_match_poll(&conn, m.id, tid, "g", "c", "m2", "2999-01-01 00:00:00").unwrap();
        assert_eq!(get_active_poll_for_match(&conn, m.id).unwrap().unwrap().message_id, "m2");
        let due = list_due_active_polls(&conn, "2026-01-01 00:00:00").unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].message_id, "m1");
        assert!(transition_poll_status(&conn, "m1", POLL_ACTIVE, POLL_TIED).unwrap());
        assert!(!transition_poll_status(&conn, "m1", POLL_ACTIVE, POLL_RESOLVED).unwrap());
        assert!(list_due_active_polls(&conn, "2026-01-01 00:00:00").unwrap().is_empty());
        cancel_polls_for_match(&conn, m.id).unwrap();
        assert_eq!(get_match_poll_by_message(&conn, "m1").unwrap().unwrap().status, POLL_CANCELLED);
        assert!(get_active_poll_for_match(&conn, m.id).unwrap().is_none());
        // message_id is unique
        assert!(insert_match_poll(&conn, m.id, tid, "g", "c", "m1", "2999-01-01 00:00:00").is_err());
    }
}
