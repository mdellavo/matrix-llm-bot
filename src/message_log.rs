use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::{Connection, Row, Transaction, params};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::classify::MessageAnalysis;
use crate::db::Database;
use crate::links::LinkPreview;

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

/// One line of an old reaction log file, and the shape of a reaction row.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReactionRecord {
    pub target_event_id: String,
    pub sender_name: String,
    pub key: String,
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
    /// Previews of the pages the message links to.
    pub links: &'a [LinkPreview],
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
    /// The thread the message is part of, or that the bot's reply started on it.
    pub thread_root_id: Option<&'a str>,
}

/// The new version of an edited message, for `MessageLogger::record_edit`. `None`
/// fields keep what was stored.
pub struct EditedMessage<'a> {
    pub room_id: &'a str,
    /// The message that was edited, not the edit event.
    pub event_id: &'a str,
    pub body: &'a str,
    pub links: &'a [LinkPreview],
    pub analysis: Option<&'a MessageAnalysis>,
    pub classify_prompt: Option<&'a PromptRecord>,
    pub classify_raw_json: Option<&'a serde_json::Value>,
    pub response: Option<&'a GeneratedReply>,
    pub response_event_id: Option<&'a str>,
    pub thread_root_id: Option<&'a str>,
}

/// A logged message as read back. Also serialized directly as the JSON response of
/// the status server's message API (`src/status_server.rs`), and deserialized from
/// the JSONL logs older versions wrote, so newer fields must have `#[serde(default)]`.
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
    #[serde(default)]
    pub links: Vec<LinkPreview>,
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
    #[serde(default)]
    pub thread_root_id: Option<String>,
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

/// Every message the bot has seen, whether or not it replied, stored in the bot's
/// database.
pub struct MessageLogger {
    database: Arc<Database>,
}

/// The columns `read_message` expects. Prompts are left out unless asked for, since
/// they're most of a row's size and only the status page shows them.
const MESSAGE_COLUMNS: &str = "room_id, event_id, sender, sender_name, origin_server_ts_ms, logged_at, body, \
    has_image, analysis_json, links_json, response, response_label, response_event_id, thread_root_id";
const PROMPT_COLUMNS: &str = "classify_prompt_json, classify_raw_json, response_prompt_json";

/// Most terms from one search that are matched in SQL.
const MAX_SEARCH_TERMS: usize = 20;

fn parse_column<T: serde::de::DeserializeOwned>(raw: Option<String>, event_id: &str) -> Option<T> {
    let raw = raw?;
    match serde_json::from_str(&raw) {
        Ok(value) => Some(value),
        Err(err) => {
            warn!(?err, event_id, "skipping malformed JSON column");
            None
        }
    }
}

fn read_message(row: &Row<'_>, with_prompts: bool) -> rusqlite::Result<Option<LoggedMessage>> {
    let event_id: String = row.get(1)?;
    let Some(analysis) = parse_column(row.get(8)?, &event_id) else {
        return Ok(None);
    };

    Ok(Some(LoggedMessage {
        room_id: row.get(0)?,
        sender: row.get(2)?,
        sender_name: row.get(3)?,
        origin_server_ts_ms: row.get(4)?,
        logged_at: row.get(5)?,
        body: row.get(6)?,
        has_image: row.get(7)?,
        analysis,
        links: parse_column(row.get(9)?, &event_id).unwrap_or_default(),
        response: row.get(10)?,
        response_label: row.get(11)?,
        response_event_id: row.get(12)?,
        thread_root_id: row.get(13)?,
        classify_prompt: if with_prompts { parse_column(row.get(14)?, &event_id) } else { None },
        classify_raw_json: if with_prompts { parse_column(row.get(15)?, &event_id) } else { None },
        response_prompt: if with_prompts { parse_column(row.get(16)?, &event_id) } else { None },
        event_id,
        reactions: Vec::new(),
        response_reactions: Vec::new(),
    }))
}

fn reactions_on(connection: &Connection, room_id: &str, event_id: &str) -> rusqlite::Result<Vec<LoggedReaction>> {
    let mut statement = connection.prepare_cached(
        "SELECT sender_name, key FROM reactions WHERE room_id = ?1 AND target_event_id = ?2 ORDER BY id",
    )?;
    statement
        .query_map([room_id, event_id], |row| Ok(LoggedReaction { sender_name: row.get(0)?, key: row.get(1)? }))?
        .collect()
}

