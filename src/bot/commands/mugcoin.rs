//! /mugcoin: balance, leaderboard, pay, daily, stats, admin set/give/take.

use anyhow::bail;
use poise::serenity_prelude as serenity;
use serenity::{
    ButtonStyle, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedFooter, CreateInteractionResponse,
    CreateInteractionResponseMessage, FullEvent, Interaction,
};

use crate::bot::{Context, Data, Error};
use crate::config;
use crate::db::Db;
use crate::services::{authz, wallets};

const LEADERBOARD_PAGE_SIZE: i64 = 10;
const LEADERBOARD_PREFIX: &str = "mugcoin_leaderboard:";
const EMBED_COLOR: u32 = 0x00bfff;

/// Which ranking the leaderboard shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, poise::ChoiceParameter)]
pub enum LeaderboardSort {
    #[name = "balance"]
    Balance,
    #[name = "profit"]
    Profit,
}

impl LeaderboardSort {
    fn key(self) -> &'static str {
        match self {
            LeaderboardSort::Balance => "balance",
            LeaderboardSort::Profit => "profit",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        match key {
            "balance" => Some(LeaderboardSort::Balance),
            "profit" => Some(LeaderboardSort::Profit),
            _ => None,
        }
    }
}

/// Parses a leaderboard button id: `mugcoin_leaderboard:<sort>:<page>` or the old
/// `mugcoin_leaderboard:<page>` form (balance).
fn parse_leaderboard_id(custom_id: &str) -> Option<(LeaderboardSort, i64)> {
    let rest = custom_id.strip_prefix(LEADERBOARD_PREFIX)?;
    let parts: Vec<&str> = rest.split(':').collect();
    let (sort, page) = match parts.as_slice() {
        [page] => (LeaderboardSort::Balance, *page),
        [sort, page] => (LeaderboardSort::from_key(sort)?, *page),
        _ => return None,
    };
    Some((sort, page.parse::<i64>().unwrap_or(0).max(0)))
}

fn signed(value: i64) -> String {
    if value > 0 {
        format!("+{value}")
    } else {
        value.to_string()
    }
}

/// One leaderboard page: the embed and its Previous/Next buttons. An out-of-range page falls
/// back to the last page that has rows.
fn build_leaderboard_page(
    db: &Db,
    guild_id: &str,
    sort: LeaderboardSort,
    page: i64,
) -> anyhow::Result<(CreateEmbed, CreateActionRow)> {
    let mut page = page.max(0);
    loop {
        let offset = page * LEADERBOARD_PAGE_SIZE;
        // One extra row tells us whether a next page exists.
        let mut lines: Vec<String> = match sort {
            LeaderboardSort::Balance => wallets::get_wallet_leaderboard_page(db, guild_id, LEADERBOARD_PAGE_SIZE + 1, offset)?
                .into_iter()
                .map(|row| (row.user_id, format!("{} MugCoins", row.balance)))
                .enumerate()
                .map(|(i, (user, tail))| format!("**{}.** <@{user}> - {tail}", offset + i as i64 + 1))
                .collect(),
            LeaderboardSort::Profit => wallets::get_profit_leaderboard(db, guild_id, LEADERBOARD_PAGE_SIZE + 1, offset)?
                .into_iter()
                .enumerate()
                .map(|(i, row)| {
                    format!(
                        "**{}.** <@{}> - {} MugCoins ({}W-{}L)",
                        offset + i as i64 + 1,
                        row.user_id,
                        signed(row.net_profit),
                        row.won,
                        row.lost
                    )
                })
                .collect(),
        };

        if lines.is_empty() && page > 0 {
            page -= 1;
            continue;
        }

        let has_more = lines.len() as i64 > LEADERBOARD_PAGE_SIZE;
        lines.truncate(LEADERBOARD_PAGE_SIZE as usize);

        let (title, empty) = match sort {
            LeaderboardSort::Balance => ("MugCoin Leaderboard", "No users have MugCoins yet."),
            LeaderboardSort::Profit => ("MugCoin Profit Leaderboard", "No resolved bets yet."),
        };
        let description = if lines.is_empty() { empty.to_string() } else { lines.join("\n") };
        let embed = CreateEmbed::new()
            .colour(EMBED_COLOR)
            .title(title)
            .description(description)
            .footer(CreateEmbedFooter::new(format!("Page {}", page + 1)));

        let row = CreateActionRow::Buttons(vec![
            CreateButton::new(format!("{LEADERBOARD_PREFIX}{}:{}", sort.key(), page - 1))
                .label("Previous")
                .style(ButtonStyle::Secondary)
                .disabled(page == 0),
            CreateButton::new(format!("{LEADERBOARD_PREFIX}{}:{}", sort.key(), page + 1))
                .label("Next")
                .style(ButtonStyle::Secondary)
                .disabled(!has_more),
        ]);
        return Ok((embed, row));
    }
}

