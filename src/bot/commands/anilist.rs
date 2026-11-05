//! /anime, /manga, /anilist and the legacy `!al` / `!alm` / ((anime)) / [[manga]] lookups.

use std::collections::HashMap;
use std::time::Instant;

use once_cell::sync::Lazy;
use poise::serenity_prelude as serenity;
use regex::Regex;
use serenity::{CreateEmbed, CreateEmbedFooter, CreateMessage, EditMessage};

use crate::bot::{Context, Data, Error};
use crate::services::anilist::{AnimeInfo, AnimeMatch, MediaType};

const UPDATE_IN_PROGRESS: &str = "Anilist data is currently being updated. Please wait a moment and try again.";
const LEGACY_USAGE: &str = "Usage:\n\
    \u{2022} `!al <anime name>` or `((anime name))`\n\
    \u{2022} `!alm <manga name>` or `[[manga name]]`\n\
    \u{2022} `!al register <Anilist username>`\n\
    \u{2022} `!al unregister`\n\
    \u{2022} `!al update`";

static ANIME_BRACKET: Lazy<Regex> = Lazy::new(|| Regex::new(r"\(\(+.+?\)\)").unwrap());
static MANGA_BRACKET: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[\[+.+?\]\]").unwrap());

pub fn commands() -> Vec<poise::Command<Data, Error>> {
    vec![anime(), manga(), anilist()]
}

// ---- embed ----

/// Discord caps embed field values at 1024 characters.
fn clamp_field(value: String) -> String {
    if value.chars().count() > 1024 {
        value.chars().take(1021).collect::<String>() + "..."
    } else {
        value
    }
}

/// Builds the lookup embed: scores, then every registered user grouped by list status.
pub fn create_anime_embed(info: AnimeInfo, start: Instant, media_type: MediaType) -> CreateEmbed {
    let is_anime = media_type == MediaType::Anime;
    let colour = if is_anime { 0x1f8b4c } else { 0xff6b9d };

    let AnimeInfo { resolved_title, description, anilist_url, score, cover_image, mut matches } = info;

    let description = if description.chars().count() > 600 {
        description.chars().take(597).collect::<String>() + "..."
    } else {
        description
    };

    let mut embed = CreateEmbed::new()
        .title(format!("{} \u{2022} {}", media_type.label(), resolved_title))
        .description(description)
        .colour(colour);
    if !anilist_url.is_empty() {
        embed = embed.url(anilist_url);
    }
    if !cover_image.is_empty() {
        embed = embed.thumbnail(cover_image);
    }

    let mut sum = 0i64;
    let mut zero_scores = 0usize;
    for user in &matches {
        if user.score != 0 {
            sum += user.score;
        } else {
            zero_scores += 1;
        }
    }
    let divisor = matches.len() - zero_scores;
    let mug_avg = sum as f64 / if divisor == 0 { 1 } else { divisor } as f64;

    embed = embed
        .field("Anilist Score", score.to_string(), true)
        .field("MugScore", ((mug_avg + 0.5).floor() as i64).to_string(), true);

    let mut status_map: HashMap<String, Vec<AnimeMatch>> = HashMap::new();
    for m in matches.drain(..) {
        let mut m = m;
        if m.status == "REPEATING" && m.repeat == 0 {
            m.repeat = 1;
        }
        if m.status == "REPEATING" {
            m.status = "CURRENT".to_string();
        }
        status_map.entry(m.status.clone()).or_default().push(m);
    }

    const DISPLAY_ORDER: [&str; 6] = ["CURRENT", "COMPLETED", "PAUSED", "DROPPED", "PLANNING", "NOT_ON_LIST"];
    let mut user_scores = String::new();
    for status in DISPLAY_ORDER {
        let Some(group) = status_map.get(status) else { continue };

        let users: Vec<String> = group
            .iter()
            .map(|m| {
                let mut s = m.ani_username.clone();
                if m.progress > 0 && m.status != "COMPLETED" {
                    s.push_str(&format!(" [{}]", m.progress));
                }
                if m.score > 0 {
                    s.push_str(&format!("  **{}**", m.score));
                }
                if m.repeat > 0 {
                    s.push_str(&format!(" (R{})", m.repeat));
                }
                if m.is_favorite {
                    s.push_str(" :heart:");
                }
                s
            })
            .collect();

        if !users.is_empty() {
            let label = if status == "NOT_ON_LIST" {
                "Not On List".to_string()
            } else {
                let mut chars = status.chars();
                let first = chars.next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
                first + &chars.as_str().to_lowercase()
            };
            user_scores.push_str(&format!("**{}**: {}\n", label, users.join(" | ")));
        }
    }

    if !user_scores.is_empty() {
        embed = embed.field("User Scores", clamp_field(user_scores), false);
    }

    embed.footer(CreateEmbedFooter::new(format!("Execution took {}ms", start.elapsed().as_millis())))
}

// ---- slash commands ----

async fn slash_lookup(ctx: Context<'_>, query: String, media_type: MediaType) -> Result<(), Error> {
    let data = ctx.data();
    if data.anilist.update_in_progress() {
        ctx.send(poise::CreateReply::default().content(UPDATE_IN_PROGRESS).ephemeral(true)).await?;
        return Ok(());
    }

    let start = Instant::now();
    ctx.defer().await?;

    let reply = match data.anilist.get_anime_info_with_scores(&query, media_type, None).await? {
        Some(info) => poise::CreateReply::default().embed(create_anime_embed(info, start, media_type)),
        None => poise::CreateReply::default().content(format!("No {} found for \"{}\"", media_type.lower(), query)),
    };
    ctx.send(reply).await?;
    Ok(())
}

