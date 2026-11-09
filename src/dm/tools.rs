//! The tools Claude plays a round with: real dice, checked changes to the world, and
//! the round's ending, which carries what gets posted.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::{Value, json};

use super::state::{Change, GameState, TILES};
use crate::llm::LocalTools;

pub const ROLL_DICE: &str = "roll_dice";
pub const UPDATE_STATE: &str = "update_state";
pub const END_ROUND: &str = "end_round";

const MAX_DICE: u32 = 20;
const SIDES: [u32; 8] = [2, 4, 6, 8, 10, 12, 20, 100];
const MAX_MODIFIER: i32 = 100;
const MAX_ROLLS: usize = 30;
const MAX_NARRATION_CHARS: usize = 3_000;
const MAX_SUMMARY_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Ending {
    pub narration: String,
    pub summary: String,
    #[serde(default)]
    pub game_over: bool,
}

/// One round's tools, holding the world as Claude changes it.
pub struct RoundTools {
    pub state: Mutex<GameState>,
    pub rolls: Mutex<Vec<String>>,
    pub ending: Mutex<Option<Ending>>,
}

impl RoundTools {
    pub fn new(state: GameState) -> Self {
        Self { state: Mutex::new(state), rolls: Mutex::new(Vec::new()), ending: Mutex::new(None) }
    }

    fn run_tool(&self, name: &str, input: &Value) -> Result<String, String> {
        match name {
            ROLL_DICE => {
                let dice = input["dice"].as_str().ok_or("dice is required, like 1d20+3")?;
                let reason = input["reason"].as_str().unwrap_or("").trim();
                let mut rolls = self.rolls.lock().expect("rolls mutex poisoned");
                if rolls.len() >= MAX_ROLLS {
                    return Err(format!("that's {MAX_ROLLS} rolls this round; resolve the rest without dice"));
                }
                let (total, detail) = roll(dice, |sides| rand::random_range(1..=sides))?;
                let line = if reason.is_empty() { format!("{dice} → {total}") } else { format!("{reason}: {dice} → {total}") };
                rolls.push(line);
                Ok(format!("{dice} = {total} ({detail})"))
            }
            UPDATE_STATE => {
                let changes: Vec<Change> = serde_json::from_value(input["changes"].clone()).map_err(|error| format!("the changes couldn't be read: {error}"))?;
                let mut state = self.state.lock().expect("state mutex poisoned");
                state.apply(&changes)?;
                Ok(format!("Done. The state is now:\n{}", serde_json::to_string(&*state).unwrap_or_default()))
            }
            END_ROUND => {
                let ending: Ending = serde_json::from_value(input.clone()).map_err(|error| format!("the ending couldn't be read: {error}"))?;
                if ending.narration.trim().is_empty() || ending.summary.trim().is_empty() {
                    return Err("both narration and summary are needed".to_string());
                }
                let ending = Ending {
                    narration: truncate(ending.narration.trim(), MAX_NARRATION_CHARS),
                    summary: truncate(ending.summary.trim(), MAX_SUMMARY_CHARS),
                    game_over: ending.game_over,
                };
                *self.ending.lock().expect("ending mutex poisoned") = Some(ending);
                Ok("The round is over and will be posted. Stop here.".to_string())
            }
            _ => Err(format!("there's no tool called {name}")),
        }
    }
}

impl LocalTools for RoundTools {
    fn definitions(&self) -> Vec<Value> {
        let legend: Vec<String> = TILES.iter().map(|(tile, meaning)| format!("'{tile}' {meaning}")).collect();
        vec![
            json!({
                "name": ROLL_DICE,
                "description": "Roll real dice for anything uncertain: attacks, damage, sneaking, persuading, \
                    searching. Use the result as it comes; never make up a roll.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "dice": {"type": "string", "description": "Like 1d20, 2d6+3, or 1d8-1. Up to 20 dice of 2, 4, 6, 8, 10, 12, 20, or 100 sides."},
                        "reason": {"type": "string", "description": "What it's for, shown to the players, like \"Alice attacks the wolf\"."}
                    },
                    "required": ["dice", "reason"]
                }
            }),
            json!({
                "name": UPDATE_STATE,
                "description": format!(
                    "Change the world so the map matches the story. Changes are made together, or not at all if \
                    one is wrong. Squares run from (0, 0) at the top left. Map characters: {}. Ops: \
                    set_map {{rows}} replaces the map for a new area, up to 24 wide and 16 tall, every row the same length; \
                    set_tile {{x, y, tile}}; add {{id, kind: monster|npc|item|feature, name, x, y, hp?, max_hp?}}, where a \
                    feature is a place worth naming, like an altar, a den, or a glowing crystal; \
                    move {{id, x, y}}; set_hp {{id, hp}}, where 0 means fallen; remove {{id}}, not for players; \
                    give {{id, item}}; take {{id, item}}; set_quest {{quest}}.",
                    legend.join(", ")
                ),
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "changes": {"type": "array", "items": {"type": "object", "properties": {"op": {"type": "string"}}, "required": ["op"]}}
                    },
                    "required": ["changes"]
                }
            }),
            json!({
                "name": END_ROUND,
                "description": "End the round with what to post. Call it once, last.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "narration": {"type": "string", "description": "What happens, told to the party in the game room."},
                        "summary": {"type": "string", "description": "One short line on what happened, in the third person, for the room the game started from."},
                        "game_over": {"type": "boolean", "description": "True only when the story has ended."}
                    },
                    "required": ["narration", "summary"]
                }
            }),
        ]
    }

    fn run<'a>(&'a self, name: &'a str, input: &'a Value) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(std::future::ready(self.run_tool(name, input)))
    }
}

