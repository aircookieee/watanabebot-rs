//! Discord gateway client: shared state, framework setup, event fan-out.

pub mod commands;
pub mod events;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use poise::serenity_prelude as serenity;
use serenity::{FullEvent, GatewayIntents, Http};

use crate::config;
use crate::db::Db;
use crate::services;

pub type Error = anyhow::Error;
/// poise's user data type. `Arc` so the web server and background jobs can share it.
pub type Data = Arc<AppState>;
pub type Context<'a> = poise::Context<'a, Data, Error>;

/// Everything the bot, web server and scheduled jobs share.
pub struct AppState {
    pub db: Db,
    /// Discord REST client (usable before/without the gateway, e.g. from the web server).
    pub http: Arc<Http>,
    /// Outbound HTTP client for third-party APIs.
    pub client: reqwest::Client,
    pub anilist: services::anilist::Anilist,
    pub spotify: services::spotify::Spotify,
    pub linkfix: services::linkfix::LinkFixer,
    /// (guild_id, user_id) -> last time a chat message earned MugCoins.
    pub message_rewards: Mutex<HashMap<(u64, u64), Instant>>,
}

impl AppState {
    pub fn new(db: Db, http: Arc<Http>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("watanabebot/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        AppState {
            anilist: services::anilist::Anilist::new(db.clone(), client.clone()),
            spotify: services::spotify::Spotify::new(client.clone()),
            linkfix: services::linkfix::LinkFixer::new(db.clone()),
            db,
            http,
            client,
            message_rewards: Mutex::new(HashMap::new()),
        }
    }
}

/// Mentions the bot may trigger: user pings only, never @everyone/@here or roles, so user text
/// echoed back (pick options, search terms, translations, contestant names) cannot mass-ping.
pub fn safe_mentions() -> serenity::CreateAllowedMentions {
    serenity::CreateAllowedMentions::new().all_users(true)
}

/// `ChannelId::say` with [`safe_mentions`]. Use this for every plain channel message.
#[allow(clippy::result_large_err)] // serenity's own error type; boxing it here buys nothing.
pub async fn say_safe(
    cache_http: impl serenity::CacheHttp,
    channel: serenity::ChannelId,
    content: impl Into<String>,
) -> serenity::Result<serenity::Message> {
    channel
        .send_message(cache_http, serenity::CreateMessage::new().content(content).allowed_mentions(safe_mentions()))
        .await
}

pub async fn run(state: Data) -> anyhow::Result<()> {
    let cfg = config::get();

    let intents = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::GUILD_MESSAGE_POLLS
        | GatewayIntents::GUILD_WEBHOOKS;

    let setup_state = state.clone();
    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: commands::all(),
            event_handler: |ctx, event, _framework, data| Box::pin(on_event(ctx, event, data)),
            on_error: |err| Box::pin(on_error(err)),
            // Legacy `!` commands are parsed by hand in `events::on_message`.
            prefix_options: poise::PrefixFrameworkOptions {
                prefix: None,
                // Otherwise poise treats every @You-chan ping as a prefix command attempt.
                mention_as_prefix: false,
                ..Default::default()
            },
            allowed_mentions: Some(safe_mentions()),
            ..Default::default()
        })
        .setup(move |ctx, ready, framework| {
            Box::pin(async move {
                tracing::info!("Logged in as {} ({})", ready.user.name, ready.user.id);
                let commands = &framework.options().commands;
                // Economy commands stay in one guild; everything else is global so every server gets it.
                let (guild_only, global): (Vec<_>, Vec<_>) =
                    commands.iter().partition(|c| matches!(c.name.as_str(), "mugcoin" | "tournament"));
                let build = |cmds: Vec<&poise::Command<_, _>>| -> Vec<serenity::CreateCommand> {
                    cmds.into_iter()
                        .flat_map(|c| poise::builtins::create_application_commands(std::slice::from_ref(c)))
                        .collect()
                };
                serenity::Command::set_global_commands(ctx, build(global)).await?;
                tracing::info!("Global commands deployed");
                let economy_guild = cfg.currency.guild_id.or(cfg.discord.guild_id);
                // Clear old guild-scoped copies of the global commands so they don't show twice.
                if let Some(old) = cfg.discord.guild_id.filter(|g| Some(*g) != economy_guild) {
                    old.set_commands(ctx, vec![]).await?;
                }
                match economy_guild {
                    Some(guild_id) => {
                        guild_id.set_commands(ctx, build(guild_only)).await?;
                        tracing::info!("Economy commands deployed to guild {guild_id}");
                    }
                    None => tracing::warn!("No CURRENCY_GUILD_ID/DISCORD_GUILD_ID: mugcoin/tournament not registered"),
                }
                Ok(setup_state)
            })
        })
        .build();

    let mut client = serenity::ClientBuilder::new(&cfg.discord.token, intents)
        .framework(framework)
        .await?;
    client.start().await?;
    Ok(())
}

/// Fans every gateway event out to the features that care. Each hook filters for itself.
/// A failing hook is logged and does not stop the others.
async fn on_event(ctx: &serenity::Context, event: &FullEvent, data: &Data) -> Result<(), Error> {
    if let Err(err) = events::on_event(ctx, event, data).await {
        tracing::error!("core event handler: {err:#}");
    }
    if let Err(err) = services::linkfix::on_event(ctx, event, data).await {
        tracing::error!("linkfix event handler: {err:#}");
    }
    if let Err(err) = commands::tournament::on_event(ctx, event, data).await {
        tracing::error!("tournament event handler: {err:#}");
    }
    if let Err(err) = commands::mugcoin::on_event(ctx, event, data).await {
        tracing::error!("mugcoin event handler: {err:#}");
    }
    if let Err(err) = commands::linkfix::on_event(ctx, event, data).await {
        tracing::error!("linkfix command event handler: {err:#}");
    }
    Ok(())
}

/// Command errors are shown to the invoking user as an ephemeral message; services
/// return user-readable `anyhow` messages (e.g. "No active tournament.").
async fn on_error(err: poise::FrameworkError<'_, Data, Error>) {
    match err {
        poise::FrameworkError::Command { error, ctx, .. } => {
            tracing::warn!("/{} failed: {error:#}", ctx.command().qualified_name);
            let _ = ctx
                .send(poise::CreateReply::default().content(error.to_string()).ephemeral(true))
                .await;
        }
        other => {
            if let Err(e) = poise::builtins::on_error(other).await {
                tracing::error!("error while handling error: {e}");
            }
        }
    }
}
