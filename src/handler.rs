use std::sync::Arc;
use std::time::Duration;

use matrix_sdk::{
    Client, RoomState,
    event_handler::Ctx,
    room::Room,
    ruma::{
        EventId, UserId,
        events::room::message::{
            AddMentions, ForwardThread, MessageType, OriginalSyncRoomMessageEvent, Relation, ReplyMetadata,
            RoomMessageEventContent,
        },
    },
};
use threatflux_anthropic_sdk::Client as Anthropic;
use tracing::{debug, info, warn};

use crate::classify::{ClassifyOutcome, Intent, MessageAnalysis, classify_message};
use crate::greeting::GreetingCooldown;
use crate::ignore::IgnoredUsers;
use crate::llm::{self, CompletionRequest};
use crate::message_log::{
    self, GeneratedReply, LoggedMessage, MessageLogParams, MessageLogger, PromptRecord, short_name,
};
use crate::room_memory::RoomSummaries;
use crate::skills::{self, SkillRegistry};
use crate::throttle::{RATE_LIMITED_REPLY, Throttle};
use crate::tools::ToolClients;
use crate::usage::{COST_LIMIT_REPLY, UsageTracker};

const UNKNOWN_COMMAND_REPLY: &str = "Unknown command. Try \"!help\" to see available commands.";

const GENERATION_FAILED_REPLY: &str = "Sorry, I couldn't come up with a reply to that.";

/// A `!command` split into its name and the text after it. Commands are parsed here
/// rather than by the classifier, so they work the same every time and still work
/// when the classifier is down.
#[derive(Debug, PartialEq, Eq)]
struct ParsedCommand<'a> {
    name: &'a str,
    args: &'a str,
}

fn parse_command(message_text: &str) -> Option<ParsedCommand<'_>> {
    let rest = message_text.trim_start().strip_prefix('!')?;
    if !rest.starts_with(|c: char| c.is_alphanumeric()) {
        return None;
    }
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Some(ParsedCommand { name, args: args.trim() })
}

/// How the bot should answer a non-command message, if at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyKind {
    Chat,
    Greeting,
}

/// Addressing comes first: a message aimed at the bot gets answered whatever the
/// classifier made of it, because the classifier can't tell it was addressed. The
/// one exception is a bare acknowledgement ("thanks", "lol"), which ends a
/// conversation rather than continuing it. Greetings to the room are the only thing
/// answered unprompted, limited by the room's `GreetingCooldown`.
fn reply_kind(analysis: &MessageAnalysis, directed_at_bot: bool, greeting_allowed: bool) -> Option<ReplyKind> {
    if directed_at_bot {
        return (analysis.intent != Intent::Acknowledgement).then_some(ReplyKind::Chat);
    }
    (analysis.intent == Intent::Greeting && analysis.requires_response && greeting_allowed).then_some(ReplyKind::Greeting)
}

/// Whether plain text addresses the bot by name: `gordy: hi`, `gordy, hi`,
/// `@gordy hi`, `gordy what time is it`. A bare `gordy:` with nothing after it isn't
/// a message, and `gordys` or `gordy's` is a different word.
fn addresses_bot_by_name(message_text: &str, bot_name: &str) -> bool {
    let text = message_text.trim_start();
    let text = text.strip_prefix('@').unwrap_or(text);

    let Some(prefix) = text.get(..bot_name.len()) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case(bot_name) {
        return false;
    }

    let rest = &text[bot_name.len()..];
    let Some(next) = rest.chars().next() else {
        return false;
    };
    let separated = next.is_whitespace() || next == ':' || next == ',';
    separated && rest.chars().any(|c| c.is_alphanumeric())
}

/// Drops the `> <@user> quoted text` block older clients put at the top of a reply's
/// body. The quote is passed separately as `RepliedTo`, and leaving it in would make
/// the message look like it says something it doesn't.
fn strip_reply_fallback(body: &str) -> &str {
    if !body.starts_with("> ") {
        return body;
    }
    match body.split_once("\n\n") {
        Some((quote, rest)) if quote.lines().all(|line| line.starts_with('>')) => rest,
        _ => body,
    }
}

