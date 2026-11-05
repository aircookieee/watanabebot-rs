//! /mugcoin: balance, leaderboard, pay, daily, stats, admin set/give/take.
//! OWNER: Economy/tournament agent. STUB: replace with the real implementation.

use poise::serenity_prelude as serenity;

use crate::bot::{Context, Data, Error};

/// Manage your MugCoins
#[poise::command(slash_command)]
pub async fn mugcoin(ctx: Context<'_>) -> Result<(), Error> {
    ctx.say("Not implemented yet").await?;
    Ok(())
}

/// Component interactions (leaderboard pagination, ...).
pub async fn on_event(_ctx: &serenity::Context, _event: &serenity::FullEvent, _data: &Data) -> Result<(), Error> {
    Ok(())
}
