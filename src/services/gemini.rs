//! Gemini translation of Japanese tweets relayed by the music/art webhook.

use anyhow::bail;
use serde_json::{json, Value};

use crate::config;

/// Translates `text` to English. Returns `(translation, backup_model_used)`, or `None`
/// if no API key is configured or the model returned nothing.
///
/// The primary model is tried first; on any error (including free-tier rate limiting,
/// HTTP 429) or an empty answer, the fallback model is tried once.
pub async fn translate_tweet(client: &reqwest::Client, text: &str) -> anyhow::Result<Option<(String, bool)>> {
    let cfg = config::get();
    if cfg.gemini_api_key.is_empty() {
        tracing::error!("Gemini API key not configured");
        return Ok(None);
    }

    let prompt = format!(
        "Translate this tweet from Japanese to English. Keep any eventual emojis and maintain the original tone. Only respond with the translation:\n\n{text}"
    );

    match generate(client, &cfg.gemini_api_key, &cfg.gemini_model, &prompt).await {
        Ok(Some(translation)) => return Ok(Some((translation, false))),
        Ok(None) => tracing::warn!(
            "Model {} returned no text, retrying with {}...",
            cfg.gemini_model,
            cfg.gemini_fallback_model
        ),
        Err(err) => tracing::warn!(
            "Model {} failed ({err:#}), retrying with {}...",
            cfg.gemini_model,
            cfg.gemini_fallback_model
        ),
    }

    let translation = generate(client, &cfg.gemini_api_key, &cfg.gemini_fallback_model, &prompt).await?;
    Ok(translation.map(|t| (t, true)))
}

/// One `generateContent` call. `Ok(None)` means the model answered without any text.
async fn generate(client: &reqwest::Client, api_key: &str, model: &str, prompt: &str) -> anyhow::Result<Option<String>> {
    let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent");
    let body = json!({
        "contents": [{ "parts": [{ "text": prompt }] }],
        // Thinking tokens count against maxOutputTokens on newer Flash models; 2048 (the
        // TS value) can leave nothing for the answer.
        "generationConfig": { "temperature": 0.1, "maxOutputTokens": 8192 },
    });

    let response = client
        .post(url)
        .header("x-goog-api-key", api_key)
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        if status.as_u16() == 429 {
            tracing::error!("Gemini model {model} is rate limited (HTTP 429): free-tier quota exceeded");
        }
        let detail = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<Value>(&detail)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .unwrap_or(detail);
        bail!("Gemini API error {}: {}", status.as_u16(), message.chars().take(300).collect::<String>());
    }

    let data: Value = response.json().await?;
    let text: String = data["candidates"][0]["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["thought"].as_bool() != Some(true))
        .filter_map(|part| part["text"].as_str())
        .collect();

    if text.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}