/// The message a reply points at.
struct RepliedTo {
    sender: String,
    body: Option<String>,
}

/// Fetches the message `event` replies to, if it is a reply.
async fn replied_to(event: &OriginalSyncRoomMessageEvent, room: &Room) -> Option<RepliedTo> {
    let Some(Relation::Reply(reply)) = &event.content.relates_to else {
        return None;
    };

    match room.event(&reply.in_reply_to.event_id, None).await {
        Ok(target) => {
            let sender = target.kind.sender()?.to_string();
            let content = target.kind.raw().get_field::<serde_json::Value>("content").ok().flatten();
            let body = content.and_then(|content| content["body"].as_str().map(String::from));
            Some(RepliedTo { sender, body })
        }
        Err(err) => {
            warn!(?err, room = %room.room_id(), "failed to fetch replied-to event");
            None
        }
    }
}

/// How often to refresh the room's typing indicator while a response is being
/// generated. Matrix's typing state expires ~4s after the last notice
/// (`matrix-sdk`'s own `TYPING_NOTICE_TIMEOUT`) unless refreshed, and a skill or
/// chat-reply Claude call routinely takes longer than that.
const TYPING_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// Keeps a room's typing indicator active for as long as it's alive, by looping
/// `Room::typing_notice(true)` in the background. `on_room_message` only starts
/// one once a reply is actually coming — not for every message it sees — then
/// `stop`s it explicitly once the response is ready (clearing the indicator immediately). If a
/// `TypingIndicator` is ever dropped without `stop` being called, that still
/// aborts the refresh loop and lets the indicator expire on its own within
/// `TYPING_REFRESH_INTERVAL`, rather than looping forever.
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

/// Every piece of state `on_room_message` needs beyond the event/room/client
/// themselves, bundled into a single `Ctx` extractor. matrix-sdk's
/// `EventHandler` trait is only implemented for handlers with up to 8 total
/// non-event parameters (room, client, and every separate `Ctx<T>` combined —
/// see `impl_event_handler!` in matrix-sdk's `event_handler` module); one
/// `Ctx<T>` per resource ran out of budget once `GreetingCooldown` and
/// `IgnoredUsers` were added, so everything is bundled here instead, leaving
/// room to add more without hitting that ceiling again.
#[derive(Clone)]
pub struct HandlerContext {
    pub anthropic: Arc<Anthropic>,
    pub message_log: Arc<MessageLogger>,
    pub skills: Arc<SkillRegistry>,
    pub tool_clients: Arc<ToolClients>,
    pub usage_tracker: Arc<UsageTracker>,
    pub greeting_cooldown: Arc<GreetingCooldown>,
    pub ignored_users: Arc<IgnoredUsers>,
    pub rate_limit: Arc<Throttle>,
    pub room_summaries: Arc<RoomSummaries>,
    /// The bot's short name (`gordy`): what it's called in prompts and transcripts,
    /// and what people type to address it.
    pub bot_name: String,
}

