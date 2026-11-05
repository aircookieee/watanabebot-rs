//! /define, /pick and their legacy `!define` / `!pick` forms.

use poise::serenity_prelude as serenity;
use rand::seq::SliceRandom;
use serenity::{CreateEmbed, CreateMessage};

use crate::bot::{Context, Data, Error};
use crate::services::wordnik;

const PICK_TOO_FEW: &str = "Please provide at least two options, separated by commas.";

/// Definitions embed, or `None` if the word has none (or the lookup failed).
async fn lookup_definitions(data: &Data, word: &str) -> Option<CreateEmbed> {
    let definitions = wordnik::get_word_definitions(&data.client, word).await?;
    if definitions.is_empty() {
        return None;
    }
    wordnik::create_word_embed(&definitions)
}

/// Splits a comma-separated list, dropping blanks.
fn parse_options(raw: &str) -> Vec<&str> {
    raw.split(',').map(str::trim).filter(|s| !s.is_empty()).collect()
}

fn pick_one<'a>(options: &[&'a str]) -> &'a str {
    options.choose(&mut rand::thread_rng()).copied().unwrap_or_default()
}

/// Get word definitions
#[poise::command(slash_command)]
pub async fn define(ctx: Context<'_>, #[description = "Word to define"] query: String) -> Result<(), Error> {
    ctx.defer().await?;
    let reply = match lookup_definitions(ctx.data(), &query).await {
        Some(embed) => poise::CreateReply::default().embed(embed),
        None => poise::CreateReply::default().content(format!("No definitions found for \"{query}\"")),
    };
    ctx.send(reply).await?;
    Ok(())
}

/// Random picker
#[poise::command(slash_command)]
pub async fn pick(ctx: Context<'_>, #[description = "Comma-separated options"] options: String) -> Result<(), Error> {
    let options = parse_options(&options);
    if options.len() < 2 {
        ctx.say(PICK_TOO_FEW).await?;
        return Ok(());
    }
    let choice = pick_one(&options);
    ctx.say(format!("I pick: {choice}")).await?;
    Ok(())
}

/// Handles `!define <word>` and `!pick a, b, c`. Returns `true` if handled.
pub async fn handle_legacy_message(ctx: &serenity::Context, data: &Data, msg: &serenity::Message) -> Result<bool, Error> {
    let content = msg.content.as_str();
    let channel = msg.channel_id;

    if content.starts_with("!define") {
        let parts: Vec<&str> = content.trim().split(' ').collect();
        let args = &parts[1..];
        if args.is_empty() {
            channel.say(ctx, "Usage: `!define <word>`").await?;
            return Ok(true);
        }
        let word = args.join(" ");
        match lookup_definitions(data, &word).await {
            Some(embed) => {
                channel.send_message(ctx, CreateMessage::new().embed(embed)).await?;
            }
            None => {
                channel.say(ctx, format!("No definitions found for \"{word}\"")).await?;
            }
        }
        return Ok(true);
    }

    if content.starts_with("!pick") {
        let raw = content[5..].trim();
        if raw.is_empty() {
            channel.say(ctx, "Usage: `!pick option1, option2, option3`").await?;
            return Ok(true);
        }
        let options = parse_options(raw);
        if options.len() < 2 {
            channel.say(ctx, PICK_TOO_FEW).await?;
            return Ok(true);
        }
        let choice = pick_one(&options);
        channel.say(ctx, format!("I pick: {choice}")).await?;
        return Ok(true);
    }

    Ok(false)
}
