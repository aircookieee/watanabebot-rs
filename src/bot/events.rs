//! Core gateway events: startup, chat rewards, legacy `!` commands, You-chan reactions,
//! and translation of the music/art webhook.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fancy_regex::Regex;
use once_cell::sync::Lazy;
use poise::serenity_prelude as serenity;
use serenity::{FullEvent, Message, Permissions, ReactionType};

use super::commands::{anilist, misc, yousoro};
use super::{Data, Error};
use crate::config;
use crate::db::ReactMode;
use crate::services::gemini;

static WATASHI: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?<![a-zA-Z])You(?!\s*\(not [Ww]atanabe\)|[a-zA-Z])").unwrap());
static SMOL_WATASHI: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?<![a-zA-Z])you-chan(?!\s*\(not [Ww]atanabe\)|[a-zA-Z])").unwrap());
static YES_WATANABE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\(yes [Ww]atanabe\)").unwrap());

/// Ready fires again after every gateway reconnect; background jobs must start only once.
static STARTED: AtomicBool = AtomicBool::new(false);

pub async fn on_event(ctx: &serenity::Context, event: &FullEvent, data: &Data) -> Result<(), Error> {
    match event {
        FullEvent::Ready { data_about_bot } => {
            for guild in &data_about_bot.guilds {
                data.db.ensure_guild_config(guild.id.get())?;
            }
            if !STARTED.swap(true, Ordering::SeqCst) {
                tracing::info!("You-chan started at {}", chrono::Local::now());
                crate::scheduler::start(ctx.clone(), data.clone());
            }
        }
        FullEvent::Message { new_message } => on_message(ctx, data, new_message).await?,
        _ => {}
    }
    Ok(())
}

async fn on_message(ctx: &serenity::Context, data: &Data, msg: &Message) -> Result<(), Error> {
    // Bots are ignored, but webhooks (PluralKit, the music/art feed) are not.
    if msg.author.bot && msg.webhook_id.is_none() {
        return Ok(());
    }
    // Our own webhook messages (link-fix reposts) repeat a user's text that was already
    // handled when the original arrived; processing them again would double-answer commands.
    if msg.webhook_id.is_some() && is_own_application(ctx, msg) {
        return Ok(());
    }

    reward_chat_message(data, msg);

    if handle_legacy_yousoro(ctx, data, msg).await? {
        return Ok(());
    }
    if misc::handle_legacy_message(ctx, data, msg).await? {
        return Ok(());
    }
    if anilist::handle_legacy_message(ctx, data, msg).await? {
        return Ok(());
    }

    let bot_id = ctx.cache.current_user().id;
    if msg.author.id == bot_id {
        return Ok(());
    }

    react_to_summons(ctx, data, msg, bot_id).await?;

    let cfg = config::get();
    if msg.webhook_id.is_some() && msg.webhook_id.map(|w| w.get()) == cfg.channels.musicart_webhook_id {
        translate_webhook_message(ctx, data, msg).await;
    }
    Ok(())
}

/// Grants the per-message MugCoin reward, rate limited per user.
fn reward_chat_message(data: &Data, msg: &Message) {
    let cfg = config::get();
    if msg.author.bot || !cfg.is_currency_guild(msg.guild_id) || cfg.currency.message_reward <= 0 {
        return;
    }
    let guild_id = msg.guild_id.expect("currency guild").get();
    let key = (guild_id, msg.author.id.get());
    let cooldown = Duration::from_secs(cfg.currency.message_reward_cooldown_secs);
    {
        let mut last = data.message_rewards.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if last.get(&key).is_some_and(|t| now.duration_since(*t) < cooldown) {
            return;
        }
        last.insert(key, now);
    }
    if let Err(err) = data.db.add_currency(key.1, guild_id, cfg.currency.message_reward, "message", None) {
        tracing::error!("message reward failed: {err:#}");
    }
}

