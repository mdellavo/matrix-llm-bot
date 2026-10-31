use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::classify::MessageAnalysis;

/// The system/user turn sent to Claude for a single call — recorded alongside
/// the message that triggered it (classification, a chat/greeting reply, or a
/// skill command) so the status page can show exactly what was sent, not just
/// what came back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRecord {
    pub system: String,
    pub user: String,
}

/// A reply to a message: the text actually sent (or a
/// canned fallback, e.g. "unknown command"), which prompt produced it — `None`
/// when no Claude call was made at all, such as an unrecognized command or a
/// skill that short-circuited on invalid args — and a `label` identifying the
/// kind of reply (`"chat"`, `"greeting"`, or a skill name) for display.
#[derive(Debug, Clone)]
pub struct GeneratedReply {
    pub text: String,
    pub prompt: Option<PromptRecord>,
    pub label: String,
}

impl GeneratedReply {
    /// A reply with no backing Claude call — a canned/fallback string, e.g.
    /// "unknown command" or a skill's own "invalid arguments" message.
    pub fn plain(text: impl Into<String>, label: impl Into<String>) -> Self {
        Self { text: text.into(), prompt: None, label: label.into() }
    }
}

/// A single logged message: the metadata the bot already has from the Matrix
/// event, the `MessageAnalysis` and prompt the classifier produced/used, and
/// — if the bot replied — the reply's own prompt, text, and label.
#[derive(Debug, Serialize)]
struct MessageLogEntry<'a> {
    logged_at: String,
    room_id: &'a str,
    event_id: &'a str,
    sender: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    sender_name: Option<&'a str>,
    origin_server_ts_ms: i64,
    body: &'a str,
    has_image: bool,
    analysis: &'a MessageAnalysis,
    #[serde(skip_serializing_if = "Option::is_none")]
    classify_prompt: Option<&'a PromptRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    classify_raw_json: Option<&'a serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_prompt: Option<&'a PromptRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_label: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_event_id: Option<&'a str>,
}

/// One line of a room's reaction log.
#[derive(Debug, Serialize, Deserialize)]
struct ReactionLogEntry {
    target_event_id: String,
    sender_name: String,
    key: String,
}

/// A reaction on a logged message or on the bot's reply to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoggedReaction {
    pub sender_name: String,
    pub key: String,
}

/// Bundles every argument `MessageLogger::log` needs — grown too large for a
/// plain parameter list once prompts/responses joined the metadata/analysis
/// already there (matches the `HandlerContext` bundling in `handler.rs` for
/// the same reason).
pub struct MessageLogParams<'a> {
    pub room_id: &'a str,
    pub event_id: &'a str,
    pub sender: &'a str,
    /// The sender's display name at the time, if they have one.
    pub sender_name: Option<&'a str>,
    pub origin_server_ts_ms: i64,
    pub body: &'a str,
    pub has_image: bool,
    pub analysis: &'a MessageAnalysis,
    /// `None` when the classifier didn't run (`!` commands) or failed; the message
    /// is still logged so it stays in the room's history.
    pub classify_prompt: Option<&'a PromptRecord>,
    pub classify_raw_json: Option<&'a serde_json::Value>,
    /// `Some` whenever the bot generated (or attempted to generate) a reply —
    /// `None` when the message never reached response generation at all (e.g.
    /// `requires_response` was false).
    pub response: Option<&'a GeneratedReply>,
    /// The event the reply was sent as, which is how reactions find it.
    pub response_event_id: Option<&'a str>,
}

/// Owned counterpart of `MessageLogEntry`, for reading log lines back — also
/// serialized directly as the JSON response of the status server's message API
/// (`src/status_server.rs`). The prompt/response fields default to `None` on
/// deserialize so log lines written before they existed still parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggedMessage {
    pub logged_at: String,
    pub room_id: String,
    pub event_id: String,
    pub sender: String,
    #[serde(default)]
    pub sender_name: Option<String>,
    pub origin_server_ts_ms: i64,
    pub body: String,
    #[serde(default)]
    pub has_image: bool,
    pub analysis: MessageAnalysis,
    #[serde(default)]
    pub classify_prompt: Option<PromptRecord>,
    #[serde(default)]
    pub classify_raw_json: Option<serde_json::Value>,
    #[serde(default)]
    pub response_prompt: Option<PromptRecord>,
    #[serde(default)]
    pub response: Option<String>,
    #[serde(default)]
    pub response_label: Option<String>,
    #[serde(default)]
    pub response_event_id: Option<String>,
    /// Filled in from the room's reaction log on read, not stored on the line.
    #[serde(default)]
    pub reactions: Vec<LoggedReaction>,
    #[serde(default)]
    pub response_reactions: Vec<LoggedReaction>,
}

