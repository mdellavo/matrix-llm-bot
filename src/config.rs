use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Bot configuration, loaded from a TOML file.
///
/// See `config.toml.example` for the expected shape.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub homeserver_url: String,
    pub username: String,
    pub password: String,

    /// Room IDs (`!abc:example.org`) or aliases (`#room:example.org`) to join and monitor.
    pub rooms: Vec<String>,

    /// Device name shown to other users/sessions for this login.
    #[serde(default = "default_device_name")]
    pub device_name: String,

    /// Where matrix-sdk's SQLite-backed state/crypto store is persisted.
    /// Required for E2E encryption to survive restarts.
    #[serde(default = "default_store_path")]
    pub store_path: PathBuf,

    /// The bot's SQLite database: messages, reactions, facts, summaries, and usage.
    #[serde(default = "default_database_path")]
    pub database_path: PathBuf,

    /// Where per-room JSONL message logs were kept before SQLite. Read once, to
    /// import them into the database.
    #[serde(default = "default_message_log_dir")]
    pub message_log_dir: PathBuf,

    /// Address the status/debugging HTTP server binds to. Defaults to loopback-only
    /// (`127.0.0.1:8080`) since the page and its API expose logged message content
    /// with no authentication — only widen this deliberately.
    #[serde(default = "default_http_listen_addr")]
    pub http_listen_addr: SocketAddr,

    /// Directory of bot commands, one subdirectory per command each containing a
    /// `SKILL.md` (name/description/usage/tools frontmatter + a prompt body) — see
    /// `src/skills.rs`. Unlike `store_path`/`message_log_dir`, this holds user-authored
    /// source, not runtime state, so it's not git-ignored by default.
    #[serde(default = "default_skills_dir")]
    pub skills_dir: PathBuf,

    /// OMDb API key (<https://www.omdbapi.com/apikey.aspx>) for the `imdb` skill's
    /// movie/show lookups. Optional — not every deployment will use that skill; if
    /// unset (or blank), the `imdb` skill replies with a clear "not configured"
    /// message at invocation time instead of attempting a lookup.
    #[serde(default)]
    pub omdb_api_key: Option<String>,

    /// Matrix user IDs (`@user:example.org`) whose messages the bot ignores
    /// entirely — dropped in `on_room_message` before classification, logging,
    /// or any reply, so they never show up in message history either. Defaults
    /// to empty (ignore nobody). See `IgnoredUsers` (`src/ignore.rs`).
    #[serde(default)]
    pub ignored_users: Vec<String>,

    /// Where usage totals were kept before SQLite. Read once, to import them.
    #[serde(default = "default_usage_state_path")]
    pub usage_state_path: PathBuf,

    /// How long to pause outgoing Claude API calls after one comes back
    /// rate-limited (HTTP 429), before trying again — see `Throttle`
    /// (`src/throttle.rs`). Defaults to 60 seconds.
    #[serde(default = "default_rate_limit_backoff_secs")]
    pub rate_limit_backoff_secs: u64,

    /// Hard cap on cumulative estimated Claude API cost in USD (see
    /// `UsageTracker`). Once accumulated cost reaches this, every Claude call
    /// site (classifier, chat/greeting replies, skills) stops making API
    /// calls entirely, until the limit is raised or the persisted usage state
    /// (`usage_state_path`) is reset. `None` (the default) means no limit.
    #[serde(default)]
    pub cost_limit_usd: Option<f64>,

    /// Where facts and room summaries were kept before SQLite. Read once, to import them.
    #[serde(default = "default_memory_path")]
    pub memory_path: PathBuf,

    /// Chance, from 0 to 1, that the bot considers joining in on a message that
    /// isn't addressed to it. 0 (the default) turns this off entirely.
    #[serde(default)]
    pub interjection_chance: f64,

    /// Minimum time between unprompted comments in a room. Defaults to 30 minutes.
    #[serde(default = "default_interjection_cooldown_secs")]
    pub interjection_cooldown_secs: u64,

    /// Whether to fetch pages linked in chat so the bot knows what was shared. Only
    /// public addresses are fetched. Defaults to true.
    #[serde(default = "default_fetch_links")]
    pub fetch_links: bool,

    /// Who may start a private conversation by inviting the bot: user IDs
    /// (`@alice:example.org`) and whole servers (`:example.org`). Invites from anyone
    /// else are declined. Empty (the default) means the bot's own homeserver.
    #[serde(default)]
    pub dm_allowed_users: Vec<String>,

    /// A headless Chrome's DevTools address, like `http://chrome:9222`, for rendering
    /// linked pages so their scripts run. Unset (the default) fetches pages without
    /// running scripts. The browser must have no internet access of its own: it reaches
    /// sites only through the bot's proxy. See the README for the container setup.
    #[serde(default)]
    pub browser_url: Option<String>,

    /// Where the bot listens for the browser's connections. Defaults to `0.0.0.0:3128`.
    #[serde(default = "default_browser_proxy_listen")]
    pub browser_proxy_listen: String,

    /// The proxy's address as the browser reaches it, like `http://matrix-llm-bot:3128`.
    /// Required with `browser_url`.
    #[serde(default)]
    pub browser_proxy_url: Option<String>,
}

fn default_browser_proxy_listen() -> String {
    "0.0.0.0:3128".to_string()
}

fn default_database_path() -> PathBuf {
    PathBuf::from("./data/bot.db")
}

fn default_device_name() -> String {
    "matrix-llm-bot".to_string()
}

fn default_store_path() -> PathBuf {
    PathBuf::from("./data/store")
}

fn default_message_log_dir() -> PathBuf {
    PathBuf::from("./data/messages")
}

fn default_http_listen_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

fn default_skills_dir() -> PathBuf {
    PathBuf::from("./skills")
}

fn default_usage_state_path() -> PathBuf {
    PathBuf::from("./data/usage.json")
}

fn default_rate_limit_backoff_secs() -> u64 {
    60
}

fn default_memory_path() -> PathBuf {
    PathBuf::from("./data/memory.json")
}

fn default_fetch_links() -> bool {
    true
}

fn default_interjection_cooldown_secs() -> u64 {
    30 * 60
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file at {}", path.display()))?;
        let config: Config = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config file at {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.homeserver_url.starts_with("http://") || self.homeserver_url.starts_with("https://"),
            "homeserver_url must start with http:// or https://"
        );
        anyhow::ensure!(!self.username.is_empty(), "username must not be empty");
        anyhow::ensure!(!self.password.is_empty(), "password must not be empty");
        anyhow::ensure!(!self.rooms.is_empty(), "rooms must list at least one room to monitor");
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.interjection_chance),
            "interjection_chance must be between 0 and 1"
        );
        if self.browser_url.is_some() {
            anyhow::ensure!(self.browser_proxy_url.is_some(), "browser_proxy_url is required with browser_url");
            anyhow::ensure!(self.fetch_links, "browser_url needs fetch_links on");
        }
        for entry in &self.dm_allowed_users {
            anyhow::ensure!(
                (entry.starts_with('@') || entry.starts_with(':')) && entry.contains(':') && entry.len() > 1,
                "dm_allowed_users entries must look like @user:example.org or :example.org, got {entry}"
            );
        }
        Ok(())
    }
}
