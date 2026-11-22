mod bot;
mod bot_docs;
mod browser;
mod browser_proxy;
mod cast_sheet;
mod changelog;
mod classify;
mod config;
mod db;
mod direct_rooms;
mod dm;
mod greeting;
mod handler;
mod ignore;
mod legacy_import;
mod links;
mod llm;
mod log_buffer;
mod mcp_server;
mod message_log;
mod notify;
mod photo_sheet;
mod pp;
mod room_memory;
mod rotten_tomatoes;
mod room_queue;
mod skills;
mod stats;
mod stats_command;
mod startup_notice;
mod status_server;
mod throttle;
mod tools;
mod usage;
mod weather;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

use bot::Bot;
use config::Config;
use status_server::AppState;

/// Default log filter when `RUST_LOG` isn't set. Beyond the blanket `info`, this
/// quiets two `matrix-sdk` internals that are noisy but not actionable for this
/// bot: `matrix_sdk_base::client` logs "Processed a sync response" at `info` on
/// every ~30s sync (this bot's own `sync_once`/`receive_sync_response` spans
/// already show that timing), and `matrix_sdk_crypto::backups` warns "no backup
/// key was found" on every attempted key backup — expected and permanent, since
/// this bot doesn't set up server-side key backup. `chromiumoxide::handler` warns on
/// every DevTools event newer than the crate knows, which is most of them on a current
/// Chrome, and those events aren't ones it uses. `rmcp` logs three lines at `info` for
/// every MCP request, since each one is served as its own short session, which would
/// crowd the bot's own lines out of the buffer `recent_logs` reads. Explicitly set
/// `RUST_LOG` to see any of these again.
const DEFAULT_LOG_FILTER: &str =
    "info,matrix_sdk_base::client=warn,matrix_sdk_crypto::backups=error,chromiumoxide::handler=error,rmcp=warn";

#[tokio::main]
async fn main() -> Result<()> {
    // `matrix-llm-bot export-game [name]` prints a `!dm` game's log as JSON lines, or
    // lists the games. Before logging starts, since logs also go to stdout.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("export-game") {
        let config = Config::load(&config_path())?;
        let database = db::Database::open(&config.database_path)?;
        return dm::export(&database, args.get(1).map(String::as_str), &mut std::io::stdout().lock());
    }

    let config = Config::load(&config_path())?;

    // With MCP on, log lines also go to a buffer that `recent_logs` reads.
    let log_filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    let logs = config.mcp.as_ref().map(|mcp| log_buffer::LogBuffer::new(mcp.log_buffer_lines));
    let buffer_layer = logs
        .clone()
        .map(|logs| log_buffer::layer(logs).with_filter(log_filter()));
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(log_filter()))
        .with(buffer_layer)
        .init();

    let bot = Bot::new(&config).await?;
    bot.join_configured_rooms(&config.rooms).await?;

    let mcp = config.mcp.clone().map(|mcp| {
        if !config.http_listen_addr.ip().is_loopback() {
            tracing::warn!(
                addr = %config.http_listen_addr,
                "the MCP endpoint is on and the status server isn't loopback-only; make sure only trusted machines can reach it"
            );
        }
        tracing::info!(allow_llm = mcp.allow_llm, allow_control = mcp.allow_control, "MCP endpoint on at /mcp");
        Arc::new(bot.mcp_parts(mcp, logs))
    });
    let status_state = Arc::new(AppState {
        start_time: Instant::now(),
        client: bot.client(),
        message_log: bot.message_log(),
        skills: bot.skills(),
        tool_clients: bot.tool_clients(),
        usage_tracker: bot.usage_tracker(),
        rate_limit: bot.rate_limit(),
        direct_rooms: bot.direct_rooms(),
        database: bot.database(),
        homeserver_url: config.homeserver_url.clone(),
        fetch_links: config.fetch_links,
        browser_rendering: config.browser_url.is_some(),
        notify_users: config.notify_users.clone(),
        mcp,
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

fn config_path() -> PathBuf {
    std::env::var("MATRIX_LLM_BOT_CONFIG").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("config.toml"))
}