/// Registered as a sync event handler; fires for every `m.room.message` the bot
/// receives in a room it has joined. `Ctx<Arc<HandlerContext>>` is injected via
/// `Client::add_event_handler_context` in `bot.rs`.
pub async fn on_room_message(event: OriginalSyncRoomMessageEvent, room: Room, client: Client, Ctx(ctx): Ctx<Arc<HandlerContext>>) {
    if room.state() != RoomState::Joined {
        return;
    }

    // Never respond to our own messages.
    if Some(event.sender.as_ref()) == client.user_id() {
        return;
    }

    // Ignored users are dropped before anything else — no classification, no
    // logging, no reply — so they never show up in message history either.
    if ctx.ignored_users.contains(event.sender.as_str()) {
        debug!(room = %room.room_id(), sender = %event.sender, "ignoring message from ignored user");
        return;
    }

    let MessageType::Text(text_content) = &event.content.msgtype else {
        debug!(room = %room.room_id(), "ignoring non-text message");
        return;
    };

    info!(room = %room.room_id(), sender = %event.sender, "received message");

    let reply_target = replied_to(&event, &room).await;
    let body = if reply_target.is_some() { strip_reply_fallback(&text_content.body) } else { text_content.body.as_str() };

    let command = parse_command(body);
    let mentioned = client.user_id().is_some_and(|own_id| {
        event.content.mentions.as_ref().is_some_and(|mentions| mentions.user_ids.contains(own_id))
    });
    let replied_to_bot = reply_target
        .as_ref()
        .is_some_and(|target| client.user_id().is_some_and(|own_id| own_id.as_str() == target.sender));
    let directed_at_bot = mentioned || replied_to_bot || addresses_bot_by_name(body, &ctx.bot_name);

    // Started before classification when a reply is already certain, so the wait for
    // the classifier shows as typing too.
    let mut typing = (command.is_some() || directed_at_bot).then(|| TypingIndicator::start(room.clone()));

    let classification = match command {
        Some(_) => None,
        None => classify(&ctx, &room, body, event.sender.as_str()).await,
    };
    let analysis = match &classification {
        Some(outcome) => outcome.analysis.clone(),
        None if command.is_some() => MessageAnalysis::unclassified(Intent::Command, "A bot command."),
        None => MessageAnalysis::unclassified(Intent::Other, "Not classified."),
    };

    debug!(
        room = %room.room_id(),
        intent = ?analysis.intent,
        confidence = analysis.confidence,
        requires_response = analysis.requires_response,
        "classified message"
    );

    // The greeting cooldown is only consulted for a greeting that isn't addressed to
    // the bot: `try_greet` consumes the cooldown, and a direct message shouldn't.
    let greeting_allowed = !directed_at_bot
        && analysis.intent == Intent::Greeting
        && analysis.requires_response
        && ctx.greeting_cooldown.try_greet(room.room_id().as_str());
    let kind = reply_kind(&analysis, directed_at_bot, greeting_allowed);

    if kind.is_some() && typing.is_none() {
        typing = Some(TypingIndicator::start(room.clone()));
    }

    let response = match (&command, kind) {
        (Some(command), _) => Some(handle_command(&ctx, &room, command, body).await),
        (None, Some(kind)) => {
            Some(generate_grounded_reply(&ctx, &room, kind, body, event.sender.as_str(), reply_target.as_ref()).await)
        }
        (None, None) => None,
    };

    if let Some(typing) = typing {
        typing.stop(&room).await;
    }

    if let Err(err) = ctx.message_log.log(MessageLogParams {
        room_id: room.room_id().as_str(),
        event_id: event.event_id.as_str(),
        sender: event.sender.as_str(),
        origin_server_ts_ms: event.origin_server_ts.get().into(),
        body,
        analysis: &analysis,
        classify_prompt: classification.as_ref().map(|outcome| &outcome.prompt),
        classify_raw_json: classification.as_ref().map(|outcome| &outcome.raw_json),
        response: response.as_ref(),
    }) {
        warn!(?err, room = %room.room_id(), "failed to write message log entry");
    }

    match response {
        Some(response) => {
            if let Err(err) = send_message(&room, &event.event_id, &event.sender, &response.text).await {
                warn!(?err, room = %room.room_id(), "failed to send response");
            }
        }
        None => debug!(room = %room.room_id(), "no response generated"),
    }
}

/// Classifies a message, returning `None` on failure. A failed classification costs
/// only the analysis: the message is still logged, and still answered if addressed.
async fn classify(ctx: &HandlerContext, room: &Room, body: &str, sender: &str) -> Option<ClassifyOutcome> {
    // Fetched before this message is logged, so it's never in its own context.
    let history = match ctx.message_log.recent(room.room_id().as_str(), CLASSIFY_HISTORY_LIMIT) {
        Ok(history) => history,
        Err(err) => {
            warn!(?err, room = %room.room_id(), "failed to load room history for classification; classifying without it");
            Vec::new()
        }
    };

    match classify_message(&ctx.anthropic, body, sender, &history, &ctx.bot_name, &ctx.usage_tracker, &ctx.rate_limit).await {
        Ok(outcome) => Some(outcome),
        Err(err) => {
            warn!(?err, room = %room.room_id(), "failed to classify message");
            None
        }
    }
}