/// Search for anime on Anilist
#[poise::command(slash_command)]
pub async fn anime(ctx: Context<'_>, #[description = "Anime name"] query: String) -> Result<(), Error> {
    slash_lookup(ctx, query, MediaType::Anime).await
}

/// Search for manga on Anilist
#[poise::command(slash_command)]
pub async fn manga(ctx: Context<'_>, #[description = "Manga name"] query: String) -> Result<(), Error> {
    slash_lookup(ctx, query, MediaType::Manga).await
}

/// Manage Anilist connection
#[poise::command(slash_command, subcommands("register", "unregister", "update"), subcommand_required)]
pub async fn anilist(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Register username
#[poise::command(slash_command)]
async fn register(ctx: Context<'_>, #[description = "Anilist username"] username: String) -> Result<(), Error> {
    ctx.defer().await?;
    let user_id = ctx.author().id;
    let content = if ctx.data().anilist.register_user(&user_id.to_string(), &username).await? {
        format!("Registered `{username}` to <@{user_id}>.")
    } else {
        format!("Anilist username `{username}` is already registered to another user.")
    };
    ctx.say(content).await?;
    Ok(())
}

/// Unregister
#[poise::command(slash_command)]
async fn unregister(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let content = if ctx.data().anilist.unregister_user(&user_id.to_string())? {
        format!("Unregistered <@{user_id}>.")
    } else {
        format!("Could not unregister <@{user_id}>.")
    };
    ctx.say(content).await?;
    Ok(())
}

/// Update Anilist data for all users
#[poise::command(slash_command)]
async fn update(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer().await?;
    let start = Instant::now();
    ctx.data().anilist.update_all_user_data().await?;
    ctx.say(format!("Anilist data updated, took {}s.", start.elapsed().as_secs_f64().round() as u64)).await?;
    Ok(())
}

// ---- legacy message commands ----

/// Handles `!al ...`, `!alm ...`, `((title))` and `[[title]]` in a message.
/// Returns `true` if the message was handled (the caller then stops processing it).
pub async fn handle_legacy_message(ctx: &serenity::Context, data: &Data, msg: &serenity::Message) -> Result<bool, Error> {
    let content = msg.content.as_str();
    let manga_match = MANGA_BRACKET.find(content);
    let anime_match = ANIME_BRACKET.find(content);

    if !(content.starts_with("!al") || manga_match.is_some() || anime_match.is_some()) {
        return Ok(false);
    }

    let channel = msg.channel_id;
    let user_id = msg.author.id;

    let mut media_type = MediaType::Anime;
    let mut search_query = content.to_string();
    if let Some(m) = manga_match {
        let name = m.as_str().replace(['[', ']'], "");
        search_query = format!("!alm {}", name.trim());
        media_type = MediaType::Manga;
    } else if let Some(m) = anime_match {
        let name = m.as_str().replace(['(', ')'], "");
        search_query = format!("!al {}", name.trim());
        media_type = MediaType::Anime;
    }
    if search_query.starts_with("!alm") {
        media_type = MediaType::Manga;
    }

    let args: Vec<&str> = search_query.trim().split(' ').skip(1).collect();
    let sub = args.first().map(|s| s.to_lowercase());
    let rest = args.iter().skip(1).copied().collect::<Vec<_>>().join(" ");
    let full_query = [sub.clone().unwrap_or_default(), rest]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    if sub.as_deref() == Some("register") {
        if let Some(username) = args.get(1).filter(|a| !a.is_empty()) {
            let content = if data.anilist.register_user(&user_id.to_string(), username).await? {
                format!("Registered `{username}` to <@{user_id}>.")
            } else {
                format!("Anilist username `{username}` is already registered to another user.")
            };
            crate::bot::say_safe(ctx, channel, content).await?;
            return Ok(true);
        }
    }

    if sub.as_deref() == Some("unregister") {
        let content = if data.anilist.unregister_user(&user_id.to_string())? {
            format!("Unregistered <@{user_id}>.")
        } else {
            format!("Could not unregister <@{user_id}>.")
        };
        crate::bot::say_safe(ctx, channel, content).await?;
        return Ok(true);
    }

    if sub.as_deref() == Some("update") {
        let mut reply = crate::bot::say_safe(ctx, channel, "Updating Anilist data...").await?;
        let start = Instant::now();
        data.anilist.update_all_user_data().await?;
        let text = format!("Anilist data updated, took {}s.", start.elapsed().as_secs_f64().round() as u64);
        reply.edit(ctx, EditMessage::new().content(text)).await?;
        return Ok(true);
    }

    if !full_query.is_empty() {
        let start = Instant::now();
        match data
            .anilist
            .get_anime_info_with_scores(&full_query, media_type, Some(&user_id.to_string()))
            .await?
        {
            Some(info) => {
                let embed = create_anime_embed(info, start, media_type);
                channel.send_message(ctx, CreateMessage::new().embed(embed)).await?;
            }
            None => {
                crate::bot::say_safe(ctx, channel, format!("No {} found for \"{}\"", media_type.lower(), full_query)).await?;
            }
        }
        return Ok(true);
    }

    crate::bot::say_safe(ctx, channel, LEGACY_USAGE).await?;
    Ok(true)
}
