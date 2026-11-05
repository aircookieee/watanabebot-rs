//! JSON API under `/api/*`. Every route requires a session (checked by `handle`).

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{json, Value};

use super::http::{self, BodyError};
use super::session::{secure_eq, Session};
use super::Ctx;
use crate::services::{authz, bracket_sorter, tournaments as tour, wallets};

const PAGE_SIZE: usize = 25;
const MAX_PAGE: usize = 10_000_000;
const DEFAULT_LEADERBOARD_LIMIT: i64 = 10;
const MAX_LEADERBOARD_LIMIT: i64 = 100;
/// How many rows are fetched before paging (same windows as the TS server).
const TRANSACTION_WINDOW: i64 = 500;
const AUDIT_WINDOW: i64 = 250;

/// An error answered as `{ "error": message }`.
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiError {
            status,
            message: message.into(),
        }
    }

    fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn forbidden() -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden")
    }
}

impl From<anyhow::Error> for ApiError {
    /// Service errors are user-readable (400); database failures are logged and hidden (500).
    fn from(err: anyhow::Error) -> Self {
        if err.chain().any(|cause| cause.is::<rusqlite::Error>()) {
            tracing::error!("web api database error: {err:#}");
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        } else {
            ApiError::bad(err.to_string())
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        http::api_error(self.status, &self.message)
    }
}

type ApiResult = Result<Response, ApiError>;

fn ok_json<T: Serialize>(data: &T) -> ApiResult {
    Ok(http::json(StatusCode::OK, data))
}

/// Entry point for every `/api/*` request.
pub async fn handle(ctx: &Ctx, req: Request, session: Option<Session>) -> Response {
    let Some(session) = session else {
        return http::api_error(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    let (parts, body) = req.into_parts();
    let query = http::parse_query(parts.uri.query());
    match route(
        ctx,
        &session,
        &parts.method,
        parts.uri.path(),
        &parts.headers,
        &query,
        body,
    )
    .await
    {
        Ok(res) => res,
        Err(err) => err.into_response(),
    }
}

async fn route(
    ctx: &Ctx,
    session: &Session,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
    body: Body,
) -> ApiResult {
    let get = method == Method::GET;
    let post = method == Method::POST;
    match path {
        "/api/me" if get => me(ctx, session),
        "/api/session" if get => ok_json(&json!({
            "authenticated": true,
            "user": {
                "userId": session.user_id,
                "username": session.username,
                "role": tour::get_user_role(&session.user_id),
                "csrfToken": session.csrf_token,
            },
        })),
        "/api/tournament" if get => {
            let guild = guild_id(ctx)?;
            ok_json(&tour::get_tournament_overview(
                &ctx.app.db,
                &guild,
                &session.user_id,
            )?)
        }
        "/api/tournament" if post => Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
        )),
        "/api/bets" if get => {
            let guild = guild_id(ctx)?;
            ok_json(&tour::get_current_user_bets(
                &ctx.app.db,
                &guild,
                &session.user_id,
            )?)
        }
        "/api/bets" if post => place_bet(ctx, session, headers, body).await,
        "/api/wallet" if get => wallet(ctx, session, query).await,
        "/api/wallet/pay" if post => pay(ctx, session, headers, body).await,
        "/api/leaderboard" if get => leaderboard(ctx, query).await,
        "/api/stats" if get => stats(ctx, session, query),
        "/api/daily" if post => daily(ctx, session, headers),
        "/api/operator/history" if get => history(ctx, session, query).await,
        "/api/admin/transactions" if get => admin_transactions(ctx, session, query).await,
        "/api/operator/bracket-sorter" if post => bracket_sort(session, headers, body).await,
        "/api/operator/open"
        | "/api/operator/close-all"
        | "/api/operator/resolve"
        | "/api/operator/resolve-runner-up"
        | "/api/operator/close"
        | "/api/operator/close-runner-up"
        | "/api/operator/advance-round"
        | "/api/operator/end"
        | "/api/admin/give"
        | "/api/admin/set"
        | "/api/admin/take"
            if post =>
        {
            mutate(ctx, session, headers, path, body).await
        }
        _ => Err(ApiError::new(StatusCode::NOT_FOUND, "not_found")),
    }
}

// ---------------------------------------------------------------------------------------------
// Request helpers
// ---------------------------------------------------------------------------------------------

/// The guild all dashboard data belongs to (`CURRENCY_GUILD_ID`).
fn guild_id(ctx: &Ctx) -> Result<String, ApiError> {
    ctx.opts
        .currency_guild
        .map(|g| g.get().to_string())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "The dashboard is not configured: CURRENCY_GUILD_ID is not set.",
            )
        })
}