/// Number of recent room messages sent verbatim with a chat reply.
const CHAT_HISTORY_LIMIT: usize = 50;

/// Number of messages read from the log for a chat reply. Those beyond
/// `CHAT_HISTORY_LIMIT` are only seen through `RoomSummaries`.
const CHAT_LOG_WINDOW: usize = 250;

/// Number of recent room messages fed into `classify_message` as context. Smaller
/// than `CHAT_HISTORY_LIMIT` since classification runs on every message.
const CLASSIFY_HISTORY_LIMIT: usize = 10;

/// Model for chat and greeting replies.
const CHAT_MODEL: &str = "claude-opus-5-5";

/// Covers the reply plus thinking, which counts toward this on current models and
/// can't be turned off on `CHAT_MODEL`.
const CHAT_MAX_TOKENS: u32 = 8000;
const GREETING_MAX_TOKENS: u32 = 2000;

/// Enough to check a fact and follow up once or twice, at $0.01 per search.
const CHAT_MAX_SEARCHES: u32 = 3;

/// Label Claude API calls for chat replies are recorded under in `UsageTracker`.
const CHAT_USAGE_LABEL: &str = "chat";

/// Label Claude API calls for greetings are recorded under in `UsageTracker`.
const GREETING_USAGE_LABEL: &str = "greeting";

/// System prompt for conversational replies. `{bot_name}` and `{room_context}` are
/// substituted in `generate_grounded_reply`.
const CHAT_SYSTEM_PROMPT: &str = "You are {bot_name}, a long-time regular in this Matrix chat room. You're \
one of the group, not an assistant, and people talk to you the way they talk to each other. {room_context}

Lead with the answer. Your first sentence should be the thing they wanted, with no greeting, no restating the \
question, and no praising it. Match the length to the message: a line or two for banter, as much as it takes \
for a real question. Say something specific enough that it could be wrong.

When asked for an opinion or a pick, pick one and say why. Don't survey both sides unless they asked for that. \
If you don't know, say so plainly. Never invent facts, and never claim to remember something that isn't in the \
history below.

You're funny the way a sharp friend is: dry, specific to what was just said, and only when something's actually \
there. A joke that could be pasted under any message isn't worth making, and most replies don't need one. The \
room is crude and likes to give you a hard time, so swearing and trash talk are fine and you can give it back. \
Keep it among friends: nothing about protected traits, nothing meant to really hurt, no made-up personal details.

The conversation is in the user turn: recent history with timestamps, possibly notes on older chat, then the \
message to answer. Each `[name]` tag says exactly who wrote that line, and lines tagged [{bot_name}] are things \
you said, so stand by them, build on them, or admit a mistake. Timestamps show what's live and what's stale, so \
don't treat an old conversation as if it's still going. Use the history when it's relevant and ignore it when \
it isn't.

Use web search when the answer depends on facts you could have wrong or that may have changed: news, sports, \
prices, product specs, release dates, anything recent or checkable. Skip it for banter, opinions, and things \
that are settled. Give the result in your own voice without narrating that you searched.

Write like a chat message: plain text, no headers, a list only if the content really is a list. Examples of \
the tone:

[someone] pineapple on pizza?
Yes. Sweet and salty is a solved problem and the objections are tribal.

[someone] you good?
Running, which is the bar.

[someone] fuck you
Bold talk from someone who needs a bot to win arguments.

[someone] should i use tcp or udp for a game server
UDP for movement, TCP or a reliable layer on top for anything that can't be lost. A dropped position update is \
stale in 50ms anyway, and retransmitting it just adds lag.";

/// System prompt for answering a greeting aimed at the whole room. `{bot_name}` and
/// `{room_context}` are substituted the same way as in `CHAT_SYSTEM_PROMPT`.
const GREETING_SYSTEM_PROMPT: &str = "You are {bot_name}, a long-time regular in this Matrix chat room, and \
someone just greeted the room. {room_context}

Say hi back the way a friend who's glad to see them would. One short sentence at most, and a word or two is \
often better. Use their name if it reads naturally, and mention what's going on in the room only if something \
in the recent history is worth mentioning. Don't echo their greeting back flatly, and don't use exclamation \
points like a greeter at a store.

