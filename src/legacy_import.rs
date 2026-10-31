//! One-time import of the JSONL and JSON files the bot used before SQLite. Each
//! source is imported in its own transaction and recorded in `imports`, so an
//! interrupted import retries cleanly and nothing is imported twice. The old files
//! are left untouched, as a backup.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::Deserialize;
use tracing::{info, warn};

use crate::config::Config;
use crate::db::Database;
use crate::message_log::{self, LoggedMessage, ReactionRecord};
use crate::room_memory::{self, Fact};
use crate::usage::{self, UsageTotals};

const REACTION_LOG_SUFFIX: &str = ".reactions.jsonl";

pub fn import_legacy_data(db: &Database, config: &Config) -> Result<()> {
    import_message_logs(db, &config.message_log_dir)?;
    import_memory(db, &config.memory_path)?;
    import_usage(db, &config.usage_state_path)
}

/// Runs `import` in a transaction unless `source` was already imported.
fn once(db: &Database, source: &str, import: impl FnOnce(&Transaction<'_>) -> Result<()>) -> Result<()> {
    db.transaction(|transaction| {
        let done = transaction
            .query_row("SELECT 1 FROM imports WHERE source = ?1", [source], |_| Ok(()))
            .optional()
            .context("failed to check import status")?
            .is_some();
        if done {
            return Ok(());
        }

        import(transaction)?;
        transaction
            .execute(
                "INSERT INTO imports (source, imported_at) VALUES (?1, ?2)",
                params![source, chrono::Utc::now().to_rfc3339()],
            )
            .context("failed to record import")?;
        Ok(())
    })
}

fn import_message_logs(db: &Database, dir: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", dir.display())),
    };

    let mut paths: Vec<_> = entries.filter_map(|entry| entry.ok().map(|entry| entry.path())).collect();
    paths.sort();

    // Messages first: a reaction log's lines don't say which room they belong to.
    let mut rooms_by_stem = HashMap::new();
    for path in paths.iter().filter(|path| is_message_log(path)) {
        let name = file_name(path);
        once(db, &format!("messages:{name}"), |transaction| {
            let (imported, skipped) = import_message_log(transaction, path)?;
            info!(file = name, imported, skipped, "imported message log");
            Ok(())
        })?;
        if let Some(room_id) = first_room_id(path)? {
            rooms_by_stem.insert(name.trim_end_matches(".jsonl").to_string(), room_id);
        }
    }

    for path in paths.iter().filter(|path| file_name(path).ends_with(REACTION_LOG_SUFFIX)) {
        let name = file_name(path);
        let Some(room_id) = rooms_by_stem.get(name.trim_end_matches(REACTION_LOG_SUFFIX)) else {
            warn!(file = name, "no message log for this reaction log; not importing it");
            continue;
        };
        once(db, &format!("reactions:{name}"), |transaction| import_reaction_log(transaction, path, room_id))?;
    }
    Ok(())
}

fn file_name(path: &Path) -> &str {
    path.file_name().and_then(|name| name.to_str()).unwrap_or_default()
}

fn is_message_log(path: &Path) -> bool {
    let name = file_name(path);
    name.ends_with(".jsonl") && !name.ends_with(REACTION_LOG_SUFFIX)
}

fn lines(path: &Path) -> Result<impl Iterator<Item = std::io::Result<String>>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    Ok(BufReader::new(file).lines())
}

fn first_room_id(path: &Path) -> Result<Option<String>> {
    for line in lines(path)? {
        if let Ok(entry) = serde_json::from_str::<LoggedMessage>(&line?) {
            return Ok(Some(entry.room_id));
        }
    }
    Ok(None)
}