fn require_csrf(headers: &HeaderMap, session: &Session) -> Result<(), ApiError> {
    let token = headers
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !token.is_empty() && secure_eq(token, &session.csrf_token) {
        Ok(())
    } else {
        Err(ApiError::bad("Invalid CSRF token."))
    }
}

fn require_operator(session: &Session) -> Result<(), ApiError> {
    if authz::can_manage_tournaments(session.user_id.parse().unwrap_or(0)) {
        Ok(())
    } else {
        Err(ApiError::forbidden())
    }
}

fn require_admin(session: &Session) -> Result<(), ApiError> {
    if authz::can_use_admin_tools(session.user_id.parse().unwrap_or(0)) {
        Ok(())
    } else {
        Err(ApiError::forbidden())
    }
}

async fn read_json(body: Body) -> Result<Value, ApiError> {
    http::read_json_object(body).await.map_err(|err| match err {
        BodyError::TooLarge => {
            ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "Request body is too large.")
        }
        BodyError::Invalid => ApiError::bad("Request body must be a JSON object."),
    })
}

fn whole_f64(f: f64) -> Option<i64> {
    (f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e15).then_some(f as i64)
}

/// A JSON number or numeric string that is a whole number.
fn to_whole_number(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().and_then(whole_f64)),
        Value::String(s) => {
            let text = s.trim();
            text.parse::<i64>()
                .ok()
                .or_else(|| text.parse::<f64>().ok().and_then(whole_f64))
        }
        _ => None,
    }
}

fn field_i64(body: &Value, name: &str) -> Result<i64, ApiError> {
    body.get(name)
        .and_then(to_whole_number)
        .ok_or_else(|| ApiError::bad(format!("{name} must be a whole number.")))
}

fn field_string(body: &Value, name: &str) -> Result<String, ApiError> {
    match body.get(name) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        _ => Err(ApiError::bad(format!("{name} is required."))),
    }
}

/// `?page=` as a 1-based page number.
fn page_param(query: &HashMap<String, String>) -> usize {
    query
        .get("page")
        .and_then(|p| p.trim().parse::<f64>().ok())
        .filter(|p| p.is_finite() && *p >= 1.0)
        .map(|p| (p.trunc() as usize).min(MAX_PAGE))
        .unwrap_or(1)
}

/// `?limit=`: defaults to 10, at most 100.
fn limit_param(query: &HashMap<String, String>) -> i64 {
    let n = query
        .get("limit")
        .and_then(|p| p.trim().parse::<f64>().ok())
        .filter(|p| p.is_finite())
        .map(|p| p.trunc() as i64)
        .unwrap_or(DEFAULT_LEADERBOARD_LIMIT);
    if n < 1 {
        DEFAULT_LEADERBOARD_LIMIT
    } else {
        n.min(MAX_LEADERBOARD_LIMIT)
    }
}

/// One page of `items` plus whether more follow.
fn paginate<T>(items: Vec<T>, page: usize) -> (Vec<T>, bool) {
    let start = (page - 1).saturating_mul(PAGE_SIZE);
    let has_more = start.saturating_add(PAGE_SIZE) < items.len();
    (
        items.into_iter().skip(start).take(PAGE_SIZE).collect(),
        has_more,
    )
}

/// Serializes `row` and adds extra top-level fields.
fn with_fields<T: Serialize>(row: &T, extra: Vec<(&str, Value)>) -> Value {
    let mut value = serde_json::to_value(row).unwrap_or_else(|_| json!({}));
    if let Value::Object(map) = &mut value {
        for (key, val) in extra {
            map.insert(key.to_string(), val);
        }
    }
    value
}

