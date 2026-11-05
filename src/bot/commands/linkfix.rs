//! /linkfix: per-server link replacement configuration, plus the "Delete fixed message"
//! message context-menu command.

use poise::serenity_prelude as serenity;
use serenity::{CreateEmbed, Permissions};

use crate::bot::{Context, Data, Error};
use crate::db::linkfix::FixMode;
use crate::services::linkfix::{find_site, normalize_domain, site_state, SITES};

#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
pub enum ModeChoice {
    #[name = "webhook"]
    Webhook,
    #[name = "reply"]
    Reply,
}

fn guild_id(ctx: Context<'_>) -> Result<u64, Error> {
    ctx.guild_id()
        .map(|g| g.get())
        .ok_or_else(|| anyhow::anyhow!("This command can only be used in a server."))
}

fn site_or_err(input: &str) -> Result<&'static crate::services::linkfix::Site, Error> {
    find_site(input).ok_or_else(|| {
        anyhow::anyhow!("Unknown site `{input}`. Use `/linkfix status` to list site ids.")
    })
}

async fn autocomplete_site(_ctx: Context<'_>, partial: &str) -> Vec<serenity::AutocompleteChoice> {
    let partial = partial.to_ascii_lowercase();
    SITES
        .iter()
        .filter(|s| s.id.contains(&partial) || s.name.to_ascii_lowercase().contains(&partial))
        .map(|s| serenity::AutocompleteChoice::new(format!("{} ({})", s.name, s.id), s.id))
        .collect()
}