fn attach_reactions(connection: &Connection, entry: &mut LoggedMessage) -> rusqlite::Result<()> {
    entry.reactions = reactions_on(connection, &entry.room_id, &entry.event_id)?;
    if let Some(reply_id) = &entry.response_event_id {
        entry.response_reactions = reactions_on(connection, &entry.room_id, reply_id)?;
    }
    Ok(())
}

/// Bound parameters for one `messages` row.
struct NewMessage<'a> {
    room_id: &'a str,
    event_id: &'a str,
    sender: &'a str,
    sender_name: Option<&'a str>,
    origin_server_ts_ms: i64,
    logged_at: &'a str,
    body: &'a str,
    has_image: bool,
    analysis_json: String,
    links_json: Option<String>,
    classify_prompt_json: Option<String>,
    classify_raw_json: Option<String>,
    response_prompt_json: Option<String>,
    response: Option<&'a str>,
    response_label: Option<&'a str>,
    response_event_id: Option<&'a str>,
    thread_root_id: Option<&'a str>,
}

/// A repeated event ID is ignored, so a message can never be logged twice.
fn insert_message(connection: &Connection, message: &NewMessage<'_>) -> rusqlite::Result<usize> {
    connection.prepare_cached(
        "INSERT OR IGNORE INTO messages (room_id, event_id, sender, sender_name, origin_server_ts_ms, logged_at, body, \
         has_image, analysis_json, links_json, classify_prompt_json, classify_raw_json, response_prompt_json, response, \
         response_label, response_event_id, thread_root_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
    )?
    .execute(params![
        message.room_id,
        message.event_id,
        message.sender,
        message.sender_name,
        message.origin_server_ts_ms,
        message.logged_at,
        message.body,
        message.has_image,
        message.analysis_json,
        message.links_json,
        message.classify_prompt_json,
        message.classify_raw_json,
        message.response_prompt_json,
        message.response,
        message.response_label,
        message.response_event_id,
        message.thread_root_id,
    ])
}

fn to_json<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    serde_json::to_string(value).context("failed to serialize message column")
}

fn optional_json<T: Serialize>(value: Option<&T>) -> Result<Option<String>> {
    value.map(to_json).transpose()
}

fn links_json(links: &[LinkPreview]) -> Result<Option<String>> {
    if links.is_empty() { Ok(None) } else { to_json(links).map(Some) }
}

fn insert_reaction(connection: &Connection, room_id: &str, reaction: &ReactionRecord) -> rusqlite::Result<usize> {
    connection.execute(
        "INSERT OR IGNORE INTO reactions (room_id, target_event_id, sender_name, key) VALUES (?1, ?2, ?3, ?4)",
        params![room_id, reaction.target_event_id, reaction.sender_name, reaction.key],
    )
}

/// Inserts a message from an old log file.
pub fn import_entry(transaction: &Transaction<'_>, entry: &LoggedMessage) -> Result<()> {
    insert_message(
        transaction,
        &NewMessage {
            room_id: &entry.room_id,
            event_id: &entry.event_id,
            sender: &entry.sender,
            sender_name: entry.sender_name.as_deref(),
            origin_server_ts_ms: entry.origin_server_ts_ms,
            logged_at: &entry.logged_at,
            body: &entry.body,
            has_image: entry.has_image,
            analysis_json: to_json(&entry.analysis)?,
            links_json: links_json(&entry.links)?,
            classify_prompt_json: optional_json(entry.classify_prompt.as_ref())?,
            classify_raw_json: optional_json(entry.classify_raw_json.as_ref())?,
            response_prompt_json: optional_json(entry.response_prompt.as_ref())?,
            response: entry.response.as_deref(),
            response_label: entry.response_label.as_deref(),
            response_event_id: entry.response_event_id.as_deref(),
            thread_root_id: entry.thread_root_id.as_deref(),
        },
    )
    .context("failed to import message")?;
    Ok(())
}

/// Inserts a reaction from an old log file.
pub fn import_reaction(transaction: &Transaction<'_>, room_id: &str, reaction: &ReactionRecord) -> Result<()> {
    insert_reaction(transaction, room_id, reaction).context("failed to import reaction")?;
    Ok(())
}

impl MessageLogger {
    pub fn new(database: Arc<Database>) -> Self {
        Self { database }
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
            links,
            analysis,
            classify_prompt,
            classify_raw_json,
            response,
            response_event_id,
            thread_root_id,
        } = params;