/// Bad lines are skipped, as they always were on read, but a file that can't be read
/// fails startup: running without the history would quietly look like amnesia.
fn import_message_log(transaction: &Transaction<'_>, path: &Path) -> Result<(usize, usize)> {
    let (mut imported, mut skipped) = (0, 0);
    for line in lines(path)? {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<LoggedMessage>(&line) {
            Ok(entry) => {
                message_log::import_entry(transaction, &entry)?;
                imported += 1;
            }
            Err(err) => {
                warn!(?err, file = file_name(path), "skipping malformed message log line");
                skipped += 1;
            }
        }
    }
    Ok((imported, skipped))
}

fn import_reaction_log(transaction: &Transaction<'_>, path: &Path, room_id: &str) -> Result<()> {
    for line in lines(path)? {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        match serde_json::from_str::<ReactionRecord>(&line) {
            Ok(reaction) => message_log::import_reaction(transaction, room_id, &reaction)?,
            Err(err) => warn!(?err, file = file_name(path), "skipping malformed reaction log line"),
        }
    }
    Ok(())
}

#[derive(Default, Deserialize)]
struct LegacyMemory {
    #[serde(default)]
    summaries: HashMap<String, LegacySummary>,
    #[serde(default)]
    facts: Vec<Fact>,
    #[serde(default)]
    next_fact_id: i64,
}

#[derive(Deserialize)]
struct LegacySummary {
    through_event_id: String,
    text: String,
}

fn import_memory(db: &Database, path: &Path) -> Result<()> {
    let Some(raw) = read_if_present(path)? else {
        return Ok(());
    };

    once(db, "memory", |transaction| {
        let memory: LegacyMemory = serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
        for fact in &memory.facts {
            room_memory::import_fact(transaction, fact)?;
        }
        for (room_id, summary) in &memory.summaries {
            room_memory::save_summary(transaction, room_id, &summary.through_event_id, &summary.text)?;
        }
        room_memory::reserve_fact_ids(transaction, memory.next_fact_id)?;
        info!(facts = memory.facts.len(), summaries = memory.summaries.len(), "imported memory");
        Ok(())
    })
}

#[derive(Deserialize)]
struct LegacyUsage {
    overall: UsageTotals,
    #[serde(default)]
    by_label: HashMap<String, UsageTotals>,
}

/// A usage file that won't parse is only an estimate lost, as it always was, so it
/// doesn't stop the bot from starting.
fn import_usage(db: &Database, path: &Path) -> Result<()> {
    let Some(raw) = read_if_present(path)? else {
        return Ok(());
    };

    once(db, "usage", |transaction| {
        match serde_json::from_str::<LegacyUsage>(&raw) {
            Ok(legacy) => {
                usage::save_totals(transaction, usage::OVERALL_LABEL, &legacy.overall)?;
                for (label, totals) in &legacy.by_label {
                    usage::save_totals(transaction, label, totals)?;
                }
                info!(requests = legacy.overall.request_count, "imported usage totals");
            }
            Err(err) => warn!(?err, path = %path.display(), "failed to parse usage file; starting from zero"),
        }
        Ok(())
    })
}