impl LoggedMessage {
    /// The name to show for the sender: their display name, or for lines logged
    /// before those were recorded, the short form of their user ID.
    pub fn display_name(&self) -> &str {
        self.sender_name.as_deref().unwrap_or_else(|| short_name(&self.sender))
    }
}

/// Appends one JSON object per line to a per-room log file — a durable record of
/// every classified message the bot has seen, independent of whether it replied.
/// One file per room under `dir`, e.g. `!abcDEFghi_matrix.org.jsonl`, opened lazily
/// on the room's first logged message and kept open thereafter.
///
/// Writes are synchronous (a single small `write()` under a `std::sync::Mutex`),
/// which is fine at chat-message volume; if logging ever becomes a bottleneck,
/// move the write onto `tokio::task::spawn_blocking`.
///
/// Queries (`recent`) are disk-only: they re-read and re-parse the room's log file
/// on every call, with no in-memory cache. Simpler and always consistent with what's
/// persisted; revisit with an in-memory ring buffer if command latency or file size
/// becomes a problem.
pub struct MessageLogger {
    dir: PathBuf,
    files: Mutex<HashMap<String, File>>,
}

impl MessageLogger {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create message log dir at {}", dir.display()))?;

        Ok(Self {
            dir: dir.to_path_buf(),
            files: Mutex::new(HashMap::new()),
        })
    }

    pub fn log(&self, params: MessageLogParams) -> Result<()> {
        let MessageLogParams {
            room_id,
            event_id,
            sender,
            sender_name,
            origin_server_ts_ms,
            body,
            has_image,
            analysis,
            classify_prompt,
            classify_raw_json,
            response,
            response_event_id,
        } = params;

        let entry = MessageLogEntry {
            logged_at: chrono::Utc::now().to_rfc3339(),
            room_id,
            event_id,
            sender,
            sender_name,
            origin_server_ts_ms,
            body,
            has_image,
            analysis,
            classify_prompt,
            classify_raw_json,
            response_prompt: response.and_then(|response| response.prompt.as_ref()),
            response: response.map(|response| response.text.as_str()),
            response_label: response.map(|response| response.label.as_str()),
            response_event_id,
        };
        let line = serde_json::to_string(&entry).context("failed to serialize message log entry")?;

        let mut files = self.files.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let file = match files.entry(room_id.to_string()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let path = self.room_log_path(room_id);
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .with_context(|| format!("failed to open room log at {}", path.display()))?;
                entry.insert(file)
            }
        };

        writeln!(file, "{line}").context("failed to write message log entry")?;
        Ok(())
    }

    /// Records a reaction. Kept in its own file because it's about an existing
    /// message, and message lines are never rewritten.
    pub fn log_reaction(&self, room_id: &str, target_event_id: &str, sender_name: &str, key: &str) -> Result<()> {
        let entry = ReactionLogEntry {
            target_event_id: target_event_id.to_string(),
            sender_name: sender_name.to_string(),
            key: key.to_string(),
        };
        let line = serde_json::to_string(&entry).context("failed to serialize reaction log entry")?;

        let path = self.reaction_log_path(room_id);
        let _guard = self.files.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open reaction log at {}", path.display()))?;
        writeln!(file, "{line}").context("failed to write reaction log entry")
    }

    /// Returns up to `limit` most recent logged messages for `room_id`, oldest first.
    /// Reads and parses the room's log file from disk on every call — there is no
    /// in-memory cache (see the "disk-only" note in the module docs above).
    pub fn recent(&self, room_id: &str, limit: usize) -> Result<Vec<LoggedMessage>> {
        let mut entries = self.read_all(room_id)?;
        let start = entries.len().saturating_sub(limit);
        Ok(entries.split_off(start))
    }

    /// The messages in the room's whole log that best match `query`, oldest first.
    /// Ranked by how many of the query's words appear in the sender's name, the
    /// message, or the bot's reply to it, with newer messages winning ties.
    pub fn search(&self, room_id: &str, query: &str, limit: usize) -> Result<Vec<LoggedMessage>> {
        let terms = search_terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        let mut scored: Vec<(usize, LoggedMessage)> = self
            .read_all(room_id)?
            .into_iter()
            .filter(|entry| !entry.body.trim_start().starts_with("!search"))
            .filter_map(|entry| {
                let score = match_score(&entry, &terms);
                (score > 0).then_some((score, entry))
            })
            .collect();

        scored.sort_by_key(|(score, entry)| std::cmp::Reverse((*score, entry.origin_server_ts_ms)));
        scored.truncate(limit);

        let mut matches: Vec<LoggedMessage> = scored.into_iter().map(|(_, entry)| entry).collect();
        matches.sort_by_key(|entry| entry.origin_server_ts_ms);
        Ok(matches)
    }

    fn read_all(&self, room_id: &str) -> Result<Vec<LoggedMessage>> {
        let path = self.room_log_path(room_id);

        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read room log at {}", path.display()));
            }
        };

        let mut entries = Vec::new();
        for (line_no, line) in contents.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<LoggedMessage>(line) {
                Ok(entry) => entries.push(entry),
                Err(err) => warn!(?err, room_id, line_no, "skipping malformed message log line"),
            }
        }

        self.attach_reactions(room_id, &mut entries);
        Ok(entries)
    }

    fn attach_reactions(&self, room_id: &str, entries: &mut [LoggedMessage]) {
        let Ok(contents) = std::fs::read_to_string(self.reaction_log_path(room_id)) else {
            return;
        };

        let mut by_target: HashMap<String, Vec<LoggedReaction>> = HashMap::new();
        for line in contents.lines() {
            match serde_json::from_str::<ReactionLogEntry>(line) {
                Ok(entry) => by_target
                    .entry(entry.target_event_id)
                    .or_default()
                    .push(LoggedReaction { sender_name: entry.sender_name, key: entry.key }),
                Err(err) => warn!(?err, room_id, "skipping malformed reaction log line"),
            }
        }

        for entry in entries {
            if let Some(reactions) = by_target.remove(&entry.event_id) {
                entry.reactions = reactions;
            }
            if let Some(reactions) = entry.response_event_id.as_ref().and_then(|id| by_target.remove(id)) {
                entry.response_reactions = reactions;
            }
        }
    }

    fn reaction_log_path(&self, room_id: &str) -> PathBuf {
        self.dir.join(format!("{}.reactions.jsonl", sanitize_filename(room_id)))
    }

    fn room_log_path(&self, room_id: &str) -> PathBuf {
        self.dir.join(format!("{}.jsonl", sanitize_filename(room_id)))
    }
}