/// The guild id as a string, after checking the command is used in the MugCoin guild.
fn currency_guild(ctx: Context<'_>) -> Result<String, Error> {
    let Some(guild_id) = ctx.guild_id() else {
        bail!("This command can only be used in a server.");
    };
    if !config::get().is_currency_guild(Some(guild_id)) {
        bail!("MugCoins cannot be used in this server.");
    }
    Ok(guild_id.to_string())
}

async fn reply_ephemeral(ctx: Context<'_>, content: impl Into<String>) -> Result<(), Error> {
    ctx.send(poise::CreateReply::default().content(content).ephemeral(true)).await?;
    Ok(())
}

/// Manage your MugCoins
#[poise::command(
    slash_command,
    subcommands("balance", "leaderboard", "pay", "daily", "stats", "admin"),
    subcommand_required
)]
pub async fn mugcoin(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Check your MugCoin balance
#[poise::command(slash_command)]
async fn balance(
    ctx: Context<'_>,
    #[description = "User to check (optional)"] user: Option<serenity::User>,
) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    let user = user.unwrap_or_else(|| ctx.author().clone());
    let balance = wallets::get_balance(&ctx.data().db, &user.id.to_string(), &guild)?;

    let embed = CreateEmbed::new()
        .colour(EMBED_COLOR)
        .title(format!("{}'s Wallet", user.name))
        .description(format!("**Balance:** {balance} MugCoins"))
        .thumbnail(user.face());
    ctx.send(poise::CreateReply::default().embed(embed)).await?;
    Ok(())
}

/// View the richest MugCoin holders or the best bettors
#[poise::command(slash_command)]
async fn leaderboard(
    ctx: Context<'_>,
    #[description = "Ranking to show (default: balance)"] sort: Option<LeaderboardSort>,
) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    let (embed, row) = build_leaderboard_page(&ctx.data().db, &guild, sort.unwrap_or(LeaderboardSort::Balance), 0)?;
    ctx.send(poise::CreateReply::default().embed(embed).components(vec![row])).await?;
    Ok(())
}

/// Pay MugCoins to another user
#[poise::command(slash_command)]
async fn pay(
    ctx: Context<'_>,
    #[description = "User to pay"] user: serenity::User,
    #[description = "Amount to pay"] amount: i64,
) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    let sender = ctx.author();

    if user.id == sender.id {
        return reply_ephemeral(ctx, "You cannot send MugCoins to yourself.").await;
    }
    if amount <= 0 {
        return reply_ephemeral(ctx, "Amount must be greater than 0.").await;
    }
    if user.bot {
        return reply_ephemeral(ctx, "You cannot send MugCoins to bots.").await;
    }

    let paid = wallets::pay_user(&ctx.data().db, &sender.id.to_string(), &user.id.to_string(), &guild, amount)?;
    if !paid {
        return reply_ephemeral(ctx, "Insufficient funds.").await;
    }
    ctx.say(format!("**{}** has sent **{amount}** MugCoins to **{}**!", sender.name, user.name)).await?;
    Ok(())
}

