//! /tournament: brackets, betting panel, polls, resolution.
//! OWNER: Economy/tournament agent. STUB: replace with the real implementation.

use poise::serenity_prelude as serenity;

use crate::bot::{Context, Data, Error};

/// Anime Tournament Betting
#[poise::command(slash_command)]
pub async fn tournament(ctx: Context<'_>) -> Result<(), Error> {
    ctx.say("Not implemented yet").await?;
    Ok(())
}

/// Component interactions, poll votes, poll finalization.
pub async fn on_event(_ctx: &serenity::Context, _event: &serenity::FullEvent, _data: &Data) -> Result<(), Error> {
    Ok(())
}
