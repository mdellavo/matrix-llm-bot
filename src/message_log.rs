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

/// The outcome of `handler::generate_response`: the text actually sent (or a
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
    origin_server_ts_ms: i64,
    body: &'a str,
    analysis: &'a MessageAnalysis,
    classify_prompt: &'a PromptRecord,
    classify_raw_json: &'a serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_prompt: Option<&'a PromptRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_label: Option<&'a str>,
}

/// Bundles every argument `MessageLogger::log` needs — grown too large for a
/// plain parameter list once prompts/responses joined the metadata/analysis
/// already there (matches the `HandlerContext` bundling in `handler.rs` for
/// the same reason).
pub struct MessageLogParams<'a> {
    pub room_id: &'a str,
    pub event_id: &'a str,
    pub sender: &'a str,
    pub origin_server_ts_ms: i64,
    pub body: &'a str,
    pub analysis: &'a MessageAnalysis,
    pub classify_prompt: &'a PromptRecord,
    pub classify_raw_json: &'a serde_json::Value,
    /// `Some` whenever the bot generated (or attempted to generate) a reply —
    /// `None` when the message never reached response generation at all (e.g.
    /// `requires_response` was false).
    pub response: Option<&'a GeneratedReply>,
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
    pub origin_server_ts_ms: i64,
    pub body: String,
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
        let MessageLogParams { room_id, event_id, sender, origin_server_ts_ms, body, analysis, classify_prompt, classify_raw_json, response } =
            params;

        let entry = MessageLogEntry {
            logged_at: chrono::Utc::now().to_rfc3339(),
            room_id,
            event_id,
            sender,
            origin_server_ts_ms,
            body,
            analysis,
            classify_prompt,
            classify_raw_json,
            response_prompt: response.and_then(|response| response.prompt.as_ref()),
            response: response.map(|response| response.text.as_str()),
            response_label: response.map(|response| response.label.as_str()),
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

    /// Returns up to `limit` most recent logged messages for `room_id`, oldest first.
    /// Reads and parses the room's log file from disk on every call — there is no
    /// in-memory cache (see the "disk-only" note in the module docs above).
    pub fn recent(&self, room_id: &str, limit: usize) -> Result<Vec<LoggedMessage>> {
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

        let start = entries.len().saturating_sub(limit);
        Ok(entries.split_off(start))
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

/// Formats `history` as a "recent room history" block, one line per message as
/// `[sender] message` (oldest first), or an empty string if `history` is empty.
/// Shared by every Claude call grounded in recent room messages — chat/greeting
/// replies (`handler.rs`) and message classification (`classify.rs`) — so the
/// transcript format (and the misattribution fix below) stays consistent
/// everywhere it's used.
pub fn format_history_block(history: &[LoggedMessage]) -> String {
    if history.is_empty() {
        return String::new();
    }
    let mut block = String::from("Recent room history (one line per message, oldest first, as `[sender] message`):\n");
    for entry in history {
        block.push_str(&format!("[{}] {}\n", entry.sender, single_line(&entry.body)));
    }
    block.push('\n');
    block
}

/// Collapses embedded newlines/repeated whitespace to spaces so a multi-line
/// message can never be mistaken for more than one transcript line — without
/// this, a message containing `\n` would produce a continuation line with no
/// `[sender]` prefix at all, which is exactly the kind of thing that gets a
/// model to misattribute a line to the wrong speaker or the current sender.
pub fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
        }
    }

    #[test]
    fn format_history_block_is_empty_for_no_history() {
        assert_eq!(format_history_block(&[]), "");
    }

    #[test]
    fn format_history_block_lists_one_line_per_message_oldest_first() {
        let history = vec![logged_message("@alice:example.org", "hi everyone"), logged_message("@bob:example.org", "yo")];
        let block = format_history_block(&history);

        assert!(block.starts_with("Recent room history"), "{block}");
        let alice_pos = block.find("[@alice:example.org] hi everyone").expect("alice line present");
        let bob_pos = block.find("[@bob:example.org] yo").expect("bob line present");
        assert!(alice_pos < bob_pos, "{block}");
        assert!(block.ends_with('\n'), "{block}");
    }

    #[test]
    fn format_history_block_collapses_embedded_newlines_so_one_line_is_one_message() {
        let history = vec![logged_message("@alice:example.org", "line one\nline two")];
        let block = format_history_block(&history);

        // Without collapsing, "line two" would appear on its own line with no
        // `[sender]` prefix — indistinguishable from an unattributed line.
        assert!(block.contains("[@alice:example.org] line one line two"), "{block}");
        assert_eq!(block.lines().filter(|line| line.contains("line one") || line.contains("line two")).count(), 1, "{block}");
    }

    #[test]
    fn single_line_collapses_newlines_and_repeated_whitespace() {
        assert_eq!(single_line("line one\nline two"), "line one line two");
        assert_eq!(single_line("a\r\nb"), "a b");
        assert_eq!(single_line("no newlines here"), "no newlines here");
    }
}