/// Claim your daily MugCoins (once per UTC day, with streak bonuses)
#[poise::command(slash_command)]
async fn daily(ctx: Context<'_>) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    let claim = wallets::claim_daily(&ctx.data().db, &ctx.author().id.to_string(), &guild)?;
    let next = format!("<t:{}:R>", claim.next_claim_unix);

    if !claim.claimed {
        return reply_ephemeral(
            ctx,
            format!(
                "You already claimed your daily reward today. Your streak is **{}** day(s). You can claim again {next}.",
                claim.streak
            ),
        )
        .await;
    }

    ctx.say(format!(
        "You claimed your daily reward of **{}** MugCoins!\nStreak: **{}** day(s) (best: {})\nBalance: **{}** MugCoins\nNext claim available {next}.",
        claim.reward, claim.streak, claim.best_streak, claim.balance
    ))
    .await?;
    Ok(())
}

/// Show betting statistics
#[poise::command(slash_command)]
async fn stats(
    ctx: Context<'_>,
    #[description = "User to check (optional)"] user: Option<serenity::User>,
) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    let user = user.unwrap_or_else(|| ctx.author().clone());
    let stats = wallets::get_bettor_stats(&ctx.data().db, &user.id.to_string(), &guild)?;

    if stats.totals.placed == 0 {
        return reply_ephemeral(ctx, format!("{} has not placed any bets yet.", user.name)).await;
    }

    let t = &stats.totals;
    let win_rate = match stats.win_rate {
        Some(rate) => format!("{:.1}%", rate * 100.0),
        None => "N/A".to_string(),
    };
    let embed = CreateEmbed::new()
        .colour(EMBED_COLOR)
        .title(format!("{}'s Betting Stats", user.name))
        .thumbnail(user.face())
        .field(
            "Bets",
            format!(
                "Placed: **{}**\nWon: **{}** | Lost: **{}**\nPending: **{}** | Refunded: **{}**",
                t.placed, t.won, t.lost, t.pending, t.refunded
            ),
            true,
        )
        .field(
            "Results",
            format!(
                "Win rate: **{win_rate}**\nTotal wagered: **{}**\nTotal paid out: **{}**\nNet profit: **{}**\nBiggest payout: **{}**",
                t.total_wagered,
                t.total_paid_out,
                signed(stats.net_profit),
                t.biggest_payout
            ),
            true,
        )
        .field(
            "Breakdown",
            format!(
                "Match bets: **{}** placed, net **{}**\nSpecial bets: **{}** placed, net **{}**",
                stats.match_bets.placed,
                signed(stats.match_bets.net_profit()),
                stats.special_bets.placed,
                signed(stats.special_bets.net_profit())
            ),
            false,
        );
    ctx.send(poise::CreateReply::default().embed(embed)).await?;
    Ok(())
}

/// Admin commands
#[poise::command(slash_command, subcommands("admin_set", "admin_give", "admin_take"), subcommand_required)]
async fn admin(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Checks the admin permission with the TS wording.
async fn require_admin(ctx: Context<'_>) -> Result<bool, Error> {
    if authz::can_use_admin_tools(ctx.author().id.get()) {
        return Ok(true);
    }
    reply_ephemeral(ctx, "You do not have permission to use admin commands.").await?;
    Ok(false)
}

/// Set a user's balance
#[poise::command(slash_command, rename = "set")]
async fn admin_set(
    ctx: Context<'_>,
    #[description = "Target user"] user: serenity::User,
    #[description = "New balance"] amount: i64,
) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    if !require_admin(ctx).await? {
        return Ok(());
    }
    if amount < 0 {
        return reply_ephemeral(ctx, "Balance cannot be negative.").await;
    }
    let actor = ctx.author().id.to_string();
    wallets::admin_set_user_balance(&ctx.data().db, &actor, &user.id.to_string(), &guild, amount)?;
    ctx.say(format!("Set {}'s balance to **{amount}** MugCoins.", user.name)).await?;
    Ok(())
}

