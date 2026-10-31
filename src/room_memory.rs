use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use threatflux_anthropic_sdk::Client as Anthropic;
use tracing::warn;

use crate::classify;
use crate::llm::{self, CompletionRequest, LocalTools};
use crate::message_log::{LoggedMessage, format_transcript};
use crate::throttle::Throttle;
use crate::usage::UsageTracker;

/// Older messages needed before a summary is worth a Claude call at all.
const MIN_MESSAGES_TO_SUMMARIZE: usize = 15;

/// New older messages tolerated before a cached summary is regenerated.
const REFRESH_AFTER_MESSAGES: usize = 25;

/// Keeps what's fed into every chat prompt bounded.
const MAX_FACTS_PER_ROOM: usize = 200;
const MAX_FACT_CHARS: usize = 300;

const USAGE_LABEL: &str = "room_summary";

const SYSTEM_PROMPT: &str = "You keep the long-term memory of a Matrix chat room's resident bot. Given a \
transcript, write short notes the bot can use later: ongoing topics, running jokes, what each regular \
has been up to, and anything people said they'd do. Attribute everything to the person who said it. \
Record only what's in the transcript. At most 150 words, plain sentences.";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedSummary {
    /// The newest message this summary covers.
    through_event_id: String,
    text: String,
}

/// Something the bot was told to remember. Facts belong to the room they were
/// learned in, so what's said in one room doesn't surface in another.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: u64,
    pub room_id: String,
    /// Who it's about, if anyone in particular.
    pub about: Option<String>,
    pub text: String,
    pub told_by: String,
    pub added_at: String,
}