        let logged_at = chrono::Utc::now().to_rfc3339();
        let message = NewMessage {
            room_id,
            event_id,
            sender,
            sender_name,
            origin_server_ts_ms,
            logged_at: &logged_at,
            body,
            has_image,
            analysis_json: to_json(analysis)?,
            links_json: links_json(links)?,
            classify_prompt_json: optional_json(classify_prompt)?,
            classify_raw_json: optional_json(classify_raw_json)?,
            response_prompt_json: optional_json(response.and_then(|response| response.prompt.as_ref()))?,
            response: response.map(|response| response.text.as_str()),
            response_label: response.map(|response| response.label.as_str()),
            response_event_id,
            thread_root_id,
        };
        self.database.with(|connection| insert_message(connection, &message))?;
        Ok(())
    }

    /// The stored message with this event ID, without its prompts.
    pub fn get(&self, room_id: &str, event_id: &str) -> Result<Option<LoggedMessage>> {
        let sql = format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE room_id = ?1 AND event_id = ?2");
        let entry = self.database.with(|connection| {
            let mut statement = connection.prepare_cached(&sql)?;
            let mut rows = statement.query_map([room_id, event_id], |row| read_message(row, false))?;
            rows.next().transpose().map(Option::flatten)
        })?;
        Ok(entry)
    }

    /// Rewrites a message after its sender edited it, keeping its place in history.
    /// Whatever the edit didn't produce, like a reply when the bot stayed quiet, keeps
    /// its earlier value.
    pub fn record_edit(&self, edit: EditedMessage<'_>) -> Result<()> {
        let analysis_json = edit.analysis.map(to_json).transpose()?;
        let classify_prompt_json = optional_json(edit.classify_prompt)?;
        let classify_raw_json = optional_json(edit.classify_raw_json)?;
        let response_prompt_json = optional_json(edit.response.and_then(|response| response.prompt.as_ref()))?;
        let links_json = links_json(edit.links)?;
        self.database.with(|connection| {
            connection
                .prepare_cached(
                    "UPDATE messages SET body = ?3, links_json = ?4, analysis_json = COALESCE(?5, analysis_json), \
                     classify_prompt_json = COALESCE(?6, classify_prompt_json), \
                     classify_raw_json = COALESCE(?7, classify_raw_json), \
                     response_prompt_json = COALESCE(?8, response_prompt_json), response = COALESCE(?9, response), \
                     response_label = COALESCE(?10, response_label), \
                     response_event_id = COALESCE(?11, response_event_id), \
                     thread_root_id = COALESCE(thread_root_id, ?12) \
                     WHERE room_id = ?1 AND event_id = ?2",
                )?
                .execute(params![
                    edit.room_id,
                    edit.event_id,
                    edit.body,
                    links_json,
                    analysis_json,
                    classify_prompt_json,
                    classify_raw_json,
                    response_prompt_json,
                    edit.response.map(|response| response.text.as_str()),
                    edit.response.map(|response| response.label.as_str()),
                    edit.response_event_id,
                    edit.thread_root_id,
                ])
        })?;
        Ok(())
    }

    /// Records a reaction. It's its own row because it can arrive long after the
    /// message it's on, and can also be on the bot's reply rather than the message.
    pub fn log_reaction(&self, room_id: &str, target_event_id: &str, sender_name: &str, key: &str) -> Result<()> {
        let reaction = ReactionRecord {
            target_event_id: target_event_id.to_string(),
            sender_name: sender_name.to_string(),
            key: key.to_string(),
        };
        self.database.with(|connection| insert_reaction(connection, room_id, &reaction))?;
        Ok(())
    }

    /// Up to `limit` most recent messages for `room_id`, oldest first, without their
    /// prompts.
    pub fn recent(&self, room_id: &str, limit: usize) -> Result<Vec<LoggedMessage>> {
        self.read_recent(room_id, limit, false)
    }

    /// `recent`, with the prompts behind each classification and reply.
    pub fn recent_with_prompts(&self, room_id: &str, limit: usize) -> Result<Vec<LoggedMessage>> {
        self.read_recent(room_id, limit, true)
    }

    fn read_recent(&self, room_id: &str, limit: usize, with_prompts: bool) -> Result<Vec<LoggedMessage>> {
        let columns = if with_prompts { format!("{MESSAGE_COLUMNS}, {PROMPT_COLUMNS}") } else { MESSAGE_COLUMNS.to_string() };
        let sql = format!("SELECT {columns} FROM messages WHERE room_id = ?1 ORDER BY id DESC LIMIT ?2");

        self.database.with(|connection| {
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let mut statement = connection.prepare_cached(&sql)?;
            let mut entries = statement
                .query_map(params![room_id, limit], |row| read_message(row, with_prompts))?
                .filter_map(Result::transpose)
                .collect::<rusqlite::Result<Vec<_>>>()?;
            entries.reverse();
            for entry in &mut entries {
                attach_reactions(connection, entry)?;
            }
            Ok(entries)
        })
    }

    /// How many messages are stored for `room_id`.
    pub fn count(&self, room_id: &str) -> Result<usize> {
        let count: i64 = self.database.with(|connection| {
            connection.query_row("SELECT COUNT(*) FROM messages WHERE room_id = ?1", [room_id], |row| row.get(0))
        })?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Up to `limit` most recent messages of a thread, oldest first. Includes the root,
    /// which may have been logged before anyone replied to it, and the message whose
    /// reply is the root when someone starts a thread on something the bot said.
    pub fn thread(&self, room_id: &str, root_id: &str, limit: usize) -> Result<Vec<LoggedMessage>> {
        let sql = format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE room_id = ?1 \
             AND (thread_root_id = ?2 OR event_id = ?2 OR response_event_id = ?2) ORDER BY id DESC LIMIT ?3"
        );

        self.database.with(|connection| {
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let mut statement = connection.prepare_cached(&sql)?;
            let mut entries = statement
                .query_map(params![room_id, root_id, limit], |row| read_message(row, false))?
                .filter_map(Result::transpose)
                .collect::<rusqlite::Result<Vec<_>>>()?;
            entries.reverse();
            for entry in &mut entries {
                attach_reactions(connection, entry)?;
            }
            Ok(entries)
        })
    }

    /// The messages in the room's whole history that best match `query`, oldest
    /// first. Ranked by how many of the query's words appear in the sender's name,
    /// the message, the bot's reply, or a linked page's title and description, with
    /// newer messages winning ties.
    pub fn search(&self, room_id: &str, query: &str, limit: usize) -> Result<Vec<LoggedMessage>> {
        let mut terms = search_terms(query);
        terms.truncate(MAX_SEARCH_TERMS);
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        let searchable = "lower(body || ' ' || sender || ' ' || coalesce(sender_name, '') || ' ' || \
            coalesce(response, '') || ' ' || coalesce(links_json, ''))";
        let conditions = (0..terms.len())
            .map(|index| format!("instr({searchable}, ?{}) > 0", index + 2))
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE room_id = ?1 AND ({conditions})");

        let candidates = self.database.with(|connection| {
            let mut bound: Vec<&dyn rusqlite::ToSql> = vec![&room_id];
            bound.extend(terms.iter().map(|term| term as &dyn rusqlite::ToSql));

            let mut statement = connection.prepare(&sql)?;
            statement
                .query_map(bound.as_slice(), |row| read_message(row, false))?
                .filter_map(Result::transpose)
                .collect::<rusqlite::Result<Vec<_>>>()
        })?;

        let mut scored: Vec<(usize, LoggedMessage)> = candidates
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

        self.database.with(|connection| {
            for entry in &mut matches {
                attach_reactions(connection, entry)?;
            }
            Ok(())
        })?;
        Ok(matches)
    }
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
    let mut text = format!("{} {} {}", entry.display_name(), entry.body, entry.response.as_deref().unwrap_or_default());
    for link in &entry.links {
        text.push_str(&format!(" {} {}", link.title.as_deref().unwrap_or_default(), link.description.as_deref().unwrap_or_default()));
    }
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
         `{{link: ...}}` shows what a posted link is, and `{{reactions: ...}}` is how people reacted):\n{}\n",
        format_transcript(history, bot_name)
    )
}

