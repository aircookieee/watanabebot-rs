mod bot;
mod config;
mod db;
mod scheduler;
mod services;
mod web;

use std::sync::Arc;

use anyhow::Context as _;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,serenity=warn,tracing::span=warn")),
        )
        .init();

    let cfg = config::init();
    if cfg.discord.token.is_empty() {
        anyhow::bail!("DISCORD_TOKEN is not set");
    }

    tracing::info!("Initializing database...");
    let db = db::Db::open(cfg).context("failed to open database")?;

    let http = Arc::new(serenity::all::Http::new(&cfg.discord.token));
    let state = Arc::new(bot::AppState::new(db, http.clone()));

    // The web dashboard only needs the REST client, so it can start before the gateway connects.
    let web_state = state.clone();
    tokio::spawn(async move {
        if let Err(err) = web::serve(web_state).await {
            tracing::error!("web server stopped: {err:#}");
        }
    });

    bot::run(state).await
}
