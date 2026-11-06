//! /tournament: brackets, betting panel, polls, resolution.
//!
//! Slash commands, the component/modal handlers behind the betting panel (custom ids
//! `tournament_*`), and everything Discord-poll related: posting polls, auto-resolving them when
//! they finalize (gateway `MessageUpdate` plus a once-a-minute background check), resolving
//! from an arbitrary poll link, and the vote reward.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail};
use once_cell::sync::Lazy;
use poise::serenity_prelude as serenity;
use regex::Regex;
use serenity::{
    ActionRowComponent, ButtonStyle, ChannelId, ComponentInteraction, ComponentInteractionDataKind, CreateActionRow,
    CreateButton, CreateEmbed, CreateInputText, CreateInteractionResponse, CreateInteractionResponseMessage,
    CreateMessage, CreateModal, CreatePoll, CreatePollAnswer, CreateSelectMenu, CreateSelectMenuKind,
    CreateSelectMenuOption, FullEvent, InputTextStyle, Interaction, MessageId, ModalInteraction,
};

use crate::bot::{Context, Data, Error};
use crate::config;
use crate::db::tournament::{MatchPollRecord, MatchRecord, POLL_ACTIVE, POLL_TIED};
use crate::db::Db;
use crate::services::tournaments::{
    self, PollAnswerTally, PollApplyOutcome, TournamentSpecialMarketView,
};
use crate::services::{authz, wallets};

const SPECIAL_MARKET_PAGE_SIZE: usize = 25;
const MODAL_TITLE_LIMIT: usize = 45;
const SELECT_OPTION_LIMIT: usize = 25;
const EMBED_CHUNK: usize = 3500;
const BLUE: u32 = 0x00bfff;

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

fn clamp_modal_title(title: &str) -> String {
    tournaments::truncate_chars(title, MODAL_TITLE_LIMIT)
}

fn format_match_label(round: i64, number: i64, a: &str, b: &str) -> String {
    format!("Round {round}, Match {number}: {a} vs {b}")
}

fn format_bet_status(status: &str, payout: i64) -> String {
    match status {
        "pending" => "Pending".to_string(),
        "won" => format!("Won (+{payout})"),
        "lost" => "Lost".to_string(),
        "refunded" => "Refunded".to_string(),
        other => other.to_string(),
    }
}

/// JavaScript `parseInt(text, 10)`: optional whitespace and sign, then leading digits.
fn parse_int_prefix(text: &str) -> Option<i64> {
    let text = text.trim_start();
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<i64>().ok().map(|n| n * sign)
}

/// Splits text blocks into chunks of at most `limit` characters without cutting a block.
fn chunk_blocks(blocks: &[String], limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for block in blocks {
        let block = tournaments::truncate_chars(block, limit);
        if !current.is_empty() && current.chars().count() + block.chars().count() > limit {
            out.push(std::mem::take(&mut current));
        }
        current.push_str(&block);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

static MESSAGE_LINK: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^https?://(?:(?:ptb|canary)\.)?discord(?:app)?\.com/channels/(\d+)/(\d+)/(\d+)/?(?:[?#].*)?$").unwrap()
});

/// (guild, channel, message) from a Discord message link.
fn parse_message_link(link: &str) -> Option<(u64, u64, u64)> {
    let link = link.trim().trim_start_matches('<').trim_end_matches('>');
    let caps = MESSAGE_LINK.captures(link)?;
    let ids = (caps[1].parse().ok()?, caps[2].parse().ok()?, caps[3].parse().ok()?);
    (ids.0 != 0 && ids.1 != 0 && ids.2 != 0).then_some(ids)
}

fn is_not_found(err: &serenity::Error) -> bool {
    matches!(
        err,
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(resp)) if resp.status_code.as_u16() == 404
    )
}

/// Answer texts and (final or current) vote counts of a Discord poll.
fn poll_tallies(poll: &serenity::Poll) -> Vec<PollAnswerTally> {
    poll.answers
        .iter()
        .map(|answer| {
            let id = answer.answer_id.get();
            let votes = poll
                .results
                .as_ref()
                .and_then(|r| r.answer_counts.iter().find(|c| c.id.get() == id))
                .map(|c| c.count)
                .unwrap_or(0);
            PollAnswerTally { id, text: answer.poll_media.text.clone().unwrap_or_default(), votes }
        })
        .collect()
}

fn poll_is_finalized(poll: &serenity::Poll) -> bool {
    poll.results.as_ref().is_some_and(|r| r.is_finalized)
}

fn poll_has_expired(poll: &serenity::Poll) -> bool {
    poll.expiry.is_some_and(|e| e.unix_timestamp() <= chrono::Utc::now().timestamp())
}

// ---------------------------------------------------------------------------------------------
// Panels (messages built from an embed and components)
// ---------------------------------------------------------------------------------------------

/// A message body that can be sent as a slash reply, a new interaction message or an update of
/// the message a component sits on. Updates always send all three fields so stale content,
/// embeds and components are cleared.
struct Panel {
    content: Option<String>,
    embeds: Vec<CreateEmbed>,
    components: Vec<CreateActionRow>,
}

impl Panel {
    fn text(content: impl Into<String>) -> Self {
        Panel { content: Some(content.into()), embeds: Vec::new(), components: Vec::new() }
    }

    fn embed(embed: CreateEmbed, components: Vec<CreateActionRow>) -> Self {
        Panel { content: None, embeds: vec![embed], components }
    }

    fn message(self, ephemeral: bool) -> CreateInteractionResponseMessage {
        let mut message = CreateInteractionResponseMessage::new()
            .embeds(self.embeds)
            .components(self.components)
            .ephemeral(ephemeral);
        if let Some(content) = self.content {
            message = message.content(content);
        }
        message
    }

    fn update_response(self) -> CreateInteractionResponse {
        CreateInteractionResponse::UpdateMessage(self.message(false))
    }

    fn ephemeral_response(self) -> CreateInteractionResponse {
        CreateInteractionResponse::Message(self.message(true))
    }

    fn reply(self) -> poise::CreateReply {
        let mut reply = poise::CreateReply::default().ephemeral(true).components(self.components);
        for embed in self.embeds {
            reply = reply.embed(embed);
        }
        if let Some(content) = self.content {
            reply = reply.content(content);
        }
        reply
    }
}

fn button(custom_id: impl Into<String>, label: &str, style: ButtonStyle) -> CreateButton {
    CreateButton::new(custom_id).label(tournaments::truncate_chars(label, 80)).style(style)
}

