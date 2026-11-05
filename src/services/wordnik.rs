//! Wordnik definitions.

use std::collections::HashSet;

use once_cell::sync::Lazy;
use poise::serenity_prelude::CreateEmbed;
use regex::Regex;
use serde::Deserialize;

use crate::config;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WordnikDefinition {
    pub word: String,
    pub part_of_speech: Option<String>,
    pub text: Option<String>,
    pub wordnik_url: Option<String>,
}

impl WordnikDefinition {
    fn has_text(&self) -> bool {
        self.text.as_deref().is_some_and(|t| !t.trim().is_empty())
    }

    fn part(&self) -> Option<&str> {
        self.part_of_speech.as_deref().filter(|p| !p.is_empty())
    }
}

/// Fetches up to 50 definitions. Returns `None` on any API or network error.
pub async fn get_word_definitions(client: &reqwest::Client, word: &str) -> Option<Vec<WordnikDefinition>> {
    let url = format!(
        "https://api.wordnik.com/v4/word.json/{}/definitions",
        urlencoding::encode(&word.to_lowercase())
    );
    let response = client
        .get(url)
        .query(&[
            ("limit", "50"),
            ("includeRelated", "false"),
            ("useCanonical", "true"),
            ("api_key", config::get().wordnik_api_key.as_str()),
        ])
        .send()
        .await;
    let response = match response {
        Ok(r) => r,
        Err(err) => {
            tracing::error!("Failed to fetch word definitions: {err}");
            return None;
        }
    };
    if !response.status().is_success() {
        tracing::error!("Wordnik API error: {}", response.status().as_u16());
        return None;
    }
    match response.json::<Vec<WordnikDefinition>>().await {
        Ok(data) => Some(data.into_iter().filter(WordnikDefinition::has_text).collect()),
        Err(err) => {
            tracing::error!("Failed to fetch word definitions: {err}");
            None
        }
    }
}

static HTML_TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]*>").unwrap());

/// Embed with up to three definitions, preferring distinct parts of speech.
pub fn create_word_embed(word_data: &[WordnikDefinition]) -> Option<CreateEmbed> {
    let definitions: Vec<&WordnikDefinition> = word_data.iter().filter(|d| d.has_text()).collect();
    let first = definitions.first()?;

    let mut seen: HashSet<&str> = HashSet::new();
    let mut selected: Vec<usize> = Vec::new();
    for (i, def) in definitions.iter().enumerate() {
        if seen.insert(def.part().unwrap_or("Other")) {
            selected.push(i);
        }
        if selected.len() == 3 {
            break;
        }
    }
    if selected.len() < 3 {
        for i in 0..definitions.len() {
            if !selected.contains(&i) {
                selected.push(i);
                if selected.len() == 3 {
                    break;
                }
            }
        }
    }

    let mut embed = CreateEmbed::new()
        .colour(0x62ACE4)
        .title(format!("Definitions for \"{}\"", first.word));
    if let Some(url) = first.wordnik_url.as_deref().filter(|u| !u.is_empty()) {
        embed = embed.url(url);
    }
    for i in selected {
        let def = definitions[i];
        let clean = HTML_TAG.replace_all(def.text.as_deref().unwrap_or_default(), "");
        // Discord limits field values to 1024 characters.
        let value: String = if clean.chars().count() > 1024 {
            clean.chars().take(1021).collect::<String>() + "..."
        } else {
            clean.into_owned()
        };
        embed = embed.field(def.part().unwrap_or("Definition"), value, false);
    }
    Some(embed)
}