/// Replaces characters that are unsafe or reserved in filenames (notably `:`, used
/// in every Matrix room ID) with `_`, so each room maps to a stable, portable filename.
fn sanitize_filename(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect()
}

/// Bot replies longer than this are clipped in transcripts: enough to know what it
/// said, without one long skill output crowding out the conversation.
const MAX_BOT_LINE_CHARS: usize = 1000;

/// Marks a line break inside one transcript line, so a multi-line message keeps its
/// shape without producing continuation lines that have no `[sender]` tag.
const LINE_BREAK_MARKER: &str = " ⏎ ";

/// Reply labels that aren't conversation, left out of transcripts.
const NON_CONVERSATIONAL_LABELS: &[&str] = &["help", "unknown_command"];

/// Words too common to tell one message from another.
const SEARCH_STOPWORDS: &[&str] = &[
    "a", "about", "an", "and", "are", "did", "do", "does", "for", "from", "has", "have", "how", "in", "is", "it", "of",
    "on", "say", "said", "that", "the", "to", "was", "what", "when", "where", "who", "why", "with",
];

fn search_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty() && !SEARCH_STOPWORDS.contains(&term.as_str()))
        .collect()
}

fn match_score(entry: &LoggedMessage, terms: &[String]) -> usize {
    let text = format!("{} {} {}", entry.display_name(), entry.body, entry.response.as_deref().unwrap_or_default());
    let text = text.to_lowercase();
    terms.iter().filter(|term| text.contains(term.as_str())).count()
}

/// Formats `history` as a "recent room history" block (see `format_transcript`), or
/// an empty string if `history` is empty. Shared by every Claude call grounded in
/// recent room messages, so the transcript format stays consistent everywhere.
pub fn format_history_block(history: &[LoggedMessage], bot_name: &str) -> String {
    if history.is_empty() {
        return String::new();
    }
    format!(
        "Recent room history (oldest first, as `[UTC time] [sender] message`; ⏎ is a line break, and \
         anything in `{{reactions: ...}}` at the end of a line is how people reacted to it):\n{}\n",
        format_transcript(history, bot_name)
    )
}

