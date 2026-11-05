//! /linkfix: per-server link replacement configuration.
//! OWNER: Link-fixer agent. STUB: replace with the real implementation.

use poise::serenity_prelude as serenity;

use crate::bot::{Context, Data, Error};

/// Configure link embed fixing
#[poise::command(slash_command)]
pub async fn linkfix(ctx: Context<'_>) -> Result<(), Error> {
    ctx.say("Not implemented yet").await?;
    Ok(())
}

/// Component interactions on reposted messages (e.g. delete button).
pub async fn on_event(_ctx: &serenity::Context, _event: &serenity::FullEvent, _data: &Data) -> Result<(), Error> {
    Ok(())
}