fn read_if_present(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(Some(raw)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::message_log::MessageLogger;
    use crate::room_memory::RoomMemory;
    use crate::usage::UsageTracker;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("legacy-import-{label}-{nanos}"));
        std::fs::create_dir_all(dir.join("messages")).unwrap();
        dir
    }

    fn config(dir: &Path) -> Config {
        let raw = format!(
            "homeserver_url = \"https://example.org\"\nusername = \"gordy\"\npassword = \"x\"\nrooms = [\"#a:example.org\"]\n\
             message_log_dir = {:?}\nmemory_path = {:?}\nusage_state_path = {:?}\n",
            dir.join("messages"),
            dir.join("memory.json"),
            dir.join("usage.json"),
        );
        toml::from_str(&raw).unwrap()
    }

    fn line(event_id: &str, body: &str) -> String {
        json!({
            "logged_at": "2026-07-18T00:00:00Z",
            "room_id": "!r:example.org",
            "event_id": event_id,
            "sender": "@alice:example.org",
            "origin_server_ts_ms": 1,
            "body": body,
            "analysis": {"intent": "chitchat", "confidence": 0.9, "requires_response": false, "summary": "s", "sentiment": "neutral"},
            "response": "a reply",
            "response_label": "chat",
            "response_event_id": "$reply",
        })
        .to_string()
    }

    fn write_fixture(dir: &Path) {
        let log = format!("{}\nnot json\n{}\n", line("$1", "first"), line("$2", "second"));
        std::fs::write(dir.join("messages/_r_example.org.jsonl"), log).unwrap();
        std::fs::write(
            dir.join("messages/_r_example.org.reactions.jsonl"),
            r#"{"target_event_id":"$reply","sender_name":"bob","key":"👍"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("memory.json"),
            json!({
                "summaries": {"!r:example.org": {"through_event_id": "$1", "text": "a summary"}},
                "facts": [{"id": 4, "room_id": "!r:example.org", "about": null, "text": "likes tea", "told_by": "bob", "added_at": "t"}],
                "next_fact_id": 9,
            })
            .to_string(),
        )
        .unwrap();
        let totals = json!({"input_tokens": 10, "output_tokens": 5, "request_count": 2, "estimated_cost_usd": 1.5, "unpriced_requests": 0});
        std::fs::write(dir.join("usage.json"), json!({"overall": totals, "by_label": {"chat": totals}}).to_string()).unwrap();
    }

    #[test]
    fn imports_everything_and_leaves_the_files_alone() {
        let dir = temp_dir("all");
        write_fixture(&dir);
        let db = Arc::new(Database::open_in_memory());

        import_legacy_data(&db, &config(&dir)).unwrap();

        let messages = MessageLogger::new(Arc::clone(&db)).recent("!r:example.org", 10).unwrap();
        assert_eq!(messages.iter().map(|entry| entry.body.as_str()).collect::<Vec<_>>(), vec!["first", "second"]);
        assert_eq!(messages[0].response_reactions[0].key, "👍");

        let memory = RoomMemory::new(Arc::clone(&db));
        assert_eq!(memory.facts("!r:example.org")[0].id, 4);
        assert_eq!(memory.remember("!r:example.org", None, "new", "bob").unwrap(), 10, "ids continue past the old counter");

        let usage = UsageTracker::open(Arc::clone(&db), None).unwrap().snapshot();
        assert_eq!(usage.overall.request_count, 2);
        assert_eq!(usage.by_label[0].label, "chat");

        assert!(dir.join("usage.json").exists());
    }

    #[test]
    fn importing_twice_changes_nothing() {
        let dir = temp_dir("twice");
        write_fixture(&dir);
        let db = Arc::new(Database::open_in_memory());

        import_legacy_data(&db, &config(&dir)).unwrap();
        RoomMemory::new(Arc::clone(&db)).forget("!r:example.org", 4).unwrap();
        import_legacy_data(&db, &config(&dir)).unwrap();

        assert_eq!(MessageLogger::new(Arc::clone(&db)).recent("!r:example.org", 10).unwrap().len(), 2);
        assert!(RoomMemory::new(db).facts("!r:example.org").is_empty(), "a forgotten fact stays forgotten");
    }

    #[test]
    fn a_fresh_install_has_nothing_to_import() {
        let dir = temp_dir("fresh");
        std::fs::remove_dir_all(dir.join("messages")).unwrap();
        import_legacy_data(&Database::open_in_memory(), &config(&dir)).unwrap();
    }

    #[test]
    fn bad_usage_files_start_from_zero() {
        let dir = temp_dir("bad-usage");
        std::fs::write(dir.join("usage.json"), "not json").unwrap();
        let db = Arc::new(Database::open_in_memory());

        import_legacy_data(&db, &config(&dir)).unwrap();
        assert_eq!(UsageTracker::open(db, None).unwrap().snapshot().overall.request_count, 0);
    }
}
