//! /anime, /manga, /anilist and the legacy `!al` / `!alm` / ((anime)) / [[manga]] lookups.
//! OWNER: AniList agent. STUB: replace with the real implementation.

use poise::serenity_prelude as serenity;

use crate::bot::{Data, Error};

pub fn commands() -> Vec<poise::Command<Data, Error>> {
    vec![]
}

/// Handles `!al ...`, `!alm ...`, `((title))` and `[[title]]` in a message.
/// Returns `true` if the message was handled (the caller then stops processing it).
pub async fn handle_legacy_message(_ctx: &serenity::Context, _data: &Data, _msg: &serenity::Message) -> Result<bool, Error> {
    Ok(false)
}
