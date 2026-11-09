//! A game's world: the map, who and what is on it, and the party's goal. Claude
//! changes it only through `Change`, and every change is checked, so the map that's
//! drawn always matches what the story says.

use serde::{Deserialize, Serialize};

pub const MIN_SIZE: usize = 4;
pub const MAX_WIDTH: usize = 24;
pub const MAX_HEIGHT: usize = 16;
const MAX_ENTITIES: usize = 30;
const MAX_NAME_CHARS: usize = 40;
const MAX_ITEMS: usize = 20;
const MAX_QUEST_CHARS: usize = 200;
const MAX_HP: i32 = 999;
pub const PLAYER_HP: i32 = 12;

/// The ground each map character stands for. Anything else is drawn as unknown.
pub const TILES: &[(char, &str)] = &[
    ('.', "grass"),
    (',', "dirt or path"),
    ('_', "stone floor"),
    ('#', "wall"),
    ('^', "rock or mountain"),
    ('T', "tree"),
    ('~', "water"),
    ('+', "door"),
    ('c', "cave mouth"),
    ('*', "glowing or magical spot"),
    (' ', "darkness or unexplored"),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Player,
    Monster,
    Npc,
    Item,
    /// A place or thing worth naming on the map, like an altar or a wolf den.
    Feature,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    pub id: String,
    pub kind: Kind,
    pub name: String,
    pub x: usize,
    pub y: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hp: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hp: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    /// The Matrix user who plays this character.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameState {
    pub width: usize,
    pub height: usize,
    /// One string per row, one character per square, from `TILES`.
    pub rows: Vec<String>,
    pub entities: Vec<Entity>,
    #[serde(default)]
    pub quest: String,
}

/// One change Claude asks for through `update_state`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    /// A new area, replacing the whole map. Everything on it must be moved onto it.
    SetMap { rows: Vec<String> },
    SetTile { x: usize, y: usize, tile: String },
    Add { id: String, kind: Kind, name: String, x: usize, y: usize, hp: Option<i32>, max_hp: Option<i32> },
    Move { id: String, x: usize, y: usize },
    SetHp { id: String, hp: i32 },
    Remove { id: String },
    Give { id: String, item: String },
    Take { id: String, item: String },
    SetQuest { quest: String },
}

impl GameState {
    /// A blank field with the party standing in a row, for the opening round to
    /// replace with the real scene.
    pub fn new(players: &[(String, String, String)]) -> Self {
        let (width, height) = (16, 10);
        let entities = players
            .iter()
            .enumerate()
            .map(|(index, (user_id, name, entity_id))| Entity {
                id: entity_id.clone(),
                kind: Kind::Player,
                name: name.clone(),
                x: 2 + index * 2,
                y: height - 2,
                hp: Some(PLAYER_HP),
                max_hp: Some(PLAYER_HP),
                items: Vec::new(),
                player: Some(user_id.clone()),
            })
            .collect();
        Self { width, height, rows: vec![".".repeat(width); height], entities, quest: String::new() }
    }

    pub fn tile(&self, x: usize, y: usize) -> char {
        self.rows.get(y).and_then(|row| row.chars().nth(x)).unwrap_or(' ')
    }

    pub fn entity(&self, id: &str) -> Option<&Entity> {
        self.entities.iter().find(|entity| entity.id == id)
    }

    /// Applies every change, or none of them if any is invalid, saying which and why.
    pub fn apply(&mut self, changes: &[Change]) -> Result<(), String> {
        let mut next = self.clone();
        for (index, change) in changes.iter().enumerate() {
            next.apply_one(change).map_err(|error| format!("Change {} was refused, so none were made: {error}", index + 1))?;
        }
        next.check_positions()?;
        *self = next;
        Ok(())
    }

