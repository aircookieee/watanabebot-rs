//! Cron jobs: daily Love Live song, AniList refresh, daily fact, log cleanup.
//! Schedules use the server's local time. `tokio-cron-scheduler` expressions start with
//! a seconds field.

use std::time::{Duration, SystemTime};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateEmbed, CreateEmbedFooter, CreateMessage};
use tokio_cron_scheduler::{Job, JobScheduler};

use crate::bot::Data;
use crate::config;
use crate::services::{facts, spotify};

const FAILURE_LOG_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Called once from the Ready event. Must not block; spawns background tasks.
pub fn start(ctx: serenity::Context, data: Data) {
    // Cache pre-warm: runs in the background and holds the "update in progress" flag.
    let prewarm_data = data.clone();
    tokio::spawn(async move {
        prewarm_data.anilist.prewarm().await;
    });

    tokio::spawn(async move {
        if let Err(err) = run_jobs(ctx, data).await {
            tracing::error!("Failed to start scheduled jobs: {err}");
        }
    });
}

async fn run_jobs(ctx: serenity::Context, data: Data) -> Result<(), tokio_cron_scheduler::JobSchedulerError> {
    let scheduler = JobScheduler::new().await?;

    // Love Live song, 04:05 daily.
    {
        let (ctx, data) = (ctx.clone(), data.clone());
        scheduler
            .add(Job::new_async_tz("0 5 4 * * *", chrono::Local, move |_, _| {
                let (ctx, data) = (ctx.clone(), data.clone());
                Box::pin(async move { post_love_live(&ctx, &data).await })
            })?)
            .await?;
    }

    // AniList refresh, every 3 hours on the hour.
    {
        let data = data.clone();
        scheduler
            .add(Job::new_async_tz("0 0 */3 * * *", chrono::Local, move |_, _| {
                let data = data.clone();
                Box::pin(async move {
                    tracing::info!("Auto-updating Anilist data...");
                    match data.anilist.update_all_user_data().await {
                        Ok(()) => tracing::info!("Anilist data updated successfully."),
                        Err(err) => tracing::error!("Failed to update Anilist data: {err:#}"),
                    }
                })
            })?)
            .await?;
    }

    // Daily fact, 09:05.
    {
        let (ctx, data) = (ctx.clone(), data.clone());
        scheduler
            .add(Job::new_async_tz("0 5 9 * * *", chrono::Local, move |_, _| {
                let (ctx, data) = (ctx.clone(), data.clone());
                Box::pin(async move {
                    let Some(channel) = config::get().channels.daily_facts_channel_id else { return };
                    tracing::info!("Posting daily fact...");
                    facts::post_daily_fact(&ctx, &data.client, ChannelId::new(channel)).await;
                })
            })?)
            .await?;
    }

    // Failure dump cleanup, midnight.
    scheduler
        .add(Job::new_async_tz("0 0 0 * * *", chrono::Local, move |_, _| {
            Box::pin(async move {
                tracing::info!("Running daily cleanup...");
                if let Err(err) = tokio::task::spawn_blocking(cleanup_old_logs).await {
                    tracing::error!("Failed to cleanup logs: {err}");
                }
            })
        })?)
        .await?;

    scheduler.start().await?;
    // The scheduler stops when dropped; keep it alive for the life of the process.
    std::future::pending::<()>().await;
    drop(scheduler);
    Ok(())
}

async fn post_love_live(ctx: &serenity::Context, data: &Data) {
    let Some(channel) = config::get().channels.lovelive_music_channel_id else { return };
    if !data.spotify.is_configured() {
        return;
    }
    tracing::info!("Love Live time!");

    let tracks = match data.spotify.get_playlist_tracks(None).await {
        Ok(t) => t,
        Err(err) => {
            tracing::error!("Error posting Love Live: {err:#}");
            return;
        }
    };
    if tracks.is_empty() {
        tracing::info!("No tracks found in the playlist.");
        return;
    }
    let Some(track) = spotify::get_random_track(&tracks) else { return };
    let links = spotify::format_track_for_embed(track);

    let mut embed = CreateEmbed::new()
        .colour(0xe4007f)
        .title("Love Live time!")
        .description(format!(
            "Today's song is:\n**{}**\n\n[Listen on Spotify]({})\n[Listen on Apple Music](https://music.apple.com/us/search?term={})\n[Listen on YouTube Music](https://music.youtube.com/search?q={})",
            links.full_name, track.link, links.apple_music_url, links.youtube_music_url
        ))
        .footer(CreateEmbedFooter::new("What the fuck did you just fucking say about \u{03BC}'s, you little bitch?"));
    if !track.link.is_empty() {
        embed = embed.url(track.link.clone());
    }
    if !track.album_cover.is_empty() {
        embed = embed.image(track.album_cover.clone());
    }

    if let Err(err) = ChannelId::new(channel).send_message(ctx, CreateMessage::new().embed(embed)).await {
        tracing::error!("Error posting Love Live: {err}");
    }
}

/// Deletes AniList failure dumps older than 7 days.
fn cleanup_old_logs() {
    let dir = config::get().paths.data_dir.join("anilist_failures");
    let Ok(entries) = std::fs::read_dir(&dir) else { return };

    let now = SystemTime::now();
    let mut deleted = 0;
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > FAILURE_LOG_MAX_AGE);
        if old && std::fs::remove_file(entry.path()).is_ok() {
            deleted += 1;
        }
    }
    tracing::info!("Cleaned up {deleted} old log files");
}
