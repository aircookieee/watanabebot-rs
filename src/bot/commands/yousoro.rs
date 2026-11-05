//! /yousoro: reaction channel mode and help. The legacy `!yousoro` form lives in
//! `bot::events` and shares the replies defined here.

use poise::serenity_prelude as serenity;
use serenity::{CreateAttachment, CreateMessage};

use crate::bot::{Context, Error};
use crate::config;
use crate::db::ReactMode;

pub const HELP_TEXT: &str = "Yousoro, sailor!\n\n\
    `/yousoro here` - makes me react only in this channel\n\
    `/yousoro everywhere` - makes me react in all channels\n\
    `/linkfix` - configure link embed fixing for this server";

/// Yousoro~! Configure reaction settings
#[poise::command(slash_command, subcommands("info", "here", "everywhere"), subcommand_required)]
pub async fn yousoro(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Show yousoro help
#[poise::command(slash_command)]
async fn info(ctx: Context<'_>) -> Result<(), Error> {
    let reply = poise::CreateReply::default()
        .content(HELP_TEXT)
        .attachment(attachment("ohayousoro.png").await?);
    ctx.send(reply).await?;
    Ok(())
}

/// React only in this channel
#[poise::command(slash_command, guild_only, required_permissions = "MANAGE_GUILD")]
async fn here(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only");
    ctx.data().db.set_guild_config(guild_id.get(), Some(ctx.channel_id().get()), ReactMode::Here)?;
    let reply = poise::CreateReply::default()
        .content("Yousoro~!")
        .attachment(attachment("yousoroHere.png").await?);
    ctx.send(reply).await?;
    Ok(())
}

/// React in all channels
#[poise::command(slash_command, guild_only, required_permissions = "MANAGE_GUILD")]
async fn everywhere(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only");
    ctx.data().db.set_guild_config(guild_id.get(), None, ReactMode::Everywhere)?;
    let reply = poise::CreateReply::default()
        .content("Zensokuzenshin... Yousoro~!")
        .attachment(attachment("yousoroEverywhere.jpg").await?);
    ctx.send(reply).await?;
    Ok(())
}

/// Loads an image from the resources directory as an upload.
pub async fn attachment(name: &str) -> Result<CreateAttachment, Error> {
    Ok(CreateAttachment::path(config::get().resource(name)).await?)
}

/// Message with text and one resource image, for the legacy `!yousoro` replies.
pub async fn message_with_image(content: &str, image: &str) -> Result<CreateMessage, Error> {
    let mut msg = CreateMessage::new().add_file(attachment(image).await?);
    if !content.is_empty() {
        msg = msg.content(content);
    }
    Ok(msg)
}
