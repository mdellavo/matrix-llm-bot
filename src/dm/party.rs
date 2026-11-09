//! The party card: a panel for each player, with their character drawn large in their
//! colour from the map, a health bar, their stats, and what they carry.

use ab_glyph::{FontRef, PxScale};
use anyhow::{Context, Result};
use image::imageops::{self, FilterType};
use image::RgbImage;

use super::map::{self, BACKGROUND, HEADING, MUTED, OUTLINE, TEXT};
use super::sprites;
use super::state::{Entity, GameState, Kind};
use crate::message_log::ReplyImage;

const PANEL_WIDTH: u32 = 260;
const COLUMNS: u32 = 3;
const GAP: u32 = 12;
const PADDING: u32 = 12;
const TITLE_HEIGHT: u32 = 34;
const PORTRAIT: u32 = 64;
const LINE: u32 = 20;
/// Items listed before the rest are counted.
const MAX_ITEMS: usize = 8;

const PANEL: [u8; 3] = [42, 43, 48];
const PORTRAIT_BACKGROUND: [u8; 3] = [58, 60, 66];
const BAR_EMPTY: [u8; 3] = [70, 72, 78];
const HEALTHY: [u8; 3] = [80, 180, 90];
const HURT: [u8; 3] = [230, 180, 50];
const BADLY_HURT: [u8; 3] = [210, 60, 50];