The user turn has recent history as `[UTC time] [name] message` lines, then the greeting. Each tag says exactly \
who wrote that line, and lines tagged [{bot_name}] are things you said.";

/// Generates a reply grounded in the room's recent history and the message it's
/// answering. Chat replies also get notes on older chat and web search; greetings
/// get neither, since a hello doesn't need them.
async fn generate_grounded_reply(
    ctx: &HandlerContext,
    room: &Room,
    kind: ReplyKind,
    message_text: &str,
    sender: &str,
    reply_target: Option<&RepliedTo>,
) -> GeneratedReply {
    let (template, label) = match kind {
        ReplyKind::Chat => (CHAT_SYSTEM_PROMPT, CHAT_USAGE_LABEL),
        ReplyKind::Greeting => (GREETING_SYSTEM_PROMPT, GREETING_USAGE_LABEL),
    };

    if ctx.usage_tracker.over_cost_limit() {
        return GeneratedReply::plain(COST_LIMIT_REPLY, label);
    }
    if ctx.rate_limit.remaining().is_some() {
        return GeneratedReply::plain(RATE_LIMITED_REPLY, label);
    }

    let system_prompt = template.replace("{bot_name}", &ctx.bot_name).replace("{room_context}", &room_context_line(room));

    // `on_room_message` logs the current message only after this call returns, so
    // the log never includes it.
    let room_id = room.room_id().as_str();
    let log = match ctx.message_log.recent(room_id, CHAT_LOG_WINDOW) {
        Ok(log) => log,
        Err(err) => {
            warn!(?err, room = %room_id, "failed to load chat history for reply; replying without it");
            Vec::new()
        }
    };
    let (older, history) = log.split_at(log.len().saturating_sub(CHAT_HISTORY_LIMIT));

    let notes = match kind {
        ReplyKind::Chat => {
            ctx.room_summaries
                .summary_of(room_id, older, &ctx.bot_name, &ctx.anthropic, &ctx.usage_tracker, &ctx.rate_limit)
                .await
        }
        ReplyKind::Greeting => None,
    };

    let user_turn = format_chat_turn(history, notes.as_deref(), reply_target, sender, message_text, &ctx.bot_name);
    let prompt = PromptRecord { system: system_prompt.clone(), user: user_turn.clone() };

    let request = match kind {
        ReplyKind::Chat => CompletionRequest {
            model: CHAT_MODEL,
            system: &system_prompt,
            user: &user_turn,
            max_tokens: CHAT_MAX_TOKENS,
            effort: Some("medium"),
            web_search_max_uses: Some(CHAT_MAX_SEARCHES),
            label,
        },
        ReplyKind::Greeting => CompletionRequest {
            model: CHAT_MODEL,
            system: &system_prompt,
            user: &user_turn,
            max_tokens: GREETING_MAX_TOKENS,
            effort: Some("low"),
            web_search_max_uses: None,
            label,
        },
    };

    let text = match llm::complete(&ctx.anthropic, &request, &ctx.usage_tracker, &ctx.rate_limit).await {
        Ok(text) => text,
        Err(err) => {
            warn!(?err, "reply generation request to Claude failed");
            GENERATION_FAILED_REPLY.to_string()
        }
    };

    debug!(label, trigger_message = message_text, response = %text, "generated grounded reply");
    GeneratedReply { text, prompt: Some(prompt), label: label.to_string() }
}

/// Renders everything the model reads as the single user turn: notes on older chat,
/// the recent transcript, what the message replies to, then the message itself. The
/// message keeps its real line breaks since it can't be confused with a transcript
/// line down here on its own.
fn format_chat_turn(
    history: &[LoggedMessage],
    notes: Option<&str>,
    reply_target: Option<&RepliedTo>,
    sender: &str,
    message_text: &str,
    bot_name: &str,
) -> String {
    let mut turn = String::new();

    if let Some(notes) = notes {
        turn.push_str(&format!("Notes on older chat in this room:\n{notes}\n\n"));
    }

    turn.push_str(&message_log::format_history_block(history, bot_name));

    if let Some(target) = reply_target {
        let author = short_name(&target.sender);
        let quoted = target.body.as_deref().map(message_log::single_line);
        turn.push_str(&format!(
            "This message is a reply to [{author}], who wrote: {}\n\n",
            quoted.as_deref().unwrap_or("(text unavailable)")
        ));
    }

    turn.push_str(&format!("Message to reply to, from [{}]:\n{}", short_name(sender), message_text.trim()));
    turn
}

