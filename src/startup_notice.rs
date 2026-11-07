//! Messages the people in `Config::notify_on_start` privately each time the bot
//! starts, so whoever runs it knows a deploy or restart went through.

use matrix_sdk::Client;
use matrix_sdk::ruma::OwnedUserId;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use tracing::{info, warn};

use crate::direct_rooms::DirectRooms;

/// Sends the notice to each user, in the private room the bot already shares with
/// them, or a new one. A failure is logged and doesn't stop the bot.
pub async fn announce(client: &Client, users: &[OwnedUserId], direct_rooms: &DirectRooms) {
    for user in users {
        let room = match client.get_dm_room(user) {
            Some(room) => room,
            None => match client.create_dm(user).await {
                Ok(room) => {
                    info!(room = %room.room_id(), %user, "created a private room for start notices");
                    room
                }
                Err(err) => {
                    warn!(?err, %user, "failed to create a private room for the start notice");
                    continue;
                }
            },
        };
        // Replies there are private conversation, like a room the user invited the
        // bot into, so they stay off the status page.
        if let Err(err) = direct_rooms.add(room.room_id().as_str(), user.as_str()) {
            warn!(?err, room = %room.room_id(), "failed to record the start notice's room as private");
        }
        match room.send(RoomMessageEventContent::text_markdown(text())).await {
            Ok(_) => info!(room = %room.room_id(), %user, "sent start notice"),
            Err(err) => warn!(?err, room = %room.room_id(), %user, "failed to send the start notice"),
        }
    }
}

fn text() -> String {
    format!("I'm up, running matrix-llm-bot {}. `!changelog` shows what's in it.", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_running_version() {
        assert!(text().contains(&format!("matrix-llm-bot {}.", env!("CARGO_PKG_VERSION"))), "{}", text());
    }
}
