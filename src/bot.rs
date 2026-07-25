use std::path::Path;
use std::sync::Arc;

use anthropic_sdk::Anthropic;
use anyhow::{Context, Result};
use matrix_sdk::{
    Client,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    ruma::{DeviceId, RoomOrAliasId},
};
use tracing::{info, warn};

use crate::config::Config;
use crate::greeting::GreetingCooldown;
use crate::handler::{HandlerContext, on_room_message};
use crate::ignore::IgnoredUsers;
use crate::message_log::MessageLogger;
use crate::skills::SkillRegistry;
use crate::tools::ToolClients;
use crate::usage::UsageTracker;

const SESSION_FILE_NAME: &str = "session.json";
const DEVICE_ID_FILE_NAME: &str = "device_id";

pub struct Bot {
    client: Client,
    message_log: Arc<MessageLogger>,
    skills: Arc<SkillRegistry>,
    tool_clients: Arc<ToolClients>,
    usage_tracker: Arc<UsageTracker>,
    greeting_cooldown: Arc<GreetingCooldown>,
    ignored_users: Arc<IgnoredUsers>,
}

impl Bot {
    /// Builds a client backed by a persistent SQLite store (required for E2E encryption
    /// state to survive restarts) and logs in with the configured credentials.
    ///
    /// The crypto store persists per-device state, so logging in fresh (a new device
    /// each time) on every restart eventually collides with it — matrix-sdk refuses to
    /// reconcile crypto state from two different devices. To avoid that, the access
    /// token from a successful login is saved to `session.json` next to the store and
    /// restored on subsequent runs instead of logging in again; if that ever falls
    /// back to a fresh password login (missing/invalid saved session), it's pinned to
    /// a device ID persisted alongside the store so the device — and its crypto state
    /// — stays the same either way.
    pub async fn new(config: &Config) -> Result<Self> {
        std::fs::create_dir_all(&config.store_path)
            .with_context(|| format!("failed to create store dir at {}", config.store_path.display()))?;

        let client = Client::builder()
            .homeserver_url(&config.homeserver_url)
            .sqlite_store(&config.store_path, None)
            .build()
            .await
            .context("failed to build matrix client")?;

        Self::login_or_restore_session(&client, config).await?;

        info!(homeserver = %config.homeserver_url, user = %config.username, "logged in");

        // NOTE: the bot currently trusts whatever devices it encounters implicitly via
        // matrix-sdk's default behavior. For production use, add explicit device
        // verification/cross-signing here before relying on encrypted rooms.

        // Reads ANTHROPIC_API_KEY from the environment.
        let anthropic = Arc::new(
            Anthropic::from_env().context("failed to create Anthropic client (is ANTHROPIC_API_KEY set?)")?,
        );
        let message_log = Arc::new(MessageLogger::open(&config.message_log_dir)?);
        let skills = Arc::new(SkillRegistry::load(&config.skills_dir)?);
        let tool_clients = Arc::new(ToolClients::new(config.omdb_api_key.clone()));
        let usage_tracker = Arc::new(UsageTracker::new());
        let greeting_cooldown = Arc::new(GreetingCooldown::new(crate::greeting::DEFAULT_COOLDOWN));
        let ignored_users = Arc::new(IgnoredUsers::new(config.ignored_users.clone()));

        // Bundled into one `HandlerContext` (see its doc comment in `handler.rs`)
        // and registered as a single event-handler context, rather than one
        // `add_event_handler_context` call per resource — matrix-sdk's
        // `EventHandler` trait only supports so many separate `Ctx<T>` params.
        client.add_event_handler_context(Arc::new(HandlerContext {
            anthropic,
            message_log: Arc::clone(&message_log),
            skills: Arc::clone(&skills),
            tool_clients: Arc::clone(&tool_clients),
            usage_tracker: Arc::clone(&usage_tracker),
            greeting_cooldown: Arc::clone(&greeting_cooldown),
            ignored_users: Arc::clone(&ignored_users),
        }));

        Ok(Self { client, message_log, skills, tool_clients, usage_tracker, greeting_cooldown, ignored_users })
    }