impl Fact {
    fn describe(&self) -> String {
        let subject = self.about.as_deref().map(|about| format!(" about {about}")).unwrap_or_default();
        format!("#{}{subject} (from {}): {}", self.id, self.told_by, self.text)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MemoryState {
    #[serde(default)]
    summaries: HashMap<String, CachedSummary>,
    #[serde(default)]
    facts: Vec<Fact>,
    #[serde(default)]
    next_fact_id: u64,
}

/// The bot's memory beyond the message log: rolling per-room summaries of older
/// chat, and facts people asked it to keep. Persisted as one JSON file, rewritten
/// whole on each change, which is fine at the rate this changes.
pub struct RoomMemory {
    path: PathBuf,
    state: Mutex<MemoryState>,
}

impl RoomMemory {
    pub fn open(path: &Path) -> Result<Self> {
        let state = match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => MemoryState::default(),
            Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
        };
        Ok(Self { path: path.to_path_buf(), state: Mutex::new(state) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Writes to a temp file first, so a crash mid-write can't corrupt the memory.
    fn save(&self, state: &MemoryState) -> Result<()> {
        let raw = serde_json::to_string_pretty(state).context("failed to serialize memory")?;
        let temp_path = self.path.with_extension("json.tmp");
        std::fs::write(&temp_path, raw).with_context(|| format!("failed to write {}", temp_path.display()))?;
        std::fs::rename(&temp_path, &self.path).with_context(|| format!("failed to replace {}", self.path.display()))
    }

    pub fn remember(&self, room_id: &str, about: Option<&str>, text: &str, told_by: &str) -> Result<u64, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("the fact is empty".to_string());
        }
        if text.chars().count() > MAX_FACT_CHARS {
            return Err(format!("the fact is over {MAX_FACT_CHARS} characters; shorten it"));
        }

        let mut state = self.lock();
        if state.facts.iter().filter(|fact| fact.room_id == room_id).count() >= MAX_FACTS_PER_ROOM {
            return Err(format!("memory is full ({MAX_FACTS_PER_ROOM} facts); forget something first"));
        }

        state.next_fact_id += 1;
        let id = state.next_fact_id;
        state.facts.push(Fact {
            id,
            room_id: room_id.to_string(),
            about: about.map(str::trim).filter(|about| !about.is_empty()).map(String::from),
            text: text.to_string(),
            told_by: told_by.to_string(),
            added_at: chrono::Utc::now().to_rfc3339(),
        });
        self.save(&state).map_err(|err| format!("couldn't save: {err}"))?;
        Ok(id)
    }

    pub fn forget(&self, room_id: &str, id: u64) -> Result<(), String> {
        let mut state = self.lock();
        let position = state
            .facts
            .iter()
            .position(|fact| fact.id == id && fact.room_id == room_id)
            .ok_or_else(|| format!("no fact #{id} in this room"))?;
        state.facts.remove(position);
        self.save(&state).map_err(|err| format!("couldn't save: {err}"))
    }

    pub fn facts(&self, room_id: &str) -> Vec<Fact> {
        self.lock().facts.iter().filter(|fact| fact.room_id == room_id).cloned().collect()
    }

    /// The room's facts as one line each, or `None` if there aren't any.
    pub fn describe_facts(&self, room_id: &str) -> Option<String> {
        let facts = self.facts(room_id);
        (!facts.is_empty()).then(|| facts.iter().map(Fact::describe).collect::<Vec<_>>().join("\n"))
    }

    /// A summary of `older` (oldest first), refreshed only once enough new messages
    /// have aged into it. A failed refresh falls back to the previous summary, since
    /// this is optional context and must never cost the bot its reply.
    pub async fn summary_of(
        &self,
        room_id: &str,
        older: &[LoggedMessage],
        bot_name: &str,
        client: &Anthropic,
        usage_tracker: &UsageTracker,
        rate_limit: &Throttle,
    ) -> Option<String> {
        let cached = self.lock().summaries.get(room_id).cloned();
        let Some(newest) = older.last() else {
            return cached.map(|summary| summary.text);
        };

        let unseen = match &cached {
            Some(summary) => older.iter().rev().take_while(|entry| entry.event_id != summary.through_event_id).count(),
            None => older.len(),
        };
        if !needs_refresh(cached.is_some(), unseen, older.len()) {
            return cached.map(|summary| summary.text);
        }

        let transcript = format_transcript(older, bot_name);
        let request = CompletionRequest {
            model: classify::MODEL,
            system: SYSTEM_PROMPT,
            user: &transcript,
            max_tokens: 1024,
            label: USAGE_LABEL,
            ..Default::default()
        };

        match llm::complete(client, &request, usage_tracker, rate_limit).await {
            Ok(text) => {
                let mut state = self.lock();
                state.summaries.insert(
                    room_id.to_string(),
                    CachedSummary { through_event_id: newest.event_id.clone(), text: text.clone() },
                );
                if let Err(err) = self.save(&state) {
                    warn!(?err, room_id, "failed to persist room summary");
                }
                Some(text)
            }
            Err(err) => {
                warn!(?err, room_id, "failed to refresh room summary");
                cached.map(|summary| summary.text)
            }
        }
    }
}

fn needs_refresh(has_summary: bool, unseen: usize, total: usize) -> bool {
    if has_summary { unseen >= REFRESH_AFTER_MESSAGES } else { total >= MIN_MESSAGES_TO_SUMMARIZE }
}

/// The `remember`/`forget` tools offered to chat replies, scoped to one room and
/// attributed to whoever the bot is answering.
pub struct MemoryTools<'a> {
    pub memory: &'a RoomMemory,
    pub room_id: &'a str,
    pub told_by: &'a str,
}

impl LocalTools for MemoryTools<'_> {
    fn definitions(&self) -> Vec<Value> {
        vec![
            json!({
                "name": "remember",
                "description": "Save a fact for future conversations in this room. Use it when someone asks you \
                    to remember something, or states something lasting about themselves. Not for passing remarks.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "fact": {"type": "string", "description": "The fact, as a short standalone sentence."},
                        "about": {"type": "string", "description": "The person it's about, if any."}
                    },
                    "required": ["fact"]
                }
            }),
            json!({
                "name": "forget",
                "description": "Delete a saved fact by its number, when someone asks you to forget it or it's wrong.",
                "input_schema": {
                    "type": "object",
                    "properties": {"id": {"type": "integer", "description": "The fact's number, without the #."}},
                    "required": ["id"]
                }
            }),
        ]
    }

    fn run(&self, name: &str, input: &Value) -> Result<String, String> {
        match name {
            "remember" => {
                let fact = input["fact"].as_str().ok_or("fact is required")?;
                let id = self.memory.remember(self.room_id, input["about"].as_str(), fact, self.told_by)?;
                Ok(format!("Saved as #{id}."))
            }
            "forget" => {
                let id = input["id"].as_u64().ok_or("id is required")?;
                self.memory.forget(self.room_id, id)?;
                Ok(format!("Forgot #{id}."))
            }
            other => Err(format!("unknown tool {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("room-memory-{label}-{nanos}.json"))
    }

    #[test]
    fn skips_rooms_without_enough_history_to_be_worth_a_call() {
        assert!(!needs_refresh(false, 14, 14));
        assert!(needs_refresh(false, 15, 15));
    }

    #[test]
    fn keeps_a_summary_until_enough_new_messages_arrive() {
        assert!(!needs_refresh(true, 24, 200));
        assert!(needs_refresh(true, 25, 200));
    }

    #[test]
    fn facts_survive_a_restart() {
        let path = temp_path("restart");
        let memory = RoomMemory::open(&path).unwrap();
        memory.remember("!a", Some("procyon"), "hates cilantro", "phizz").unwrap();

        let reopened = RoomMemory::open(&path).unwrap();
        assert_eq!(reopened.describe_facts("!a").unwrap(), "#1 about procyon (from phizz): hates cilantro");
        // Ids keep counting, so a forgotten number never comes back as a new fact.
        assert_eq!(reopened.remember("!a", None, "second", "phizz").unwrap(), 2);
    }

    #[test]
    fn facts_stay_in_their_own_room() {
        let memory = RoomMemory::open(&temp_path("rooms")).unwrap();
        let id = memory.remember("!a", None, "a secret", "phizz").unwrap();

        assert!(memory.describe_facts("!b").is_none());
        assert!(memory.forget("!b", id).is_err());
        assert!(memory.forget("!a", id).is_ok());
        assert!(memory.describe_facts("!a").is_none());
    }

    #[test]
    fn rejects_empty_and_oversized_facts() {
        let memory = RoomMemory::open(&temp_path("limits")).unwrap();
        assert!(memory.remember("!a", None, "   ", "phizz").is_err());
        assert!(memory.remember("!a", None, &"x".repeat(MAX_FACT_CHARS + 1), "phizz").is_err());
    }

    #[test]
    fn tools_attribute_facts_to_the_person_being_answered() {
        let memory = RoomMemory::open(&temp_path("tools")).unwrap();
        let tools = MemoryTools { memory: &memory, room_id: "!a", told_by: "oshuma" };

        assert_eq!(tools.run("remember", &json!({"fact": "likes tea"})).unwrap(), "Saved as #1.");
        assert_eq!(memory.facts("!a")[0].told_by, "oshuma");
        assert_eq!(tools.run("forget", &json!({"id": 1})).unwrap(), "Forgot #1.");
        assert!(tools.run("forget", &json!({"id": 1})).is_err());
    }
}
