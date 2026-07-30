mod bot;
mod classify;
mod config;
mod greeting;
mod handler;
mod ignore;
mod message_log;
mod skills;
mod status_server;
mod throttle;
mod tools;
mod usage;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

use bot::Bot;
use config::Config;
use status_server::AppState;

/// Default log filter when `RUST_LOG` isn't set. Beyond the blanket `info`, this
/// quiets two `matrix-sdk` internals that are noisy but not actionable for this
/// bot: `matrix_sdk_base::client` logs "Processed a sync response" at `info` on
/// every ~30s sync (this bot's own `sync_once`/`receive_sync_response` spans
/// already show that timing), and `matrix_sdk_crypto::backups` warns "no backup
/// key was found" on every attempted key backup — expected and permanent, since
/// this bot doesn't set up server-side key backup. Explicitly set `RUST_LOG` to
/// see either again.
const DEFAULT_LOG_FILTER: &str = "info,matrix_sdk_base::client=warn,matrix_sdk_crypto::backups=error";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER)))
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
        tool_clients: bot.tool_clients(),
        usage_tracker: bot.usage_tracker(),
        rate_limit: bot.rate_limit(),
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