/// `!yousoro`, `!yousoro here`, `!yousoro everywhere`. Returns true if handled.
async fn handle_legacy_yousoro(ctx: &serenity::Context, data: &Data, msg: &Message) -> Result<bool, Error> {
    let content = msg.content.as_str();
    let arg = match content.strip_prefix("!yousoro") {
        Some("") => None,
        Some(rest) if rest.starts_with(' ') => Some(rest.trim()),
        _ => return Ok(false),
    };
    let channel = msg.channel_id;

    let Some(guild_id) = msg.guild_id else {
        let reply = yousoro::message_with_image("Yousor- hey, wait a sec, this isn't a Discord server...", "dms.png").await?;
        channel.send_message(ctx, reply).await?;
        return Ok(true);
    };

    let db = &data.db;
    let reply = match arg {
        None => yousoro::message_with_image(yousoro::HELP_TEXT, "ohayousoro.png").await?,
        Some("here") | Some("everywhere") if !can_manage_guild(ctx, msg) => {
            yousoro::message_with_image("", "nosoro.gif").await?
        }
        Some("here") => {
            db.set_guild_config(guild_id.get(), Some(channel.get()), ReactMode::Here)?;
            yousoro::message_with_image("Yousoro~!", "yousoroHere.png").await?
        }
        Some("everywhere") => {
            db.set_guild_config(guild_id.get(), None, ReactMode::Everywhere)?;
            yousoro::message_with_image("Zensokuzenshin... Yousoro~!", "yousoroEverywhere.jpg").await?
        }
        Some(a) if a.starts_with("twitter") => {
            serenity::CreateMessage::new().content("Link replacement moved to `/linkfix`.")
        }
        Some(_) => yousoro::message_with_image("", "nosoro.gif").await?,
    };
    channel.send_message(ctx, reply).await?;
    Ok(true)
}

/// True if the message was sent by a webhook this bot's application owns.
fn is_own_application(ctx: &serenity::Context, msg: &Message) -> bool {
    let Some(app_id) = msg.application_id else { return false };
    let own = ctx.http.application_id().map(|id| id.get()).unwrap_or_else(|| ctx.cache.current_user().id.get());
    app_id.get() == own
}

fn can_manage_guild(ctx: &serenity::Context, msg: &Message) -> bool {
    #[allow(deprecated)]
    msg.author_permissions(&ctx.cache)
        .is_some_and(|p| p.contains(Permissions::MANAGE_GUILD) || p.contains(Permissions::ADMINISTRATOR))
}

/// Reacts to "You", "you-chan" and "(yes Watanabe)" (respecting the guild's channel mode),
/// and to direct @mentions of the bot. @everyone, @here and role pings never trigger a
/// reaction: only an explicit `<@bot>` in the message text counts as a direct ping.
async fn react_to_summons(
    ctx: &serenity::Context,
    data: &Data,
    msg: &Message,
    bot_id: serenity::UserId,
) -> Result<(), Error> {
    let cfg = config::get();
    let content = msg.content.as_str();

    let should_respond = match msg.guild_id {
        None => true,
        Some(guild_id) => match data.db.get_guild_config(guild_id.get())? {
            None => true,
            Some(gc) => gc.mode == ReactMode::Everywhere || gc.channel_id == Some(msg.channel_id.get()),
        },
    };

    let mut reacted_watashi = false;
    if should_respond {
        if WATASHI.is_match(content).unwrap_or(false) {
            let emoji = if YES_WATANABE.is_match(content).unwrap_or(false) {
                cfg.reactions.yes_watanabe
            } else {
                reacted_watashi = true;
                cfg.reactions.watashi
            };
            react(ctx, msg, emoji).await;
        } else if SMOL_WATASHI.is_match(content).unwrap_or(false) {
            react(ctx, msg, cfg.reactions.smol_watashi).await;
        }
    }

    if !reacted_watashi && is_direct_ping(content, bot_id) {
        react(ctx, msg, cfg.reactions.watashi).await;
    }
    Ok(())
}

/// True only for an explicit user mention of the bot in the text (`<@id>` / `<@!id>`).
/// Discord's `mentions` list also contains reply targets, and `mention_everyone` /
/// `mention_roles` cover @everyone/@here/roles; none of those count.
fn is_direct_ping(content: &str, bot_id: serenity::UserId) -> bool {
    let id = bot_id.get();
    content.contains(&format!("<@{id}>")) || content.contains(&format!("<@!{id}>"))
}

async fn react(ctx: &serenity::Context, msg: &Message, emoji: serenity::EmojiId) {
    let reaction = ReactionType::Custom { animated: false, id: emoji, name: None };
    if let Err(err) = msg.react(ctx, reaction).await {
        tracing::warn!("failed to react in {}: {err}", msg.channel_id);
    }
}

async fn translate_webhook_message(ctx: &serenity::Context, data: &Data, msg: &Message) {
    let Some(text) = msg.embeds.first().and_then(|e| e.description.as_deref()) else {
        return;
    };
    if text.trim().is_empty() {
        return;
    }
    match gemini::translate_tweet(&data.client, text).await {
        Ok(Some((translation, backup_used))) => {
            let content = if backup_used {
                format!("`Backup model used`\n{translation}")
            } else {
                translation
            };
            if let Err(err) = crate::bot::say_safe(ctx, msg.channel_id, content).await {
                tracing::error!("failed to send translation: {err}");
            }
        }
        Ok(None) => {}
        Err(err) => tracing::error!("translation failed: {err:#}"),
    }
}
