use std::sync::Arc;

use anthropic_sdk::Anthropic;
use matrix_sdk::{
    Client, RoomState,
    event_handler::Ctx,
    room::Room,
    ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
};
use tracing::{debug, info, warn};

use crate::classify::{classify_message, Intent, MessageAnalysis};
use crate::message_log::MessageLogger;
use crate::skills::{self, SkillRegistry};

const UNKNOWN_COMMAND_REPLY: &str = "Unknown command. Try \"help\" to see available commands.";

/// Registered as a sync event handler; fires for every `m.room.message` the bot receives
/// in a room it has joined. `Ctx<Arc<Anthropic>>`, `Ctx<Arc<MessageLogger>>`, and
/// `Ctx<Arc<SkillRegistry>>` are injected via `Client::add_event_handler_context` in
/// `bot.rs`.
pub async fn on_room_message(
    event: OriginalSyncRoomMessageEvent,
    room: Room,
    client: Client,
    Ctx(anthropic): Ctx<Arc<Anthropic>>,
    Ctx(message_log): Ctx<Arc<MessageLogger>>,
    Ctx(skills): Ctx<Arc<SkillRegistry>>,
) {
    if room.state() != RoomState::Joined {
        return;
    }

    // Never respond to our own messages.
    if Some(event.sender.as_ref()) == client.user_id() {
        return;
    }

    let MessageType::Text(text_content) = &event.content.msgtype else {
        debug!(room = %room.room_id(), "ignoring non-text message");
        return;
    };

    info!(room = %room.room_id(), sender = %event.sender, "received message");

    let analysis = match classify_message(&anthropic, &text_content.body).await {
        Ok(analysis) => analysis,
        Err(err) => {
            warn!(?err, room = %room.room_id(), "failed to classify message");
            return;
        }
    };

    debug!(
        room = %room.room_id(),
        intent = ?analysis.intent,
        confidence = analysis.confidence,
        requires_response = analysis.requires_response,
        "classified message"
    );

    if let Err(err) = message_log.log(
        room.room_id().as_str(),
        event.event_id.as_str(),
        event.sender.as_str(),
        event.origin_server_ts.get().into(),
        &text_content.body,
        &analysis,
    ) {
        warn!(?err, room = %room.room_id(), "failed to write message log entry");
    }

    if !analysis.requires_response {
        return;
    }

    let response = generate_response(
        &room,
        &analysis,
        &text_content.body,
        &anthropic,
        &message_log,
        &skills,
    )
    .await;

    match response {
        Some(response) => {
            if let Err(err) = send_message(&room, &response).await {
                warn!(?err, room = %room.room_id(), "failed to send response");
            }
        }
        None => debug!(room = %room.room_id(), "no response generated"),
    }
}

/// Produces a response to a classified message.
///
/// `Intent::Command` is dispatched to a loaded skill (or the built-in `help`, or an
/// "unknown command" reply) — see `handle_command`. Every other intent still picks a
/// canned reply based on `analysis.intent` rather than actually generating one.
///
/// TODO: plug in LLM-generated responses for non-command intents.
async fn generate_response(
    room: &Room,
    analysis: &MessageAnalysis,
    message_text: &str,
    anthropic: &Anthropic,
    message_log: &MessageLogger,
    skills: &SkillRegistry,
) -> Option<String> {
    if analysis.intent == Intent::Command {
        return Some(handle_command(room, analysis, message_text, anthropic, message_log, skills).await);
    }

    let reply = match analysis.intent {
        Intent::Greeting => "Hello!".to_string(),
        Intent::Farewell => "Goodbye!".to_string(),
        Intent::Acknowledgement => return None,
        _ => format!("(stub) noted: {}", analysis.summary),
    };
    Some(reply)
}

/// Dispatches a `Command`-intent message to the built-in `help` listing, a loaded
/// skill (`skills::execute` — sends the skill's prompt to Claude), or a friendly
/// "unknown command" reply if `command.name` is missing or unrecognized.
async fn handle_command(
    room: &Room,
    analysis: &MessageAnalysis,
    message_text: &str,
    anthropic: &Anthropic,
    message_log: &MessageLogger,
    skills: &SkillRegistry,
) -> String {
    let Some(command) = &analysis.command else {
        return UNKNOWN_COMMAND_REPLY.to_string();
    };
    let Some(name) = command.name.as_deref() else {
        return UNKNOWN_COMMAND_REPLY.to_string();
    };

    if name.eq_ignore_ascii_case("help") {
        return help_reply(skills);
    }

    match skills.get(name) {
        Some(skill) => skills::execute(anthropic, message_log, room, skill, message_text, command).await,
        None => UNKNOWN_COMMAND_REPLY.to_string(),
    }
}

/// Lists every loaded skill's name, usage (if any), aliases (if any), and
/// description, plus the built-in `help` entry itself.
fn help_reply(skills: &SkillRegistry) -> String {
    let mut reply = String::from("Available commands:\n- help: List available commands.\n");
    for skill in skills.list() {
        let usage = skill.usage.as_deref().map(|usage| format!(" ({usage})")).unwrap_or_default();
        let aliases = if skill.aliases.is_empty() {
            String::new()
        } else {
            format!(" [aka: {}]", skill.aliases.join(", "))
        };
        reply.push_str(&format!("- {}{usage}{aliases}: {}\n", skill.name, skill.description));
    }
    reply
}

/// Sends a plain-text message into `room`.
pub async fn send_message(room: &Room, body: &str) -> matrix_sdk::Result<()> {
    room.send(RoomMessageEventContent::text_plain(body)).await?;
    Ok(())
}
