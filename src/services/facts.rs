//! Daily random fact (thefact.space).

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateEmbed, CreateEmbedFooter, CreateMessage};
use serde_json::Value;

const FACTS_API: &str = "https://thefact.space/random";

#[derive(Debug, Clone)]
pub struct Fact {
    pub text: String,
    pub source: String,
    pub source_url: String,
    pub id: String,
}

/// Fetches a random fact. Returns `None` (after logging) on any failure.
pub async fn get_random_fact(client: &reqwest::Client) -> Option<Fact> {
    let result: anyhow::Result<Value> = async { Ok(client.get(FACTS_API).send().await?.error_for_status()?.json().await?) }.await;
    let data = match result {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("Failed to fetch random fact: {err:#}");
            return None;
        }
    };

    let non_empty = |key: &str| data[key].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let id = match &data["index"] {
        Value::Number(n) if n.as_f64() != Some(0.0) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    Some(Fact {
        text: data["text"].as_str().unwrap_or_default().to_string(),
        source: non_empty("source").unwrap_or_else(|| "Unknown".to_string()),
        source_url: non_empty("source").unwrap_or_default(),
        id,
    })
}

/// Fetches a fact and posts it as an embed to `channel_id`.
pub async fn post_daily_fact(ctx: &serenity::Context, client: &reqwest::Client, channel_id: ChannelId) {
    let Some(fact) = get_random_fact(client).await else {
        tracing::error!("Failed to get fact, skipping post");
        return;
    };

    let mut embed = CreateEmbed::new()
        .colour(0xf39c12)
        .title(format!("Fact #{}", fact.id))
        .description(fact.text)
        .footer(CreateEmbedFooter::new(fact.source));
    // The source is free text; Discord rejects embed URLs that are not http(s) links.
    if fact.source_url.starts_with("http://") || fact.source_url.starts_with("https://") {
        embed = embed.url(fact.source_url);
    }

    match channel_id.send_message(ctx, CreateMessage::new().embed(embed)).await {
        Ok(_) => tracing::info!("Posted daily fact to channel {channel_id}"),
        Err(err) => tracing::error!("Error posting daily fact: {err}"),
    }
}
