use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use threatflux_anthropic_sdk::Client as Anthropic;
use tracing::warn;

use crate::classify;
use crate::db::Database;
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

struct CachedSummary {
    /// The newest message this summary covers.
    through_event_id: String,
    text: String,
}

/// Something the bot was told to remember. Facts belong to the room they were
/// learned in, so what's said in one room doesn't surface in another.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: i64,
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

/// Saves a room's summary, replacing any earlier one.
pub fn save_summary(connection: &Connection, room_id: &str, through_event_id: &str, text: &str) -> Result<()> {
    connection
        .execute(
            "INSERT INTO room_summaries (room_id, through_event_id, text) VALUES (?1, ?2, ?3) \
             ON CONFLICT (room_id) DO UPDATE SET through_event_id = excluded.through_event_id, text = excluded.text",
            params![room_id, through_event_id, text],
        )
        .context("failed to save room summary")?;
    Ok(())
}

/// Inserts a fact from the old memory file, keeping its number.
pub fn import_fact(transaction: &Transaction<'_>, fact: &Fact) -> Result<()> {
    transaction
        .execute(
            "INSERT OR IGNORE INTO facts (id, room_id, about, text, told_by, added_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![fact.id, fact.room_id, fact.about, fact.text, fact.told_by, fact.added_at],
        )
        .context("failed to import fact")?;
    Ok(())
}

/// Makes new fact numbers start after `last_id`, so a forgotten number never returns.
pub fn reserve_fact_ids(transaction: &Transaction<'_>, last_id: i64) -> Result<()> {
    let updated = transaction
        .execute("UPDATE sqlite_sequence SET seq = max(seq, ?1) WHERE name = 'facts'", [last_id])
        .context("failed to reserve fact numbers")?;
    if updated == 0 {
        transaction
            .execute("INSERT INTO sqlite_sequence (name, seq) VALUES ('facts', ?1)", [last_id])
            .context("failed to reserve fact numbers")?;
    }
    Ok(())
}

/// The bot's memory beyond the message log, stored in its database: rolling per-room
/// summaries of older chat, and facts people asked it to keep.
pub struct RoomMemory {
    database: Arc<Database>,
}

impl RoomMemory {
    pub fn new(database: Arc<Database>) -> Self {
        Self { database }
    }