/// One line per message, oldest first. The bot's own replies get their own lines,
/// because they're stored on the message that triggered them and would otherwise
/// be invisible, leaving it unable to follow up on anything it said.
pub fn format_transcript(history: &[LoggedMessage], bot_name: &str) -> String {
    let mut transcript = String::new();
    for entry in history {
        let time = format_timestamp(entry.origin_server_ts_ms);
        let reactions = format_reactions(&entry.reactions);
        transcript.push_str(&format!("[{time}] [{}] {}{reactions}\n", entry.display_name(), single_line(&entry.body)));

        if let Some(reply) = conversational_reply(entry) {
            let reactions = format_reactions(&entry.response_reactions);
            transcript.push_str(&format!("[{time}] [{bot_name}] {}{reactions}\n", clip(&single_line(reply))));
        }
    }
    transcript
}

fn format_reactions(reactions: &[LoggedReaction]) -> String {
    if reactions.is_empty() {
        return String::new();
    }
    let list: Vec<String> = reactions.iter().map(|reaction| format!("{} {}", reaction.key, reaction.sender_name)).collect();
    format!(" {{reactions: {}}}", list.join(", "))
}

/// The bot's reply to `entry`, unless it's boilerplate rather than conversation.
pub fn conversational_reply(entry: &LoggedMessage) -> Option<&str> {
    let label = entry.response_label.as_deref().unwrap_or_default();
    if NON_CONVERSATIONAL_LABELS.contains(&label) {
        return None;
    }
    entry.response.as_deref()
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_BOT_LINE_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

fn format_timestamp(ts_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts_ms)
        .map_or_else(|| "unknown time".to_string(), |time| time.format("%Y-%m-%d %H:%M").to_string())
}

/// `@procyon:p.ound.town` → `procyon`. What people actually call each other, and
/// what the bot should call them.
pub fn short_name(user_id: &str) -> &str {
    let name = user_id.strip_prefix('@').unwrap_or(user_id);
    name.split(':').next().unwrap_or(name)
}