/// The card for every player, or `None` for a game without any.
pub fn render(state: &GameState, round: u32) -> Result<Option<ReplyImage>> {
    let font = FontRef::try_from_slice(crate::pp::FONT).context("bundled font is invalid")?;
    let markers = map::markers(state);
    let players: Vec<(usize, &Entity)> = state.entities.iter().enumerate().filter(|(_, entity)| entity.kind == Kind::Player).collect();
    if players.is_empty() {
        return Ok(None);
    }

    let panel_height = players.iter().map(|(_, entity)| panel_height(entity)).max().unwrap_or(0);
    let count = players.len() as u32;
    let columns = count.min(COLUMNS);
    let rows = count.div_ceil(COLUMNS);
    let width = PADDING * 2 + columns * PANEL_WIDTH + (columns - 1) * GAP;
    let height = TITLE_HEIGHT + PADDING + rows * panel_height + (rows - 1) * GAP;
    let mut image = RgbImage::from_pixel(width, height, BACKGROUND);

    let text = |image: &mut RgbImage, x: u32, baseline: u32, size: f32, line: &str, ink: [u8; 3]| {
        crate::cast_sheet::draw_text(image, &font, PxScale::from(size), x as f32, baseline as f32, line, ink);
    };
    text(&mut image, PADDING, 24, 17.0, &format!("The party · round {round}"), HEADING);

    for (slot, (index, entity)) in players.iter().enumerate() {
        let slot = slot as u32;
        let left = PADDING + (slot % COLUMNS) * (PANEL_WIDTH + GAP);
        let top = TITLE_HEIGHT + (slot / COLUMNS) * (panel_height + GAP);
        let marker = &markers[*index];
        let (l, t) = (left as i32, top as i32);
        map::rectangle(&mut image, l, t, l + PANEL_WIDTH as i32 - 1, t + panel_height as i32 - 1, PANEL);
        // A stripe in the player's colour, as on their badge on the map.
        map::rectangle(&mut image, l, t, l + PANEL_WIDTH as i32 - 1, t + 3, marker.colour);

        let (portrait_left, portrait_top) = (left + PADDING, top + PADDING + 4);
        map::rectangle(
            &mut image,
            portrait_left as i32,
            portrait_top as i32,
            (portrait_left + PORTRAIT) as i32 - 1,
            (portrait_top + PORTRAIT) as i32 - 1,
            PORTRAIT_BACKGROUND,
        );
        if let Some(sprite) = &marker.sprite {
            map::overlay(&mut image, &imageops::resize(sprite, PORTRAIT, PORTRAIT, FilterType::Nearest), portrait_left, portrait_top);
        }

        let info_left = portrait_left + PORTRAIT + 12;
        let info_width = left + PANEL_WIDTH - PADDING - info_left;
        let name_fits = (info_width as f32 / map::text_width(&font, PxScale::from(17.0), "M")) as usize;
        text(&mut image, info_left, portrait_top + 16, 17.0, &map::fit(&entity.name, name_fits), TEXT);

        // Health, as a bar coloured by how hurt they are, and in numbers.
        let (bar_top, bar_bottom) = (portrait_top + 26, portrait_top + 37);
        let (bl, br) = (info_left as i32, (info_left + info_width) as i32 - 1);
        map::rectangle(&mut image, bl - 1, bar_top as i32 - 1, br + 1, bar_bottom as i32 + 1, OUTLINE);
        map::rectangle(&mut image, bl, bar_top as i32, br, bar_bottom as i32, BAR_EMPTY);
        let (hp, max_hp) = (entity.hp.unwrap_or(0).max(0), entity.max_hp.unwrap_or(0).max(1));
        let share = (hp as f32 / max_hp as f32).clamp(0.0, 1.0);
        if share > 0.0 {
            let colour = if share > 0.6 { HEALTHY } else if share > 0.3 { HURT } else { BADLY_HURT };
            let filled = bl + ((br - bl) as f32 * share).round() as i32;
            map::rectangle(&mut image, bl, bar_top as i32, filled, bar_bottom as i32, colour);
        }
        let health = if entity.hp == Some(0) { "Fallen".to_string() } else { format!("{hp} / {max_hp} hp") };
        text(&mut image, info_left, portrait_top + 56, 14.0, &health, if entity.hp == Some(0) { BADLY_HURT } else { MUTED });

        // Stats, two to a line.
        let mut y = portrait_top + PORTRAIT + 10;
        let stats: Vec<(&String, &i32)> = entity.stats.iter().collect();
        let column_width = (PANEL_WIDTH - PADDING * 2) / 2;
        let stat_fits = (column_width as f32 / map::text_width(&font, PxScale::from(14.0), "M")) as usize;
        for pair in stats.chunks(2) {
            for (column, (name, value)) in pair.iter().enumerate() {
                let x = left + PADDING + column as u32 * column_width;
                let value = value.to_string();
                let name = map::fit(name, stat_fits.saturating_sub(value.len() + 1));
                text(&mut image, x, y + 15, 14.0, &name, MUTED);
                let name_width = map::text_width(&font, PxScale::from(14.0), &format!("{name} "));
                text(&mut image, x + name_width as u32, y + 15, 14.0, &value, TEXT);
            }
            y += LINE;
        }

        // What they carry, with a picture where an item has one.
        y += 4;
        text(&mut image, left + PADDING, y + 15, 14.0, if entity.items.is_empty() { "Carrying nothing" } else { "Carrying" }, HEADING);
        y += LINE;
        let item_fits = ((PANEL_WIDTH - PADDING * 2 - 22) as f32 / map::text_width(&font, PxScale::from(14.0), "M")) as usize;
        for item in entity.items.iter().take(MAX_ITEMS) {
            match sprites::item_icon(item) {
                Some(icon) => map::overlay(&mut image, &icon, left + PADDING, y + 2),
                None => map::rectangle(&mut image, (left + PADDING + 6) as i32, (y + 8) as i32, (left + PADDING + 9) as i32, (y + 11) as i32, MUTED),
            }
            text(&mut image, left + PADDING + 22, y + 15, 14.0, &map::fit(item, item_fits), TEXT);
            y += LINE;
        }
        if entity.items.len() > MAX_ITEMS {
            text(&mut image, left + PADDING + 22, y + 15, 14.0, &format!("and {} more", entity.items.len() - MAX_ITEMS), MUTED);
        }
    }

    map::png(&image, "party.png").map(Some)
}

