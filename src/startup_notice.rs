//! Messages the people in `Config::notify_users` privately each time the bot
//! starts, with what's new since the last notice they got, so whoever runs it knows a
//! deploy or restart went through and what it brought.

use matrix_sdk::Client;
use matrix_sdk::ruma::OwnedUserId;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use rusqlite::{OptionalExtension, params};
use tracing::{info, warn};

use crate::db::Database;
use crate::direct_rooms::DirectRooms;

/// Changelog lines listed before the rest are left to `!changelog`.
const MAX_NEW_LINES: usize = 12;

/// Sends the notice to each user, in the private room the bot already shares with
/// them, or a new one. A failure is logged and doesn't stop the bot.
pub async fn announce(client: &Client, users: &[OwnedUserId], direct_rooms: &DirectRooms, database: &Database) {
    let (version, notes) = crate::changelog::current();
    for user in users {
        let Some(room) = crate::notify::private_room(client, user, direct_rooms).await else { continue };

        let seen = last_seen(database, user.as_str()).unwrap_or_else(|err| {
            warn!(?err, %user, "failed to read the last start notice; treating it as the first");
            None
        });
        let new = crate::changelog::new_since(seen.as_ref().map(|(version, notes)| (version.as_str(), notes.as_str())));
        match room.send(RoomMessageEventContent::text_markdown(text(version, &new))).await {
            Ok(_) => {
                info!(room = %room.room_id(), %user, new = new.len(), "sent start notice");
                if let Err(err) = remember(database, user.as_str(), version, notes) {
                    warn!(?err, %user, "failed to record the start notice; the next one will repeat what's new");
                }
            }
            Err(err) => warn!(?err, room = %room.room_id(), %user, "failed to send the start notice"),
        }
    }
}

fn text(version: &str, new: &[&str]) -> String {
    if new.is_empty() {
        return format!("I'm up, running matrix-llm-bot {version}. Nothing's new since last time.");
    }
    let mut text = format!("I'm up, running matrix-llm-bot {version}. New since last time:\n");
    for line in new.iter().take(MAX_NEW_LINES) {
        text.push_str(&format!("- {line}\n"));
    }
    if new.len() > MAX_NEW_LINES {
        text.push_str(&format!("\n…and {} more. `!changelog` has the rest.\n", new.len() - MAX_NEW_LINES));
    }
    text.trim_end().to_string()
}

/// The version and notes the user's last start notice was sent with.
fn last_seen(database: &Database, user_id: &str) -> anyhow::Result<Option<(String, String)>> {
    database.with(|connection| {
        connection
            .query_row("SELECT version, notes FROM start_notices WHERE user_id = ?1", [user_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()
    })
}

fn remember(database: &Database, user_id: &str, version: &str, notes: &str) -> anyhow::Result<()> {
    let sent_at = chrono::Utc::now().to_rfc3339();
    database.with(|connection| {
        connection.execute(
            "INSERT INTO start_notices (user_id, version, notes, sent_at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (user_id) DO UPDATE SET version = excluded.version, notes = excluded.notes, sent_at = excluded.sent_at",
            params![user_id, version, notes, sent_at],
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_whats_new_or_says_nothing_is() {
        assert_eq!(text("0.16.0", &[]), "I'm up, running matrix-llm-bot 0.16.0. Nothing's new since last time.");
        assert_eq!(
            text("0.16.0", &["`!stats`: numbers", "Faster replies"]),
            "I'm up, running matrix-llm-bot 0.16.0. New since last time:\n- `!stats`: numbers\n- Faster replies"
        );
        let many: Vec<String> = (1..=15).map(|n| format!("Change {n}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let long = text("0.16.0", &many);
        assert!(long.contains("- Change 12\n\n…and 3 more. `!changelog` has the rest."), "{long}");
        assert!(!long.contains("Change 13"), "{long}");
    }

    #[test]
    fn remembers_the_last_notice_per_user() {
        let database = Database::open_in_memory();
        assert_eq!(last_seen(&database, "@a:x").unwrap(), None);
        remember(&database, "@a:x", "0.1.0", "- One").unwrap();
        remember(&database, "@a:x", "0.2.0", "- Two").unwrap();
        assert_eq!(last_seen(&database, "@a:x").unwrap(), Some(("0.2.0".to_string(), "- Two".to_string())));
        assert_eq!(last_seen(&database, "@b:x").unwrap(), None);
    }
}
