//! Tournament rules: brackets, rounds, betting markets, poll-backed resolution.
//!
//! Port of `services/tournaments.ts`. Every public function that moves money runs inside one
//! SQLite transaction (`Db::tx`), so a failure never leaves a half-applied payout. The functions
//! are synchronous and take plain string ids so the slash commands and the web API share them.
//!
//! The `*_conn` / `*_in_tx` functions work on a `&Connection` and never touch the global config
//! or permissions, which keeps them unit-testable against an in-memory database.

// This is a service API: parts of it are consumed by the web dashboard only.
#![allow(dead_code)]

use std::collections::HashMap;

use anyhow::{anyhow, bail};
use poise::serenity_prelude as serenity;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::authz::{self, UserRole};
use crate::config;
use crate::db::economy::{self, WalletTransactionRow};
use crate::db::tournament::{
    self as t, MatchBetRow, MatchPollRecord, MatchRecord, ResolutionSummary, SpecialMarketRecord, TournamentRecord,
    TournamentSummary, UserMatchBet, MARKET_FINAL_RUNNER_UP, MARKET_TOURNAMENT_WINNER, POLL_ACTIVE, POLL_CANCELLED,
    POLL_RESOLVED, POLL_TIED,
};
use crate::db::Db;

const NO_PERMISSION: &str = "You do not have permission to manage tournaments.";
const NO_ACTIVE_TOURNAMENT: &str = "No active tournament.";
const BET_FAILED: &str = "Bet failed. You may not have enough MugCoins or you already placed a bet.";

/// Discord limits for poll content.
pub const POLL_QUESTION_MAX: usize = 300;
pub const POLL_ANSWER_MAX: usize = 55;

fn parse_id(id: &str) -> u64 {
    id.parse().unwrap_or(0)
}

fn require_manage(actor_user_id: &str) -> anyhow::Result<()> {
    if authz::can_manage_tournaments(parse_id(actor_user_id)) {
        Ok(())
    } else {
        bail!(NO_PERMISSION)
    }
}

pub fn get_user_role(user_id: &str) -> UserRole {
    authz::user_role(parse_id(user_id))
}

// ---------------------------------------------------------------------------------------------
// View structs (JSON shapes match the TypeScript API)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TournamentUserBetView {
    pub picked: String,
    pub amount: i64,
    pub status: String,
    pub payout: i64,
}