fn build_bet_home(db: &Db, guild_id: &str, user_id: &str) -> anyhow::Result<Panel> {
    let overview = tournaments::get_tournament_overview(db, guild_id, user_id)?;
    let has_open_matches = overview.matches.iter().any(|m| m.betting_open && m.winner.is_none());
    let open_special: Vec<&TournamentSpecialMarketView> =
        overview.special_markets.iter().filter(|m| m.status == "open").collect();

    if !has_open_matches && open_special.is_empty() {
        return Ok(Panel::text("There are currently no open betting markets."));
    }

    let mut buttons = Vec::new();
    if has_open_matches {
        buttons.push(button("tournament_bet_market:matches", "Match Bets", ButtonStyle::Primary));
    }
    for market in &open_special {
        buttons.push(button(
            format!("tournament_bet_market:special:{}:0", market.id),
            &market.title,
            ButtonStyle::Secondary,
        ));
    }

    let name = overview.tournament.as_ref().map(|t| t.name.as_str()).unwrap_or("Tournament");
    let embed = CreateEmbed::new()
        .colour(BLUE)
        .title(format!("Betting Panel: {name}"))
        .description(format!("Your balance: **{} MugCoins**\n\nChoose a betting market below.", overview.wallet_balance));
    Ok(Panel::embed(embed, vec![CreateActionRow::Buttons(buttons)]))
}

fn build_special_market_page(market: &TournamentSpecialMarketView, page: usize) -> Panel {
    let last_page = market.options.len().saturating_sub(1) / SPECIAL_MARKET_PAGE_SIZE;
    let page = page.min(last_page);
    let start = page * SPECIAL_MARKET_PAGE_SIZE;

    let options: Vec<CreateSelectMenuOption> = market
        .options
        .iter()
        .enumerate()
        .skip(start)
        .take(SPECIAL_MARKET_PAGE_SIZE)
        .map(|(index, option)| {
            CreateSelectMenuOption::new(option.chars().take(100).collect::<String>(), index.to_string())
        })
        .collect();
    let select = CreateSelectMenu::new(
        format!("tournament_special_select:{}:{}", market.id, page),
        CreateSelectMenuKind::String { options },
    )
    .placeholder(format!("Pick for {}", market.title));

    let controls = CreateActionRow::Buttons(vec![
        button(
            format!("tournament_bet_market:special:{}:{}", market.id, page.saturating_sub(1)),
            "Previous",
            ButtonStyle::Secondary,
        )
        .disabled(page == 0),
        button("tournament_bet_market:home", "Back", ButtonStyle::Secondary),
        button(format!("tournament_bet_market:special:{}:{}", market.id, page + 1), "Next", ButtonStyle::Secondary)
            .disabled(start + SPECIAL_MARKET_PAGE_SIZE >= market.options.len()),
    ]);

    let total_pages = market.options.len().div_ceil(SPECIAL_MARKET_PAGE_SIZE).max(1);
    let embed = CreateEmbed::new()
        .colour(BLUE)
        .title(format!("Betting Panel: {}", market.title))
        .description(format!("{}\n\nPage **{}** of **{}**.", market.description, page + 1, total_pages));
    Panel::embed(embed, vec![CreateActionRow::SelectMenu(select), controls])
}

fn amount_modal(custom_id: String, picked: &str, balance: i64) -> CreateModal {
    CreateModal::new(custom_id, clamp_modal_title(&format!("Bet on {picked}"))).components(vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, format!("Amount to bet (Balance: {balance})"), "amountInput")
                .required(true),
        ),
    ])
}

// ---------------------------------------------------------------------------------------------
// Command plumbing
// ---------------------------------------------------------------------------------------------

fn currency_guild(ctx: Context<'_>) -> Result<String, Error> {
    let Some(guild_id) = ctx.guild_id() else {
        bail!("This command can only be used in a server.");
    };
    if !config::get().is_currency_guild(Some(guild_id)) {
        bail!("Tournaments are not available in this server.");
    }
    Ok(guild_id.to_string())
}

fn require_operator(ctx: Context<'_>) -> Result<(), Error> {
    if authz::can_manage_tournaments(ctx.author().id.get()) {
        Ok(())
    } else {
        bail!("You do not have permission to manage tournaments.")
    }
}

async fn reply_ephemeral(ctx: Context<'_>, content: impl Into<String>) -> Result<(), Error> {
    ctx.send(poise::CreateReply::default().content(content).ephemeral(true)).await?;
    Ok(())
}

/// Anime Tournament Betting
#[poise::command(
    slash_command,
    subcommands(
        "open",
        "status",
        "bet",
        "mybets",
        "close",
        "resolve",
        "resolve_runner_up",
        "end",
        "advance",
        "poll",
        "resolve_from_poll"
    ),
    subcommand_required
)]
pub async fn tournament(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Create a tournament from a JSON bracket file
#[poise::command(slash_command)]
async fn open(
    ctx: Context<'_>,
    #[description = "Tournament Name"] name: String,
    #[description = "JSON file with matches"] bracket_file: serenity::Attachment,
) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    require_operator(ctx)?;
    let data = ctx.data();

    if let Some(active) = tournaments::get_active_tournament(&data.db, &guild)? {
        return reply_ephemeral(ctx, format!("A tournament is already active: **{}**. End it first.", active.name)).await;
    }
    if !bracket_file.filename.ends_with(".json") {
        return reply_ephemeral(ctx, "File must be a .json file.").await;
    }

    ctx.defer().await?;
    let outcome = async {
        if bracket_file.size > 1_000_000 {
            bail!("The bracket file is too large.");
        }
        let body = data.client.get(&bracket_file.url).send().await?.error_for_status()?.text().await?;
        let value: serde_json::Value = serde_json::from_str(&body)?;
        let matches = tournaments::parse_bracket_json(&value)?;
        tournaments::open_tournament_from_bracket(&data.db, &ctx.author().id.to_string(), &guild, &name, &matches)?;
        Ok::<usize, anyhow::Error>(matches.len())
    }
    .await;

    match outcome {
        Ok(count) => {
            ctx.say(format!(
                "Successfully opened tournament **{name}** with {count} matches. Match betting and winner futures are now open."
            ))
            .await?;
        }
        Err(err) => {
            ctx.say(format!("Error parsing bracket JSON: {err}")).await?;
        }
    }
    Ok(())
}

fn poll_status_line(poll: &tournaments::MatchPollView) -> String {
    match poll.status.as_str() {
        POLL_ACTIVE => match tournaments::sql_to_unix(&poll.expires_at) {
            Some(unix) => format!("Poll: {} (ends <t:{unix}:R>)\n", poll.url),
            None => format!("Poll: {}\n", poll.url),
        },
        POLL_TIED => format!("Poll: {} (tied, awaiting manual resolution)\n", poll.url),
        _ => format!("Poll: {} (decided)\n", poll.url),
    }
}