fn name_value(names: &HashMap<String, String>, id: &str) -> Value {
    names
        .get(id)
        .map(|n| Value::String(n.clone()))
        .unwrap_or(Value::Null)
}

async fn transactions_with_names(
    ctx: &Ctx,
    txs: Vec<crate::db::economy::WalletTransactionRow>,
) -> Vec<Value> {
    let names = ctx
        .names
        .resolve_many(txs.iter().map(|t| t.user_id.as_str()))
        .await;
    txs.iter()
        .map(|tx| with_fields(tx, vec![("username", name_value(&names, &tx.user_id))]))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Read routes
// ---------------------------------------------------------------------------------------------

fn me(ctx: &Ctx, session: &Session) -> ApiResult {
    let guild = guild_id(ctx)?;
    let balance = wallets::get_wallet_summary(&ctx.app.db, &session.user_id, &guild)?.balance;
    ok_json(&json!({
        "userId": session.user_id,
        "username": session.username,
        "role": tour::get_user_role(&session.user_id),
        "balance": balance,
        "csrfToken": session.csrf_token,
    }))
}

async fn wallet(ctx: &Ctx, session: &Session, query: &HashMap<String, String>) -> ApiResult {
    let guild = guild_id(ctx)?;
    let page = page_param(query);
    let all = wallets::get_wallet_transactions(
        &ctx.app.db,
        &guild,
        Some(&session.user_id),
        TRANSACTION_WINDOW,
    )?;
    let (slice, has_more) = paginate(all, page);
    let summary = wallets::get_wallet_summary(&ctx.app.db, &session.user_id, &guild)?;
    ok_json(&json!({
        "summary": summary,
        "transactions": transactions_with_names(ctx, slice).await,
        "page": page,
        "hasMore": has_more,
    }))
}

async fn leaderboard(ctx: &Ctx, query: &HashMap<String, String>) -> ApiResult {
    let guild = guild_id(ctx)?;
    let limit = limit_param(query);
    if query.get("sort").map(String::as_str) == Some("profit") {
        let offset = page_param(query).saturating_sub(1) as i64 * limit;
        let rows = wallets::get_profit_leaderboard(&ctx.app.db, &guild, limit, offset)?;
        let names = ctx
            .names
            .resolve_many(rows.iter().map(|r| r.user_id.as_str()))
            .await;
        let rows: Vec<Value> = rows
            .iter()
            .map(|row| with_fields(row, vec![("username", name_value(&names, &row.user_id))]))
            .collect();
        return ok_json(&json!({ "sort": "profit", "leaderboard": rows }));
    }
    let rows = wallets::get_wallet_leaderboard(&ctx.app.db, &guild, limit)?;
    let names = ctx
        .names
        .resolve_many(rows.iter().map(|r| r.user_id.as_str()))
        .await;
    let rows: Vec<Value> = rows
        .iter()
        .map(|row| with_fields(row, vec![("username", name_value(&names, &row.user_id))]))
        .collect();
    ok_json(&json!({ "leaderboard": rows }))
}

fn stats(ctx: &Ctx, session: &Session, query: &HashMap<String, String>) -> ApiResult {
    let guild = guild_id(ctx)?;
    let user_id = match query
        .get("userId")
        .map(|u| u.trim())
        .filter(|u| !u.is_empty())
    {
        Some(id) if id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit()) => id.to_string(),
        Some(_) => return Err(ApiError::bad("userId must be a Discord user id.")),
        None => session.user_id.clone(),
    };
    ok_json(&wallets::get_bettor_stats(&ctx.app.db, &user_id, &guild)?)
}

fn daily(ctx: &Ctx, session: &Session, headers: &HeaderMap) -> ApiResult {
    require_csrf(headers, session)?;
    let guild = guild_id(ctx)?;
    ok_json(&wallets::claim_daily(
        &ctx.app.db,
        &session.user_id,
        &guild,
    )?)
}