    /// Restores a previously saved session if one exists at `<store_path>/session.json`
    /// (skipping password login entirely); otherwise logs in with the configured
    /// credentials — pinned to a device ID persisted at `<store_path>/device_id` so a
    /// fresh login still lands on the same device as any crypto state already in the
    /// store — and saves the resulting session for next time.
    async fn login_or_restore_session(client: &Client, config: &Config) -> Result<()> {
        let session_path = config.store_path.join(SESSION_FILE_NAME);

        if let Some(session) = load_session(&session_path)? {
            client.restore_session(session).await.context("failed to restore saved session")?;
            info!(path = %session_path.display(), "restored saved session");
            return Ok(());
        }

        let device_id = load_or_create_device_id(&config.store_path)?;

        client
            .matrix_auth()
            .login_username(&config.username, &config.password)
            .device_id(&device_id)
            .initial_device_display_name(&config.device_name)
            .send()
            .await
            .context("failed to log in to homeserver")?;

        if let Some(session) = client.matrix_auth().session() {
            save_session(&session_path, &session)?;
            info!(path = %session_path.display(), "saved new session");
        }

        Ok(())
    }

    /// A cheap clone of the underlying matrix-sdk client, for callers (e.g. the status
    /// server) that need to query room/session state alongside the sync loop.
    pub fn client(&self) -> Client {
        self.client.clone()
    }

    /// A clone of the shared message logger handle, for callers that need read access
    /// to logged messages without going through the event handler.
    pub fn message_log(&self) -> Arc<MessageLogger> {
        Arc::clone(&self.message_log)
    }

    /// A clone of the shared skill registry handle, for callers (e.g. the status
    /// server) that need to list loaded commands without going through the event
    /// handler.
    pub fn skills(&self) -> Arc<SkillRegistry> {
        Arc::clone(&self.skills)
    }

    /// A clone of the shared HTTP-tool clients handle (Urban Dictionary/OMDb/Leafly),
    /// for callers that need it outside the event handler.
    pub fn tool_clients(&self) -> Arc<ToolClients> {
        Arc::clone(&self.tool_clients)
    }

    /// A clone of the shared usage tracker handle (Claude API token/request
    /// counts), for callers (e.g. the status server) that need to report it
    /// without going through the event handler.
    pub fn usage_tracker(&self) -> Arc<UsageTracker> {
        Arc::clone(&self.usage_tracker)
    }

    /// A clone of the shared per-room greeting-cooldown handle. No current
    /// external consumer, but every other shared resource has this accessor.
    #[allow(dead_code)]
    pub fn greeting_cooldown(&self) -> Arc<GreetingCooldown> {
        Arc::clone(&self.greeting_cooldown)
    }

    /// A clone of the shared ignored-users set. No current external consumer,
    /// but every other shared resource has this accessor.
    #[allow(dead_code)]
    pub fn ignored_users(&self) -> Arc<IgnoredUsers> {
        Arc::clone(&self.ignored_users)
    }

    /// Joins every room configured for monitoring. Already-joined rooms are skipped.
    pub async fn join_configured_rooms(&self, rooms: &[String]) -> Result<()> {
        for room in rooms {
            let room_id = RoomOrAliasId::parse(room)
                .with_context(|| format!("invalid room id/alias in config: {room}"))?;

            match self.client.join_room_by_id_or_alias(&room_id, &[]).await {
                Ok(joined) => info!(room = %joined.room_id(), "joined room"),
                Err(err) => warn!(?err, room, "failed to join room (already joined?)"),
            }
        }
        Ok(())
    }

    /// Runs an initial sync to catch up on state without triggering the message handler
    /// for backlog, then registers the handler and syncs forever.
    pub async fn run(self) -> Result<()> {
        let response = self
            .client
            .sync_once(SyncSettings::default())
            .await
            .context("initial sync failed")?;

        self.client.add_event_handler(on_room_message);

        info!("starting sync loop");
        let settings = SyncSettings::default().token(response.next_batch);
        self.client.sync(settings).await.context("sync loop failed")?;
        Ok(())
    }
}

fn load_session(path: &Path) -> Result<Option<MatrixSession>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read session file at {}", path.display()))?;
    let session = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse session file at {}", path.display()))?;
    Ok(Some(session))
}

fn save_session(path: &Path, session: &MatrixSession) -> Result<()> {
    let raw = serde_json::to_string_pretty(session).context("failed to serialize session")?;
    std::fs::write(path, raw).with_context(|| format!("failed to write session file at {}", path.display()))?;
    Ok(())
}

/// Reads the device ID persisted at `<store_dir>/device_id`, generating and saving a
/// new random one on first run. Kept stable across restarts so a fresh password login
/// (when there's no saved session to restore) still lands on the same device as
/// whatever crypto state is already in the store at `store_dir`.
fn load_or_create_device_id(store_dir: &Path) -> Result<String> {
    let path = store_dir.join(DEVICE_ID_FILE_NAME);

    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let device_id = DeviceId::new().to_string();
    std::fs::write(&path, &device_id)
        .with_context(|| format!("failed to write device id file at {}", path.display()))?;
    Ok(device_id)
}