/// Folds a message onto one transcript line, keeping its line breaks visible as
/// `⏎`. Without this, a continuation line would have no `[sender]` tag at all,
/// which is exactly what gets a model to misattribute it.
pub fn single_line(text: &str) -> String {
    text.trim().lines().map(str::trim_end).filter(|line| !line.is_empty()).collect::<Vec<_>>().join(LINE_BREAK_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{Intent, MessageAnalysis, Sentiment};

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
            classify_prompt: None,
            classify_raw_json: None,
            response_prompt: None,
            response: None,
            response_label: None,
            sender_name: None,
            has_image: false,
            response_event_id: None,
            reactions: Vec::new(),
            response_reactions: Vec::new(),
        }
    }

    #[test]
    fn format_history_block_is_empty_for_no_history() {
        assert_eq!(format_history_block(&[], "gordy"), "");
    }

    #[test]
    fn format_history_block_lists_timestamped_short_names_oldest_first() {
        let history = vec![logged_message("@alice:example.org", "hi everyone"), logged_message("@bob:example.org", "yo")];
        let block = format_history_block(&history, "gordy");

        assert!(block.starts_with("Recent room history"), "{block}");
        let alice_pos = block.find("[1970-01-01 00:00] [alice] hi everyone").expect("alice line present");
        let bob_pos = block.find("[1970-01-01 00:00] [bob] yo").expect("bob line present");
        assert!(alice_pos < bob_pos, "{block}");
        assert!(block.ends_with('\n'), "{block}");
    }

    #[test]
    fn transcript_includes_the_bots_own_replies_after_their_trigger() {
        let mut asked = logged_message("@alice:example.org", "gordy: pick one");
        asked.response = Some("the blue one".to_string());
        asked.response_label = Some("chat".to_string());
        let history = vec![asked, logged_message("@alice:example.org", "why that one?")];

        let transcript = format_transcript(&history, "gordy");
        let question = transcript.find("[alice] gordy: pick one").expect("question present");
        let reply = transcript.find("[gordy] the blue one").expect("reply present");
        let follow_up = transcript.find("[alice] why that one?").expect("follow-up present");
        assert!(question < reply && reply < follow_up, "{transcript}");
    }

    #[test]
    fn transcript_leaves_out_replies_that_are_not_conversation() {
        let mut asked = logged_message("@alice:example.org", "!help");
        asked.response = Some("Available commands: ...".to_string());
        asked.response_label = Some("help".to_string());

        assert!(!format_transcript(&[asked], "gordy").contains("Available commands"));
    }

    #[test]
    fn transcript_clips_very_long_bot_replies() {
        let mut asked = logged_message("@alice:example.org", "!digest");
        asked.response = Some("é".repeat(MAX_BOT_LINE_CHARS + 50));
        asked.response_label = Some("digest".to_string());

        let transcript = format_transcript(&[asked], "gordy");
        assert!(transcript.contains('…'), "{transcript}");
        assert!(transcript.chars().filter(|c| *c == 'é').count() == MAX_BOT_LINE_CHARS, "{transcript}");
    }

    #[test]
    fn format_history_block_keeps_one_line_per_message_with_visible_line_breaks() {
        let history = vec![logged_message("@alice:example.org", "line one\nline two")];
        let block = format_history_block(&history, "gordy");

        assert!(block.contains("[alice] line one ⏎ line two"), "{block}");
        assert_eq!(block.lines().filter(|line| line.contains("line one") || line.contains("line two")).count(), 1, "{block}");
    }

    #[test]
    fn single_line_marks_line_breaks() {
        assert_eq!(single_line("line one\nline two"), "line one ⏎ line two");
        assert_eq!(single_line("a\r\n\r\nb"), "a ⏎ b");
        assert_eq!(single_line("no newlines here"), "no newlines here");
    }

    #[test]
    fn short_name_strips_sigil_and_server() {
        assert_eq!(short_name("@procyon:p.ound.town"), "procyon");
        assert_eq!(short_name("gordy"), "gordy");
    }

    #[test]
    fn display_names_are_used_when_known() {
        let mut entry = logged_message("@procyon:p.ound.town", "hi");
        assert_eq!(entry.display_name(), "procyon");
        entry.sender_name = Some("Proc".to_string());
        assert!(format_transcript(&[entry], "gordy").contains("[Proc] hi"));
    }

    fn test_logger(label: &str) -> MessageLogger {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        MessageLogger::open(&std::env::temp_dir().join(format!("message-log-{label}-{nanos}"))).unwrap()
    }

    fn log_line(logger: &MessageLogger, event_id: &str, sender: &str, body: &str, ts: i64, response: Option<(&str, &str)>) {
        let analysis = logged_message(sender, body).analysis;
        let reply = response.map(|(text, _)| GeneratedReply::plain(text, "chat"));
        logger
            .log(MessageLogParams {
                room_id: "!r",
                event_id,
                sender,
                sender_name: None,
                origin_server_ts_ms: ts,
                body,
                has_image: false,
                analysis: &analysis,
                classify_prompt: None,
                classify_raw_json: None,
                response: reply.as_ref(),
                response_event_id: response.map(|(_, id)| id),
            })
            .unwrap();
    }

    #[test]
    fn reactions_attach_to_the_message_or_the_bots_reply() {
        let logger = test_logger("reactions");
        log_line(&logger, "$q", "@alice:x", "pick one", 1, Some(("the blue one", "$reply")));
        logger.log_reaction("!r", "$q", "bob", "😂").unwrap();
        logger.log_reaction("!r", "$reply", "alice", "👍").unwrap();

        let entries = logger.recent("!r", 10).unwrap();
        assert_eq!(entries[0].reactions, vec![LoggedReaction { sender_name: "bob".to_string(), key: "😂".to_string() }]);

        let transcript = format_transcript(&entries, "gordy");
        assert!(transcript.contains("[alice] pick one {reactions: 😂 bob}"), "{transcript}");
        assert!(transcript.contains("[gordy] the blue one {reactions: 👍 alice}"), "{transcript}");
    }

    #[test]
    fn search_ranks_by_matching_words_and_returns_oldest_first() {
        let logger = test_logger("search");
        log_line(&logger, "$1", "@alice:x", "the world cup was great", 1, None);
        log_line(&logger, "$2", "@oshuma:x", "the world cup was boring", 2, None);
        log_line(&logger, "$3", "@oshuma:x", "lunch soon", 3, None);
        log_line(&logger, "$4", "@bob:x", "!search oshuma world cup", 4, None);

        let matches = logger.search("!r", "what did oshuma say about the world cup", 2).unwrap();
        let ids: Vec<&str> = matches.iter().map(|entry| entry.event_id.as_str()).collect();
        assert_eq!(ids, vec!["$1", "$2"]);
    }

    #[test]
    fn search_finds_words_in_the_bots_replies() {
        let logger = test_logger("search-replies");
        log_line(&logger, "$1", "@alice:x", "pick one", 1, Some(("the blue one", "$r")));
        assert_eq!(logger.search("!r", "blue", 5).unwrap().len(), 1);
    }

    #[test]
    fn search_with_only_stopwords_finds_nothing() {
        let logger = test_logger("search-empty");
        log_line(&logger, "$1", "@alice:x", "what is the point", 1, None);
        assert!(logger.search("!r", "what is the", 5).unwrap().is_empty());
    }
}
