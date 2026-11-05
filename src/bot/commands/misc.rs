//! /define, /pick and their legacy `!define` / `!pick` forms.
//! OWNER: AniList agent. STUB: replace with the real implementation.

use poise::serenity_prelude as serenity;

use crate::bot::{Context, Data, Error};

/// Get word definitions
#[poise::command(slash_command)]
pub async fn define(ctx: Context<'_>, #[description = "Word to define"] query: String) -> Result<(), Error> {
    ctx.say(format!("Not implemented yet: {query}")).await?;
    Ok(())
}

/// Randomly pick from a list of options
#[poise::command(slash_command)]
pub async fn pick(ctx: Context<'_>, #[description = "Comma-separated options"] options: String) -> Result<(), Error> {
    ctx.say(format!("Not implemented yet: {options}")).await?;
    Ok(())
}

/// Handles `!define <word>` and `!pick a, b, c`. Returns `true` if handled.
pub async fn handle_legacy_message(_ctx: &serenity::Context, _data: &Data, _msg: &serenity::Message) -> Result<bool, Error> {
    Ok(false)
}