    pub fn remember(&self, room_id: &str, about: Option<&str>, text: &str, told_by: &str) -> Result<i64, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("the fact is empty".to_string());
        }
        if text.chars().count() > MAX_FACT_CHARS {
            return Err(format!("the fact is over {MAX_FACT_CHARS} characters; shorten it"));
        }

        let about = about.map(str::trim).filter(|about| !about.is_empty());
        let added_at = chrono::Utc::now().to_rfc3339();

        // In one transaction so the count can't go stale before the insert.
        let result = self.database.transaction(|transaction| {
            let count: usize = transaction
                .query_row("SELECT COUNT(*) FROM facts WHERE room_id = ?1", [room_id], |row| row.get(0))
                .context("failed to count facts")?;
            if count >= MAX_FACTS_PER_ROOM {
                return Ok(None);
            }
            transaction
                .execute(
                    "INSERT INTO facts (room_id, about, text, told_by, added_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![room_id, about, text, told_by, added_at],
                )
                .context("failed to save fact")?;
            Ok(Some(transaction.last_insert_rowid()))
        });

        match result {
            Ok(Some(id)) => Ok(id),
            Ok(None) => Err(format!("memory is full ({MAX_FACTS_PER_ROOM} facts); forget something first")),
            Err(err) => Err(format!("couldn't save: {err}")),
        }
    }

    pub fn forget(&self, room_id: &str, id: i64) -> Result<(), String> {
        let deleted = self
            .database
            .with(|connection| connection.execute("DELETE FROM facts WHERE id = ?1 AND room_id = ?2", params![id, room_id]))
            .map_err(|err| format!("couldn't save: {err}"))?;
        if deleted == 0 {
            return Err(format!("no fact #{id} in this room"));
        }
        Ok(())
    }

    pub fn facts(&self, room_id: &str) -> Vec<Fact> {
        let result = self.database.with(|connection| {
            let mut statement = connection.prepare_cached(
                "SELECT id, room_id, about, text, told_by, added_at FROM facts WHERE room_id = ?1 ORDER BY id",
            )?;
            statement
                .query_map([room_id], |row| {
                    Ok(Fact {
                        id: row.get(0)?,
                        room_id: row.get(1)?,
                        about: row.get(2)?,
                        text: row.get(3)?,
                        told_by: row.get(4)?,
                        added_at: row.get(5)?,
                    })
                })?
                .collect()
        });
        result.unwrap_or_else(|err| {
            warn!(?err, room_id, "failed to load facts");
            Vec::new()
        })
    }

    /// The room's facts as one line each, or `None` if there aren't any.
    pub fn describe_facts(&self, room_id: &str) -> Option<String> {
        let facts = self.facts(room_id);
        (!facts.is_empty()).then(|| facts.iter().map(Fact::describe).collect::<Vec<_>>().join("\n"))
    }

    fn cached_summary(&self, room_id: &str) -> Option<CachedSummary> {
        let result = self.database.with(|connection| {
            connection
                .query_row("SELECT through_event_id, text FROM room_summaries WHERE room_id = ?1", [room_id], |row| {
                    Ok(CachedSummary { through_event_id: row.get(0)?, text: row.get(1)? })
                })
                .optional()
        });
        result.unwrap_or_else(|err| {
            warn!(?err, room_id, "failed to load room summary");
            None
        })
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
        let cached = self.cached_summary(room_id);
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
                let saved = self.database.transaction(|transaction| save_summary(transaction, room_id, &newest.event_id, &text));
                if let Err(err) = saved {
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
                let id = input["id"].as_i64().ok_or("id is required")?;
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

    fn memory() -> RoomMemory {
        RoomMemory::new(Arc::new(Database::open_in_memory()))
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
        let path = std::env::temp_dir().join(format!("room-memory-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let memory = RoomMemory::new(Arc::new(Database::open(&path).unwrap()));
        memory.remember("!a", Some("procyon"), "hates cilantro", "phizz").unwrap();
        memory.forget("!a", memory.remember("!a", None, "gone", "phizz").unwrap()).unwrap();
        drop(memory);

        let reopened = RoomMemory::new(Arc::new(Database::open(&path).unwrap()));
        assert_eq!(reopened.describe_facts("!a").unwrap(), "#1 about procyon (from phizz): hates cilantro");
        // Ids keep counting, so a forgotten number never comes back as a new fact.
        assert_eq!(reopened.remember("!a", None, "next", "phizz").unwrap(), 3);
    }

    #[test]
    fn facts_stay_in_their_own_room() {
        let memory = memory();
        let id = memory.remember("!a", None, "a secret", "phizz").unwrap();

        assert!(memory.describe_facts("!b").is_none());
        assert!(memory.forget("!b", id).is_err());
        assert!(memory.forget("!a", id).is_ok());
        assert!(memory.describe_facts("!a").is_none());
    }

    #[test]
    fn a_full_room_refuses_new_facts_until_one_is_forgotten() {
        let memory = memory();
        for index in 0..MAX_FACTS_PER_ROOM {
            memory.remember("!a", None, &format!("fact {index}"), "phizz").unwrap();
        }
        assert!(memory.remember("!a", None, "one more", "phizz").unwrap_err().contains("full"));
        memory.forget("!a", 1).unwrap();
        assert!(memory.remember("!a", None, "one more", "phizz").is_ok());
    }

    #[test]
    fn rejects_empty_and_oversized_facts() {
        let memory = memory();
        assert!(memory.remember("!a", None, "   ", "phizz").is_err());
        assert!(memory.remember("!a", None, &"x".repeat(MAX_FACT_CHARS + 1), "phizz").is_err());
    }

    #[test]
    fn tools_attribute_facts_to_the_person_being_answered() {
        let memory = memory();
        let tools = MemoryTools { memory: &memory, room_id: "!a", told_by: "oshuma" };

        assert_eq!(tools.run("remember", &json!({"fact": "likes tea"})).unwrap(), "Saved as #1.");
        assert_eq!(memory.facts("!a")[0].told_by, "oshuma");
        assert_eq!(tools.run("forget", &json!({"id": 1})).unwrap(), "Forgot #1.");
        assert!(tools.run("forget", &json!({"id": 1})).is_err());
    }
}
