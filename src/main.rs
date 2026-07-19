mod bot;
mod classify;
mod config;
mod handler;
mod message_log;
mod skills;
mod status_server;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

use bot::Bot;
use config::Config;
use status_server::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config_path = std::env::var("MATRIX_LLM_BOT_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config.toml"));
    let config = Config::load(&config_path)?;

    let bot = Bot::new(&config).await?;
    bot.join_configured_rooms(&config.rooms).await?;

    let status_state = Arc::new(AppState {
        start_time: Instant::now(),
        client: bot.client(),
        message_log: bot.message_log(),
        skills: bot.skills(),
        homeserver_url: config.homeserver_url.clone(),
    });
    let http_listen_addr = config.http_listen_addr;
    tokio::spawn(async move {
        if let Err(err) = status_server::serve(http_listen_addr, status_state).await {
            tracing::error!(?err, "status/debug HTTP server exited");
        }
    });

    bot.run().await?;

    Ok(())
}