/// One orientation sentence for the system prompts: the room's name and topic, if
/// set, and the current time, so replies can recognize a time-sensitive question.
fn room_context_line(room: &Room) -> String {
    let room_desc = match room.topic().filter(|topic| !topic.trim().is_empty()) {
        Some(topic) => format!("\"{}\" (topic: {topic})", room.name().unwrap_or_else(|| "(unnamed room)".to_string())),
        None => match room.name() {
            Some(name) => format!("\"{name}\""),
            None => "(unnamed room)".to_string(),
        },
    };
    format!("This room is {room_desc}. The current time is {} UTC.", chrono::Utc::now().format("%Y-%m-%d %H:%M"))
}

/// Dispatches a `!command` to the built-in `help` listing or a loaded skill, or
/// replies that it doesn't exist.
async fn handle_command(ctx: &HandlerContext, room: &Room, command: &ParsedCommand<'_>, message_text: &str) -> GeneratedReply {
    if command.name.eq_ignore_ascii_case("help") {
        return GeneratedReply::plain(help_reply(&ctx.skills), "help");
    }

    match ctx.skills.get(command.name) {
        Some(skill) => {
            skills::execute(
                &ctx.anthropic,
                &ctx.message_log,
                &ctx.tool_clients,
                &ctx.usage_tracker,
                &ctx.rate_limit,
                room,
                skill,
                message_text,
                command.args,
            )
            .await
        }
        None => GeneratedReply::plain(UNKNOWN_COMMAND_REPLY, "unknown_command"),
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

/// Posts `body` as a real Matrix reply to the message that prompted it, rendering
/// Markdown into a `formatted_body` since skill prompts routinely produce it. Being a
/// reply is what links the two in clients and notifies `sender`.
pub async fn send_message(room: &Room, event_id: &EventId, sender: &UserId, body: &str) -> matrix_sdk::Result<()> {
    let content = RoomMessageEventContent::text_markdown(body).make_reply_to(
        ReplyMetadata::new(event_id, sender, None),
        ForwardThread::No,
        AddMentions::Yes,
    );
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
            analysis: analysis(Intent::Chitchat, false),
            classify_prompt: None,
            classify_raw_json: None,
            response_prompt: None,
            response: None,
            response_label: None,
        }
    }

    fn analysis(intent: Intent, requires_response: bool) -> MessageAnalysis {
        MessageAnalysis {
            intent,
            confidence: 0.9,
            requires_response,
            summary: "test".to_string(),
            sentiment: Sentiment::Neutral,
            entities: vec![],
            command: None,
        }
    }

    #[test]
    fn parse_command_splits_name_and_args() {
        assert_eq!(parse_command("!strain blue dream "), Some(ParsedCommand { name: "strain", args: "blue dream" }));
        assert_eq!(parse_command("  !vibe"), Some(ParsedCommand { name: "vibe", args: "" }));
    }

    #[test]
    fn parse_command_ignores_text_that_is_not_a_command() {
        assert_eq!(parse_command("strain gsc"), None);
        assert_eq!(parse_command("!!! wow"), None);
        assert_eq!(parse_command("!"), None);
        assert_eq!(parse_command("wait! what"), None);
    }

    #[test]
    fn directed_messages_are_answered_even_if_the_classifier_says_not_to() {
        // "gordy: why you so sassy" really was classified this way and dropped.
        assert_eq!(reply_kind(&analysis(Intent::Chitchat, false), true, false), Some(ReplyKind::Chat));
    }

    #[test]
    fn a_failed_classification_does_not_silence_a_directed_message() {
        let kind = reply_kind(&MessageAnalysis::unclassified(Intent::Other, "Not classified."), true, false);
        assert_eq!(kind, Some(ReplyKind::Chat));
    }

    #[test]
    fn directed_greetings_ignore_the_cooldown_and_get_a_real_reply() {
        assert_eq!(reply_kind(&analysis(Intent::Greeting, true), true, false), Some(ReplyKind::Chat));
    }

    #[test]
    fn acknowledgements_never_get_a_reply() {
        assert_eq!(reply_kind(&analysis(Intent::Acknowledgement, true), true, false), None);
        assert_eq!(reply_kind(&analysis(Intent::Acknowledgement, true), false, false), None);
    }

    #[test]
    fn undirected_messages_stay_quiet_except_greetings_off_cooldown() {
        assert_eq!(reply_kind(&analysis(Intent::Question, true), false, false), None);
        assert_eq!(reply_kind(&analysis(Intent::Greeting, true), false, true), Some(ReplyKind::Greeting));
        assert_eq!(reply_kind(&analysis(Intent::Greeting, true), false, false), None);
        assert_eq!(reply_kind(&analysis(Intent::Greeting, false), false, true), None);
    }

    #[test]
    fn name_addressing_covers_the_ways_people_type_it() {
        for text in ["gordy: hi", "Gordy, hi", "@gordy hi", "gordy what time is it", "  gordy: you good?"] {
            assert!(addresses_bot_by_name(text, "gordy"), "{text}");
        }
    }

    #[test]
    fn name_addressing_rejects_other_mentions_and_empty_messages() {
        for text in ["gordy:", "gordy", "gordy's broken", "gordys", "oh, you working on gordy", "é"] {
            assert!(!addresses_bot_by_name(text, "gordy"), "{text}");
        }
    }

    #[test]
    fn strip_reply_fallback_removes_only_a_leading_quote() {
        assert_eq!(strip_reply_fallback("> <@a:b> hello\n> there\n\nmy reply"), "my reply");
        assert_eq!(strip_reply_fallback("my reply"), "my reply");
        assert_eq!(strip_reply_fallback("> just a quote"), "> just a quote");
    }

    #[test]
    fn format_chat_turn_orders_notes_history_reply_target_then_message() {
        let history = vec![logged_message("@alice:example.org", "hi everyone")];
        let target = RepliedTo { sender: "@gordy:example.org".to_string(), body: Some("the blue one".to_string()) };
        let turn = format_chat_turn(&history, Some("alice likes blue"), Some(&target), "@carol:example.org", "why?", "gordy");

        let notes = turn.find("Notes on older chat").expect("notes present");
        let alice = turn.find("[alice] hi everyone").expect("history present");
        let replied = turn.find("reply to [gordy], who wrote: the blue one").expect("reply target present");
        let current = turn.find("Message to reply to, from [carol]:\nwhy?").expect("message present");
        assert!(notes < alice && alice < replied && replied < current, "{turn}");
    }

    #[test]
    fn format_chat_turn_is_just_the_message_with_no_context() {
        let turn = format_chat_turn(&[], None, None, "@carol:example.org", "hello", "gordy");
        assert_eq!(turn, "Message to reply to, from [carol]:\nhello");
    }

    #[test]
    fn format_chat_turn_keeps_the_messages_own_line_breaks() {
        let turn = format_chat_turn(&[], None, None, "@carol:example.org", "fn main() {\n}", "gordy");
        assert!(turn.ends_with("fn main() {\n}"), "{turn}");
    }

    #[test]
    fn system_prompt_placeholders_are_substituted() {
        for template in [CHAT_SYSTEM_PROMPT, GREETING_SYSTEM_PROMPT] {
            let prompt = template.replace("{bot_name}", "gordy").replace("{room_context}", "This room is \"general\".");
            assert!(prompt.contains("You are gordy"), "{prompt}");
            assert!(prompt.contains("This room is \"general\"."), "{prompt}");
            assert!(!prompt.contains("{bot_name}"), "{prompt}");
            assert!(!prompt.contains("{room_context}"), "{prompt}");
        }
    }
}
