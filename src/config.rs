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

    /// Directory holding one append-only JSONL log file per room (named after the
    /// room ID) of every classified message the bot sees. Each line is metadata
    /// (room, sender, event id, timestamp) plus the parsed `MessageAnalysis`.
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
        Ok(())
    }
}