/// Every panel is as tall as the one with the most to show, so a row lines up.
fn panel_height(entity: &Entity) -> u32 {
    let stat_lines = entity.stats.len().div_ceil(2) as u32;
    let item_lines = entity.items.len().min(MAX_ITEMS) as u32 + u32::from(entity.items.len() > MAX_ITEMS);
    PADDING + 4 + PORTRAIT + 10 + stat_lines * LINE + 4 + LINE + item_lines * LINE + PADDING
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dm::state::Change;

    fn party() -> GameState {
        let mut state = GameState::new(&[
            ("@a:x".to_string(), "Alice".to_string(), "alice".to_string()),
            ("@b:x".to_string(), "Bob".to_string(), "bob".to_string()),
        ]);
        let changes: Vec<Change> = serde_json::from_str(
            r#"[
            {"op": "give", "id": "alice", "item": "Rusty sword"},
            {"op": "give", "id": "alice", "item": "Old map"},
            {"op": "set_stat", "id": "alice", "stat": "Gold", "value": 12},
            {"op": "set_stat", "id": "alice", "stat": "XP", "value": 40},
            {"op": "set_stat", "id": "alice", "stat": "Armour", "value": 3},
            {"op": "set_hp", "id": "bob", "hp": 0},
            {"op": "add", "id": "wolf", "kind": "monster", "name": "Wolf", "x": 1, "y": 1, "hp": 5}
        ]"#,
        )
        .unwrap();
        state.apply(&changes).unwrap();
        state
    }

    #[test]
    fn draws_a_panel_for_each_player() {
        let state = party();
        let card = render(&state, 4).unwrap().unwrap();
        // Two panels side by side; the wolf isn't a player.
        assert_eq!(card.width, PADDING * 2 + 2 * PANEL_WIDTH + GAP);
        let alice = state.entity("alice").unwrap();
        assert_eq!(card.height, TITLE_HEIGHT + PADDING + panel_height(alice));

        let image = image::load_from_memory(&card.data).unwrap().to_rgb8();
        let markers = map::markers(&state);
        // Each panel's stripe is in its player's colour from the map, and Bob is fallen.
        assert_eq!(image.get_pixel(PADDING + 5, TITLE_HEIGHT + 1).0, markers[0].colour);
        assert_eq!(image.get_pixel(PADDING + PANEL_WIDTH + GAP + 5, TITLE_HEIGHT + 1).0, markers[1].colour);
        // Alice is unhurt, so her health bar starts green.
        let bar_left = PADDING + PADDING + PORTRAIT + 12;
        assert_eq!(image.get_pixel(bar_left + 2, TITLE_HEIGHT + PADDING + 4 + 30).0, HEALTHY);
    }

    #[test]
    fn panels_grow_with_stats_and_items_and_wrap_after_three() {
        let state = party();
        let alice = state.entity("alice").unwrap();
        let bob = state.entity("bob").unwrap();
        assert_eq!(panel_height(alice) - panel_height(bob), 2 * LINE + 2 * LINE, "two lines of stats and two items");

        let mut crowd = GameState::new(&(0..4).map(|n| (format!("@{n}:x"), format!("P{n}"), format!("p{n}"))).collect::<Vec<_>>());
        let many: Vec<Change> = (0..12).map(|n| Change::Give { id: "p0".to_string(), item: format!("thing {n}") }).collect();
        crowd.apply(&many).unwrap();
        let card = render(&crowd, 1).unwrap().unwrap();
        let tallest = panel_height(crowd.entity("p0").unwrap());
        assert_eq!(tallest, panel_height(crowd.entity("p1").unwrap()) + (MAX_ITEMS as u32 + 1) * LINE, "eight items, then a count");
        assert_eq!(card.width, PADDING * 2 + 3 * PANEL_WIDTH + 2 * GAP);
        assert_eq!(card.height, TITLE_HEIGHT + PADDING + 2 * tallest + GAP);
    }

    #[test]
    fn no_players_no_card() {
        assert!(render(&GameState::new(&[]), 1).unwrap().is_none());
    }
}