/// Show current tournament status
#[poise::command(slash_command)]
async fn status(ctx: Context<'_>) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    let data = ctx.data();
    let user = ctx.author().id.to_string();
    let Some(active) = tournaments::get_active_tournament(&data.db, &guild)? else {
        return reply_ephemeral(ctx, "No active tournament.").await;
    };

    let overview = tournaments::get_tournament_overview(&data.db, &guild, &user)?;
    let counts = tournaments::get_match_bet_counts(&data.db, active.id)?;

    let mut blocks: Vec<String> = Vec::new();
    for m in &overview.matches {
        let state = match (&m.winner, m.betting_open) {
            (Some(winner), _) => format!("Winner: {winner}"),
            (None, true) => "Betting Open".to_string(),
            (None, false) => "Betting Closed".to_string(),
        };
        let mut block = format!(
            "**{}**\nStatus: {state}\nBets Placed: **{}**\n",
            format_match_label(m.round_number, m.match_number, &m.contestant_a, &m.contestant_b),
            counts.get(&m.id).copied().unwrap_or(0)
        );
        if let Some(poll) = &m.poll {
            block.push_str(&poll_status_line(poll));
        }
        block.push('\n');
        blocks.push(block);
    }
    if !overview.special_markets.is_empty() {
        let mut section = String::from("**Special Markets**\n");
        for market in &overview.special_markets {
            section.push_str(&format!("{}: **{}**", market.title, market.status));
            if !market.options.is_empty() {
                section.push_str(&format!(" ({})", market.options.join(" vs ")));
            }
            section.push('\n');
        }
        blocks.push(section);
    }

    let chunks = chunk_blocks(&blocks, EMBED_CHUNK);
    if chunks.is_empty() {
        let embed = CreateEmbed::new()
            .colour(0xffaa00)
            .title(format!("Tournament: {}", active.name))
            .description("No matches found for this tournament.");
        ctx.send(poise::CreateReply::default().embed(embed)).await?;
        return Ok(());
    }
    for (index, chunk) in chunks.into_iter().enumerate() {
        let mut embed = CreateEmbed::new().colour(0xffaa00).description(chunk);
        if index == 0 {
            embed = embed.title(format!("Tournament: {}", active.name));
        }
        ctx.send(poise::CreateReply::default().embed(embed)).await?;
    }
    Ok(())
}

/// Open the interactive betting panel
#[poise::command(slash_command)]
async fn bet(ctx: Context<'_>) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    let data = ctx.data();
    if tournaments::get_active_tournament(&data.db, &guild)?.is_none() {
        return reply_ephemeral(ctx, "No active tournament.").await;
    }
    let panel = build_bet_home(&data.db, &guild, &ctx.author().id.to_string())?;
    ctx.send(panel.reply()).await?;
    Ok(())
}

/// Show your active bets
#[poise::command(slash_command)]
async fn mybets(ctx: Context<'_>) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    let data = ctx.data();
    let Some(active) = tournaments::get_active_tournament(&data.db, &guild)? else {
        return reply_ephemeral(ctx, "No active tournament.").await;
    };

    let bets = tournaments::get_current_user_bets(&data.db, &guild, &ctx.author().id.to_string())?;
    if bets.match_bets.is_empty() && bets.special_bets.is_empty() {
        return reply_ephemeral(ctx, "You have no bets in this tournament.").await;
    }

    let mut blocks = Vec::new();
    for bet in &bets.match_bets {
        blocks.push(format!(
            "**{}**\nPicked: **{}** | Amount: **{}** | {}\n\n",
            format_match_label(bet.round_number, bet.match_number, &bet.contestant_a, &bet.contestant_b),
            bet.picked,
            bet.amount,
            format_bet_status(&bet.status, bet.payout)
        ));
    }
    for bet in &bets.special_bets {
        blocks.push(format!(
            "**{}**\nPicked: **{}** | Amount: **{}** | {}\n\n",
            bet.title,
            bet.picked,
            bet.amount,
            format_bet_status(&bet.status, bet.payout)
        ));
    }

    for (index, chunk) in chunk_blocks(&blocks, EMBED_CHUNK).into_iter().enumerate() {
        let mut embed = CreateEmbed::new().colour(0x00ff00).description(chunk);
        if index == 0 {
            embed = embed.title(format!("Your Bets: {}", active.name)).thumbnail(ctx.author().face());
        }
        ctx.send(poise::CreateReply::default().embed(embed).ephemeral(true)).await?;
    }
    Ok(())
}

/// Close betting for one match, the runner-up market, or all matches
#[poise::command(slash_command)]
async fn close(
    ctx: Context<'_>,
    #[description = "Match Number (e.g. 1)"] match_number: Option<i64>,
    #[description = "Close the Final Runner-Up market"] runner_up: Option<bool>,
    #[description = "Close all open matches"] all: Option<bool>,
) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    require_operator(ctx)?;
    let data = ctx.data();
    let actor = ctx.author().id.to_string();
    if tournaments::get_active_tournament(&data.db, &guild)?.is_none() {
        return reply_ephemeral(ctx, "No active tournament.").await;
    }

    let match_number = match_number.filter(|n| *n != 0);
    if match_number.is_none() && all != Some(true) && runner_up != Some(true) {
        return reply_ephemeral(ctx, "You must specify a match_number, set runner_up to true, or set all to true.").await;
    }

    ctx.defer().await?;
    if all == Some(true) {
        let closed = tournaments::close_all_tournament_matches(&data.db, &actor, &guild)?;
        ctx.say(format!(
            "Betting has been **closed** for all {closed} open matches and any linked futures markets that depended on them."
        ))
        .await?;
        return Ok(());
    }
    if runner_up == Some(true) {
        let market = tournaments::close_final_runner_up_market(&data.db, &actor, &guild)?;
        ctx.say(format!("Betting has been **closed** for **{}**.", market.title)).await?;
        return Ok(());
    }
    let number = match_number.unwrap_or_default();
    let m = tournaments::close_tournament_match(&data.db, &actor, &guild, number)?;
    ctx.say(format!(
        "Betting has been **closed** for {}.",
        format_match_label(m.round_number, m.match_number, &m.contestant_a, &m.contestant_b)
    ))
    .await?;
    Ok(())
}

/// Resolve a match
#[poise::command(slash_command)]
async fn resolve(
    ctx: Context<'_>,
    #[description = "Match Number (e.g. 1)"] match_number: i64,
    #[description = "Winner name (exact match)"] winner: String,
) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    require_operator(ctx)?;
    let data = ctx.data();
    if tournaments::get_active_tournament(&data.db, &guild)?.is_none() {
        return reply_ephemeral(ctx, "No active tournament.").await;
    }

    ctx.defer().await?;
    let result =
        tournaments::resolve_tournament_match(&data.db, &ctx.author().id.to_string(), &guild, match_number, &winner)?;
    ctx.say(format!("Resolved {}. Winner: **{}**.", result.match_.label(), result.winner)).await?;
    if !result.resolved_special_markets.is_empty() {
        let lines: Vec<String> = result
            .resolved_special_markets
            .iter()
            .map(|m| format!("Resolved **{}** on **{}**.", m.market_type, m.winning_pick))
            .collect();
        ctx.say(lines.join("\n")).await?;
    }
    if result.tournament_ended {
        ctx.say(format!(
            "**All matches have been resolved.** The tournament **{}** has automatically ended.",
            result.tournament_name
        ))
        .await?;
    }
    Ok(())
}

