//! Gemini translation of Japanese tweets relayed by the music/art webhook.
//! OWNER: AniList agent. STUB: replace with the real implementation.

/// Translates `text` to English. Returns `(translation, backup_model_used)`, or `None`
/// if no API key is configured or the model returned nothing.
pub async fn translate_tweet(_client: &reqwest::Client, _text: &str) -> anyhow::Result<Option<(String, bool)>> {
    Ok(None)
}