    fn apply_one(&mut self, change: &Change) -> Result<(), String> {
        match change {
            Change::SetMap { rows } => {
                let height = rows.len();
                let width = rows.first().map_or(0, |row| row.chars().count());
                if !(MIN_SIZE..=MAX_HEIGHT).contains(&height) || !(MIN_SIZE..=MAX_WIDTH).contains(&width) {
                    return Err(format!(
                        "the map is {width}x{height}, and must be {MIN_SIZE} to {MAX_WIDTH} wide and {MIN_SIZE} to {MAX_HEIGHT} tall"
                    ));
                }
                if let Some(row) = rows.iter().position(|row| row.chars().count() != width) {
                    return Err(format!("row {row} isn't {width} characters long like the first"));
                }
                (self.width, self.height, self.rows) = (width, height, rows.clone());
            }
            Change::SetTile { x, y, tile } => {
                let mut chars = tile.chars();
                let (Some(tile), None) = (chars.next(), chars.next()) else {
                    return Err("a tile is exactly one character".to_string());
                };
                self.in_bounds(*x, *y)?;
                let row: Vec<char> = self.rows[*y].chars().enumerate().map(|(column, old)| if column == *x { tile } else { old }).collect();
                self.rows[*y] = row.into_iter().collect();
            }
            Change::Add { id, kind, name, x, y, hp, max_hp } => {
                if *kind == Kind::Player {
                    return Err("players are only the party's own characters, which already exist".to_string());
                }
                if self.entity(id).is_some() {
                    return Err(format!("{id} already exists"));
                }
                if self.entities.len() >= MAX_ENTITIES {
                    return Err(format!("there can be at most {MAX_ENTITIES} things on the map"));
                }
                check_name(id)?;
                check_name(name)?;
                let hp = hp.map(check_hp).transpose()?;
                let max_hp = max_hp.or(hp).map(check_hp).transpose()?;
                self.in_bounds(*x, *y)?;
                self.entities.push(Entity {
                    id: id.clone(),
                    kind: kind.clone(),
                    name: name.clone(),
                    x: *x,
                    y: *y,
                    hp,
                    max_hp,
                    items: Vec::new(),
                    player: None,
                });
            }
            Change::Move { id, x, y } => {
                self.in_bounds(*x, *y)?;
                let entity = self.entity_mut(id)?;
                (entity.x, entity.y) = (*x, *y);
            }
            Change::SetHp { id, hp } => {
                let entity = self.entity_mut(id)?;
                let hp = (*hp).clamp(0, entity.max_hp.unwrap_or(MAX_HP));
                entity.hp = Some(hp);
            }
            Change::Remove { id } => {
                if self.entity_mut(id)?.kind == Kind::Player {
                    return Err("a player's character can't be removed; set its hp to 0 if it falls".to_string());
                }
                self.entities.retain(|entity| entity.id != *id);
            }
            Change::Give { id, item } => {
                check_name(item)?;
                let entity = self.entity_mut(id)?;
                if entity.items.len() >= MAX_ITEMS {
                    return Err(format!("{id} can carry at most {MAX_ITEMS} things"));
                }
                entity.items.push(item.clone());
            }
            Change::Take { id, item } => {
                let entity = self.entity_mut(id)?;
                let Some(position) = entity.items.iter().position(|held| held.eq_ignore_ascii_case(item)) else {
                    return Err(format!("{id} isn't carrying {item}"));
                };
                entity.items.remove(position);
            }
            Change::SetQuest { quest } => {
                if quest.chars().count() > MAX_QUEST_CHARS {
                    return Err(format!("the quest is at most {MAX_QUEST_CHARS} characters"));
                }
                self.quest = quest.clone();
            }
        }
        Ok(())
    }

    fn in_bounds(&self, x: usize, y: usize) -> Result<(), String> {
        if x < self.width && y < self.height {
            Ok(())
        } else {
            Err(format!("({x}, {y}) is off the {}x{} map, whose squares run from (0, 0)", self.width, self.height))
        }
    }

    fn entity_mut(&mut self, id: &str) -> Result<&mut Entity, String> {
        self.entities.iter_mut().find(|entity| entity.id == id).ok_or_else(|| format!("nothing on the map is called {id}"))
    }

    /// After a new map, everything must still be on it.
    fn check_positions(&self) -> Result<(), String> {
        match self.entities.iter().find(|entity| entity.x >= self.width || entity.y >= self.height) {
            Some(entity) => Err(format!("{} is at ({}, {}), off the new map; move it in the same update", entity.id, entity.x, entity.y)),
            None => Ok(()),
        }
    }
}