/// Rolls dice like `2d6+3`, with `die` giving each die's result, so tests can fix it.
fn roll(dice: &str, mut die: impl FnMut(u32) -> u32) -> Result<(i64, String), String> {
    let spec = dice.trim().to_ascii_lowercase().replace(' ', "");
    let bad = || format!("{dice} isn't dice like 1d20 or 2d6+3");
    let (count, rest) = spec.split_once('d').ok_or_else(bad)?;
    let count: u32 = if count.is_empty() { 1 } else { count.parse().map_err(|_| bad())? };
    let (sides, modifier) = match rest.find(['+', '-']) {
        Some(at) => (&rest[..at], rest[at..].parse::<i32>().map_err(|_| bad())?),
        None => (rest, 0),
    };
    let sides: u32 = sides.parse().map_err(|_| bad())?;
    if !(1..=MAX_DICE).contains(&count) || !SIDES.contains(&sides) || modifier.abs() > MAX_MODIFIER {
        return Err(format!("{dice}: use 1 to {MAX_DICE} dice of 2, 4, 6, 8, 10, 12, 20, or 100 sides, and a modifier up to ±{MAX_MODIFIER}"));
    }
    let results: Vec<u32> = (0..count).map(|_| die(sides)).collect();
    let total = results.iter().map(|&result| i64::from(result)).sum::<i64>() + i64::from(modifier);
    let shown: Vec<String> = results.iter().map(u32::to_string).collect();
    let detail = match modifier {
        0 => format!("rolled {}", shown.join(", ")),
        _ => format!("rolled {}, {modifier:+}", shown.join(", ")),
    };
    Ok((total, detail))
}

fn truncate(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(chars - 1).collect();
    format!("{}…", kept.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolls_dice() {
        assert_eq!(roll("1d20+3", |_| 14), Ok((17, "rolled 14, +3".to_string())));
        assert_eq!(roll("2d6", |_| 4), Ok((8, "rolled 4, 4".to_string())));
        assert_eq!(roll("d8-1", |_| 1), Ok((0, "rolled 1, -1".to_string())));
        assert_eq!(roll(" 1D20 ", |_| 20).unwrap().0, 20);
        for bad in ["", "20", "1d7", "0d6", "21d6", "1d6+500", "xd6", "1d20+a"] {
            assert!(roll(bad, |_| 1).is_err(), "{bad}");
        }
        for _ in 0..200 {
            let (total, _) = roll("1d20", |sides| rand::random_range(1..=sides)).unwrap();
            assert!((1..=20).contains(&total));
        }
    }

    #[test]
    fn plays_a_round_through_its_tools() {
        let tools = RoundTools::new(GameState::new(&[("@a:x".to_string(), "Alice".to_string(), "alice".to_string())]));
        let rolled = tools.run_tool(ROLL_DICE, &json!({"dice": "1d20", "reason": "Alice attacks"})).unwrap();
        assert!(rolled.starts_with("1d20 = "), "{rolled}");
        assert!(tools.rolls.lock().unwrap()[0].starts_with("Alice attacks: 1d20 → "));

        let moved = tools.run_tool(UPDATE_STATE, &json!({"changes": [{"op": "move", "id": "alice", "x": 0, "y": 0}]})).unwrap();
        assert!(moved.contains("\"x\":0"), "{moved}");
        let refused = tools.run_tool(UPDATE_STATE, &json!({"changes": [{"op": "fly", "id": "alice"}]})).unwrap_err();
        assert!(refused.contains("couldn't be read"), "{refused}");

        assert!(tools.run_tool(END_ROUND, &json!({"narration": " ", "summary": "x"})).is_err());
        tools.run_tool(END_ROUND, &json!({"narration": "The wolves flee.", "summary": "the wolves ran away"})).unwrap();
        assert_eq!(
            *tools.ending.lock().unwrap(),
            Some(Ending { narration: "The wolves flee.".to_string(), summary: "the wolves ran away".to_string(), game_over: false })
        );
        assert!(tools.run_tool("teleport", &json!({})).is_err());
    }
}
