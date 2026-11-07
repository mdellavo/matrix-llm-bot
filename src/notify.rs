//! Private messages to the people in `Config::notify_users`: the start notice, and
//! alerts about things that need someone's attention while the bot runs.
//!
//! Alerts are raised from code that can't wait on Matrix, like `UsageTracker::record`,
//! so they go into a queue that one task delivers once the bot has synced.

use std::time::Duration;

use matrix_sdk::Client;
use matrix_sdk::Room;
use matrix_sdk::ruma::OwnedUserId;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::direct_rooms::DirectRooms;

/// How long a final alert, sent as the bot shuts down, may take.
const LAST_ALERT_TIMEOUT: Duration = Duration::from_secs(10);

/// Queues alerts for delivery. Cheap to clone, and never blocks.
#[derive(Debug, Clone)]
pub struct Alerts(mpsc::UnboundedSender<String>);

impl Alerts {
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<String>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Self(sender), receiver)
    }

    pub fn raise(&self, text: String) {
        warn!(alert = %text, "raising an alert");
        // Only fails once the delivery task is gone, at shutdown.
        let _ = self.0.send(text);
    }
}

/// Delivers queued alerts to every user, in order, until the queue closes.
pub async fn deliver(mut alerts: mpsc::UnboundedReceiver<String>, client: Client, users: Vec<OwnedUserId>, direct_rooms: std::sync::Arc<DirectRooms>) {
    while let Some(text) = alerts.recv().await {
        send_to_all(&client, &users, &direct_rooms, &format!("⚠️ {text}")).await;
    }
}

/// Sends one alert straight away, for when the bot is about to stop and the queue
/// won't be delivered. Gives up after `LAST_ALERT_TIMEOUT`.
pub async fn last_alert(client: &Client, users: &[OwnedUserId], direct_rooms: &DirectRooms, text: &str) {
    let text = format!("⚠️ {text}");
    if tokio::time::timeout(LAST_ALERT_TIMEOUT, send_to_all(client, users, direct_rooms, &text)).await.is_err() {
        warn!("gave up sending the last alert");
    }
}

async fn send_to_all(client: &Client, users: &[OwnedUserId], direct_rooms: &DirectRooms, text: &str) {
    for user in users {
        let Some(room) = private_room(client, user, direct_rooms).await else { continue };
        match room.send(RoomMessageEventContent::text_markdown(text)).await {
            Ok(_) => info!(room = %room.room_id(), %user, "sent alert"),
            Err(err) => warn!(?err, room = %room.room_id(), %user, "failed to send an alert"),
        }
    }
}

/// The private room the bot shares with `user`, or a new one. Either way it's
/// recorded as a private conversation, like a room the user invited the bot into, so
/// replies there stay off the status page.
pub async fn private_room(client: &Client, user: &OwnedUserId, direct_rooms: &DirectRooms) -> Option<Room> {
    let room = match client.get_dm_room(user) {
        Some(room) => room,
        None => match client.create_dm(user).await {
            Ok(room) => {
                info!(room = %room.room_id(), %user, "created a private room for notices");
                room
            }
            Err(err) => {
                warn!(?err, %user, "failed to create a private room for notices");
                return None;
            }
        },
    };
    if let Err(err) = direct_rooms.add(room.room_id().as_str(), user.as_str()) {
        warn!(?err, room = %room.room_id(), "failed to record the notice room as private");
    }
    Some(room)
}