/// Formats a thread for a reply inside it, or an empty string if there isn't one.
pub fn format_thread_block(thread: &[LoggedMessage], bot_name: &str) -> String {
    if thread.is_empty() {
        return String::new();
    }
    format!("This thread so far, which is the conversation you're in (same format, oldest first):\n{}\n", format_transcript(thread, bot_name))
}

/// One line per message, oldest first. The bot's own replies get their own lines,
/// because they're stored on the message that triggered them and would otherwise
/// be invisible, leaving it unable to follow up on anything it said.
pub fn format_transcript(history: &[LoggedMessage], bot_name: &str) -> String {
    let mut transcript = String::new();
    for entry in history {
        let time = format_timestamp(entry.origin_server_ts_ms);
        let reactions = format_reactions(&entry.reactions);
        let links = format_links(&entry.links);
        transcript.push_str(&format!("[{time}] [{}] {}{links}{reactions}\n", entry.display_name(), single_line(&entry.body)));

        if let Some(reply) = conversational_reply(entry) {
            let reactions = format_reactions(&entry.response_reactions);
            transcript.push_str(&format!("[{time}] [{bot_name}] {}{reactions}\n", clip(&single_line(reply))));
        }
    }
    transcript
}

/// What a message's links turned out to be, for the end of its transcript line. The
/// page text is left out: it's only sent for the links being talked about right now.
pub fn format_links(links: &[LinkPreview]) -> String {
    links
        .iter()
        .filter_map(|link| {
            let about = [link.title.as_deref(), link.description.as_deref()].into_iter().flatten().collect::<Vec<_>>();
            (!about.is_empty()).then(|| format!(" {{link: {}}}", single_line(&about.join(" — "))))
        })
        .collect()
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
            links: Vec::new(),
            response_event_id: None,
            thread_root_id: None,
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

    fn test_logger(_label: &str) -> MessageLogger {
        MessageLogger::new(Arc::new(Database::open_in_memory()))
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
                links: &[],
                analysis: &analysis,
                classify_prompt: None,
                classify_raw_json: None,
                response: reply.as_ref(),
                response_event_id: response.map(|(_, id)| id),
                thread_root_id: None,
            })
            .unwrap();
    }

    fn edited<'a>(event_id: &'a str, body: &'a str) -> EditedMessage<'a> {
        EditedMessage {
            room_id: "!r",
            event_id,
            body,
            links: &[],
            analysis: None,
            classify_prompt: None,
            classify_raw_json: None,
            response: None,
            response_event_id: None,
            thread_root_id: None,
        }
    }

    #[test]
    fn an_edit_rewrites_the_message_in_place() {
        let logger = test_logger("edits");
        log_line(&logger, "$1", "@bob:x", "gordy whats teh capital of autralia", 1, Some(("Canberra", "$reply")));
        log_line(&logger, "$2", "@alice:x", "later", 2, None);

        // Answered again: the reply text changes, and it's still the same reply event.
        let answer = GeneratedReply::plain("Vienna", "chat");
        logger
            .record_edit(EditedMessage {
                response: Some(&answer),
                response_event_id: Some("$reply"),
                thread_root_id: Some("$1"),
                ..edited("$1", "gordy what's the capital of austria")
            })
            .unwrap();

        let entry = logger.get("!r", "$1").unwrap().expect("still logged");
        assert_eq!(entry.body, "gordy what's the capital of austria");
        assert_eq!(entry.response.as_deref(), Some("Vienna"));
        assert_eq!(entry.response_event_id.as_deref(), Some("$reply"));
        assert_eq!(entry.thread_root_id.as_deref(), Some("$1"));
        let order: Vec<String> = logger.recent("!r", 10).unwrap().into_iter().map(|entry| entry.event_id).collect();
        assert_eq!(order, ["$1", "$2"]);

        // Not answered this time: the earlier answer stays.
        logger.record_edit(edited("$1", "gordy nvm")).unwrap();
        let entry = logger.get("!r", "$1").unwrap().unwrap();
        assert_eq!(entry.body, "gordy nvm");
        assert_eq!(entry.response.as_deref(), Some("Vienna"));
        assert_eq!(entry.thread_root_id.as_deref(), Some("$1"));

        assert!(logger.get("!r", "$missing").unwrap().is_none());
    }

    #[test]
    fn a_thread_is_its_root_and_replies_without_the_rest_of_the_room() {
        let logger = test_logger("threads");
        let analysis = logged_message("@alice:x", "").analysis;
        let log = |event_id: &str, body: &str, thread_root_id: Option<&str>, response_event_id: Option<&str>| {
            logger
                .log(MessageLogParams {
                    room_id: "!r",
                    event_id,
                    sender: "@alice:x",
                    sender_name: None,
                    origin_server_ts_ms: 1,
                    body,
                    has_image: false,
                    links: &[],
                    analysis: &analysis,
                    classify_prompt: None,
                    classify_raw_json: None,
                    response: None,
                    response_event_id,
                    thread_root_id,
                })
                .unwrap();
        };
        log("$root", "the root", None, None);
        log("$other", "something else", None, None);
        log("$reply", "in the thread", Some("$root"), None);
        log("$greeted", "hello", None, Some("$bot"));
        log("$on-bot", "about that", Some("$bot"), None);

        let bodies = |root: &str| -> Vec<String> {
            logger.thread("!r", root, 10).unwrap().into_iter().map(|entry| entry.body).collect()
        };
        assert_eq!(bodies("$root"), ["the root", "in the thread"]);
        assert_eq!(bodies("$bot"), ["hello", "about that"]);
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
    fn logging_the_same_event_twice_keeps_one_row() {
        let logger = test_logger("duplicates");
        log_line(&logger, "$1", "@alice:x", "hello", 1, None);
        log_line(&logger, "$1", "@alice:x", "hello", 1, None);
        assert_eq!(logger.recent("!r", 10).unwrap().len(), 1);
    }

    #[test]
    fn recent_returns_the_newest_messages_oldest_first() {
        let logger = test_logger("recent");
        for (index, event_id) in ["$1", "$2", "$3"].iter().enumerate() {
            log_line(&logger, event_id, "@alice:x", "hi", index as i64, None);
        }
        let ids: Vec<String> = logger.recent("!r", 2).unwrap().into_iter().map(|entry| entry.event_id).collect();
        assert_eq!(ids, vec!["$2", "$3"]);
    }

    #[test]
    fn prompts_are_only_read_when_asked_for() {
        let logger = test_logger("prompts");
        let analysis = logged_message("@alice:x", "hi").analysis;
        let prompt = PromptRecord { system: "s".to_string(), user: "u".to_string() };
        logger
            .log(MessageLogParams {
                room_id: "!r",
                event_id: "$1",
                sender: "@alice:x",
                sender_name: None,
                origin_server_ts_ms: 1,
                body: "hi",
                has_image: false,
                links: &[],
                analysis: &analysis,
                classify_prompt: Some(&prompt),
                classify_raw_json: None,
                response: None,
                response_event_id: None,
                thread_root_id: None,
            })
            .unwrap();

        assert!(logger.recent("!r", 1).unwrap()[0].classify_prompt.is_none());
        assert!(logger.recent_with_prompts("!r", 1).unwrap()[0].classify_prompt.is_some());
    }

    #[test]
    fn rooms_are_kept_apart() {
        let logger = test_logger("rooms");
        log_line(&logger, "$1", "@alice:x", "secret", 1, None);
        assert!(logger.recent("!other", 10).unwrap().is_empty());
        assert!(logger.search("!other", "secret", 10).unwrap().is_empty());
    }

    #[test]
    fn repeated_reactions_count_once() {
        let logger = test_logger("reaction-dupes");
        log_line(&logger, "$1", "@alice:x", "hi", 1, None);
        logger.log_reaction("!r", "$1", "bob", "👍").unwrap();
        logger.log_reaction("!r", "$1", "bob", "👍").unwrap();
        assert_eq!(logger.recent("!r", 1).unwrap()[0].reactions.len(), 1);
    }

    #[test]
    fn search_with_only_stopwords_finds_nothing() {
        let logger = test_logger("search-empty");
        log_line(&logger, "$1", "@alice:x", "what is the point", 1, None);
        assert!(logger.search("!r", "what is the", 5).unwrap().is_empty());
    }

    #[test]
    fn transcript_says_what_a_link_is_without_the_page_text() {
        let mut entry = logged_message("@alice:example.org", "read this https://example.com/a");
        entry.links = vec![LinkPreview {
            url: "https://example.com/a".to_string(),
            title: Some("Big news".to_string()),
            description: Some("Something happened".to_string()),
            text: Some("The full article".to_string()),
            image: false,
        }];

        let transcript = format_transcript(&[entry], "gordy");
        assert!(transcript.contains("https://example.com/a {link: Big news — Something happened}"), "{transcript}");
        assert!(!transcript.contains("The full article"), "{transcript}");
    }

    #[test]
    fn old_log_lines_without_links_still_parse() {
        let entry = logged_message("@alice:x", "hi");
        let mut value = serde_json::to_value(&entry).unwrap();
        value.as_object_mut().unwrap().remove("links");
        assert!(serde_json::from_value::<LoggedMessage>(value).unwrap().links.is_empty());
    }
}
