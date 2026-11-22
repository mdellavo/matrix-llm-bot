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
    /// calls entirely, until the limit is raised or the usage totals in the
    /// database are reset. `None` (the default) means no limit.
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

    /// Users to message privately, like `@alice:example.org`: each time the bot
    /// starts, with what's new since their last notice, and with alerts about spend,
    /// failing Claude calls, and the sync loop stopping. The bot reuses the private
    /// room it shares with each, or creates one. Empty (the default) sends nothing.
    /// `notify_on_start` is its old name.
    #[serde(default, alias = "notify_on_start")]
    pub notify_users: Vec<String>,

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

    /// The MCP endpoint at `/mcp` on the status server, for operating the bot from
    /// Claude Code or another MCP client. Absent (the default) leaves it off.
    #[serde(default)]
    pub mcp: Option<McpConfig>,
}

/// See `src/mcp_server.rs`. Each tier is a separate opt-in, since each gives whoever
/// holds the token more than the last.
#[derive(Clone, Deserialize)]
pub struct McpConfig {
    /// Sent as `Authorization: Bearer <token>`. Every request needs it.
    pub token: String,
    /// Tools that call Claude, and so cost money: `classify` and `replay_reply`.
    #[serde(default)]
    pub allow_llm: bool,
    /// Tools that change things: sending messages, facts, ignoring people, and so on.
    #[serde(default)]
    pub allow_control: bool,
    /// Private rooms and `!dm` games are hidden unless this is on.
    #[serde(default)]
    pub include_private_rooms: bool,
    /// How many recent log lines `recent_logs` can return.
    #[serde(default = "default_mcp_log_lines")]
    pub log_buffer_lines: usize,
    /// `Host` headers to accept beyond loopback, like `bot.tailnet.ts.net:8080`, for
    /// reaching the endpoint other than through localhost. Checking `Host` is what stops
    /// a web page from reaching it through DNS rebinding.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

/// Leaves the token out, since `Config` is sometimes debug-printed.
impl std::fmt::Debug for McpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConfig")
            .field("token", &"<redacted>")
            .field("allow_llm", &self.allow_llm)
            .field("allow_control", &self.allow_control)
            .field("include_private_rooms", &self.include_private_rooms)
            .field("log_buffer_lines", &self.log_buffer_lines)
            .field("allowed_hosts", &self.allowed_hosts)
            .finish()
    }
}

/// Shortest token accepted, so a placeholder like "changeme" can't go live.
const MIN_MCP_TOKEN_LEN: usize = 24;

fn default_mcp_log_lines() -> usize {
    2000
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
        for user in &self.notify_users {
            anyhow::ensure!(
                matrix_sdk::ruma::UserId::parse(user.as_str()).is_ok(),
                "notify_users entries must be user IDs like @user:example.org, got {user}"
            );
        }
        for entry in &self.dm_allowed_users {
            anyhow::ensure!(
                (entry.starts_with('@') || entry.starts_with(':')) && entry.contains(':') && entry.len() > 1,
                "dm_allowed_users entries must look like @user:example.org or :example.org, got {entry}"
            );
        }
        if let Some(mcp) = &self.mcp {
            anyhow::ensure!(
                mcp.token.trim().len() >= MIN_MCP_TOKEN_LEN,
                "mcp.token must be at least {MIN_MCP_TOKEN_LEN} characters; generate one with `openssl rand -hex 32`"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra: &str) -> Result<Config> {
        let config: Config = toml::from_str(&format!(
            "homeserver_url = \"https://example.org\"\nusername = \"bot\"\npassword = \"x\"\nrooms = [\"#a:example.org\"]\n{extra}"
        ))?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn mcp_is_off_unless_configured_and_needs_a_real_token() {
        assert!(config("").unwrap().mcp.is_none());

        let on = config("[mcp]\ntoken = \"0123456789abcdef0123456789abcdef\"").unwrap().mcp.unwrap();
        assert!(!on.allow_llm && !on.allow_control && !on.include_private_rooms);
        assert_eq!(on.log_buffer_lines, 2000);
        assert!(!format!("{on:?}").contains("0123456789abcdef"));

        let err = config("[mcp]\ntoken = \"changeme\"").unwrap_err().to_string();
        assert!(err.contains("mcp.token must be at least"), "{err}");
    }

    #[test]
    fn notify_users_takes_user_ids() {
        assert!(config("").unwrap().notify_users.is_empty());
        assert_eq!(config("notify_users = [\"@procyon:p.ound.town\"]").unwrap().notify_users, ["@procyon:p.ound.town"]);
        // The name it shipped with first.
        assert_eq!(config("notify_on_start = [\"@procyon:p.ound.town\"]").unwrap().notify_users, ["@procyon:p.ound.town"]);
        for bad in ["procyon", ":p.ound.town", "@procyon"] {
            let err = config(&format!("notify_users = [\"{bad}\"]")).unwrap_err().to_string();
            assert!(err.contains("notify_users entries must be user IDs"), "{bad}: {err}");
        }
    }
}
