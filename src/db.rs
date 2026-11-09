//! The bot's own SQLite database: messages, reactions, saved facts, room summaries,
//! private rooms, usage totals, what each start notice said, and `!dm` games. Matrix
//! session and encryption state is separate, in matrix-sdk's own stores.
//!
//! Queries are synchronous and run on the caller's task. They're short, and a single
//! connection behind a mutex keeps writes ordered without any further coordination.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, Transaction};

/// Schema changes, in order. Each entry runs once, so never edit a shipped one: add
/// another. The count applied so far is kept in SQLite's `user_version`.
const MIGRATIONS: &[&str] = &["
    CREATE TABLE messages (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        room_id TEXT NOT NULL,
        event_id TEXT NOT NULL,
        sender TEXT NOT NULL,
        sender_name TEXT,
        origin_server_ts_ms INTEGER NOT NULL,
        logged_at TEXT NOT NULL,
        body TEXT NOT NULL,
        has_image INTEGER NOT NULL DEFAULT 0,
        analysis_json TEXT NOT NULL,
        links_json TEXT,
        classify_prompt_json TEXT,
        classify_raw_json TEXT,
        response_prompt_json TEXT,
        response TEXT,
        response_label TEXT,
        response_event_id TEXT,
        UNIQUE (room_id, event_id)
    );
    CREATE INDEX messages_room_id ON messages (room_id, id);

    CREATE TABLE reactions (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        room_id TEXT NOT NULL,
        target_event_id TEXT NOT NULL,
        sender_name TEXT NOT NULL,
        key TEXT NOT NULL,
        UNIQUE (room_id, target_event_id, sender_name, key)
    );

    CREATE TABLE facts (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        room_id TEXT NOT NULL,
        about TEXT,
        text TEXT NOT NULL,
        told_by TEXT NOT NULL,
        added_at TEXT NOT NULL
    );
    CREATE INDEX facts_room ON facts (room_id, id);

    CREATE TABLE room_summaries (
        room_id TEXT PRIMARY KEY,
        through_event_id TEXT NOT NULL,
        text TEXT NOT NULL
    );

    CREATE TABLE usage_totals (
        label TEXT PRIMARY KEY,
        input_tokens INTEGER NOT NULL,
        output_tokens INTEGER NOT NULL,
        request_count INTEGER NOT NULL,
        estimated_cost_usd REAL NOT NULL,
        unpriced_requests INTEGER NOT NULL,
        web_search_requests INTEGER NOT NULL,
        web_search_cost_usd REAL NOT NULL
    );

    CREATE TABLE imports (
        source TEXT PRIMARY KEY,
        imported_at TEXT NOT NULL
    );
", "
    ALTER TABLE messages ADD COLUMN thread_root_id TEXT;
    CREATE INDEX messages_thread ON messages (room_id, thread_root_id, id);
    CREATE INDEX messages_response ON messages (room_id, response_event_id);
", "
    CREATE TABLE direct_rooms (
        room_id TEXT PRIMARY KEY,
        invited_by TEXT NOT NULL,
        joined_at TEXT NOT NULL
    );
", "
    ALTER TABLE usage_totals ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE usage_totals ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE usage_totals ADD COLUMN cache_saved_usd REAL NOT NULL DEFAULT 0;
", "
    CREATE TABLE start_notices (
        user_id TEXT PRIMARY KEY,
        version TEXT NOT NULL,
        notes TEXT NOT NULL,
        sent_at TEXT NOT NULL
    );
", "
    CREATE TABLE games (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL UNIQUE,
        premise TEXT NOT NULL,
        owner TEXT NOT NULL,
        main_room_id TEXT NOT NULL,
        thread_root_id TEXT NOT NULL,
        game_room_id TEXT NOT NULL UNIQUE,
        status TEXT NOT NULL,
        round INTEGER NOT NULL DEFAULT 0,
        state_json TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE TABLE game_players (
        game_id INTEGER NOT NULL REFERENCES games (id),
        user_id TEXT NOT NULL,
        name TEXT NOT NULL,
        entity_id TEXT NOT NULL,
        PRIMARY KEY (game_id, user_id)
    );
    CREATE TABLE game_actions (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        game_id INTEGER NOT NULL REFERENCES games (id),
        user_id TEXT NOT NULL,
        text TEXT NOT NULL,
        sent_at_ms INTEGER NOT NULL
    );
    CREATE TABLE game_rounds (
        game_id INTEGER NOT NULL REFERENCES games (id),
        round INTEGER NOT NULL,
        actions_json TEXT NOT NULL,
        narration TEXT NOT NULL,
        summary TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (game_id, round)
    );
", "
    CREATE TABLE game_log (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        game_id INTEGER NOT NULL REFERENCES games (id),
        round INTEGER NOT NULL,
        kind TEXT NOT NULL,
        user_id TEXT,
        data_json TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE INDEX game_log_game ON game_log (game_id, id);
"];

#[derive(Debug)]
pub struct Database {
    connection: Mutex<Connection>,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let connection = Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        connection.busy_timeout(Duration::from_secs(5)).context("failed to set busy timeout")?;
        connection.pragma_update(None, "journal_mode", "WAL").context("failed to enable WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL").context("failed to set synchronous")?;
        Self::prepare(connection)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Self {
        Self::prepare(Connection::open_in_memory().expect("open in-memory database")).expect("prepare database")
    }

    fn prepare(mut connection: Connection) -> Result<Self> {
        migrate(&mut connection)?;
        Ok(Self { connection: Mutex::new(connection) })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs `operation` on the connection.
    pub fn with<T>(&self, operation: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        operation(&self.lock()).context("database query failed")
    }

    /// Runs `operation` in a transaction, committing only if it succeeds.
    pub fn transaction<T>(&self, operation: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut connection = self.lock();
        let transaction = connection.transaction().context("failed to start transaction")?;
        let result = operation(&transaction)?;
        transaction.commit().context("failed to commit transaction")?;
        Ok(result)
    }
}

fn migrate(connection: &mut Connection) -> Result<()> {
    let version: usize = connection.pragma_query_value(None, "user_version", |row| row.get(0)).context("failed to read schema version")?;

    anyhow::ensure!(
        version <= MIGRATIONS.len(),
        "the database is at schema version {version}, newer than this build's {}; run a newer build",
        MIGRATIONS.len()
    );

    for (index, migration) in MIGRATIONS.iter().enumerate().skip(version) {
        let transaction = connection.transaction().context("failed to start migration")?;
        transaction.execute_batch(migration).with_context(|| format!("schema migration {} failed", index + 1))?;
        transaction.pragma_update(None, "user_version", index + 1).context("failed to record schema version")?;
        transaction.commit().context("failed to commit migration")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("bot-db-{label}-{nanos}")).join("bot.db")
    }

    #[test]
    fn reopening_keeps_data_and_does_not_rerun_migrations() {
        let path = temp_path("reopen");
        Database::open(&path)
            .unwrap()
            .with(|connection| connection.execute("INSERT INTO imports VALUES ('a', 'b')", []))
            .unwrap();

        let count: i64 = Database::open(&path)
            .unwrap()
            .with(|connection| connection.query_row("SELECT COUNT(*) FROM imports", [], |row| row.get(0)))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn a_database_from_before_threads_gains_the_thread_column() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(MIGRATIONS[0]).unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
        connection
            .execute(
                "INSERT INTO messages (room_id, event_id, sender, origin_server_ts_ms, logged_at, body, analysis_json) \
                 VALUES ('!r', '$a', '@a:x', 1, 'now', 'hi', '{}')",
                [],
            )
            .unwrap();

        migrate(&mut connection).unwrap();

        let (body, thread): (String, Option<String>) = connection
            .query_row("SELECT body, thread_root_id FROM messages", [], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap();
        assert_eq!((body.as_str(), thread), ("hi", None));
    }

    #[test]
    fn a_failed_transaction_leaves_nothing_behind() {
        let db = Database::open_in_memory();
        let result: Result<()> = db.transaction(|transaction| {
            transaction.execute("INSERT INTO imports VALUES ('a', 'b')", [])?;
            anyhow::bail!("failed partway")
        });
        assert!(result.is_err());

        let count: i64 = db.with(|connection| connection.query_row("SELECT COUNT(*) FROM imports", [], |row| row.get(0))).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn refuses_a_database_from_a_newer_build() {
        let path = temp_path("newer");
        Database::open(&path)
            .unwrap()
            .with(|connection| connection.pragma_update(None, "user_version", MIGRATIONS.len() + 1))
            .unwrap();
        assert!(Database::open(&path).unwrap_err().to_string().contains("newer"));
    }
}