async fn history(ctx: &Ctx, session: &Session, query: &HashMap<String, String>) -> ApiResult {
    require_operator(session)?;
    let action = query.get("action").map(String::as_str).unwrap_or("");
    let target_type = query.get("targetType").map(String::as_str).unwrap_or("");
    let page = page_param(query);
    let logs: Vec<_> = wallets::get_audit_logs(&ctx.app.db, AUDIT_WINDOW)?
        .into_iter()
        .filter(|log| {
            (action.is_empty() || log.action.contains(action))
                && (target_type.is_empty() || log.target_type.contains(target_type))
        })
        .collect();
    let (slice, has_more) = paginate(logs, page);

    let wallet_target = |log: &crate::db::economy::AuditLogRow| -> Option<String> {
        if log.target_type == "wallet" {
            log.target_id.clone().filter(|id| !id.is_empty())
        } else {
            None
        }
    };
    let ids = slice
        .iter()
        .flat_map(|log| [Some(log.actor_user_id.clone()), wallet_target(log)])
        .flatten()
        .collect::<Vec<_>>();
    let names = ctx.names.resolve_many(ids.iter().map(String::as_str)).await;
    let entries: Vec<Value> = slice
        .iter()
        .map(|log| {
            let target = wallet_target(log)
                .map(|id| name_value(&names, &id))
                .unwrap_or(Value::Null);
            with_fields(
                log,
                vec![
                    ("actorUsername", name_value(&names, &log.actor_user_id)),
                    ("targetUsername", target),
                ],
            )
        })
        .collect();
    ok_json(&json!({ "entries": entries, "page": page, "hasMore": has_more }))
}

async fn admin_transactions(
    ctx: &Ctx,
    session: &Session,
    query: &HashMap<String, String>,
) -> ApiResult {
    require_admin(session)?;
    let guild = guild_id(ctx)?;
    let user_id = query
        .get("userId")
        .map(|u| u.trim())
        .filter(|u| !u.is_empty());
    let page = page_param(query);
    let all = wallets::get_wallet_transactions(&ctx.app.db, &guild, user_id, TRANSACTION_WINDOW)?;
    let (slice, has_more) = paginate(all, page);
    ok_json(&json!({
        "transactions": transactions_with_names(ctx, slice).await,
        "page": page,
        "hasMore": has_more,
    }))
}

// ---------------------------------------------------------------------------------------------
// Write routes
// ---------------------------------------------------------------------------------------------

async fn place_bet(ctx: &Ctx, session: &Session, headers: &HeaderMap, body: Body) -> ApiResult {
    require_csrf(headers, session)?;
    let body = read_json(body).await?;
    let guild = guild_id(ctx)?;
    let amount = field_i64(&body, "amount")?;
    let picked = field_string(&body, "picked")?;
    if body.get("marketId").is_some_and(|v| !v.is_null()) {
        let market_id = field_i64(&body, "marketId")?;
        tour::place_tournament_special_bet(
            &ctx.app.db,
            &guild,
            &session.user_id,
            market_id,
            &picked,
            amount,
        )?;
    } else {
        let match_id = field_i64(&body, "matchId")?;
        tour::place_tournament_bet(
            &ctx.app.db,
            &guild,
            &session.user_id,
            match_id,
            &picked,
            amount,
        )?;
    }
    ok_json(&json!({ "ok": true }))
}

async fn pay(ctx: &Ctx, session: &Session, headers: &HeaderMap, body: Body) -> ApiResult {
    require_csrf(headers, session)?;
    let body = read_json(body).await?;
    let guild = guild_id(ctx)?;
    let receiver = field_string(&body, "userId")?;
    let amount = field_i64(&body, "amount")?;
    // Only real, non-bot members of the server can receive coins; otherwise junk ids would
    // get wallets (and leaderboard rows), and "0123" would slip past the self-pay check.
    let receiver_id = receiver
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .ok_or_else(|| ApiError::bad("userId must be a Discord user id."))?;
    let receiver = receiver_id.to_string();
    if receiver == session.user_id {
        return Err(ApiError::bad("You cannot send MugCoins to yourself."));
    }
    let guild_snowflake = guild.parse::<u64>().map_err(|_| ApiError::bad("Server is not configured."))?;
    match ctx
        .app
        .http
        .get_member(serenity::all::GuildId::new(guild_snowflake), serenity::all::UserId::new(receiver_id))
        .await
    {
        Ok(member) if member.user.bot => return Err(ApiError::bad("You cannot send MugCoins to bots.")),
        Ok(_) => {}
        Err(_) => return Err(ApiError::bad("That user is not a member of the server.")),
    }
    let ok = wallets::pay_user(
        &ctx.app.db,
        &session.user_id,
        &receiver,
        &guild,
        amount,
    )?;
    ok_json(&json!({ "ok": ok }))
}