/// Give MugCoins to a user
#[poise::command(slash_command, rename = "give")]
async fn admin_give(
    ctx: Context<'_>,
    #[description = "Target user"] user: serenity::User,
    #[description = "Amount to give"] amount: i64,
) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    if !require_admin(ctx).await? {
        return Ok(());
    }
    if amount <= 0 {
        return reply_ephemeral(ctx, "Amount must be positive.").await;
    }
    let actor = ctx.author().id.to_string();
    wallets::admin_give_user_balance(&ctx.data().db, &actor, &user.id.to_string(), &guild, amount)?;
    ctx.say(format!("Gave **{amount}** MugCoins to {}.", user.name)).await?;
    Ok(())
}

/// Take MugCoins from a user
#[poise::command(slash_command, rename = "take")]
async fn admin_take(
    ctx: Context<'_>,
    #[description = "Target user"] user: serenity::User,
    #[description = "Amount to take"] amount: i64,
) -> Result<(), Error> {
    let guild = currency_guild(ctx)?;
    if !require_admin(ctx).await? {
        return Ok(());
    }
    if amount <= 0 {
        return reply_ephemeral(ctx, "Amount must be positive.").await;
    }
    let actor = ctx.author().id.to_string();
    if wallets::admin_take_user_balance(&ctx.data().db, &actor, &user.id.to_string(), &guild, amount)? {
        ctx.say(format!("Took **{amount}** MugCoins from {}.", user.name)).await?;
        Ok(())
    } else {
        reply_ephemeral(ctx, format!("{} does not have enough MugCoins.", user.name)).await
    }
}

/// Component interactions (leaderboard pagination).
pub async fn on_event(ctx: &serenity::Context, event: &FullEvent, data: &Data) -> Result<(), Error> {
    let FullEvent::InteractionCreate { interaction: Interaction::Component(component) } = event else {
        return Ok(());
    };
    if !component.data.custom_id.starts_with(LEADERBOARD_PREFIX) {
        return Ok(());
    }

    let Some((sort, page)) = parse_leaderboard_id(&component.data.custom_id) else {
        return Ok(());
    };

    let refusal = match component.guild_id {
        None => Some("This command can only be used in a server."),
        Some(guild) if !config::get().is_currency_guild(Some(guild)) => Some("MugCoins cannot be used in this server."),
        Some(_) => None,
    };
    if let Some(text) = refusal {
        component
            .create_response(
                ctx,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new().content(text).ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    }

    let guild_id = component.guild_id.map(|g| g.to_string()).unwrap_or_default();
    let (embed, row) = build_leaderboard_page(&data.db, &guild_id, sort, page)?;
    component
        .create_response(
            ctx,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new().embed(embed).components(vec![row]),
            ),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaderboard_ids_new_and_legacy() {
        assert_eq!(parse_leaderboard_id("mugcoin_leaderboard:profit:3"), Some((LeaderboardSort::Profit, 3)));
        assert_eq!(parse_leaderboard_id("mugcoin_leaderboard:balance:0"), Some((LeaderboardSort::Balance, 0)));
        // old form without a sort means balance
        assert_eq!(parse_leaderboard_id("mugcoin_leaderboard:2"), Some((LeaderboardSort::Balance, 2)));
        // a negative or garbage page clamps to the first page
        assert_eq!(parse_leaderboard_id("mugcoin_leaderboard:-1"), Some((LeaderboardSort::Balance, 0)));
        assert_eq!(parse_leaderboard_id("mugcoin_leaderboard:profit:x"), Some((LeaderboardSort::Profit, 0)));
        assert_eq!(parse_leaderboard_id("mugcoin_leaderboard:wins:1"), None);
        assert_eq!(parse_leaderboard_id("mugcoin_leaderboard:a:b:c"), None);
        assert_eq!(parse_leaderboard_id("other:1"), None);
    }

    #[test]
    fn signed_numbers() {
        assert_eq!(signed(5), "+5");
        assert_eq!(signed(0), "0");
        assert_eq!(signed(-7), "-7");
    }
}