fn check_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() || name.chars().count() > MAX_NAME_CHARS {
        Err(format!("names and items are 1 to {MAX_NAME_CHARS} characters"))
    } else {
        Ok(())
    }
}

fn check_hp(hp: i32) -> Result<i32, String> {
    if (0..=MAX_HP).contains(&hp) { Ok(hp) } else { Err(format!("hp is 0 to {MAX_HP}")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> GameState {
        GameState::new(&[("@a:x".to_string(), "Alice".to_string(), "alice".to_string())])
    }

    fn changes(json: &str) -> Vec<Change> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn starts_with_the_party_on_a_blank_field() {
        let state = state();
        assert_eq!((state.width, state.height, state.rows.len()), (16, 10, 10));
        let alice = state.entity("alice").unwrap();
        assert_eq!((alice.kind.clone(), alice.hp, alice.player.as_deref()), (Kind::Player, Some(PLAYER_HP), Some("@a:x")));
    }

    #[test]
    fn applies_a_scene() {
        let mut state = state();
        state
            .apply(&changes(
                r#"[
                {"op": "set_map", "rows": ["TTTT..", "T....c", "......", "~~...."]},
                {"op": "move", "id": "alice", "x": 1, "y": 2},
                {"op": "add", "id": "wolf-1", "kind": "monster", "name": "Wolf", "x": 4, "y": 1, "hp": 5},
                {"op": "set_tile", "x": 0, "y": 3, "tile": "."},
                {"op": "give", "id": "alice", "item": "torch"},
                {"op": "set_quest", "quest": "Find the glow"}
            ]"#,
            ))
            .unwrap();
        assert_eq!((state.width, state.height), (6, 4));
        assert_eq!(state.tile(5, 1), 'c');
        assert_eq!(state.tile(0, 3), '.');
        assert_eq!(state.entity("wolf-1").unwrap().max_hp, Some(5));
        assert_eq!(state.entity("alice").unwrap().items, ["torch"]);

        state.apply(&changes(r#"[{"op": "set_hp", "id": "wolf-1", "hp": 99}, {"op": "take", "id": "alice", "item": "Torch"}]"#)).unwrap();
        assert_eq!(state.entity("wolf-1").unwrap().hp, Some(5), "hp stays within its maximum");
        assert!(state.entity("alice").unwrap().items.is_empty());
        state.apply(&changes(r#"[{"op": "remove", "id": "wolf-1"}]"#)).unwrap();
        assert!(state.entity("wolf-1").is_none());
    }

    #[test]
    fn refuses_bad_changes_and_keeps_the_state() {
        let original = state();
        for (json, error) in [
            (r#"[{"op": "move", "id": "alice", "x": 40, "y": 0}]"#, "off the 16x10 map"),
            (r#"[{"op": "move", "id": "bob", "x": 1, "y": 1}]"#, "nothing on the map is called bob"),
            (r#"[{"op": "set_map", "rows": ["....", "..."]}]"#, "must be 4 to 24 wide"),
            (r#"[{"op": "set_map", "rows": ["....", "...", "....", "...."]}]"#, "row 1 isn't 4 characters"),
            (r#"[{"op": "set_map", "rows": ["....", "....", "....", "...."]}]"#, "alice is at (2, 8), off the new map"),
            (r#"[{"op": "remove", "id": "alice"}]"#, "can't be removed"),
            (r#"[{"op": "add", "id": "p2", "kind": "player", "name": "Sneaky", "x": 1, "y": 1}]"#, "players are only"),
            (r#"[{"op": "add", "id": "alice", "kind": "npc", "name": "Twin", "x": 1, "y": 1}]"#, "alice already exists"),
            (r#"[{"op": "set_tile", "x": 1, "y": 1, "tile": "TT"}]"#, "exactly one character"),
            (r#"[{"op": "take", "id": "alice", "item": "sword"}]"#, "isn't carrying sword"),
        ] {
            let mut state = original.clone();
            // A good change first, to show nothing is kept when a later one fails.
            let mut batch = changes(r#"[{"op": "set_quest", "quest": "changed"}]"#);
            batch.extend(changes(json));
            let refused = state.apply(&batch).unwrap_err();
            assert!(refused.contains(error), "{json}: {refused}");
            assert_eq!(state, original, "{json}");
        }
    }
}
