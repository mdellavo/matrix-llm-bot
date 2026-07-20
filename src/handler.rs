use std::sync::Arc;
use std::time::Duration;

use anthropic_sdk::types::{ContentBlock, MessageCreateBuilder};
use anthropic_sdk::Anthropic;
use matrix_sdk::{
    Client, RoomState,
    event_handler::Ctx,
    room::Room,
    ruma::{
        UserId,
        events::Mentions,
        events::room::message::{
            MessageType, OriginalSyncRoomMessageEvent, Relation, RoomMessageEventContent,
        },
    },
};
use tracing::{debug, info, warn};

use crate::classify::{self, Intent, MessageAnalysis, classify_message};
use crate::message_log::{LoggedMessage, MessageLogger};
use crate::skills::{self, SkillRegistry};
use crate::tools::ToolClients;
use crate::usage::UsageTracker;

const UNKNOWN_COMMAND_REPLY: &str = "Unknown command. Try \"help\" to see available commands.";

/// How often to refresh the room's typing indicator while a message is being
/// processed. Matrix's typing state expires ~4s after the last notice
/// (`matrix-sdk`'s own `TYPING_NOTICE_TIMEOUT`) unless refreshed, and
/// classification plus response generation routinely take longer than that.
const TYPING_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// Keeps a room's typing indicator active for as long as it's alive, by looping
/// `Room::typing_notice(true)` in the background. Started before the first Claude
/// call in `on_room_message`; either `stop`ped explicitly once a response is ready
/// (clears the indicator immediately) or, on an early return (classification
/// failed, no response needed), just dropped — which aborts the refresh loop and
/// lets the indicator expire on its own within `TYPING_REFRESH_INTERVAL`.
struct TypingIndicator {
    refresh_task: tokio::task::JoinHandle<()>,
}

impl TypingIndicator {
    fn start(room: Room) -> Self {
        let refresh_task = tokio::spawn(async move {
            loop {
                if let Err(err) = room.typing_notice(true).await {
                    warn!(?err, room = %room.room_id(), "failed to send typing notice");
                }
                tokio::time::sleep(TYPING_REFRESH_INTERVAL).await;
            }
        });
        Self { refresh_task }
    }

    /// Stops the refresh loop and clears the indicator immediately, rather than
    /// leaving it to expire on its own.
    async fn stop(self, room: &Room) {
        self.refresh_task.abort();
        if let Err(err) = room.typing_notice(false).await {
            warn!(?err, room = %room.room_id(), "failed to clear typing notice");
        }
    }
}

impl Drop for TypingIndicator {
    fn drop(&mut self) {
        self.refresh_task.abort();
    }
}

