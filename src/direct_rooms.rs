//! Private conversations: rooms the bot was invited into, rather than rooms it was
//! configured to join. Every message there is for the bot, so who may start one
//! matters: each reply is a paid Claude call, and anyone on a federated server can
//! send an invite.

use std::sync::Arc;

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::db::Database;

pub struct DirectRooms {
    database: Arc<Database>,
    /// See `Config::dm_allowed_users`.
    allowed: Vec<String>,
}

impl DirectRooms {
    pub fn new(database: Arc<Database>, allowed: Vec<String>) -> Self {
        Self { database, allowed }
    }

    /// Whether `user_id` may start a conversation. Nothing configured means the bot's
    /// own homeserver only. Servers match exactly, so `:example.org` doesn't let in
    /// `evil-example.org`.
    pub fn allows(&self, user_id: &str, own_server: &str) -> bool {
        let server = user_id.split_once(':').map_or("", |(_, server)| server);
        if self.allowed.is_empty() {
            return server == own_server;
        }
        self.allowed.iter().any(|entry| entry == user_id || entry.strip_prefix(':') == Some(server))
    }

    pub fn add(&self, room_id: &str, invited_by: &str) -> Result<()> {
        let joined_at = chrono::Utc::now().to_rfc3339();
        self.database.with(|connection| {
            connection.execute(
                "INSERT OR IGNORE INTO direct_rooms (room_id, invited_by, joined_at) VALUES (?1, ?2, ?3)",
                params![room_id, invited_by, joined_at],
            )
        })?;
        Ok(())
    }

    pub fn contains(&self, room_id: &str) -> Result<bool> {
        self.database.with(|connection| {
            connection
                .query_row("SELECT 1 FROM direct_rooms WHERE room_id = ?1", [room_id], |_| Ok(()))
                .optional()
                .map(|row| row.is_some())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rooms(allowed: &[&str]) -> DirectRooms {
        DirectRooms::new(Arc::new(Database::open_in_memory()), allowed.iter().map(ToString::to_string).collect())
    }

    #[test]
    fn with_nothing_configured_only_the_bots_own_server_may_start_one() {
        let rooms = rooms(&[]);
        assert!(rooms.allows("@alice:p.ound.town", "p.ound.town"));
        assert!(!rooms.allows("@mallory:matrix.org", "p.ound.town"));
    }

    #[test]
    fn configured_users_and_servers_replace_the_default() {
        let rooms = rooms(&["@alice:matrix.org", ":example.org"]);
        assert!(rooms.allows("@alice:matrix.org", "p.ound.town"));
        assert!(rooms.allows("@bob:example.org", "p.ound.town"));
        assert!(!rooms.allows("@mallory:matrix.org", "p.ound.town"));
        assert!(!rooms.allows("@carol:p.ound.town", "p.ound.town"));
    }

    #[test]
    fn a_server_entry_is_not_a_suffix_match() {
        let rooms = rooms(&[":example.org"]);
        assert!(!rooms.allows("@mallory:evil-example.org", "p.ound.town"));
        assert!(!rooms.allows("@mallory:example.org.evil.com", "p.ound.town"));
    }

    #[test]
    fn only_rooms_it_was_invited_into_count() {
        let rooms = rooms(&[]);
        rooms.add("!dm:x", "@alice:x").unwrap();
        rooms.add("!dm:x", "@alice:x").unwrap();
        assert!(rooms.contains("!dm:x").unwrap());
        assert!(!rooms.contains("!pound:x").unwrap());
    }
}
