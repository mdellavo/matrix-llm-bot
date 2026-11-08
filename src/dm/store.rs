//! Games in the database: each game, its players, the actions waiting for the next
//! round, and every round played. Kept there so a restart loses nothing.

use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use super::state::GameState;
use crate::db::Database;

pub const ACTIVE: &str = "active";
pub const OVER: &str = "over";

#[derive(Debug, Clone)]
pub struct Game {
    pub id: i64,
    pub name: String,
    pub premise: String,
    pub owner: String,
    pub main_room_id: String,
    pub thread_root_id: String,
    pub game_room_id: String,
    pub status: String,
    /// Rounds played so far. The opening scene is round 0.
    pub round: u32,
    pub state: GameState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Player {
    pub user_id: String,
    pub name: String,
    pub entity_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub user_id: String,
    pub text: String,
    pub sent_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Round {
    pub round: u32,
    pub actions: Vec<Action>,
    pub narration: String,
    pub summary: String,
}

pub struct NewGame<'a> {
    pub name: &'a str,
    pub premise: &'a str,
    pub owner: &'a str,
    pub main_room_id: &'a str,
    pub thread_root_id: &'a str,
    pub game_room_id: &'a str,
    pub players: &'a [Player],
    pub state: &'a GameState,
}

const GAME_COLUMNS: &str = "id, name, premise, owner, main_room_id, thread_root_id, game_room_id, status, round, state_json";

fn read_game(row: &Row<'_>) -> rusqlite::Result<(Game, String)> {
    let state_json: String = row.get(9)?;
    Ok((
        Game {
            id: row.get(0)?,
            name: row.get(1)?,
            premise: row.get(2)?,
            owner: row.get(3)?,
            main_room_id: row.get(4)?,
            thread_root_id: row.get(5)?,
            game_room_id: row.get(6)?,
            status: row.get(7)?,
            round: row.get(8)?,
            state: GameState::new(&[]),
        },
        state_json,
    ))
}

fn with_state((mut game, state_json): (Game, String)) -> Result<Game> {
    game.state = serde_json::from_str(&state_json).with_context(|| format!("game {} has unreadable state", game.id))?;
    Ok(game)
}

pub fn create(database: &Database, new: &NewGame<'_>) -> Result<i64> {
    let now = chrono::Utc::now().to_rfc3339();
    let state_json = serde_json::to_string(new.state)?;
    database.transaction(|transaction| {
        transaction.execute(
            "INSERT INTO games (name, premise, owner, main_room_id, thread_root_id, game_room_id, status, round, state_json, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, ?9)",
            params![new.name, new.premise, new.owner, new.main_room_id, new.thread_root_id, new.game_room_id, ACTIVE, state_json, now],
        )?;
        let id = transaction.last_insert_rowid();
        for player in new.players {
            transaction.execute(
                "INSERT INTO game_players (game_id, user_id, name, entity_id) VALUES (?1, ?2, ?3, ?4)",
                params![id, player.user_id, player.name, player.entity_id],
            )?;
        }
        Ok(id)
    })
}

pub fn by_room(database: &Database, game_room_id: &str) -> Result<Option<Game>> {
    let found = database.with(|connection| {
        connection.query_row(&format!("SELECT {GAME_COLUMNS} FROM games WHERE game_room_id = ?1"), [game_room_id], read_game).optional()
    })?;
    found.map(with_state).transpose()
}

pub fn by_id(database: &Database, id: i64) -> Result<Option<Game>> {
    let found =
        database.with(|connection| connection.query_row(&format!("SELECT {GAME_COLUMNS} FROM games WHERE id = ?1"), [id], read_game).optional())?;
    found.map(with_state).transpose()
}

pub fn active(database: &Database) -> Result<Vec<Game>> {
    let rows = database.with(|connection| {
        connection.prepare(&format!("SELECT {GAME_COLUMNS} FROM games WHERE status = ?1 ORDER BY id"))?.query_map([ACTIVE], read_game)?.collect::<rusqlite::Result<Vec<_>>>()
    })?;
    rows.into_iter().map(with_state).collect()
}

/// Whether a game room already uses this name.
pub fn name_taken(database: &Database, name: &str) -> Result<bool> {
    database.with(|connection| connection.query_row("SELECT 1 FROM games WHERE name = ?1", [name], |_| Ok(())).optional().map(|row| row.is_some()))
}

pub fn players(database: &Database, game_id: i64) -> Result<Vec<Player>> {
    database.with(|connection| {
        connection
            .prepare("SELECT user_id, name, entity_id FROM game_players WHERE game_id = ?1 ORDER BY rowid")?
            .query_map([game_id], |row| Ok(Player { user_id: row.get(0)?, name: row.get(1)?, entity_id: row.get(2)? }))?
            .collect()
    })
}

pub fn add_action(database: &Database, game_id: i64, action: &Action) -> Result<()> {
    database.with(|connection| {
        connection.execute(
            "INSERT INTO game_actions (game_id, user_id, text, sent_at_ms) VALUES (?1, ?2, ?3, ?4)",
            params![game_id, action.user_id, action.text, action.sent_at_ms],
        )
    })?;
    Ok(())
}

pub fn pending_actions(database: &Database, game_id: i64) -> Result<Vec<Action>> {
    database.with(|connection| {
        connection
            .prepare("SELECT user_id, text, sent_at_ms FROM game_actions WHERE game_id = ?1 ORDER BY id")?
            .query_map([game_id], |row| Ok(Action { user_id: row.get(0)?, text: row.get(1)?, sent_at_ms: row.get(2)? }))?
            .collect()
    })
}

/// Takes the waiting actions for the round about to be played. Ones sent while it's
/// played stay for the next.
pub fn take_actions(database: &Database, game_id: i64) -> Result<Vec<Action>> {
    database.transaction(|transaction| {
        let actions: Vec<Action> = transaction
            .prepare("SELECT user_id, text, sent_at_ms FROM game_actions WHERE game_id = ?1 ORDER BY id")?
            .query_map([game_id], |row| Ok(Action { user_id: row.get(0)?, text: row.get(1)?, sent_at_ms: row.get(2)? }))?
            .collect::<rusqlite::Result<_>>()?;
        transaction.execute("DELETE FROM game_actions WHERE game_id = ?1", [game_id])?;
        Ok(actions)
    })
}

/// Puts back actions from a round that couldn't be played, ahead of newer ones.
pub fn restore_actions(database: &Database, game_id: i64, actions: &[Action]) -> Result<()> {
    database.transaction(|transaction| {
        let newer: Vec<Action> = transaction
            .prepare("SELECT user_id, text, sent_at_ms FROM game_actions WHERE game_id = ?1 ORDER BY id")?
            .query_map([game_id], |row| Ok(Action { user_id: row.get(0)?, text: row.get(1)?, sent_at_ms: row.get(2)? }))?
            .collect::<rusqlite::Result<_>>()?;
        transaction.execute("DELETE FROM game_actions WHERE game_id = ?1", [game_id])?;
        for action in actions.iter().chain(&newer) {
            transaction.execute(
                "INSERT INTO game_actions (game_id, user_id, text, sent_at_ms) VALUES (?1, ?2, ?3, ?4)",
                params![game_id, action.user_id, action.text, action.sent_at_ms],
            )?;
        }
        Ok(())
    })
}

/// Saves a played round and the state it left, and moves the game on to the next.
pub fn finish_round(database: &Database, game: &Game, played: &Round, state: &GameState, over: bool) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let state_json = serde_json::to_string(state)?;
    let actions_json = serde_json::to_string(&played.actions)?;
    database.transaction(|transaction| {
        transaction.execute(
            "INSERT INTO game_rounds (game_id, round, actions_json, narration, summary, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![game.id, played.round, actions_json, played.narration, played.summary, now],
        )?;
        transaction.execute(
            "UPDATE games SET round = ?2, state_json = ?3, status = ?4, updated_at = ?5 WHERE id = ?1",
            params![game.id, played.round + 1, state_json, if over { OVER } else { ACTIVE }, now],
        )?;
        Ok(())
    })
}

