//! Cron jobs: daily Love Live song, AniList refresh, daily fact, log cleanup.
//! OWNER: AniList agent. STUB: replace with the real implementation.

use poise::serenity_prelude as serenity;

use crate::bot::Data;

/// Called once from the Ready event. Must not block; spawn background tasks.
pub fn start(_ctx: serenity::Context, _data: Data) {}