/// Registered as a sync event handler; fires for every `m.room.message` the bot receives
/// in a room it has joined. `Ctx<Arc<Anthropic>>`, `Ctx<Arc<MessageLogger>>`,
/// `Ctx<Arc<SkillRegistry>>`, `Ctx<Arc<ToolClients>>`, and `Ctx<Arc<UsageTracker>>` are
/// injected via `Client::add_event_handler_context` in `bot.rs`.
#[allow(clippy::too_many_arguments)]
pub async fn on_room_message(
    event: OriginalSyncRoomMessageEvent,
    room: Room,
    client: Client,
    Ctx(anthropic): Ctx<Arc<Anthropic>>,
    Ctx(message_log): Ctx<Arc<MessageLogger>>,
    Ctx(skills): Ctx<Arc<SkillRegistry>>,
    Ctx(tool_clients): Ctx<Arc<ToolClients>>,
    Ctx(usage_tracker): Ctx<Arc<UsageTracker>>,
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

    let typing = TypingIndicator::start(room.clone());

    let analysis = match classify_message(
        &anthropic,
        &text_content.body,
        &skills.command_reference(),
        &usage_tracker,
    )
    .await
    {
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

    let directed_at_bot = is_directed_at_bot(&event, &room, &client).await;

    let response = generate_response(
        &room,
        &analysis,
        &text_content.body,
        event.sender.as_str(),
        directed_at_bot,
        &anthropic,
        &message_log,
        &skills,
        &tool_clients,
        &usage_tracker,
    )
    .await;

    typing.stop(&room).await;

    match response {
        Some(response) => {
            if let Err(err) = send_message(&room, &event.sender, &response).await {
                warn!(?err, room = %room.room_id(), "failed to send response");
            }
        }
        None => debug!(room = %room.room_id(), "no response generated"),
    }
}

/// Whether `event` explicitly addresses the bot: either a Matrix `m.mentions`
/// pingback naming its user ID, or a reply to one of the bot's own prior
/// messages. This is Matrix-native addressing data the classifier never sees
/// (`classify_message` only gets the message body) — used to keep the bot from
/// jumping into ambient conversation between humans that merely sounds like it
/// wants a reply. Cheap in the common case: the mention check is free, and the
/// reply check only does a network fetch when the message is actually a reply.
async fn is_directed_at_bot(event: &OriginalSyncRoomMessageEvent, room: &Room, client: &Client) -> bool {
    let Some(own_user_id) = client.user_id() else {
        return false;
    };

    if let Some(mentions) = &event.content.mentions
        && mentions.user_ids.contains(own_user_id)
    {
        return true;
    }

    let Some(Relation::Reply(reply)) = &event.content.relates_to else {
        return false;
    };

    match room.event(&reply.in_reply_to.event_id, None).await {
        Ok(replied_to) => replied_to.kind.sender().as_deref() == Some(own_user_id),
        Err(err) => {
            warn!(?err, room = %room.room_id(), "failed to fetch replied-to event while checking bot addressing");
            false
        }
    }
}

/// Produces a response to a classified message.
///
/// `Intent::Command` is only dispatched to a loaded skill (or the built-in `help`,
/// or an "unknown command" reply — see `handle_command`) when `message_text` itself
/// starts with `!`: the classifier can recognize command *intent* from natural
/// language alone ("strain gsc" as much as "!strain gsc"), but running a skill off
/// of that guess is too easy to trigger by accident in ordinary chat, so the literal
/// prefix is required as a deterministic, LLM-independent gate on top of it. A
/// message classified as a command but missing the prefix falls through to the
/// same handling as any other intent, below.
///
/// Every other intent (and a would-be command without its `!`) gets a real
/// Claude-generated conversational reply (see `generate_chat_reply`), but only
/// when `directed_at_bot` is true (an explicit `@mention` or a reply to one of the
/// bot's own messages — see `is_directed_at_bot`); otherwise the bot stays quiet
/// rather than jumping into ambient conversation between humans. `Acknowledgement`
/// never gets a reply either way — "ok"/"thanks"/"lol" doesn't need one.
#[allow(clippy::too_many_arguments)]
async fn generate_response(
    room: &Room,
    analysis: &MessageAnalysis,
    message_text: &str,
    sender: &str,
    directed_at_bot: bool,
    anthropic: &Anthropic,
    message_log: &MessageLogger,
    skills: &SkillRegistry,
    tool_clients: &ToolClients,
    usage_tracker: &UsageTracker,
) -> Option<String> {
    if analysis.intent == Intent::Command && message_text.trim_start().starts_with('!') {
        return Some(
            handle_command(
                room,
                analysis,
                message_text,
                anthropic,
                message_log,
                skills,
                tool_clients,
                usage_tracker,
            )
            .await,
        );
    }

    if analysis.intent == Intent::Acknowledgement || !directed_at_bot {
        return None;
    }

    Some(generate_chat_reply(room, message_text, sender, anthropic, message_log, usage_tracker).await)
}

/// Number of recent room messages fed into `generate_chat_reply` as context, so
/// its replies can call back to what people actually said — running jokes, a
/// specific user's own history in the room — instead of replying in a vacuum.
const CHAT_HISTORY_LIMIT: usize = 20;

/// System prompt for the bot's free-form conversational replies (as opposed to a
/// skill's prompt or the classifier's structured tool call) — informative and
/// helpful first, with a light, mildly snarky sense of humor woven in, rather
/// than a formal-assistant tone or (an earlier, dialed-back version of this
/// prompt) a mean/dark roasting personality.
const CHAT_SYSTEM_PROMPT: &str = "You are a member of this Matrix chat room replying to a message that \
was directly addressed to you (an @mention or a reply to one of your own messages). You are not a formal \
assistant, but you're not the room's resident troll either — be genuinely informative and helpful first, \
with a light, mildly snarky sense of humor woven in: a dry aside, a wry observation, occasional teasing. \
Actually answer what was asked or respond to what was said; the humor is seasoning, not the whole reply. \
You'll be given the room's recent chat history (sender and message, oldest first) before the message \
you're replying to — use it: call back to what people said earlier, running jokes, a specific user's own \
history in the room, rather than replying in a vacuum.\n\n\
Keep it good-natured: no targeting protected traits, no genuinely cutting remarks meant to actually hurt \
someone, no inventing personal details about someone that aren't in the history you were given.\n\n\
Keep it brief (a sentence or two) unless the message clearly calls for more.";

/// Label Claude API calls from `generate_chat_reply` are recorded under in `UsageTracker`.
const CHAT_USAGE_LABEL: &str = "chat";

/// Generates a free-form conversational reply to a message directed at the bot,
/// grounded in the room's recent history (`MessageLogger::recent`) and who sent
/// the message, using the same default model as the classifier (`classify::MODEL`)
/// — see `generate_response` and `CHAT_SYSTEM_PROMPT`.
async fn generate_chat_reply(
    room: &Room,
    message_text: &str,
    sender: &str,
    anthropic: &Anthropic,
    message_log: &MessageLogger,
    usage_tracker: &UsageTracker,
) -> String {
    // Fetch one extra entry and drop it if it's this same message: `on_room_message`
    // already logged the current message before calling this, so `recent` would
    // otherwise include it a second time (once in the history block, once as "the
    // message to reply to").
    let mut history = match message_log.recent(room.room_id().as_str(), CHAT_HISTORY_LIMIT + 1) {
        Ok(history) => history,
        Err(err) => {
            warn!(?err, room = %room.room_id(), "failed to load chat history for chat reply; replying without it");
            Vec::new()
        }
    };
    if matches!(history.last(), Some(last) if last.sender == sender && last.body == message_text) {
        history.pop();
    }

    let user_turn = format_chat_turn(&history, sender, message_text);

    let params = MessageCreateBuilder::new(classify::MODEL, 512)
        .system(CHAT_SYSTEM_PROMPT)
        .user(user_turn)
        .build();

    let message = match anthropic.messages().create(params).await {
        Ok(message) => message,
        Err(err) => {
            warn!(?err, "chat reply request to Claude failed");
            return "Sorry, I couldn't come up with a reply to that.".to_string();
        }
    };

    usage_tracker.record(CHAT_USAGE_LABEL, &message.usage);

    for block in message.content {
        if let ContentBlock::Text { text } = block {
            return text;
        }
    }

    "Sorry, I couldn't come up with a reply to that.".to_string()
}

/// Renders recent room history plus the message being replied to into the single
/// user-turn string sent to Claude: one `sender: body` line per historical
/// message (oldest first), then the current message and who sent it.
fn format_chat_turn(history: &[LoggedMessage], sender: &str, message_text: &str) -> String {
    let mut turn = String::new();
    if !history.is_empty() {
        turn.push_str("Recent room history:\n");
        for entry in history {
            turn.push_str(&format!("{}: {}\n", entry.sender, entry.body));
        }
        turn.push('\n');
    }
    turn.push_str(&format!("Message to reply to, from {sender}:\n{message_text}"));
    turn
}

/// Dispatches a `Command`-intent message to the built-in `help` listing, a loaded
/// skill (`skills::execute` — sends the skill's prompt to Claude), or a friendly
/// "unknown command" reply if `command.name` is missing or unrecognized.
#[allow(clippy::too_many_arguments)]
async fn handle_command(
    room: &Room,
    analysis: &MessageAnalysis,
    message_text: &str,
    anthropic: &Anthropic,
    message_log: &MessageLogger,
    skills: &SkillRegistry,
    tool_clients: &ToolClients,
    usage_tracker: &UsageTracker,
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
        Some(skill) => {
            skills::execute(
                anthropic,
                message_log,
                tool_clients,
                usage_tracker,
                room,
                skill,
                message_text,
                command,
            )
            .await
        }
        None => UNKNOWN_COMMAND_REPLY.to_string(),
    }
}

