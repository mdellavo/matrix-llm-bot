use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use serde::Serialize;

use crate::db::Database;

/// The Matrix user IDs whose messages the bot ignores entirely: those in
/// `Config::ignored_users`, plus any added at runtime through the MCP endpoint, which
/// are kept in the database so they stay ignored after a restart. `on_room_message`
/// (`src/handler.rs`) checks this before doing anything else with an incoming
/// event: an ignored sender's message is dropped before classification,
/// logging, or any reply, so it can never end up in message history either
/// (`MessageLogger::recent`, read by the `message_log` skill tool and by
/// chat/greeting reply context) — there's nothing to filter out later because
/// it was never written in the first place.
#[derive(Debug, Default)]
pub struct IgnoredUsers {
    configured: HashSet<String>,
    added: RwLock<BTreeMap<String, AddedIgnore>>,
    /// `None` in tests that don't need the additions saved.
    database: Option<Arc<Database>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AddedIgnore {
    pub added_by: String,
    pub added_at: String,
}

/// One ignored user, as listed for the MCP endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct IgnoredUser {
    pub user_id: String,
    /// `"config"` or `"runtime"`. Only runtime entries can be removed without editing
    /// `config.toml`.
    pub source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unignored {
    Removed,
    WasNotIgnored,
    /// It's in `config.toml`, which would bring it back on the next start.
    InConfig,
}

impl IgnoredUsers {
    #[cfg(test)]
    pub fn new(users: impl IntoIterator<Item = String>) -> Self {
        Self { configured: users.into_iter().collect(), ..Default::default() }
    }

    /// The configured users plus the ones saved in the database.
    pub fn open(database: Arc<Database>, users: impl IntoIterator<Item = String>) -> Result<Self> {
        let rows = database.with(|connection| {
            let mut statement = connection.prepare("SELECT user_id, added_by, added_at FROM ignored_users")?;
            statement
                .query_map([], |row| Ok((row.get(0)?, AddedIgnore { added_by: row.get(1)?, added_at: row.get(2)? })))?
                .collect::<rusqlite::Result<BTreeMap<String, AddedIgnore>>>()
        })?;
        Ok(Self { configured: users.into_iter().collect(), added: RwLock::new(rows), database: Some(database) })
    }

    pub fn contains(&self, user_id: &str) -> bool {
        self.configured.contains(user_id) || self.read().contains_key(user_id)
    }

    /// Starts ignoring `user_id`. `false` if they already were.
    pub fn ignore(&self, user_id: &str, added_by: &str) -> Result<bool> {
        if self.contains(user_id) {
            return Ok(false);
        }
        let entry = AddedIgnore { added_by: added_by.to_string(), added_at: chrono::Utc::now().to_rfc3339() };
        if let Some(database) = &self.database {
            database.with(|connection| {
                connection.execute(
                    "INSERT OR REPLACE INTO ignored_users (user_id, added_by, added_at) VALUES (?1, ?2, ?3)",
                    rusqlite::params![user_id, entry.added_by, entry.added_at],
                )
            })?;
        }
        self.write().insert(user_id.to_string(), entry);
        Ok(true)
    }

    pub fn unignore(&self, user_id: &str) -> Result<Unignored> {
        if self.configured.contains(user_id) {
            return Ok(Unignored::InConfig);
        }
        if !self.read().contains_key(user_id) {
            return Ok(Unignored::WasNotIgnored);
        }
        if let Some(database) = &self.database {
            database.with(|connection| connection.execute("DELETE FROM ignored_users WHERE user_id = ?1", [user_id]))?;
        }
        self.write().remove(user_id);
        Ok(Unignored::Removed)
    }

    /// Everyone ignored, configured users first, each group sorted.
    pub fn list(&self) -> Vec<IgnoredUser> {
        let mut configured: Vec<&String> = self.configured.iter().collect();
        configured.sort();
        let configured = configured
            .into_iter()
            .map(|user_id| IgnoredUser { user_id: user_id.clone(), source: "config", added_by: None, added_at: None });
        let added: Vec<IgnoredUser> = self
            .read()
            .iter()
            .filter(|(user_id, _)| !self.configured.contains(*user_id))
            .map(|(user_id, entry)| IgnoredUser {
                user_id: user_id.clone(),
                source: "runtime",
                added_by: Some(entry.added_by.clone()),
                added_at: Some(entry.added_at.clone()),
            })
            .collect();
        configured.chain(added).collect()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, AddedIgnore>> {
        self.added.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, AddedIgnore>> {
        self.added.write().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_matches_configured_users_only() {
        let ignored = IgnoredUsers::new(["@spammer:example.org".to_string(), "@troll:example.org".to_string()]);
        assert!(ignored.contains("@spammer:example.org"));
        assert!(ignored.contains("@troll:example.org"));
        assert!(!ignored.contains("@alice:example.org"));
    }

    #[test]
    fn empty_ignore_list_ignores_nobody() {
        let ignored = IgnoredUsers::default();
        assert!(!ignored.contains("@alice:example.org"));
    }

    #[test]
    fn runtime_ignores_are_saved_and_survive_reopening() {
        let database = Arc::new(Database::open_in_memory());
        let ignored = IgnoredUsers::open(Arc::clone(&database), Vec::new()).unwrap();
        assert!(ignored.ignore("@spammer:example.org", "mcp").unwrap());
        assert!(!ignored.ignore("@spammer:example.org", "mcp").unwrap());
        assert!(ignored.contains("@spammer:example.org"));

        let reopened = IgnoredUsers::open(Arc::clone(&database), Vec::new()).unwrap();
        assert!(reopened.contains("@spammer:example.org"));
        assert_eq!(reopened.unignore("@spammer:example.org").unwrap(), Unignored::Removed);
        assert!(!reopened.contains("@spammer:example.org"));
        assert!(!IgnoredUsers::open(database, Vec::new()).unwrap().contains("@spammer:example.org"));
    }

    #[test]
    fn configured_users_cant_be_unignored_at_runtime() {
        let ignored = IgnoredUsers::open(Arc::new(Database::open_in_memory()), ["@troll:example.org".to_string()]).unwrap();
        assert_eq!(ignored.unignore("@troll:example.org").unwrap(), Unignored::InConfig);
        assert_eq!(ignored.unignore("@alice:example.org").unwrap(), Unignored::WasNotIgnored);
        assert!(ignored.contains("@troll:example.org"));
        assert_eq!(ignored.list()[0].source, "config");
    }
}