/// Manually resolve the Final Runner-Up market
#[poise::command(slash_command, rename = "resolve-runner-up")]
async fn resolve_runner_up(
    ctx: Context<'_>,
    #[description = "Third-place finisher from the semi-final losers"] picked: String,
) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    require_operator(ctx)?;
    let data = ctx.data();
    if tournaments::get_active_tournament(&data.db, &guild)?.is_none() {
        return reply_ephemeral(ctx, "No active tournament.").await;
    }

    ctx.defer().await?;
    let result = tournaments::resolve_final_runner_up_market(&data.db, &ctx.author().id.to_string(), &guild, &picked)?;
    ctx.say(format!("Resolved Final Runner-Up market. Third place: **{}**.", result.picked)).await?;
    Ok(())
}

/// End the current tournament
#[poise::command(slash_command)]
async fn end(ctx: Context<'_>) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    require_operator(ctx)?;
    let data = ctx.data();
    let Some(active) = tournaments::get_active_tournament(&data.db, &guild)? else {
        return reply_ephemeral(ctx, "No active tournament.").await;
    };

    ctx.defer().await?;
    tournaments::complete_tournament(&data.db, &ctx.author().id.to_string(), &guild)?;
    ctx.say(format!(
        "Tournament **{}** has been marked as completed. Any unresolved bets were refunded.",
        active.name
    ))
    .await?;
    Ok(())
}

/// Advance to the next round after all current matches are resolved
#[poise::command(slash_command)]
async fn advance(ctx: Context<'_>) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    require_operator(ctx)?;
    let data = ctx.data();
    if tournaments::get_active_tournament(&data.db, &guild)?.is_none() {
        return reply_ephemeral(ctx, "No active tournament.").await;
    }

    ctx.defer().await?;
    let result = tournaments::advance_tournament_round(&data.db, &ctx.author().id.to_string(), &guild)?;
    ctx.say(format!("Advanced to round **{}** with **{}** matches.", result.round_number, result.match_count)).await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Polls: /tournament poll
// ---------------------------------------------------------------------------------------------

/// Post Discord polls that decide matches
#[poise::command(slash_command)]
async fn poll(
    ctx: Context<'_>,
    #[description = "Match Number (default: every unresolved match in the current round without a poll)"]
    match_number: Option<i64>,
    #[description = "Poll duration in hours (default 24)"]
    #[min = 1]
    #[max = 768]
    duration_hours: Option<i64>,
    #[description = "Channel to post the polls in (default: this channel)"]
    #[channel_types("Text", "News")]
    channel: Option<serenity::GuildChannel>,
    #[description = "Close betting for the match when its poll is posted (default: true)"] close_betting: Option<bool>,
) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    require_operator(ctx)?;
    let data = ctx.data();
    let actor = ctx.author().id.to_string();
    let Some(_) = tournaments::get_active_tournament(&data.db, &guild)? else {
        return reply_ephemeral(ctx, "No active tournament.").await;
    };

    let hours = duration_hours.unwrap_or(24).clamp(1, 768) as u64;
    let close_betting = close_betting.unwrap_or(true);
    let channel_id = channel.map(|c| c.id).unwrap_or_else(|| ctx.channel_id());
    let (_, targets) = tournaments::prepare_match_polls(&data.db, &actor, &guild, match_number)?;

    ctx.defer().await?;
    let mut lines = Vec::new();
    let mut closed_any = false;
    for m in &targets {
        match post_match_poll(ctx, &actor, &guild, channel_id, m, hours, close_betting).await {
            Ok((url, closed)) => {
                closed_any |= closed;
                lines.push(format!("{}: {url}", m.label()));
            }
            Err(err) => {
                tracing::warn!("posting poll for match {} failed: {err:#}", m.id);
                lines.push(format!("{}: failed ({err})", m.label()));
            }
        }
    }

    let mut text = format!("Posted {} poll(s) in <#{}>, open for {hours} hour(s):\n", targets.len(), channel_id);
    text.push_str(&lines.join("\n"));
    if closed_any {
        text.push_str("\nBetting was closed for the matches that were open.");
    }
    ctx.say(tournaments::truncate_chars(&text, 2000)).await?;
    Ok(())
}