/// Lists every loaded skill's name, usage (if any), aliases (if any), and
/// description, plus the built-in `help` entry itself.
fn help_reply(skills: &SkillRegistry) -> String {
    let mut reply = String::from("Available commands:\n- help: List available commands.\n");
    for skill in skills.list() {
        let usage = skill
            .usage
            .as_deref()
            .map(|usage| format!(" ({usage})"))
            .unwrap_or_default();
        let aliases = if skill.aliases.is_empty() {
            String::new()
        } else {
            format!(" [aka: {}]", skill.aliases.join(", "))
        };
        reply.push_str(&format!(
            "- {}{usage}{aliases}: {}\n",
            skill.name, skill.description
        ));
    }
    reply
}

/// Sends a message into `room`, rendering Markdown (skill prompts routinely ask
/// Claude for `**bold**`/`# headings`/`![images](url)`) into a proper HTML
/// `formatted_body` — plain `text_plain` would send that literal markdown syntax
/// as unrendered text, which is exactly what most Matrix clients show it as.
/// Sends `body` into `room` as a reply directed at `sender` — the user whose
/// message triggered this response. Prepends an `@`-mention (a Markdown link to
/// the user's `matrix.to` URI, which clients render as a highlighted "pill") and
/// sets the Matrix intentional-mention field (`m.mentions`) so it actually
/// notifies them, not just names them in the text.
pub async fn send_message(room: &Room, sender: &UserId, body: &str) -> matrix_sdk::Result<()> {
    let body_with_mention = format!("[{sender}](https://matrix.to/#/{sender}): {body}");
    let content =
        RoomMessageEventContent::text_markdown(body_with_mention).add_mentions(Mentions::with_user_ids([sender.to_owned()]));
    room.send(content).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{Intent, Sentiment};

    fn logged_message(sender: &str, body: &str) -> LoggedMessage {
        LoggedMessage {
            logged_at: "2026-01-01T00:00:00Z".to_string(),
            room_id: "!room:example.org".to_string(),
            event_id: "$event".to_string(),
            sender: sender.to_string(),
            origin_server_ts_ms: 0,
            body: body.to_string(),
            analysis: MessageAnalysis {
                intent: Intent::Chitchat,
                confidence: 0.9,
                requires_response: false,
                summary: "test".to_string(),
                sentiment: Sentiment::Neutral,
                entities: vec![],
                command: None,
            },
        }
    }

    #[test]
    fn format_chat_turn_includes_history_before_the_current_message() {
        let history = vec![logged_message("@alice:example.org", "hi everyone"), logged_message("@bob:example.org", "yo")];
        let turn = format_chat_turn(&history, "@carol:example.org", "what's up");

        let history_pos = turn.find("Recent room history:").expect("history header present");
        let alice_pos = turn.find("@alice:example.org: hi everyone").expect("alice line present");
        let bob_pos = turn.find("@bob:example.org: yo").expect("bob line present");
        let current_pos = turn.find("Message to reply to, from @carol:example.org:\nwhat's up").expect("current message present");

        assert!(history_pos < alice_pos, "{turn}");
        assert!(alice_pos < bob_pos, "{turn}");
        assert!(bob_pos < current_pos, "{turn}");
    }

    #[test]
    fn format_chat_turn_omits_history_header_when_empty() {
        let turn = format_chat_turn(&[], "@carol:example.org", "hello");
        assert!(!turn.contains("Recent room history:"), "{turn}");
        assert_eq!(turn, "Message to reply to, from @carol:example.org:\nhello");
    }
}