pub fn end(database: &Database, game_id: i64) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    database.with(|connection| connection.execute("UPDATE games SET status = ?2, updated_at = ?3 WHERE id = ?1", params![game_id, OVER, now]))?;
    Ok(())
}

pub fn rounds(database: &Database, game_id: i64) -> Result<Vec<Round>> {
    let rows: Vec<(u32, String, String, String)> = database.with(|connection| {
        connection
            .prepare("SELECT round, actions_json, narration, summary FROM game_rounds WHERE game_id = ?1 ORDER BY round")?
            .query_map([game_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
            .collect()
    })?;
    rows.into_iter()
        .map(|(round, actions_json, narration, summary)| {
            Ok(Round { round, actions: serde_json::from_str(&actions_json).context("unreadable round actions")?, narration, summary })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(database: &Database) -> i64 {
        let players = [Player { user_id: "@a:x".to_string(), name: "Alice".to_string(), entity_id: "alice".to_string() }];
        let state = GameState::new(&[("@a:x".to_string(), "Alice".to_string(), "alice".to_string())]);
        create(database, &NewGame {
            name: "cave-in-the-woods-abcd",
            premise: "a cave",
            owner: "@a:x",
            main_room_id: "!main:x",
            thread_root_id: "$root",
            game_room_id: "!game:x",
            players: &players,
            state: &state,
        })
        .unwrap()
    }

    fn action(user: &str, text: &str) -> Action {
        Action { user_id: user.to_string(), text: text.to_string(), sent_at_ms: 1 }
    }

    #[test]
    fn keeps_games_players_and_rounds() {
        let database = Database::open_in_memory();
        let id = game(&database);
        let found = by_room(&database, "!game:x").unwrap().unwrap();
        assert_eq!((found.id, found.round, found.status.as_str(), found.state.entities.len()), (id, 0, ACTIVE, 1));
        assert!(by_room(&database, "!main:x").unwrap().is_none());
        assert!(name_taken(&database, "cave-in-the-woods-abcd").unwrap());
        assert_eq!(players(&database, id).unwrap()[0].entity_id, "alice");
        assert_eq!(active(&database).unwrap().len(), 1);

        let played = Round { round: 0, actions: vec![action("@a:x", "look")], narration: "A cave.".to_string(), summary: "found a cave".to_string() };
        finish_round(&database, &found, &played, &found.state, false).unwrap();
        assert_eq!(by_id(&database, id).unwrap().unwrap().round, 1);
        assert_eq!(rounds(&database, id).unwrap(), [played]);

        end(&database, id).unwrap();
        assert!(active(&database).unwrap().is_empty());
    }

    #[test]
    fn takes_and_restores_actions_in_order() {
        let database = Database::open_in_memory();
        let id = game(&database);
        add_action(&database, id, &action("@a:x", "first")).unwrap();
        add_action(&database, id, &action("@a:x", "second")).unwrap();
        let taken = take_actions(&database, id).unwrap();
        assert_eq!(taken.len(), 2);
        assert!(pending_actions(&database, id).unwrap().is_empty());

        // One arrives while the round fails, and stays after the restored ones.
        add_action(&database, id, &action("@a:x", "third")).unwrap();
        restore_actions(&database, id, &taken).unwrap();
        let texts: Vec<String> = pending_actions(&database, id).unwrap().into_iter().map(|action| action.text).collect();
        assert_eq!(texts, ["first", "second", "third"]);
    }
}