/// Sends one poll and records it. Returns the poll link and whether betting was closed.
async fn post_match_poll(
    ctx: Context<'_>,
    actor: &str,
    guild: &str,
    channel_id: ChannelId,
    m: &MatchRecord,
    hours: u64,
    close_betting: bool,
) -> anyhow::Result<(String, bool)> {
    let poll = CreatePoll::new()
        .question(tournaments::poll_question(m))
        .answers(vec![
            CreatePollAnswer::new().text(tournaments::poll_answer_text(&m.contestant_a)),
            CreatePollAnswer::new().text(tournaments::poll_answer_text(&m.contestant_b)),
        ])
        .duration(Duration::from_secs(hours * 3600));
    let message = channel_id.send_message(ctx, CreateMessage::new().poll(poll)).await?;

    let expires_unix = message
        .poll
        .as_ref()
        .and_then(|p| p.expiry)
        .map(|e| e.unix_timestamp())
        .unwrap_or_else(|| chrono::Utc::now().timestamp() + hours as i64 * 3600);
    let expires_at = tournaments::unix_to_sql(expires_unix);

    let registered = tournaments::register_match_poll(
        &ctx.data().db,
        actor,
        guild,
        m,
        &channel_id.to_string(),
        &message.id.to_string(),
        &expires_at,
        close_betting,
    );
    match registered {
        Ok(closed) => Ok((tournaments::poll_url(guild, &channel_id.to_string(), &message.id.to_string()), closed)),
        Err(err) => {
            // Do not leave an untracked poll running.
            if let Err(end_err) = message.end_poll(ctx).await {
                tracing::warn!("could not end untracked poll {}: {end_err}", message.id);
            }
            Err(err)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Polls: /tournament resolve-from-poll
// ---------------------------------------------------------------------------------------------

/// Resolve a match from the results of a Discord poll message
#[poise::command(slash_command, rename = "resolve-from-poll")]
async fn resolve_from_poll(
    ctx: Context<'_>,
    #[description = "Link to the poll message"] message_link: String,
    #[description = "Match Number (default: inferred from the poll answers)"] match_number: Option<i64>,
    #[description = "End the poll now if it is mine and still open (default: false)"] end_now: Option<bool>,
) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    require_operator(ctx)?;
    let data = ctx.data();
    let actor = ctx.author().id.to_string();
    if tournaments::get_active_tournament(&data.db, &guild)?.is_none() {
        return reply_ephemeral(ctx, "No active tournament.").await;
    }

    let Some((link_guild, channel_raw, message_raw)) = parse_message_link(&message_link) else {
        return reply_ephemeral(
            ctx,
            "That is not a valid message link. Expected https://discord.com/channels/<server>/<channel>/<message>.",
        )
        .await;
    };
    if link_guild.to_string() != guild {
        return reply_ephemeral(ctx, "That message is not in this server.").await;
    }

    ctx.defer().await?;
    let channel_id = ChannelId::new(channel_raw);
    // The guild segment of a link is just text; check the channel really belongs here.
    let channel_guild = channel_id
        .to_channel(ctx)
        .await
        .map_err(|e| anyhow!("Could not fetch that channel: {e}"))?
        .guild()
        .map(|c| c.guild_id.to_string());
    if channel_guild.as_deref() != Some(guild.as_str()) {
        bail!("That message is not in this server.");
    }
    let mut message = channel_id
        .message(ctx, MessageId::new(message_raw))
        .await
        .map_err(|e| anyhow!("Could not fetch that message: {e}"))?;
    if message.poll.is_none() {
        bail!("That message does not contain a poll.");
    }

    // Only Discord's finalized counts are exact; payouts are irreversible, so never resolve on
    // provisional counts (an expired-but-not-finalized poll is still being tallied).
    let (finalized, expired) = message.poll.as_deref().map(|p| (poll_is_finalized(p), poll_has_expired(p))).unwrap_or_default();
    if !finalized {
        if expired {
            bail!("That poll has ended and Discord is still finalizing the results. Try again in a minute.");
        }
        if end_now != Some(true) {
            bail!("That poll is still open. Wait for it to end, or set end_now to true if I created it.");
        }
        if message.author.id != ctx.framework().bot_id {
            bail!("I can only end polls that I created. End the poll yourself, then run this command again.");
        }
        message = message.end_poll(ctx).await.map_err(|e| anyhow!("Could not end the poll: {e}"))?;
        if !message.poll.as_deref().is_some_and(poll_is_finalized) {
            bail!("Poll ended. Discord is finalizing the results; run this command again in a minute (without end_now).");
        }
    }

    let Some(discord_poll) = message.poll.as_deref() else {
        bail!("That message does not contain a poll.");
    };
    let answers = poll_tallies(discord_poll);
    if answers.len() != 2 {
        bail!("The poll must have exactly two answers that match the contestants of a match.");
    }

    let target = match match_number {
        Some(number) => {
            let m = tournaments::find_match_by_number(&data.db, &guild, number)?;
            if tournaments::match_poll_answers(&answers, &m.contestant_a, &m.contestant_b).is_none() {
                bail!(
                    "The poll answers must be {} and {} (match {} of round {}).",
                    m.contestant_a,
                    m.contestant_b,
                    m.match_number,
                    m.round_number
                );
            }
            m
        }
        None => {
            let (_, candidates) = tournaments::unresolved_current_round_matches(&data.db, &guild)?;
            let mut matching: Vec<MatchRecord> = candidates
                .into_iter()
                .filter(|m| tournaments::match_poll_answers(&answers, &m.contestant_a, &m.contestant_b).is_some())
                .collect();
            match matching.len() {
                0 => bail!("No unresolved match in the current round has contestants matching the poll answers."),
                1 => matching.remove(0),
                _ => bail!("More than one unresolved match matches the poll answers. Specify match_number."),
            }
        }
    };

    let result = tournaments::resolve_match_from_poll(
        &data.db,
        &actor,
        &guild,
        target.id,
        &message.id.to_string(),
        &answers,
    )?;

    let mut reply = format!("Resolved {} from the poll. Winner: **{}**.", result.match_.label(), result.winner);
    if result.tournament_ended {
        reply.push_str(&format!(
            "\n**All matches have been resolved.** The tournament **{}** has automatically ended.",
            result.tournament_name
        ));
    }
    ctx.say(reply.clone()).await?;

    // Keep the updates channel in the loop unless the command was already used there.
    let updates = config::get().channels.tournament_updates_channel_id;
    if updates.is_some_and(|c| c != ctx.channel_id().get()) {
        tournaments::announce(&ctx.serenity_context().http, &reply).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Polls: finalization (gateway, background check) and vote rewards
// ---------------------------------------------------------------------------------------------

async fn announce_poll_result(http: &serenity::Http, poll_channel: ChannelId, content: &str) {
    let content = tournaments::truncate_chars(content, 2000);
    if let Err(err) = crate::bot::say_safe(http, poll_channel, content.clone()).await {
        tracing::error!("failed to announce poll result in {poll_channel}: {err}");
    }
    let updates = config::get().channels.tournament_updates_channel_id;
    if updates.is_some_and(|c| c != poll_channel.get()) {
        tournaments::announce(http, &content).await;
    }
}

/// Fetches a tracked poll's message and, when Discord reports final results, applies them.
/// Safe to call repeatedly and concurrently: only one call can resolve a match.
async fn finalize_tracked_poll(http: &serenity::Http, data: &Data, poll: &MatchPollRecord) -> Result<(), Error> {
    let ids = poll.channel_id.parse::<u64>().ok().zip(poll.message_id.parse::<u64>().ok());
    let Some((channel_raw, message_raw)) = ids.filter(|(c, m)| *c != 0 && *m != 0) else {
        tournaments::cancel_match_poll(&data.db, &poll.message_id)?;
        return Ok(());
    };
    let channel_id = ChannelId::new(channel_raw);

    let message = match channel_id.message(http, MessageId::new(message_raw)).await {
        Ok(message) => message,
        Err(err) if is_not_found(&err) => {
            tournaments::cancel_match_poll(&data.db, &poll.message_id)?;
            let label = tournaments::get_match(&data.db, poll.match_id)?
                .map(|m| m.label())
                .unwrap_or_else(|| format!("match {}", poll.match_id));
            tournaments::announce(
                http,
                &format!("The poll for **{label}** was deleted, so it will not resolve the match. An operator must resolve it manually."),
            )
            .await;
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };

    let Some(discord_poll) = message.poll.as_deref() else {
        tournaments::cancel_match_poll(&data.db, &poll.message_id)?;
        return Ok(());
    };
    if !poll_is_finalized(discord_poll) {
        return Ok(());
    }

    let answers = poll_tallies(discord_poll);
    match tournaments::apply_poll_result(&data.db, &poll.message_id, &answers)? {
        PollApplyOutcome::Resolved { result, votes_for, votes_against } => {
            let mut text = format!(
                "The poll for **{}** has ended. **{}** wins with {votes_for} vote(s) to {votes_against}. Bets have been paid out.",
                result.match_.label(),
                result.winner
            );
            if result.tournament_ended {
                text.push_str(&format!(
                    "\n**All matches have been resolved.** The tournament **{}** has automatically ended.",
                    result.tournament_name
                ));
            }
            announce_poll_result(http, channel_id, &text).await;
        }
        PollApplyOutcome::Tied { votes, match_ } => {
            let how = if votes == 0 { "received no votes".to_string() } else { format!("ended in a tie ({votes}-{votes})") };
            announce_poll_result(
                http,
                channel_id,
                &format!(
                    "The poll for **{}** {how}. No winner was picked: an operator must resolve this match manually with `/tournament resolve`.",
                    match_.label()
                ),
            )
            .await;
        }
        PollApplyOutcome::Unmatched { match_ } => {
            announce_poll_result(
                http,
                channel_id,
                &format!(
                    "The poll for **{}** could not be matched to its contestants. An operator must resolve this match manually with `/tournament resolve`.",
                    match_.label()
                ),
            )
            .await;
        }
        PollApplyOutcome::AlreadyHandled => {}
    }
    Ok(())
}

/// Checks every active poll whose expiry has passed (covers downtime and missed gateway events).
async fn check_due_polls(http: &serenity::Http, data: &Data) {
    let due = match tournaments::list_due_active_polls(&data.db, &crate::db::now_sql()) {
        Ok(due) => due,
        Err(err) => {
            tracing::error!("listing due polls failed: {err:#}");
            return;
        }
    };
    for poll in due {
        if let Err(err) = finalize_tracked_poll(http, data, &poll).await {
            tracing::warn!("finalizing poll {} failed (will retry): {err:#}", poll.message_id);
        }
    }
}

static POLL_WATCHER_STARTED: AtomicBool = AtomicBool::new(false);

/// Starts the once-a-minute poll check. Ready fires again after reconnects; this runs once.
fn start_poll_watcher(ctx: &serenity::Context, data: &Data) {
    if POLL_WATCHER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let http = ctx.http.clone();
    let data = data.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            check_due_polls(&http, &data).await;
        }
    });
}

/// Pays the poll vote reward once per (poll, user) for tracked, still-active tournament polls.
fn reward_poll_vote(ctx: &serenity::Context, data: &Data, event: &serenity::MessagePollVoteAddEvent) -> Result<(), Error> {
    let cfg = config::get();
    if !cfg.is_currency_guild(event.guild_id) || cfg.currency.poll_vote_reward <= 0 {
        return Ok(());
    }
    if event.user_id == ctx.cache.current_user().id {
        return Ok(());
    }
    let message_id = event.message_id.to_string();
    let tracked = tournaments::get_match_poll_by_message(&data.db, &message_id)?;
    if tracked.is_none_or(|p| p.status != POLL_ACTIVE) {
        return Ok(());
    }
    let guild = event.guild_id.map(|g| g.to_string()).unwrap_or_default();
    wallets::reward_poll_vote(&data.db, &message_id, &event.user_id.to_string(), &guild)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Component and modal interactions
// ---------------------------------------------------------------------------------------------

/// Component interactions, poll votes, poll finalization.
pub async fn on_event(ctx: &serenity::Context, event: &FullEvent, data: &Data) -> Result<(), Error> {
    match event {
        FullEvent::Ready { .. } => start_poll_watcher(ctx, data),
        FullEvent::MessageUpdate { event, .. } => {
            if let Some(poll) = tournaments::get_match_poll_by_message(&data.db, &event.id.to_string())? {
                if poll.status == POLL_ACTIVE {
                    finalize_tracked_poll(&ctx.http, data, &poll).await?;
                }
            }
        }
        FullEvent::MessagePollVoteAdd { event } => reward_poll_vote(ctx, data, event)?,
        FullEvent::InteractionCreate { interaction: Interaction::Component(component) } => {
            handle_component(ctx, data, component).await?;
        }
        FullEvent::InteractionCreate { interaction: Interaction::Modal(modal) } => {
            handle_modal(ctx, data, modal).await?;
        }
        _ => {}
    }
    Ok(())
}

async fn handle_component(ctx: &serenity::Context, data: &Data, c: &ComponentInteraction) -> Result<(), Error> {
    let id = c.data.custom_id.as_str();
    let is_ours = id.starts_with("tournament_bet_market:")
        || id == "tournament_bet_select_match"
        || id.starts_with("tournament_special_select:")
        || id.starts_with("tournament_bet_pick_");
    if !is_ours {
        return Ok(());
    }

    let Some(guild) = c.guild_id.filter(|g| config::get().is_currency_guild(Some(*g))) else {
        let text = if c.guild_id.is_none() {
            "This command can only be used in a server."
        } else {
            "Tournaments are not available in this server."
        };
        c.create_response(ctx, Panel::text(text).ephemeral_response()).await?;
        return Ok(());
    };
    let guild = guild.to_string();

    if id.starts_with("tournament_bet_market:") {
        let panel = bet_market_panel(data, c, &guild)?;
        c.create_response(ctx, panel.update_response()).await?;
    } else if id == "tournament_bet_select_match" {
        let panel = select_match_panel(data, c, &guild)?;
        c.create_response(ctx, panel.update_response()).await?;
    } else if id.starts_with("tournament_special_select:") {
        match special_select_outcome(data, c, &guild)? {
            Err(panel) => c.create_response(ctx, panel.update_response()).await?,
            Ok(modal) => c.create_response(ctx, CreateInteractionResponse::Modal(modal)).await?,
        }
    } else {
        match contestant_pick_outcome(data, c, &guild)? {
            Err(panel) => c.create_response(ctx, panel.ephemeral_response()).await?,
            Ok(modal) => c.create_response(ctx, CreateInteractionResponse::Modal(modal)).await?,
        }
    }
    Ok(())
}

/// `tournament_bet_market:home | matches | special:<market>:<page>`
fn bet_market_panel(data: &Data, c: &ComponentInteraction, guild: &str) -> anyhow::Result<Panel> {
    let user = c.user.id.to_string();
    let parts: Vec<&str> = c.data.custom_id.split(':').collect();

    match parts.get(1).copied() {
        Some("home") => build_bet_home(&data.db, guild, &user),
        Some("matches") => {
            let Some(active) = tournaments::get_active_tournament(&data.db, guild)? else {
                return Ok(Panel::text("Tournament is no longer active."));
            };
            let overview = tournaments::get_tournament_overview(&data.db, guild, &user)?;
            let open: Vec<_> = overview.matches.iter().filter(|m| m.betting_open && m.winner.is_none()).collect();
            if open.is_empty() {
                return Ok(Panel::text("There are currently no open matches to bet on."));
            }

            let options: Vec<CreateSelectMenuOption> = open
                .iter()
                .take(SELECT_OPTION_LIMIT)
                .map(|m| {
                    let label = format_match_label(m.round_number, m.match_number, &m.contestant_a, &m.contestant_b);
                    CreateSelectMenuOption::new(label.chars().take(100).collect::<String>(), m.id.to_string())
                })
                .collect();
            let select = CreateSelectMenu::new("tournament_bet_select_match", CreateSelectMenuKind::String { options })
                .placeholder("Select a match to bet on");
            let embed = CreateEmbed::new()
                .colour(BLUE)
                .title(format!("Betting Panel: {}", active.name))
                .description(format!(
                    "Your balance: **{} MugCoins**\n\nSelect a match to place a bet.",
                    overview.wallet_balance
                ));
            Ok(Panel::embed(
                embed,
                vec![
                    CreateActionRow::SelectMenu(select),
                    CreateActionRow::Buttons(vec![button("tournament_bet_market:home", "Back", ButtonStyle::Secondary)]),
                ],
            ))
        }
        Some("special") => {
            let market_id = parts.get(2).and_then(|p| p.parse::<i64>().ok()).unwrap_or(0);
            let page = parts.get(3).and_then(|p| p.parse::<usize>().ok()).unwrap_or(0);
            let overview = tournaments::get_tournament_overview(&data.db, guild, &user)?;
            let Some(market) = overview.special_markets.iter().find(|m| m.id == market_id) else {
                return Ok(Panel::text("That betting market no longer exists."));
            };
            if market.status != "open" {
                return Ok(Panel::text("Betting is closed for this market."));
            }
            if let Some(user_bet) = &market.user_bet {
                let embed = CreateEmbed::new()
                    .colour(0xe74c3c)
                    .title(format!("Betting Panel: {}", market.title))
                    .description(format!("You have already placed a bet on **{}** and cannot change it.", user_bet.picked));
                return Ok(Panel::embed(embed, Vec::new()));
            }
            if market.options.is_empty() {
                return Ok(Panel::text("No valid picks are available for this market yet."));
            }
            Ok(build_special_market_page(market, page))
        }
        _ => Ok(Panel::text("That betting action is no longer available.")),
    }
}

/// `tournament_bet_select_match`: the user picked a match; show its two contestants.
fn select_match_panel(data: &Data, c: &ComponentInteraction, guild: &str) -> anyhow::Result<Panel> {
    let user = c.user.id.to_string();
    let match_id = first_select_value(c).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let Some(m) = tournaments::get_match(&data.db, match_id)? else {
        return Ok(Panel::text("Match not found."));
    };
    if !m.betting_open {
        return Ok(Panel::text("Betting is closed for this match."));
    }
    let active = tournaments::get_active_tournament(&data.db, guild)?.filter(|a| a.id == m.tournament_id);
    let Some(active) = active else {
        return Ok(Panel::text("Tournament is no longer active."));
    };

    let overview = tournaments::get_tournament_overview(&data.db, guild, &user)?;
    let already_bet = overview.matches.iter().any(|x| x.id == m.id && x.user_bet.is_some());
    if already_bet {
        let embed = CreateEmbed::new()
            .colour(0xe74c3c)
            .title(format!("Betting Panel: {}", active.name))
            .description(format!("You have already placed a bet on {}. You cannot change it.", m.label()));
        return Ok(Panel::embed(embed, Vec::new()));
    }

    let embed = CreateEmbed::new().colour(BLUE).title(format!("Betting Panel: {}", active.name)).description(format!(
        "**{}**\n\nYour balance: **{}** MugCoins\n\nOdds and pool sizes stay hidden until the match resolves.",
        m.label(),
        overview.wallet_balance
    ));
    let row = CreateActionRow::Buttons(vec![
        button(format!("tournament_bet_pick_a:{}", m.id), &m.contestant_a, ButtonStyle::Primary),
        button(format!("tournament_bet_pick_b:{}", m.id), &m.contestant_b, ButtonStyle::Danger),
    ]);
    Ok(Panel::embed(embed, vec![row]))
}

fn first_select_value(c: &ComponentInteraction) -> Option<&str> {
    match &c.data.kind {
        ComponentInteractionDataKind::StringSelect { values } => values.first().map(String::as_str),
        _ => None,
    }
}

/// `tournament_special_select:<market>:<page>`: open the amount modal, or explain why not.
fn special_select_outcome(
    data: &Data,
    c: &ComponentInteraction,
    guild: &str,
) -> anyhow::Result<Result<CreateModal, Panel>> {
    let user = c.user.id.to_string();
    let market_id = c.data.custom_id.split(':').nth(1).and_then(|p| p.parse::<i64>().ok()).unwrap_or(0);
    let option_index = first_select_value(c).and_then(|v| v.parse::<usize>().ok());

    let overview = tournaments::get_tournament_overview(&data.db, guild, &user)?;
    let market = overview.special_markets.iter().find(|m| m.id == market_id);
    let Some(market) = market.filter(|m| m.status == "open") else {
        return Ok(Err(Panel::text("Betting is closed for this market.")));
    };
    if market.user_bet.is_some() {
        return Ok(Err(Panel::text("You have already placed a bet on this market.")));
    }
    let Some((index, picked)) = option_index.and_then(|i| market.options.get(i).map(|p| (i, p))) else {
        return Ok(Err(Panel::text("That selection is no longer available.")));
    };
    let modal = amount_modal(format!("tournament_special_modal:{market_id}:{index}"), picked, overview.wallet_balance);
    Ok(Ok(modal))
}

/// `tournament_bet_pick_a|b:<match>`: open the amount modal, or explain why not.
fn contestant_pick_outcome(
    data: &Data,
    c: &ComponentInteraction,
    guild: &str,
) -> anyhow::Result<Result<CreateModal, Panel>> {
    let mut parts = c.data.custom_id.split(':');
    let action = parts.next().unwrap_or_default();
    let match_id = parts.next().and_then(|p| p.parse::<i64>().ok()).unwrap_or(0);

    let Some(m) = tournaments::get_match(&data.db, match_id)?.filter(|m| m.betting_open) else {
        return Ok(Err(Panel::text("Match not found or betting is closed.")));
    };
    let is_a = action == "tournament_bet_pick_a";
    let picked = if is_a { &m.contestant_a } else { &m.contestant_b };
    let balance = wallets::get_balance(&data.db, &c.user.id.to_string(), guild)?;
    let modal = amount_modal(format!("tournament_bet_modal:{match_id}:{}", if is_a { 'a' } else { 'b' }), picked, balance);
    Ok(Ok(modal))
}

async fn handle_modal(ctx: &serenity::Context, data: &Data, m: &ModalInteraction) -> Result<(), Error> {
    let id = m.data.custom_id.as_str();
    let is_match = id.starts_with("tournament_bet_modal:");
    if !is_match && !id.starts_with("tournament_special_modal:") {
        return Ok(());
    }

    let Some(guild) = m.guild_id.filter(|g| config::get().is_currency_guild(Some(*g))) else {
        let text = if m.guild_id.is_none() {
            "This command can only be used in a server."
        } else {
            "Tournaments are not available in this server."
        };
        m.create_response(ctx, Panel::text(text).ephemeral_response()).await?;
        return Ok(());
    };

    let guild = guild.to_string();
    let reply = if is_match { match_bet_submit(data, m, &guild)? } else { special_bet_submit(data, m, &guild)? };
    m.create_response(ctx, reply.ephemeral_response()).await?;
    Ok(())
}

/// The amount typed into the modal's text input.
fn modal_amount_text(m: &ModalInteraction) -> Option<&str> {
    m.data.components.iter().flat_map(|row| row.components.iter()).find_map(|component| match component {
        ActionRowComponent::InputText(input) if input.custom_id == "amountInput" => input.value.as_deref(),
        _ => None,
    })
}

fn parse_bet_amount(text: Option<&str>) -> Option<i64> {
    text.and_then(parse_int_prefix).filter(|n| *n > 0)
}

fn receipt(title: String, description: String) -> Panel {
    Panel::embed(CreateEmbed::new().colour(0x2ecc71).title(title).description(description), Vec::new())
}

/// `tournament_bet_modal:<match>:<a|b>`
fn match_bet_submit(data: &Data, m: &ModalInteraction, guild: &str) -> anyhow::Result<Panel> {
    let user = m.user.id.to_string();
    let mut parts = m.data.custom_id.split(':').skip(1);
    let match_id = parts.next().and_then(|p| p.parse::<i64>().ok()).unwrap_or(0);
    let letter = parts.next().unwrap_or("a");

    let Some(found) = tournaments::get_match(&data.db, match_id)?.filter(|x| x.betting_open) else {
        return Ok(Panel::text("Match not found or betting is closed."));
    };
    let Some(amount) = parse_bet_amount(modal_amount_text(m)) else {
        return Ok(Panel::text("Please enter a valid positive number."));
    };
    let Some(active) = tournaments::get_active_tournament(&data.db, guild)? else {
        return Ok(Panel::text("No active tournament."));
    };

    let picked = if letter == "a" { found.contestant_a.clone() } else { found.contestant_b.clone() };
    if let Err(err) = tournaments::place_tournament_bet(&data.db, guild, &user, match_id, &picked, amount) {
        return Ok(Panel::text(err.to_string()));
    }

    let balance = wallets::get_balance(&data.db, &user, guild)?;
    Ok(receipt(
        format!("Betting Receipt: {}", active.name),
        format!(
            "**{}**\nYour pick: **{picked}**\nAmount: **{amount}** MugCoins\nRemaining balance: **{balance}** MugCoins",
            found.label()
        ),
    ))
}

/// `tournament_special_modal:<market>:<option index>`
fn special_bet_submit(data: &Data, m: &ModalInteraction, guild: &str) -> anyhow::Result<Panel> {
    let user = m.user.id.to_string();
    let mut parts = m.data.custom_id.split(':').skip(1);
    let market_id = parts.next().and_then(|p| p.parse::<i64>().ok()).unwrap_or(0);
    let option_index = parts.next().and_then(|p| p.parse::<usize>().ok());

    let overview = tournaments::get_tournament_overview(&data.db, guild, &user)?;
    let market = overview.special_markets.iter().find(|x| x.id == market_id);
    let Some(market) = market.filter(|x| x.status == "open") else {
        return Ok(Panel::text("Betting is closed for this market."));
    };
    let Some(picked) = option_index.and_then(|i| market.options.get(i)) else {
        return Ok(Panel::text("That selection is no longer available."));
    };
    let Some(amount) = parse_bet_amount(modal_amount_text(m)) else {
        return Ok(Panel::text("Please enter a valid positive number."));
    };

    if let Err(err) = tournaments::place_tournament_special_bet(&data.db, guild, &user, market_id, picked, amount) {
        return Ok(Panel::text(err.to_string()));
    }

    let balance = wallets::get_balance(&data.db, &user, guild)?;
    Ok(receipt(
        format!("Betting Receipt: {}", market.title),
        format!("Your pick: **{picked}**\nAmount: **{amount}** MugCoins\nRemaining balance: **{balance}** MugCoins"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_int_matches_javascript() {
        assert_eq!(parse_int_prefix("100"), Some(100));
        assert_eq!(parse_int_prefix("  42abc"), Some(42));
        assert_eq!(parse_int_prefix("+7"), Some(7));
        assert_eq!(parse_int_prefix("-5"), Some(-5));
        assert_eq!(parse_int_prefix("12.9"), Some(12));
        assert_eq!(parse_int_prefix("abc"), None);
        assert_eq!(parse_int_prefix(""), None);
        assert_eq!(parse_bet_amount(Some("0")), None);
        assert_eq!(parse_bet_amount(Some("-3")), None);
        assert_eq!(parse_bet_amount(Some("25")), Some(25));
        assert_eq!(parse_bet_amount(None), None);
    }

    #[test]
    fn labels_and_statuses() {
        assert_eq!(format_match_label(2, 3, "A", "B"), "Round 2, Match 3: A vs B");
        assert_eq!(format_bet_status("won", 40), "Won (+40)");
        assert_eq!(format_bet_status("pending", 0), "Pending");
        assert_eq!(format_bet_status("lost", 0), "Lost");
        assert_eq!(format_bet_status("refunded", 0), "Refunded");
        assert_eq!(clamp_modal_title(&"x".repeat(60)).chars().count(), 45);
        assert_eq!(clamp_modal_title("Bet on A"), "Bet on A");
    }

    #[test]
    fn message_links() {
        assert_eq!(
            parse_message_link("https://discord.com/channels/1/22/333"),
            Some((1, 22, 333))
        );
        assert_eq!(
            parse_message_link("  <https://ptb.discord.com/channels/1/22/333?x=1>  "),
            Some((1, 22, 333))
        );
        assert_eq!(parse_message_link("https://discordapp.com/channels/1/22/333/"), Some((1, 22, 333)));
        assert_eq!(parse_message_link("https://discord.com/channels/@me/22/333"), None);
        assert_eq!(parse_message_link("https://discord.com/channels/1/22"), None);
        assert_eq!(parse_message_link("https://example.com/channels/1/22/333"), None);
        assert_eq!(parse_message_link("https://discord.com/channels/0/22/333"), None);
    }

    #[test]
    fn chunking_keeps_blocks_whole() {
        let blocks: Vec<String> = (0..5).map(|i| format!("{}{}", i, "x".repeat(9))).collect();
        let chunks = chunk_blocks(&blocks, 25);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.chars().count() <= 25));
        assert_eq!(chunks.concat(), blocks.concat());
        assert!(chunk_blocks(&[], 10).is_empty());
        // an oversized block is truncated rather than dropped
        let big = chunk_blocks(&["y".repeat(100)], 30);
        assert_eq!(big.len(), 1);
        assert_eq!(big[0].chars().count(), 30);
    }
}
