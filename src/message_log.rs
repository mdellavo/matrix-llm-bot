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

/// A single logged message: the metadata the bot already has from the Matrix
/// event, plus the `MessageAnalysis` produced by the classifier.
#[derive(Debug, Serialize)]
struct MessageLogEntry<'a> {
    logged_at: String,
    room_id: &'a str,
    event_id: &'a str,
    sender: &'a str,
    origin_server_ts_ms: i64,
    body: &'a str,
    analysis: &'a MessageAnalysis,
}

/// Owned counterpart of `MessageLogEntry`, for reading log lines back — also
/// serialized directly as the JSON response of the status server's message API
/// (`src/status_server.rs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggedMessage {
    pub logged_at: String,
    pub room_id: String,
    pub event_id: String,
    pub sender: String,
    pub origin_server_ts_ms: i64,
    pub body: String,
    pub analysis: MessageAnalysis,
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

    #[allow(clippy::too_many_arguments)]
    pub fn log(
        &self,
        room_id: &str,
        event_id: &str,
        sender: &str,
        origin_server_ts_ms: i64,
        body: &str,
        analysis: &MessageAnalysis,
    ) -> Result<()> {
        let entry = MessageLogEntry {
            logged_at: chrono::Utc::now().to_rfc3339(),
            room_id,
            event_id,
            sender,
            origin_server_ts_ms,
            body,
            analysis,
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