/// Configure link embed fixing
#[poise::command(
    slash_command,
    guild_only,
    required_permissions = "MANAGE_GUILD",
    default_member_permissions = "MANAGE_GUILD",
    subcommands(
        "status", "toggle", "domain", "enable", "disable", "set_mode", "ignore", "unignore", "rule"
    )
)]
pub async fn linkfix(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Show the current link fixing settings
#[poise::command(slash_command, guild_only, required_permissions = "MANAGE_GUILD")]
async fn status(ctx: Context<'_>) -> Result<(), Error> {
    let gid = guild_id(ctx)?;
    let cfg = ctx.data().linkfix.config(gid)?;

    let mut sites = String::new();
    for site in SITES {
        let (enabled, domain) = site_state(&cfg, site);
        sites.push_str(&format!(
            "`{}` {}: {}, {}",
            site.id,
            site.name,
            if enabled { "on" } else { "off" },
            domain
        ));
        if !site.alternatives.is_empty() {
            sites.push_str(&format!(
                " (alternatives: {})",
                site.alternatives.join(", ")
            ));
        }
        sites.push('\n');
    }

    let ignored = if cfg.ignored.is_empty() {
        "none".to_string()
    } else {
        let mut ids: Vec<u64> = cfg.ignored.iter().copied().collect();
        ids.sort_unstable();
        ids.iter()
            .map(|id| format!("<#{id}>"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let rules = if cfg.rules.is_empty() {
        "none".to_string()
    } else {
        cfg.rules
            .iter()
            .map(|(from, to)| format!("{from} -> {to}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let embed = CreateEmbed::new()
        .title("Link fixer")
        .description(truncate(&sites, 4000))
        .field(
            "Server",
            if cfg.enabled { "enabled" } else { "disabled" },
            true,
        )
        .field("Mode", cfg.mode.as_str(), true)
        .field("Ignored channels", truncate(&ignored, 1000), false)
        .field("Custom rules", truncate(&rules, 1000), false);
    ctx.send(poise::CreateReply::default().embed(embed).ephemeral(true))
        .await?;
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

/// Turn a site on or off for this server
#[poise::command(slash_command, guild_only, required_permissions = "MANAGE_GUILD")]
async fn toggle(
    ctx: Context<'_>,
    #[description = "Site"]
    #[autocomplete = "autocomplete_site"]
    site: String,
    #[description = "Fix links for this site"] enabled: bool,
) -> Result<(), Error> {
    let gid = guild_id(ctx)?;
    let site = site_or_err(&site)?;
    ctx.data()
        .db
        .set_linkfix_site_enabled(gid, site.id, enabled)?;
    ctx.data().linkfix.invalidate(gid);
    ctx.say(format!(
        "{} links will {}be fixed.",
        site.name,
        if enabled { "" } else { "not " }
    ))
    .await?;
    Ok(())
}

/// Choose the replacement domain for a site
#[poise::command(slash_command, guild_only, required_permissions = "MANAGE_GUILD")]
async fn domain(
    ctx: Context<'_>,
    #[description = "Site"]
    #[autocomplete = "autocomplete_site"]
    site: String,
    #[description = "Replacement domain, or `default` to reset"] domain: String,
) -> Result<(), Error> {
    let gid = guild_id(ctx)?;
    let site = site_or_err(&site)?;
    if matches!(
        domain.trim().to_ascii_lowercase().as_str(),
        "default" | "reset"
    ) {
        ctx.data().db.set_linkfix_site_domain(gid, site.id, None)?;
        ctx.data().linkfix.invalidate(gid);
        ctx.say(format!("{} now uses {}.", site.name, site.default_domain))
            .await?;
        return Ok(());
    }
    let Some(domain) = normalize_domain(&domain) else {
        let mut hint =
            String::from("That does not look like a hostname (example: `fxtwitter.com`).");
        if !site.alternatives.is_empty() {
            hint.push_str(&format!(
                " Known alternatives: {}.",
                site.alternatives.join(", ")
            ));
        }
        anyhow::bail!(hint);
    };
    ctx.data()
        .db
        .set_linkfix_site_domain(gid, site.id, Some(&domain))?;
    ctx.data().linkfix.invalidate(gid);
    ctx.say(format!("{} links will use {domain}.", site.name))
        .await?;
    Ok(())
}

/// Enable link fixing in this server
#[poise::command(slash_command, guild_only, required_permissions = "MANAGE_GUILD")]
async fn enable(ctx: Context<'_>) -> Result<(), Error> {
    let gid = guild_id(ctx)?;
    ctx.data().db.set_linkfix_enabled(gid, true)?;
    ctx.data().linkfix.invalidate(gid);
    ctx.say("Link fixing is enabled.").await?;
    Ok(())
}

/// Disable link fixing in this server
#[poise::command(slash_command, guild_only, required_permissions = "MANAGE_GUILD")]
async fn disable(ctx: Context<'_>) -> Result<(), Error> {
    let gid = guild_id(ctx)?;
    ctx.data().db.set_linkfix_enabled(gid, false)?;
    ctx.data().linkfix.invalidate(gid);
    ctx.say("Link fixing is disabled.").await?;
    Ok(())
}

/// Choose how fixed links are posted
#[poise::command(
    slash_command,
    guild_only,
    rename = "mode",
    required_permissions = "MANAGE_GUILD"
)]
async fn set_mode(
    ctx: Context<'_>,
    #[description = "webhook reposts the message as its author, reply keeps it and replies"]
    mode: ModeChoice,
) -> Result<(), Error> {
    let gid = guild_id(ctx)?;
    let mode = match mode {
        ModeChoice::Webhook => FixMode::Webhook,
        ModeChoice::Reply => FixMode::Reply,
    };
    ctx.data().db.set_linkfix_mode(gid, mode)?;
    ctx.data().linkfix.invalidate(gid);
    let note = match mode {
        FixMode::Webhook => {
            "Messages are reposted through a webhook (needs Manage Webhooks and Manage Messages)."
        }
        FixMode::Reply => "The bot replies with the fixed links.",
    };
    ctx.say(format!("Mode set to {}. {note}", mode.as_str()))
        .await?;
    Ok(())
}

/// Stop fixing links in a channel
#[poise::command(slash_command, guild_only, required_permissions = "MANAGE_GUILD")]
async fn ignore(
    ctx: Context<'_>,
    #[description = "Channel to ignore"] channel: serenity::GuildChannel,
) -> Result<(), Error> {
    let gid = guild_id(ctx)?;
    let added = ctx.data().db.add_linkfix_ignored(gid, channel.id.get())?;
    ctx.data().linkfix.invalidate(gid);
    let reply = if added {
        format!("Ignoring <#{}>.", channel.id)
    } else {
        format!("<#{}> was already ignored.", channel.id)
    };
    ctx.say(reply).await?;
    Ok(())
}

/// Resume fixing links in a channel
#[poise::command(slash_command, guild_only, required_permissions = "MANAGE_GUILD")]
async fn unignore(
    ctx: Context<'_>,
    #[description = "Channel to stop ignoring"] channel: serenity::GuildChannel,
) -> Result<(), Error> {
    let gid = guild_id(ctx)?;
    let removed = ctx
        .data()
        .db
        .remove_linkfix_ignored(gid, channel.id.get())?;
    ctx.data().linkfix.invalidate(gid);
    let reply = if removed {
        format!("No longer ignoring <#{}>.", channel.id)
    } else {
        format!("<#{}> was not ignored.", channel.id)
    };
    ctx.say(reply).await?;
    Ok(())
}

/// Manage custom replacement rules
#[poise::command(
    slash_command,
    guild_only,
    required_permissions = "MANAGE_GUILD",
    subcommands("rule_add", "rule_remove")
)]
async fn rule(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

fn rule_domain(input: &str) -> Result<String, Error> {
    let domain = normalize_domain(input).ok_or_else(|| {
        anyhow::anyhow!("`{input}` does not look like a hostname (example: `example.com`).")
    })?;
    Ok(domain
        .strip_prefix("www.")
        .map(str::to_string)
        .unwrap_or(domain))
}

/// Replace links from one domain with another
#[poise::command(
    slash_command,
    guild_only,
    rename = "add",
    required_permissions = "MANAGE_GUILD"
)]
async fn rule_add(
    ctx: Context<'_>,
    #[description = "Domain to match, e.g. example.com"] from: String,
    #[description = "Replacement domain"] to: String,
) -> Result<(), Error> {
    let gid = guild_id(ctx)?;
    let from = rule_domain(&from)?;
    let to = rule_domain(&to)?;
    if from == to {
        anyhow::bail!("The two domains are the same.");
    }
    ctx.data().db.set_linkfix_rule(gid, &from, &to)?;
    ctx.data().linkfix.invalidate(gid);
    ctx.say(format!("Links on {from} will use {to}.")).await?;
    Ok(())
}

/// Remove a custom replacement rule
#[poise::command(
    slash_command,
    guild_only,
    rename = "remove",
    required_permissions = "MANAGE_GUILD"
)]
async fn rule_remove(
    ctx: Context<'_>,
    #[description = "Domain of the rule to remove"] from: String,
) -> Result<(), Error> {
    let gid = guild_id(ctx)?;
    let from = rule_domain(&from)?;
    let removed = ctx.data().db.remove_linkfix_rule(gid, &from)?;
    ctx.data().linkfix.invalidate(gid);
    ctx.say(if removed {
        format!("Removed the rule for {from}.")
    } else {
        format!("No rule for {from}.")
    })
    .await?;
    Ok(())
}

/// Delete a message reposted by the link fixer (its author or a moderator).
#[poise::command(context_menu_command = "Delete fixed message", guild_only)]
pub async fn delete_fixed_message(
    ctx: Context<'_>,
    message: serenity::Message,
) -> Result<(), Error> {
    let db = &ctx.data().db;
    let record = db
        .get_linkfix_repost(message.id.get())?
        .filter(|r| Some(r.guild_id) == ctx.guild_id().map(|g| g.get()))
        .ok_or_else(|| anyhow::anyhow!("That message was not posted by the link fixer."))?;

    let is_author = record.author_id == ctx.author().id.get();
    let can_moderate = ctx
        .author_member()
        .await
        .and_then(|m| m.permissions)
        .is_some_and(|p| {
            p.contains(Permissions::MANAGE_MESSAGES) || p.contains(Permissions::ADMINISTRATOR)
        });
    if !is_author && !can_moderate {
        anyhow::bail!("Only the original author or a moderator can delete this message.");
    }

    message
        .delete(ctx)
        .await
        .map_err(|e| anyhow::anyhow!("Could not delete the message: {e}"))?;
    db.delete_linkfix_repost(message.id.get())?;
    ctx.send(
        poise::CreateReply::default()
            .content("Deleted.")
            .ephemeral(true),
    )
    .await?;
    Ok(())
}

/// Nothing to handle yet: reposts are removed through the context-menu command above.
pub async fn on_event(
    _ctx: &serenity::Context,
    _event: &serenity::FullEvent,
    _data: &Data,
) -> Result<(), Error> {
    Ok(())
}