async fn bracket_sort(session: &Session, headers: &HeaderMap, body: Body) -> ApiResult {
    require_csrf(headers, session)?;
    require_operator(session)?;
    let body = read_json(body).await?;
    let input = match body.get("input") {
        Some(Value::String(s)) => s.as_str(),
        _ => "",
    };
    let size = if body.get("size").and_then(to_whole_number) == Some(32) {
        32
    } else {
        16
    };
    ok_json(&bracket_sorter::sort_bracket(input, size)?)
}

/// The operator and admin routes that change state. The announcement texts match the TS server.
async fn mutate(
    ctx: &Ctx,
    session: &Session,
    headers: &HeaderMap,
    path: &str,
    body: Body,
) -> ApiResult {
    require_csrf(headers, session)?;
    if path.starts_with("/api/admin/") {
        require_admin(session)?;
    } else {
        require_operator(session)?;
    }
    let body = read_json(body).await?;
    let guild = guild_id(ctx)?;
    let db = &ctx.app.db;
    let actor = session.user_id.as_str();

    match path {
        "/api/operator/open" => {
            let name = field_string(&body, "name")?;
            let matches = match body.get("matches") {
                Some(value @ Value::Array(_)) => tour::parse_bracket_json(value)?,
                _ => return Err(ApiError::bad("Bracket must contain at least one match.")),
            };
            let tournament_id =
                tour::open_tournament_from_bracket(db, actor, &guild, &name, &matches)?;
            tour::announce(
                &ctx.app.http,
                &format!("Successfully opened tournament **{name}** with {} matches. Betting is now open!", matches.len()),
            )
            .await;
            ok_json(&json!({ "ok": true, "tournamentId": tournament_id }))
        }
        "/api/operator/close-all" => {
            let closed_count = tour::close_all_tournament_matches(db, actor, &guild)?;
            tour::announce(
                &ctx.app.http,
                &format!("Betting has been **closed** for all {closed_count} open matches. No more bets can be placed."),
            )
            .await;
            ok_json(&json!({ "ok": true, "closedCount": closed_count }))
        }
        "/api/operator/resolve" => {
            let match_number = field_i64(&body, "matchNumber")?;
            let winner = field_string(&body, "winner")?;
            let result = tour::resolve_tournament_match(db, actor, &guild, match_number, &winner)?;
            tour::announce(
                &ctx.app.http,
                &format!(
                    "Round {}, Match {} resolved. Winner: **{}**.",
                    result.match_.round_number, result.match_.match_number, result.winner
                ),
            )
            .await;
            if result.tournament_ended {
                tour::announce(
                    &ctx.app.http,
                    &format!(
                        "All matches have been resolved. The tournament **{}** has automatically ended.",
                        result.tournament_name
                    ),
                )
                .await;
            }
            ok_json(&result)
        }
        "/api/operator/resolve-runner-up" => {
            let picked = field_string(&body, "picked")?;
            let result = tour::resolve_final_runner_up_market(db, actor, &guild, &picked)?;
            tour::announce(
                &ctx.app.http,
                &format!(
                    "Final Runner-Up market resolved. Third place: **{}**.",
                    result.picked
                ),
            )
            .await;
            ok_json(&result)
        }
        "/api/operator/close" => {
            let match_number = field_i64(&body, "matchNumber")?;
            let m = tour::close_tournament_match(db, actor, &guild, match_number)?;
            tour::announce(
                &ctx.app.http,
                &format!(
                    "Betting has been **closed** for Round {}, Match {} ({} vs {}). No more bets can be placed.",
                    m.round_number, m.match_number, m.contestant_a, m.contestant_b
                ),
            )
            .await;
            ok_json(&json!({ "ok": true, "match": m }))
        }
        "/api/operator/close-runner-up" => {
            let market = tour::close_final_runner_up_market(db, actor, &guild)?;
            tour::announce(
                &ctx.app.http,
                &format!(
                    "Betting has been **closed** for **{}**. No more bets can be placed.",
                    market.title
                ),
            )
            .await;
            ok_json(&json!({ "ok": true, "market": market }))
        }
        "/api/operator/advance-round" => {
            let result = tour::advance_tournament_round(db, actor, &guild)?;
            tour::announce(
                &ctx.app.http,
                &format!(
                    "Advanced to round **{}** with **{}** matches.",
                    result.round_number, result.match_count
                ),
            )
            .await;
            ok_json(&result)
        }
        "/api/operator/end" => {
            let active = tour::get_tournament_overview(db, &guild, actor)?.tournament;
            tour::complete_tournament(db, actor, &guild)?;
            if let Some(active) = active {
                tour::announce(
                    &ctx.app.http,
                    &format!("Tournament **{}** has been marked as completed. Pending bets have been refunded.", active.name),
                )
                .await;
            }
            ok_json(&json!({ "ok": true }))
        }
        "/api/admin/give" | "/api/admin/set" | "/api/admin/take" => {
            let target = field_string(&body, "userId")?;
            let target = target.trim();
            let amount = field_i64(&body, "amount")?;
            let ok = match path {
                "/api/admin/give" => {
                    wallets::admin_give_user_balance(db, actor, target, &guild, amount)?
                }
                "/api/admin/set" => {
                    wallets::admin_set_user_balance(db, actor, target, &guild, amount)?
                }
                _ => wallets::admin_take_user_balance(db, actor, target, &guild, amount)?,
            };
            ok_json(&json!({ "ok": ok }))
        }
        _ => Err(ApiError::new(StatusCode::NOT_FOUND, "not_found")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn whole_numbers_accept_numbers_and_numeric_strings() {
        assert_eq!(to_whole_number(&json!(5)), Some(5));
        assert_eq!(to_whole_number(&json!(5.0)), Some(5));
        assert_eq!(to_whole_number(&json!("12")), Some(12));
        assert_eq!(to_whole_number(&json!(" 7 ")), Some(7));
        assert_eq!(to_whole_number(&json!(1.5)), None);
        assert_eq!(to_whole_number(&json!("abc")), None);
        assert_eq!(to_whole_number(&json!(null)), None);
        assert_eq!(to_whole_number(&json!(1e300)), None);
    }

    #[test]
    fn bad_numbers_give_a_message() {
        let err = field_i64(&json!({ "amount": "x" }), "amount")
            .err()
            .unwrap();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.message, "amount must be a whole number.");
    }

    #[test]
    fn paging_matches_the_ts_windows() {
        let (items, more) = paginate((0..60).collect::<Vec<_>>(), 1);
        assert_eq!(items.len(), 25);
        assert!(more);
        let (items, more) = paginate((0..60).collect::<Vec<_>>(), 3);
        assert_eq!(items.len(), 10);
        assert!(!more);
        let (items, more) = paginate((0..60).collect::<Vec<_>>(), MAX_PAGE);
        assert!(items.is_empty());
        assert!(!more);
    }

    #[test]
    fn query_parsing_never_panics() {
        assert_eq!(page_param(&q(&[("page", "-3")])), 1);
        assert_eq!(page_param(&q(&[("page", "abc")])), 1);
        assert_eq!(page_param(&q(&[("page", "2.9")])), 2);
        assert_eq!(page_param(&q(&[("page", "1e99")])), MAX_PAGE);
        assert_eq!(limit_param(&q(&[])), 10);
        assert_eq!(limit_param(&q(&[("limit", "25")])), 25);
        assert_eq!(limit_param(&q(&[("limit", "0")])), 10);
        assert_eq!(limit_param(&q(&[("limit", "100000")])), 100);
    }
}