/// Link to the Discord poll attached to a match (new; ignored by the existing frontend).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchPollView {
    pub message_id: String,
    pub channel_id: String,
    pub guild_id: String,
    pub url: String,
    /// active | resolved | tied
    pub status: String,
    /// UTC `YYYY-MM-DD HH:MM:SS`
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TournamentMatchView {
    pub id: i64,
    pub tournament_id: i64,
    pub match_number: i64,
    pub round_number: i64,
    pub contestant_a: String,
    pub contestant_b: String,
    pub winner: Option<String>,
    pub betting_open: bool,
    pub user_bet: Option<TournamentUserBetView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll: Option<MatchPollView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TournamentSpecialMarketView {
    pub id: i64,
    pub tournament_id: i64,
    #[serde(rename = "type")]
    pub market_type: String,
    pub title: String,
    pub description: String,
    pub status: String,
    pub round_number: Option<i64>,
    pub match_id: Option<i64>,
    pub options: Vec<String>,
    pub user_bet: Option<TournamentUserBetView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TournamentSpecialBetView {
    pub market_id: i64,
    pub market_type: String,
    pub title: String,
    pub picked: String,
    pub amount: i64,
    pub status: String,
    pub payout: i64,
    pub round_number: Option<i64>,
    pub match_id: Option<i64>,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TournamentOverview {
    pub tournament: Option<TournamentSummary>,
    pub role: UserRole,
    pub wallet_balance: i64,
    pub current_round_number: Option<i64>,
    pub matches: Vec<TournamentMatchView>,
    pub special_markets: Vec<TournamentSpecialMarketView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentUserBets {
    pub match_bets: Vec<UserMatchBet>,
    pub special_bets: Vec<TournamentSpecialBetView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedSpecialMarket {
    #[serde(rename = "type")]
    pub market_type: String,
    pub winning_pick: String,
}

/// Result of resolving a match (`/api/operator/resolve` returns this object as is).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveMatchResult {
    /// The match as it was before resolution (winner still null).
    #[serde(rename = "match")]
    pub match_: MatchRecord,
    pub winner: String,
    pub summary: ResolutionSummary,
    pub bets: Vec<MatchBetRow>,
    pub tournament_ended: bool,
    pub tournament_name: String,
    pub resolved_special_markets: Vec<ResolvedSpecialMarket>,
}

/// Result of resolving the runner-up market (`/api/operator/resolve-runner-up`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveRunnerUpResult {
    pub market: SpecialMarketRecord,
    pub picked: String,
    pub result: Option<ResolutionSummary>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvanceRoundResult {
    pub round_number: i64,
    pub match_count: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BracketMatchInput {
    pub a: String,
    pub b: String,
}

/// Validates a parsed bracket JSON document with the same messages as the TS command.
pub fn parse_bracket_json(value: &serde_json::Value) -> anyhow::Result<Vec<BracketMatchInput>> {
    let Some(items) = value.as_array() else {
        bail!("JSON root must be an array of matches.");
    };
    if items.is_empty() {
        bail!("Bracket array is empty.");
    }
    items
        .iter()
        .map(|item| {
            let a = item.get("a").and_then(|v| v.as_str());
            let b = item.get("b").and_then(|v| v.as_str());
            match (a, b) {
                (Some(a), Some(b)) => Ok(BracketMatchInput { a: a.to_string(), b: b.to_string() }),
                _ => Err(anyhow!("Each match must have string fields \"a\" and \"b\".")),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Special market helpers
// ---------------------------------------------------------------------------------------------

fn build_tournament_winner_options(conn: &Connection, tournament_id: i64) -> anyhow::Result<Vec<String>> {
    let mut entrants: Vec<String> = Vec::new();
    for m in t::get_tournament_matches(conn, tournament_id, Some(1))? {
        if !entrants.contains(&m.contestant_a) {
            entrants.push(m.contestant_a);
        }
        if !entrants.contains(&m.contestant_b) {
            entrants.push(m.contestant_b);
        }
    }
    Ok(entrants)
}

fn special_market_options(
    conn: &Connection,
    tournament_id: i64,
    market_type: &str,
    market_id: i64,
) -> anyhow::Result<Vec<String>> {
    if market_type == MARKET_TOURNAMENT_WINNER {
        build_tournament_winner_options(conn, tournament_id)
    } else {
        t::get_special_market_options(conn, market_id)
    }
}

pub fn special_market_title(market_type: &str) -> &'static str {
    if market_type == MARKET_TOURNAMENT_WINNER {
        "Tournament Winner"
    } else {
        "Final Runner-Up"
    }
}

fn special_market_description(market_type: &str, options: &[String]) -> String {
    if market_type == MARKET_TOURNAMENT_WINNER {
        return "Pick the champion before round 1 starts.".to_string();
    }
    if options.len() == 2 {
        return format!(
            "Pick the third-place finisher from the semi-final losers: {} or {}.",
            options[0], options[1]
        );
    }
    "Pick the third-place finisher from the semi-final losers.".to_string()
}

fn build_special_market_view(
    conn: &Connection,
    market: &SpecialMarketRecord,
    user_bet: Option<TournamentUserBetView>,
) -> anyhow::Result<TournamentSpecialMarketView> {
    let options = special_market_options(conn, market.tournament_id, &market.market_type, market.id)?;
    Ok(TournamentSpecialMarketView {
        id: market.id,
        tournament_id: market.tournament_id,
        market_type: market.market_type.clone(),
        title: special_market_title(&market.market_type).to_string(),
        description: special_market_description(&market.market_type, &options),
        status: market.status.clone(),
        round_number: market.round_number,
        match_id: market.match_id,
        options,
        user_bet,
    })
}

fn ensure_tournament_winner_market(conn: &Connection, tournament_id: i64) -> anyhow::Result<()> {
    t::create_special_market(conn, tournament_id, MARKET_TOURNAMENT_WINNER, Some(1), None)?;
    Ok(())
}

fn get_round_losers(conn: &Connection, tournament_id: i64, round_number: i64) -> anyhow::Result<Vec<String>> {
    let matches = t::get_tournament_matches(conn, tournament_id, Some(round_number))?;
    if matches.len() != 2 || !matches.iter().all(|m| m.winner.is_some()) {
        return Ok(Vec::new());
    }
    Ok(matches
        .iter()
        .map(|m| {
            if m.winner.as_deref() == Some(m.contestant_a.as_str()) {
                m.contestant_b.clone()
            } else {
                m.contestant_a.clone()
            }
        })
        .collect())
}

fn ensure_final_runner_up_market(
    conn: &Connection,
    tournament_id: i64,
    final_round_number: i64,
    semifinal_losers: &[String],
) -> anyhow::Result<()> {
    if semifinal_losers.len() != 2 {
        return Ok(());
    }
    let Some(market_id) =
        t::create_special_market(conn, tournament_id, MARKET_FINAL_RUNNER_UP, Some(final_round_number), None)?
    else {
        return Ok(());
    };
    t::update_special_market(conn, market_id, Some(final_round_number), "open")?;
    t::replace_special_market_options(conn, market_id, semifinal_losers)?;
    Ok(())
}

fn close_tournament_winner_market_if_needed(conn: &Connection, tournament_id: i64, round_number: i64) -> anyhow::Result<()> {
    if round_number == 1 {
        t::close_special_market_by_type(conn, tournament_id, MARKET_TOURNAMENT_WINNER)?;
    }
    Ok(())
}

fn finalize_tournament_winner_if_needed(conn: &Connection, tournament_id: i64, winner: &str) -> anyhow::Result<()> {
    if let Some(market) = t::get_special_market_by_type(conn, tournament_id, MARKET_TOURNAMENT_WINNER)? {
        if market.status != "resolved" && market.status != "cancelled" {
            t::resolve_special_market(conn, market.id, winner)?;
        }
    }
    Ok(())
}

fn require_active_tournament(conn: &Connection, guild_id: &str) -> anyhow::Result<TournamentSummary> {
    t::get_active_tournament(conn, guild_id)?.ok_or_else(|| anyhow!(NO_ACTIVE_TOURNAMENT))
}

fn audit(
    conn: &Connection,
    actor: &str,
    action: &str,
    target_type: &str,
    target_id: i64,
    metadata: serde_json::Value,
) -> anyhow::Result<()> {
    economy::add_audit_log(conn, actor, action, target_type, Some(&target_id.to_string()), &metadata)
}

fn user_bet_view(picked: &str, amount: i64, status: &str, payout: i64) -> TournamentUserBetView {
    TournamentUserBetView { picked: picked.to_string(), amount, status: status.to_string(), payout }
}

pub fn poll_url(guild_id: &str, channel_id: &str, message_id: &str) -> String {
    format!("https://discord.com/channels/{guild_id}/{channel_id}/{message_id}")
}

fn poll_view(poll: &MatchPollRecord) -> MatchPollView {
    MatchPollView {
        message_id: poll.message_id.clone(),
        channel_id: poll.channel_id.clone(),
        guild_id: poll.guild_id.clone(),
        url: poll_url(&poll.guild_id, &poll.channel_id, &poll.message_id),
        status: poll.status.clone(),
        expires_at: poll.expires_at.clone(),
    }
}

// ---------------------------------------------------------------------------------------------
// Open / read
// ---------------------------------------------------------------------------------------------

pub fn open_tournament_from_bracket(
    db: &Db,
    actor_user_id: &str,
    guild_id: &str,
    name: &str,
    matches: &[BracketMatchInput],
) -> anyhow::Result<i64> {
    require_manage(actor_user_id)?;
    db.tx(|tx| open_tournament_in_tx(tx, actor_user_id, guild_id, name, matches))
}

fn open_tournament_in_tx(
    conn: &Connection,
    actor_user_id: &str,
    guild_id: &str,
    name: &str,
    matches: &[BracketMatchInput],
) -> anyhow::Result<i64> {
    if let Some(active) = t::get_active_tournament(conn, guild_id)? {
        bail!("A tournament is already active: {}", active.name);
    }
    if matches.is_empty() {
        bail!("Bracket must contain at least one match.");
    }
    for (index, m) in matches.iter().enumerate() {
        if m.a.trim().is_empty() || m.b.trim().is_empty() {
            bail!("Invalid match at index {index}.");
        }
    }

    let tournament_id = t::create_tournament(conn, guild_id, name)?;
    for (index, m) in matches.iter().enumerate() {
        t::add_tournament_match(conn, tournament_id, index as i64 + 1, m.a.trim(), m.b.trim(), 1)?;
    }
    ensure_tournament_winner_market(conn, tournament_id)?;
    audit(
        conn,
        actor_user_id,
        "tournament_open",
        "tournament",
        tournament_id,
        json!({ "guildId": guild_id, "name": name, "matchCount": matches.len() }),
    )?;
    Ok(tournament_id)
}

pub fn get_tournament_overview(db: &Db, guild_id: &str, user_id: &str) -> anyhow::Result<TournamentOverview> {
    let conn = db.conn();
    overview_conn(&conn, guild_id, user_id)
}

fn overview_conn(conn: &Connection, guild_id: &str, user_id: &str) -> anyhow::Result<TournamentOverview> {
    let active = t::get_active_tournament(conn, guild_id)?;
    let role = get_user_role(user_id);
    let wallet_balance = economy::get_balance(conn, user_id, guild_id)?;

    let Some(active) = active else {
        return Ok(TournamentOverview {
            tournament: None,
            role,
            wallet_balance,
            current_round_number: None,
            matches: Vec::new(),
            special_markets: Vec::new(),
        });
    };

    let user_bets: HashMap<i64, UserMatchBet> =
        t::get_user_bets(conn, active.id, user_id)?.into_iter().map(|b| (b.match_id, b)).collect();
    // Later polls of the same match replace earlier ones; cancelled polls are not shown.
    let mut polls: HashMap<i64, MatchPollRecord> = HashMap::new();
    for poll in t::get_polls_for_tournament(conn, active.id)? {
        if poll.status != POLL_CANCELLED {
            polls.insert(poll.match_id, poll);
        }
    }

    let matches = t::get_tournament_matches(conn, active.id, None)?
        .into_iter()
        .map(|m| TournamentMatchView {
            user_bet: user_bets.get(&m.id).map(|b| user_bet_view(&b.picked, b.amount, &b.status, b.payout)),
            poll: polls.get(&m.id).map(poll_view),
            id: m.id,
            tournament_id: active.id,
            match_number: m.match_number,
            round_number: m.round_number,
            contestant_a: m.contestant_a,
            contestant_b: m.contestant_b,
            winner: m.winner,
            betting_open: m.betting_open,
        })
        .collect();

    let special_bets: HashMap<i64, _> =
        t::get_user_special_bets(conn, active.id, user_id)?.into_iter().map(|b| (b.bet.market_id, b)).collect();
    let mut special_markets = Vec::new();
    for market in t::get_special_markets(conn, active.id)? {
        let user_bet = special_bets
            .get(&market.id)
            .map(|b| user_bet_view(&b.bet.picked, b.bet.amount, &b.bet.status, b.bet.payout));
        special_markets.push(build_special_market_view(conn, &market, user_bet)?);
    }

    Ok(TournamentOverview {
        current_round_number: Some(t::get_current_round_number(conn, active.id)?),
        tournament: Some(active),
        role,
        wallet_balance,
        matches,
        special_markets,
    })
}

pub fn get_current_user_bets(db: &Db, guild_id: &str, user_id: &str) -> anyhow::Result<CurrentUserBets> {
    let conn = db.conn();
    let Some(active) = t::get_active_tournament(&conn, guild_id)? else {
        return Ok(CurrentUserBets { match_bets: Vec::new(), special_bets: Vec::new() });
    };

    let mut special_bets = Vec::new();
    for item in t::get_user_special_bets(&conn, active.id, user_id)? {
        special_bets.push(TournamentSpecialBetView {
            market_id: item.bet.market_id,
            title: special_market_title(&item.market_type).to_string(),
            picked: item.bet.picked.clone(),
            amount: item.bet.amount,
            status: item.bet.status.clone(),
            payout: item.bet.payout,
            round_number: item.round_number,
            match_id: item.match_id,
            options: special_market_options(&conn, active.id, &item.market_type, item.bet.market_id)?,
            market_type: item.market_type,
        });
    }
    Ok(CurrentUserBets { match_bets: t::get_user_bets(&conn, active.id, user_id)?, special_bets })
}

pub fn get_active_tournament(db: &Db, guild_id: &str) -> anyhow::Result<Option<TournamentSummary>> {
    t::get_active_tournament(&db.conn(), guild_id)
}

/// Looks a match up by number in the active tournament (the highest round wins, as in the TS bot).
pub fn find_match_by_number(db: &Db, guild_id: &str, match_number: i64) -> anyhow::Result<MatchRecord> {
    let conn = db.conn();
    let active = require_active_tournament(&conn, guild_id)?;
    t::get_match_by_number(&conn, active.id, match_number)?.ok_or_else(|| anyhow!("Match not found."))
}

/// Number of bets placed per match id.
pub fn get_match_bet_counts(db: &Db, tournament_id: i64) -> anyhow::Result<HashMap<i64, i64>> {
    Ok(t::get_bet_counts(&db.conn(), tournament_id)?.into_iter().collect())
}

pub fn get_tournament_transactions(
    db: &Db,
    guild_id: &str,
    user_id: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<WalletTransactionRow>> {
    economy::get_wallet_transactions(&db.conn(), guild_id, user_id, limit)
}

// ---------------------------------------------------------------------------------------------
// Betting
// ---------------------------------------------------------------------------------------------

pub fn place_tournament_bet(
    db: &Db,
    guild_id: &str,
    user_id: &str,
    match_id: i64,
    picked: &str,
    amount: i64,
) -> anyhow::Result<()> {
    if amount <= 0 {
        bail!("Amount must be greater than zero.");
    }
    db.tx(|tx| {
        let active = require_active_tournament(tx, guild_id)?;
        let m = t::get_match(tx, match_id)?
            .filter(|m| m.tournament_id == active.id)
            .ok_or_else(|| anyhow!("Match not found in the active tournament."))?;
        if !m.betting_open {
            bail!("Betting is closed for this match.");
        }
        if picked != m.contestant_a && picked != m.contestant_b {
            bail!("Invalid contestant selection.");
        }
        if !t::place_bet(tx, active.id, match_id, user_id, guild_id, picked, amount)? {
            bail!(BET_FAILED);
        }
        Ok(())
    })
}

pub fn place_tournament_special_bet(
    db: &Db,
    guild_id: &str,
    user_id: &str,
    market_id: i64,
    picked: &str,
    amount: i64,
) -> anyhow::Result<()> {
    if amount <= 0 {
        bail!("Amount must be greater than zero.");
    }
    db.tx(|tx| {
        let active = require_active_tournament(tx, guild_id)?;
        let market = t::get_special_market(tx, market_id)?
            .filter(|m| m.tournament_id == active.id)
            .ok_or_else(|| anyhow!("Betting market not found."))?;
        if market.status != "open" {
            bail!("Betting is closed for this market.");
        }
        let options = special_market_options(tx, active.id, &market.market_type, market.id)?;
        if !options.iter().any(|o| o == picked) {
            bail!("Invalid contestant selection.");
        }
        if !t::place_special_bet(tx, market.id, active.id, user_id, guild_id, picked, amount)? {
            bail!(BET_FAILED);
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------------------------
// Operator actions
// ---------------------------------------------------------------------------------------------

/// Closes betting for one match (and the winner futures market when it is a round 1 match).
fn close_match_betting(conn: &Connection, tournament_id: i64, m: &MatchRecord) -> anyhow::Result<()> {
    t::close_betting_for_match(conn, m.id)?;
    close_tournament_winner_market_if_needed(conn, tournament_id, m.round_number)
}

fn match_view(m: MatchRecord) -> TournamentMatchView {
    TournamentMatchView {
        id: m.id,
        tournament_id: m.tournament_id,
        match_number: m.match_number,
        round_number: m.round_number,
        contestant_a: m.contestant_a,
        contestant_b: m.contestant_b,
        winner: m.winner,
        betting_open: m.betting_open,
        user_bet: None,
        poll: None,
    }
}

pub fn close_tournament_match(
    db: &Db,
    actor_user_id: &str,
    guild_id: &str,
    match_number: i64,
) -> anyhow::Result<TournamentMatchView> {
    require_manage(actor_user_id)?;
    db.tx(|tx| {
        let active = require_active_tournament(tx, guild_id)?;
        let mut m = t::get_match_by_number(tx, active.id, match_number)?.ok_or_else(|| anyhow!("Match not found."))?;
        if !m.betting_open {
            bail!("Betting is already closed for this match.");
        }
        close_match_betting(tx, active.id, &m)?;
        audit(
            tx,
            actor_user_id,
            "tournament_close_match",
            "match",
            m.id,
            json!({ "guildId": guild_id, "matchNumber": match_number, "tournamentId": active.id }),
        )?;
        m.betting_open = false;
        Ok(match_view(m))
    })
}

pub fn close_final_runner_up_market(
    db: &Db,
    actor_user_id: &str,
    guild_id: &str,
) -> anyhow::Result<TournamentSpecialMarketView> {
    require_manage(actor_user_id)?;
    db.tx(|tx| {
        let active = require_active_tournament(tx, guild_id)?;
        let market = t::get_special_market_by_type(tx, active.id, MARKET_FINAL_RUNNER_UP)?
            .ok_or_else(|| anyhow!("Final Runner-Up market not found."))?;
        if market.status != "open" {
            bail!("Final Runner-Up betting is already closed.");
        }
        t::close_special_market(tx, market.id)?;
        audit(
            tx,
            actor_user_id,
            "tournament_close_runner_up",
            "special_market",
            market.id,
            json!({ "guildId": guild_id, "tournamentId": active.id }),
        )?;
        let market = t::get_special_market(tx, market.id)?.ok_or_else(|| anyhow!("Final Runner-Up market not found."))?;
        let user_bet = t::get_user_special_bets(tx, active.id, actor_user_id)?
            .into_iter()
            .find(|b| b.bet.market_id == market.id)
            .map(|b| user_bet_view(&b.bet.picked, b.bet.amount, &b.bet.status, b.bet.payout));
        build_special_market_view(tx, &market, user_bet)
    })
}

/// Closes every open match plus the dependent futures markets. Returns the closed match count.
pub fn close_all_tournament_matches(db: &Db, actor_user_id: &str, guild_id: &str) -> anyhow::Result<usize> {
    require_manage(actor_user_id)?;
    db.tx(|tx| {
        let active = require_active_tournament(tx, guild_id)?;
        let mut closed = 0;
        let mut has_round_one = false;
        for m in t::get_tournament_matches(tx, active.id, None)? {
            if m.round_number == 1 {
                has_round_one = true;
            }
            if m.betting_open {
                t::close_betting_for_match(tx, m.id)?;
                closed += 1;
            }
        }
        if has_round_one {
            t::close_special_market_by_type(tx, active.id, MARKET_TOURNAMENT_WINNER)?;
        }
        if let Some(market) = t::get_special_market_by_type(tx, active.id, MARKET_FINAL_RUNNER_UP)? {
            if market.status == "open" {
                t::close_special_market(tx, market.id)?;
            }
        }
        audit(
            tx,
            actor_user_id,
            "tournament_close_all",
            "tournament",
            active.id,
            json!({ "guildId": guild_id, "closedCount": closed }),
        )?;
        Ok(closed)
    })
}

/// Shared resolution path for manual resolves and poll results: closes the winner market after
/// a round 1 match, pays the match, auto-ends the tournament after the final, writes the audit
/// row and cancels leftover polls for the match. Must run inside a transaction.
fn resolve_match_core(
    conn: &Connection,
    tournament: &TournamentRecord,
    m: &MatchRecord,
    winner: &str,
    actor: &str,
    source: &str,
) -> anyhow::Result<ResolveMatchResult> {
    let fresh = t::get_match(conn, m.id)?.ok_or_else(|| anyhow!("Match not found."))?;
    if fresh.winner.is_some() {
        bail!("Match is already resolved.");
    }

    close_tournament_winner_market_if_needed(conn, tournament.id, fresh.round_number)?;
    let summary = t::resolve_match_payouts(conn, tournament.id, fresh.id, winner)?;
    let bets = t::get_bets_for_match(conn, tournament.id, fresh.id)?;

    let current_round = t::get_current_round_number(conn, tournament.id)?;
    let current_round_matches = t::get_tournament_matches(conn, tournament.id, Some(current_round))?;
    let current_round_resolved = current_round_matches.iter().all(|x| x.winner.is_some());
    let is_final_round = current_round_matches.len() == 1;
    let tournament_ended = current_round_resolved && is_final_round;

    if tournament_ended {
        finalize_tournament_winner_if_needed(conn, tournament.id, winner)?;
        t::end_tournament(conn, tournament.id)?;
    }
    t::cancel_polls_for_match(conn, fresh.id)?;

    audit(
        conn,
        actor,
        "tournament_resolve_match",
        "match",
        fresh.id,
        json!({
            "guildId": tournament.guild_id,
            "matchNumber": fresh.match_number,
            "winner": winner,
            "tournamentId": tournament.id,
            "source": source,
        }),
    )?;

    Ok(ResolveMatchResult {
        match_: fresh,
        winner: winner.to_string(),
        summary,
        bets,
        tournament_ended,
        tournament_name: tournament.name.clone(),
        resolved_special_markets: if tournament_ended {
            vec![ResolvedSpecialMarket {
                market_type: MARKET_TOURNAMENT_WINNER.to_string(),
                winning_pick: winner.to_string(),
            }]
        } else {
            Vec::new()
        },
    })
}

fn require_active_record(conn: &Connection, guild_id: &str) -> anyhow::Result<TournamentRecord> {
    let active = require_active_tournament(conn, guild_id)?;
    t::get_tournament(conn, active.id)?.ok_or_else(|| anyhow!(NO_ACTIVE_TOURNAMENT))
}

pub fn resolve_tournament_match(
    db: &Db,
    actor_user_id: &str,
    guild_id: &str,
    match_number: i64,
    winner_raw: &str,
) -> anyhow::Result<ResolveMatchResult> {
    require_manage(actor_user_id)?;
    db.tx(|tx| {
        let tournament = require_active_record(tx, guild_id)?;
        let m = t::get_match_by_number(tx, tournament.id, match_number)?.ok_or_else(|| anyhow!("Match not found."))?;
        if m.winner.is_some() {
            bail!("Match is already resolved.");
        }
        let lowered = winner_raw.to_lowercase();
        let winner = if lowered == m.contestant_b.to_lowercase() {
            m.contestant_b.clone()
        } else if lowered == m.contestant_a.to_lowercase() {
            m.contestant_a.clone()
        } else {
            bail!("Winner must be exactly {} or {}.", m.contestant_a, m.contestant_b);
        };
        resolve_match_core(tx, &tournament, &m, &winner, actor_user_id, "manual")
    })
}

pub fn resolve_final_runner_up_market(
    db: &Db,
    actor_user_id: &str,
    guild_id: &str,
    picked: &str,
) -> anyhow::Result<ResolveRunnerUpResult> {
    require_manage(actor_user_id)?;
    db.tx(|tx| {
        let active = require_active_tournament(tx, guild_id)?;
        let market = t::get_special_market_by_type(tx, active.id, MARKET_FINAL_RUNNER_UP)?
            .ok_or_else(|| anyhow!("Final Runner-Up market not found."))?;
        if market.status == "resolved" {
            bail!("Final Runner-Up market is already resolved.");
        }
        if market.status == "cancelled" {
            bail!("Final Runner-Up market is cancelled.");
        }
        let options = special_market_options(tx, active.id, &market.market_type, market.id)?;
        if !options.iter().any(|o| o == picked) {
            bail!("Picked contestant is not in the Final Runner-Up market.");
        }
        let result = t::resolve_special_market(tx, market.id, picked)?;
        audit(
            tx,
            actor_user_id,
            "tournament_resolve_runner_up",
            "special_market",
            market.id,
            json!({ "guildId": guild_id, "picked": picked, "tournamentId": active.id }),
        )?;
        Ok(ResolveRunnerUpResult { market, picked: picked.to_string(), result })
    })
}

pub fn complete_tournament(db: &Db, actor_user_id: &str, guild_id: &str) -> anyhow::Result<()> {
    require_manage(actor_user_id)?;
    db.tx(|tx| {
        let active = require_active_tournament(tx, guild_id)?;
        t::end_tournament(tx, active.id)?;
        audit(tx, actor_user_id, "tournament_end", "tournament", active.id, json!({ "guildId": guild_id }))
    })
}

pub fn advance_tournament_round(db: &Db, actor_user_id: &str, guild_id: &str) -> anyhow::Result<AdvanceRoundResult> {
    require_manage(actor_user_id)?;
    db.tx(|tx| {
        let active = require_active_tournament(tx, guild_id)?;
        advance_round_conn(tx, active.id)
    })
}

fn advance_round_conn(conn: &Connection, tournament_id: i64) -> anyhow::Result<AdvanceRoundResult> {
    let previous_round = t::get_current_round_number(conn, tournament_id)?;
    let previous_losers = get_round_losers(conn, tournament_id, previous_round)?;
    let (round_number, match_count) = t::create_next_round_from_winners(conn, tournament_id)?;
    if match_count == 1 && previous_losers.len() == 2 {
        ensure_final_runner_up_market(conn, tournament_id, round_number, &previous_losers)?;
    }
    Ok(AdvanceRoundResult { round_number, match_count })
}

// ---------------------------------------------------------------------------------------------
// Poll-backed matches: pure helpers
// ---------------------------------------------------------------------------------------------

/// Truncates to `max` characters, ending in "..." when something was cut.
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(3);
    let mut out: String = text.chars().take(keep).collect();
    out.push_str("...");
    out
}

/// Formats unix seconds as the UTC `YYYY-MM-DD HH:MM:SS` used by the polls table.
pub fn unix_to_sql(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_else(chrono::Utc::now)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// Parses the polls table's UTC timestamp back to unix seconds.
pub fn sql_to_unix(text: &str) -> Option<i64> {
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").ok().map(|d| d.and_utc().timestamp())
}

/// Poll question for a match, within Discord's 300 character limit.
pub fn poll_question(m: &MatchRecord) -> String {
    truncate_chars(&m.label(), POLL_QUESTION_MAX)
}

/// Poll answer text for a contestant, within Discord's 55 character limit.
pub fn poll_answer_text(contestant: &str) -> String {
    truncate_chars(contestant, POLL_ANSWER_MAX)
}

/// One answer of a Discord poll with its vote count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollAnswerTally {
    pub id: u64,
    pub text: String,
    pub votes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    Winner { contestant: String, votes_for: u64, votes_against: u64 },
    Tie { votes: u64 },
    /// The poll does not have exactly two answers matching the contestants.
    Unmatched,
}

fn normalize(text: &str) -> String {
    text.trim().to_lowercase()
}

fn answer_matches(answer: &str, contestant: &str) -> bool {
    let answer = normalize(answer);
    answer == normalize(contestant) || answer == normalize(&poll_answer_text(contestant))
}

/// Maps a two-answer poll onto the contestants (case-insensitive). Returns the indices of
/// the answers for contestant A and contestant B.
pub fn match_poll_answers(answers: &[PollAnswerTally], contestant_a: &str, contestant_b: &str) -> Option<(usize, usize)> {
    if answers.len() != 2 {
        return None;
    }
    let (first, second) = (&answers[0].text, &answers[1].text);
    if answer_matches(first, contestant_a) && answer_matches(second, contestant_b) {
        Some((0, 1))
    } else if answer_matches(first, contestant_b) && answer_matches(second, contestant_a) {
        Some((1, 0))
    } else {
        None
    }
}

/// Picks the winner of a finished poll: the answer with strictly more votes.
pub fn decide_poll_outcome(answers: &[PollAnswerTally], contestant_a: &str, contestant_b: &str) -> PollOutcome {
    let Some((index_a, index_b)) = match_poll_answers(answers, contestant_a, contestant_b) else {
        return PollOutcome::Unmatched;
    };
    let votes_a = answers[index_a].votes;
    let votes_b = answers[index_b].votes;
    match votes_a.cmp(&votes_b) {
        std::cmp::Ordering::Greater => PollOutcome::Winner {
            contestant: contestant_a.to_string(),
            votes_for: votes_a,
            votes_against: votes_b,
        },
        std::cmp::Ordering::Less => PollOutcome::Winner {
            contestant: contestant_b.to_string(),
            votes_for: votes_b,
            votes_against: votes_a,
        },
        std::cmp::Ordering::Equal => PollOutcome::Tie { votes: votes_a },
    }
}

// ---------------------------------------------------------------------------------------------
// Poll-backed matches: service functions
// ---------------------------------------------------------------------------------------------

/// Unresolved matches of the current round, in match order.
pub fn unresolved_current_round_matches(
    db: &Db,
    guild_id: &str,
) -> anyhow::Result<(TournamentSummary, Vec<MatchRecord>)> {
    let conn = db.conn();
    let active = require_active_tournament(&conn, guild_id)?;
    let round = t::get_current_round_number(&conn, active.id)?;
    let matches = t::get_tournament_matches(&conn, active.id, Some(round))?
        .into_iter()
        .filter(|m| m.winner.is_none())
        .collect();
    Ok((active, matches))
}

/// Matches `/tournament poll` should post a poll for: the given match number, or every
/// unresolved match of the current round that has no active poll.
pub fn prepare_match_polls(
    db: &Db,
    actor_user_id: &str,
    guild_id: &str,
    match_number: Option<i64>,
) -> anyhow::Result<(TournamentSummary, Vec<MatchRecord>)> {
    require_manage(actor_user_id)?;
    let conn = db.conn();
    let active = require_active_tournament(&conn, guild_id)?;

    let targets = match match_number {
        Some(number) => {
            let m = t::get_match_by_number(&conn, active.id, number)?.ok_or_else(|| anyhow!("Match not found."))?;
            if m.winner.is_some() {
                bail!("Match is already resolved.");
            }
            if t::get_active_poll_for_match(&conn, m.id)?.is_some() {
                bail!("That match already has an active poll.");
            }
            vec![m]
        }
        None => {
            let round = t::get_current_round_number(&conn, active.id)?;
            let mut out = Vec::new();
            for m in t::get_tournament_matches(&conn, active.id, Some(round))? {
                if m.winner.is_none() && t::get_active_poll_for_match(&conn, m.id)?.is_none() {
                    out.push(m);
                }
            }
            if out.is_empty() {
                bail!("There are no unresolved matches without an active poll in the current round.");
            }
            out
        }
    };
    Ok((active, targets))
}

/// Records a freshly posted poll and, optionally, closes betting for its match.
/// Returns whether betting was closed by this call.
#[allow(clippy::too_many_arguments)]
pub fn register_match_poll(
    db: &Db,
    actor_user_id: &str,
    guild_id: &str,
    m: &MatchRecord,
    channel_id: &str,
    message_id: &str,
    expires_at: &str,
    close_betting: bool,
) -> anyhow::Result<bool> {
    db.tx(|tx| {
        t::insert_match_poll(tx, m.id, m.tournament_id, guild_id, channel_id, message_id, expires_at)?;
        let mut closed = false;
        if close_betting {
            if let Some(fresh) = t::get_match(tx, m.id)? {
                if fresh.betting_open {
                    close_match_betting(tx, fresh.tournament_id, &fresh)?;
                    closed = true;
                }
            }
        }
        audit(
            tx,
            actor_user_id,
            "tournament_poll_create",
            "match",
            m.id,
            json!({
                "guildId": guild_id,
                "channelId": channel_id,
                "messageId": message_id,
                "expiresAt": expires_at,
                "closedBetting": closed,
                "tournamentId": m.tournament_id,
            }),
        )?;
        Ok(closed)
    })
}

pub fn get_match_poll_by_message(db: &Db, message_id: &str) -> anyhow::Result<Option<MatchPollRecord>> {
    t::get_match_poll_by_message(&db.conn(), message_id)
}

pub fn list_due_active_polls(db: &Db, now_sql: &str) -> anyhow::Result<Vec<MatchPollRecord>> {
    t::list_due_active_polls(&db.conn(), now_sql)
}

/// Marks a tracked poll as cancelled (e.g. its message was deleted).
pub fn cancel_match_poll(db: &Db, message_id: &str) -> anyhow::Result<()> {
    t::set_poll_status(&db.conn(), message_id, POLL_CANCELLED)
}

/// Match records for polls (for announcements): the match a poll belongs to.
pub fn get_match(db: &Db, match_id: i64) -> anyhow::Result<Option<MatchRecord>> {
    t::get_match(&db.conn(), match_id)
}

#[derive(Debug, Clone)]
pub enum PollApplyOutcome {
    Resolved { result: Box<ResolveMatchResult>, votes_for: u64, votes_against: u64 },
    Tied { votes: u64, match_: MatchRecord },
    /// The poll was already handled, the match already resolved, or the tournament is over.
    AlreadyHandled,
    /// Answers could not be mapped to contestants; the poll was cancelled.
    Unmatched { match_: MatchRecord },
}

/// Applies the final results of a tracked poll. Idempotent: only an `active` poll is acted on,
/// and the status change happens in the same transaction as the payouts, so a match is never
/// paid twice.
pub fn apply_poll_result(db: &Db, message_id: &str, answers: &[PollAnswerTally]) -> anyhow::Result<PollApplyOutcome> {
    db.tx(|tx| apply_poll_result_conn(tx, message_id, answers))
}

fn apply_poll_result_conn(
    conn: &Connection,
    message_id: &str,
    answers: &[PollAnswerTally],
) -> anyhow::Result<PollApplyOutcome> {
    let Some(poll) = t::get_match_poll_by_message(conn, message_id)? else {
        return Ok(PollApplyOutcome::AlreadyHandled);
    };
    if poll.status != POLL_ACTIVE {
        return Ok(PollApplyOutcome::AlreadyHandled);
    }

    let tournament = t::get_tournament(conn, poll.tournament_id)?.filter(|tr| tr.status == "active");
    let m = t::get_match(conn, poll.match_id)?;
    let (Some(tournament), Some(m)) = (tournament, m) else {
        t::set_poll_status(conn, message_id, POLL_CANCELLED)?;
        return Ok(PollApplyOutcome::AlreadyHandled);
    };
    if m.winner.is_some() {
        t::set_poll_status(conn, message_id, POLL_CANCELLED)?;
        return Ok(PollApplyOutcome::AlreadyHandled);
    }

    match decide_poll_outcome(answers, &m.contestant_a, &m.contestant_b) {
        PollOutcome::Winner { contestant, votes_for, votes_against } => {
            let result = resolve_match_core(conn, &tournament, &m, &contestant, "system", "poll")?;
            t::set_poll_status(conn, message_id, POLL_RESOLVED)?;
            Ok(PollApplyOutcome::Resolved { result: Box::new(result), votes_for, votes_against })
        }
        PollOutcome::Tie { votes } => {
            t::set_poll_status(conn, message_id, POLL_TIED)?;
            Ok(PollApplyOutcome::Tied { votes, match_: m })
        }
        PollOutcome::Unmatched => {
            t::set_poll_status(conn, message_id, POLL_CANCELLED)?;
            Ok(PollApplyOutcome::Unmatched { match_: m })
        }
    }
}

/// `/tournament resolve-from-poll`: resolves `match_id` from the tallies of any poll (the bot's
/// or someone else's). A tie is an error asking for a manual resolve. When the poll message is
/// tracked in `tournament_match_polls` it is marked resolved too.
pub fn resolve_match_from_poll(
    db: &Db,
    actor_user_id: &str,
    guild_id: &str,
    match_id: i64,
    poll_message_id: &str,
    answers: &[PollAnswerTally],
) -> anyhow::Result<ResolveMatchResult> {
    require_manage(actor_user_id)?;
    db.tx(|tx| {
        let tournament = require_active_record(tx, guild_id)?;
        let m = t::get_match(tx, match_id)?
            .filter(|m| m.tournament_id == tournament.id)
            .ok_or_else(|| anyhow!("Match not found."))?;
        if m.winner.is_some() {
            bail!("Match is already resolved.");
        }
        match decide_poll_outcome(answers, &m.contestant_a, &m.contestant_b) {
            PollOutcome::Winner { contestant, .. } => {
                let result = resolve_match_core(tx, &tournament, &m, &contestant, actor_user_id, "poll_link")?;
                if t::get_match_poll_by_message(tx, poll_message_id)?.is_some() {
                    t::set_poll_status(tx, poll_message_id, POLL_RESOLVED)?;
                }
                Ok(result)
            }
            PollOutcome::Tie { votes } => bail!(
                "The poll is tied at {votes} vote(s) each, so I cannot pick a winner. Resolve this match manually with /tournament resolve."
            ),
            PollOutcome::Unmatched => {
                bail!("The poll answers do not match the contestants of {}.", m.label())
            }
        }
    })
}

// ---------------------------------------------------------------------------------------------
// Announcements
// ---------------------------------------------------------------------------------------------

/// Posts to the tournament updates channel, if configured. Never fails the caller.
pub async fn announce(http: &serenity::Http, content: &str) {
    let Some(channel_id) = config::get().channels.tournament_updates_channel_id else {
        return;
    };
    let content = truncate_chars(content, 2000);
    if let Err(err) = crate::bot::say_safe(http, serenity::ChannelId::new(channel_id), content).await {
        tracing::error!("failed to send tournament update: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::economy::{self as eco, get_balance};
    use crate::db::tournament as dbt;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        eco::migrate_for_tests(&conn);
        dbt::migrate_for_tests(&conn);
        conn
    }

    fn wallet(conn: &Connection, user: &str, balance: i64) {
        conn.execute("INSERT INTO wallets (user_id, guild_id, balance) VALUES (?, 'g', ?)", rusqlite::params![user, balance])
            .unwrap();
    }

    fn bal(conn: &Connection, user: &str) -> i64 {
        get_balance(conn, user, "g").unwrap()
    }

    fn bracket(pairs: &[(&str, &str)]) -> Vec<BracketMatchInput> {
        pairs.iter().map(|(a, b)| BracketMatchInput { a: a.to_string(), b: b.to_string() }).collect()
    }

    fn tally(a: (&str, u64), b: (&str, u64)) -> Vec<PollAnswerTally> {
        vec![
            PollAnswerTally { id: 1, text: a.0.to_string(), votes: a.1 },
            PollAnswerTally { id: 2, text: b.0.to_string(), votes: b.1 },
        ]
    }

    fn open(conn: &Connection, pairs: &[(&str, &str)]) -> TournamentRecord {
        let id = open_tournament_in_tx(conn, "op", "g", "Cup", &bracket(pairs)).unwrap();
        t::get_tournament(conn, id).unwrap().unwrap()
    }

    #[test]
    fn sql_timestamps_roundtrip() {
        assert_eq!(unix_to_sql(0), "1970-01-01 00:00:00");
        let secs = 1_800_000_000;
        assert_eq!(sql_to_unix(&unix_to_sql(secs)), Some(secs));
        assert_eq!(sql_to_unix("garbage"), None);
    }

    #[test]
    fn question_and_answer_truncation() {
        let long = "x".repeat(400);
        assert_eq!(truncate_chars(&long, 300).chars().count(), 300);
        assert!(truncate_chars(&long, 300).ends_with("..."));
        assert_eq!(truncate_chars("short", 55), "short");
        assert_eq!(poll_answer_text(&"y".repeat(80)).chars().count(), 55);
        // multi-byte characters are counted, not bytes
        assert_eq!(truncate_chars(&"あ".repeat(60), 55).chars().count(), 55);
    }

    #[test]
    fn poll_outcome_picks_most_votes_case_insensitively() {
        let out = decide_poll_outcome(&tally(("alice", 5), ("BOB", 2)), "Alice", "Bob");
        assert_eq!(out, PollOutcome::Winner { contestant: "Alice".into(), votes_for: 5, votes_against: 2 });
        // answer order may differ from contestant order
        let out = decide_poll_outcome(&tally(("Bob", 7), ("Alice", 1)), "Alice", "Bob");
        assert_eq!(out, PollOutcome::Winner { contestant: "Bob".into(), votes_for: 7, votes_against: 1 });
    }

    #[test]
    fn poll_outcome_tie_and_unmatched() {
        assert_eq!(decide_poll_outcome(&tally(("Alice", 3), ("Bob", 3)), "Alice", "Bob"), PollOutcome::Tie { votes: 3 });
        assert_eq!(decide_poll_outcome(&tally(("Alice", 0), ("Bob", 0)), "Alice", "Bob"), PollOutcome::Tie { votes: 0 });
        assert_eq!(decide_poll_outcome(&tally(("Alice", 3), ("Carol", 1)), "Alice", "Bob"), PollOutcome::Unmatched);
        let three = vec![
            PollAnswerTally { id: 1, text: "Alice".into(), votes: 1 },
            PollAnswerTally { id: 2, text: "Bob".into(), votes: 1 },
            PollAnswerTally { id: 3, text: "Carol".into(), votes: 1 },
        ];
        assert_eq!(decide_poll_outcome(&three, "Alice", "Bob"), PollOutcome::Unmatched);
        // both answers naming the same contestant is not a match
        assert_eq!(decide_poll_outcome(&tally(("Alice", 1), ("alice", 2)), "Alice", "Bob"), PollOutcome::Unmatched);
    }

    #[test]
    fn long_contestant_names_match_their_truncated_poll_answers() {
        let long_a = format!("{} [Some Long Series Title]", "A".repeat(50));
        let answers = tally((&poll_answer_text(&long_a), 4), ("Bob", 1));
        assert_eq!(match_poll_answers(&answers, &long_a, "Bob"), Some((0, 1)));
        assert!(matches!(decide_poll_outcome(&answers, &long_a, "Bob"), PollOutcome::Winner { .. }));
    }

    #[test]
    fn open_validates_and_creates_winner_market() {
        let conn = setup();
        assert!(open_tournament_in_tx(&conn, "op", "g", "Cup", &[]).is_err());
        let err = open_tournament_in_tx(&conn, "op", "g", "Cup", &bracket(&[("A", "B"), ("C", " ")])).unwrap_err();
        assert_eq!(err.to_string(), "Invalid match at index 1.");
        // a rejected bracket must not leave a tournament behind
        assert!(t::get_active_tournament(&conn, "g").unwrap().is_none());

        let tr = open(&conn, &[("A", "B"), ("C", "D")]);
        let err = open_tournament_in_tx(&conn, "op", "g", "Again", &bracket(&[("A", "B")])).unwrap_err();
        assert_eq!(err.to_string(), "A tournament is already active: Cup");
        let market = t::get_special_market_by_type(&conn, tr.id, MARKET_TOURNAMENT_WINNER).unwrap().unwrap();
        assert_eq!(market.status, "open");
        assert_eq!(build_tournament_winner_options(&conn, tr.id).unwrap(), vec!["A", "B", "C", "D"]);
    }

    #[test]
    fn poll_result_resolves_exactly_once() {
        let conn = setup();
        let tr = open(&conn, &[("Alice", "Bob"), ("Carol", "Dave")]);
        let m = t::get_match_by_number(&conn, tr.id, 1).unwrap().unwrap();
        wallet(&conn, "u1", 1000);
        wallet(&conn, "u2", 1000);
        assert!(t::place_bet(&conn, tr.id, m.id, "u1", "g", "Alice", 100).unwrap());
        assert!(t::place_bet(&conn, tr.id, m.id, "u2", "g", "Bob", 300).unwrap());
        t::insert_match_poll(&conn, m.id, tr.id, "g", "chan", "pollmsg", "2000-01-01 00:00:00").unwrap();

        let answers = tally(("Alice", 10), ("Bob", 4));
        match apply_poll_result_conn(&conn, "pollmsg", &answers).unwrap() {
            PollApplyOutcome::Resolved { result, votes_for, votes_against } => {
                assert_eq!(result.winner, "Alice");
                assert_eq!((votes_for, votes_against), (10, 4));
                assert_eq!(result.summary.total_pool, 400);
                assert!(!result.tournament_ended);
            }
            other => panic!("unexpected outcome {other:?}"),
        }
        assert_eq!(bal(&conn, "u1"), 900 + 400);
        assert_eq!(bal(&conn, "u2"), 700);
        assert_eq!(t::get_match_poll_by_message(&conn, "pollmsg").unwrap().unwrap().status, POLL_RESOLVED);
        // round 1 match resolution closes the futures market
        let market = t::get_special_market_by_type(&conn, tr.id, MARKET_TOURNAMENT_WINNER).unwrap().unwrap();
        assert_eq!(market.status, "closed");

        // finalizing again (MessageUpdate + background tick racing) must not pay twice
        assert!(matches!(apply_poll_result_conn(&conn, "pollmsg", &answers).unwrap(), PollApplyOutcome::AlreadyHandled));
        assert_eq!(bal(&conn, "u1"), 1300);
        let txs = eco::get_wallet_transactions(&conn, "g", Some("u1"), 50).unwrap();
        assert_eq!(txs.iter().filter(|x| x.reason == "bet_win").count(), 1);
    }

    #[test]
    fn tied_poll_does_not_resolve_and_manual_resolve_cancels_it() {
        let conn = setup();
        let tr = open(&conn, &[("Alice", "Bob"), ("Carol", "Dave")]);
        let m = t::get_match_by_number(&conn, tr.id, 1).unwrap().unwrap();
        t::insert_match_poll(&conn, m.id, tr.id, "g", "chan", "pollmsg", "2000-01-01 00:00:00").unwrap();

        let out = apply_poll_result_conn(&conn, "pollmsg", &tally(("Alice", 2), ("Bob", 2))).unwrap();
        assert!(matches!(out, PollApplyOutcome::Tied { votes: 2, .. }));
        assert!(t::get_match(&conn, m.id).unwrap().unwrap().winner.is_none());
        assert_eq!(t::get_match_poll_by_message(&conn, "pollmsg").unwrap().unwrap().status, POLL_TIED);
        // a tied poll is not retried by the finalizer
        assert!(matches!(
            apply_poll_result_conn(&conn, "pollmsg", &tally(("Alice", 9), ("Bob", 0))).unwrap(),
            PollApplyOutcome::AlreadyHandled
        ));

        // the operator resolves manually; leftover tied/active polls for the match are cancelled
        let tournament = t::get_tournament(&conn, tr.id).unwrap().unwrap();
        resolve_match_core(&conn, &tournament, &m, "Bob", "op", "manual").unwrap();
        assert_eq!(t::get_match_poll_by_message(&conn, "pollmsg").unwrap().unwrap().status, POLL_CANCELLED);
        assert!(resolve_match_core(&conn, &tournament, &m, "Bob", "op", "manual").is_err());
    }

    #[test]
    fn manual_resolve_beats_a_later_poll_result() {
        let conn = setup();
        let tr = open(&conn, &[("Alice", "Bob"), ("Carol", "Dave")]);
        let m = t::get_match_by_number(&conn, tr.id, 1).unwrap().unwrap();
        wallet(&conn, "u1", 100);
        assert!(t::place_bet(&conn, tr.id, m.id, "u1", "g", "Bob", 100).unwrap());
        t::insert_match_poll(&conn, m.id, tr.id, "g", "chan", "pollmsg", "2000-01-01 00:00:00").unwrap();
        let tournament = t::get_tournament(&conn, tr.id).unwrap().unwrap();
        resolve_match_core(&conn, &tournament, &m, "Bob", "op", "manual").unwrap();
        assert_eq!(bal(&conn, "u1"), 100);
        // the poll finishes afterwards with the opposite result; nothing may change
        let out = apply_poll_result_conn(&conn, "pollmsg", &tally(("Alice", 9), ("Bob", 0))).unwrap();
        assert!(matches!(out, PollApplyOutcome::AlreadyHandled));
        assert_eq!(t::get_match(&conn, m.id).unwrap().unwrap().winner.as_deref(), Some("Bob"));
        assert_eq!(bal(&conn, "u1"), 100);
    }

    #[test]
    fn unmatched_poll_is_cancelled_without_payout() {
        let conn = setup();
        let tr = open(&conn, &[("Alice", "Bob"), ("Carol", "Dave")]);
        let m = t::get_match_by_number(&conn, tr.id, 1).unwrap().unwrap();
        t::insert_match_poll(&conn, m.id, tr.id, "g", "chan", "pollmsg", "2000-01-01 00:00:00").unwrap();
        let out = apply_poll_result_conn(&conn, "pollmsg", &tally(("Xavier", 2), ("Yvonne", 1))).unwrap();
        assert!(matches!(out, PollApplyOutcome::Unmatched { .. }));
        assert!(t::get_match(&conn, m.id).unwrap().unwrap().winner.is_none());
        assert_eq!(t::get_match_poll_by_message(&conn, "pollmsg").unwrap().unwrap().status, POLL_CANCELLED);
    }

    #[test]
    fn full_tournament_with_runner_up_market_and_auto_end() {
        let conn = setup();
        let tr = open(&conn, &[("A", "B"), ("C", "D")]);
        wallet(&conn, "u1", 1000);
        wallet(&conn, "u2", 1000);
        let winner_market = t::get_special_market_by_type(&conn, tr.id, MARKET_TOURNAMENT_WINNER).unwrap().unwrap();
        assert!(t::place_special_bet(&conn, winner_market.id, tr.id, "u1", "g", "A", 100).unwrap());
        assert!(t::place_special_bet(&conn, winner_market.id, tr.id, "u2", "g", "C", 100).unwrap());

        // cannot advance before the round is resolved
        assert!(advance_round_conn(&conn, tr.id).is_err());

        let ms = t::get_tournament_matches(&conn, tr.id, Some(1)).unwrap();
        resolve_match_core(&conn, &t::get_tournament(&conn, tr.id).unwrap().unwrap(), &ms[0], "A", "op", "manual").unwrap();
        let r = resolve_match_core(&conn, &t::get_tournament(&conn, tr.id).unwrap().unwrap(), &ms[1], "C", "op", "manual").unwrap();
        assert!(!r.tournament_ended);

        // semi-final losers feed the runner-up market created when the final round opens
        let semis = advance_round_conn(&conn, tr.id).unwrap();
        assert_eq!((semis.round_number, semis.match_count), (2, 1));
        let runner_up = t::get_special_market_by_type(&conn, tr.id, MARKET_FINAL_RUNNER_UP).unwrap().unwrap();
        assert_eq!(runner_up.status, "open");
        assert_eq!(runner_up.round_number, Some(2));
        assert_eq!(t::get_special_market_options(&conn, runner_up.id).unwrap(), vec!["B".to_string(), "D".to_string()]);
        let view = build_special_market_view(&conn, &runner_up, None).unwrap();
        assert_eq!(view.title, "Final Runner-Up");
        assert_eq!(view.description, "Pick the third-place finisher from the semi-final losers: B or D.");

        // resolving the final ends the tournament and settles the winner market
        let final_match = t::get_match_by_number(&conn, tr.id, 1).unwrap().unwrap();
        assert_eq!(final_match.round_number, 2);
        let tournament = t::get_tournament(&conn, tr.id).unwrap().unwrap();
        let end = resolve_match_core(&conn, &tournament, &final_match, "C", "op", "manual").unwrap();
        assert!(end.tournament_ended);
        assert_eq!(end.resolved_special_markets.len(), 1);
        assert_eq!(end.resolved_special_markets[0].winning_pick, "C");
        assert!(t::get_active_tournament(&conn, "g").unwrap().is_none());
        // u2 picked C: pool 200 all to u2
        assert_eq!(bal(&conn, "u2"), 900 + 200);
        assert_eq!(bal(&conn, "u1"), 900);
        let json = serde_json::to_value(&end).unwrap();
        assert!(json.get("match").is_some());
        assert_eq!(json["tournamentEnded"], true);
        assert_eq!(json["resolvedSpecialMarkets"][0]["type"], "tournament_winner");
        assert!(json["summary"]["payouts"].is_array());
    }

    #[test]
    fn bracket_json_validation_messages() {
        use serde_json::json;
        assert_eq!(parse_bracket_json(&json!({})).unwrap_err().to_string(), "JSON root must be an array of matches.");
        assert_eq!(parse_bracket_json(&json!([])).unwrap_err().to_string(), "Bracket array is empty.");
        assert_eq!(
            parse_bracket_json(&json!([{"a": "x"}])).unwrap_err().to_string(),
            "Each match must have string fields \"a\" and \"b\"."
        );
        let ok = parse_bracket_json(&json!([{"a": "x", "b": "y"}])).unwrap();
        assert_eq!((ok[0].a.as_str(), ok[0].b.as_str()), ("x", "y"));
    }

    #[test]
    fn overview_json_shape_matches_the_ts_api() {
        let view = TournamentMatchView {
            id: 1,
            tournament_id: 2,
            match_number: 3,
            round_number: 1,
            contestant_a: "A".into(),
            contestant_b: "B".into(),
            winner: None,
            betting_open: true,
            user_bet: None,
            poll: None,
        };
        let json = serde_json::to_value(&view).unwrap();
        for key in ["id", "tournamentId", "matchNumber", "roundNumber", "contestantA", "contestantB", "winner", "bettingOpen", "userBet"] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
        assert!(json.get("poll").is_none());
        assert!(json["winner"].is_null());
    }
}
