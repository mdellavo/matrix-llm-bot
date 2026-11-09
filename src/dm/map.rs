//! Draws a game's map, with a key beside it naming everything on it: each creature
//! with its health, each item and place by number, and each kind of ground shown. A
//! strip under the map gives the round and the party's goal.

use std::io::Cursor;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use anyhow::{Context, Result};
use image::{ImageFormat, Rgb, RgbImage};

use super::state::{Entity, GameState, Kind, TILES};
use crate::message_log::ReplyImage;

/// Sizes for one drawing of the map.
#[derive(Debug, Clone, Copy)]
struct Scale {
    square: u32,
    token_radius: i32,
    token_font: f32,
}

/// For the game room.
const FULL: Scale = Scale { square: 32, token_radius: 12, token_font: 15.0 };
/// For the thread in the room the game started from: a smaller map, with the key at
/// the same readable size.
const SMALL: Scale = Scale { square: 20, token_radius: 8, token_font: 11.0 };

const KEY_WIDTH: u32 = 250;
const KEY_LINE: u32 = 22;
const KEY_PADDING: u32 = 12;
const KEY_FONT: f32 = 14.0;
const STRIP_HEIGHT: u32 = 30;

const BACKGROUND: Rgb<u8> = Rgb([30, 31, 34]);
const TEXT: [u8; 3] = [235, 235, 235];
const MUTED: [u8; 3] = [160, 162, 168];
const HEADING: [u8; 3] = [200, 200, 205];
const OUTLINE: [u8; 3] = [20, 20, 22];
const PLAYER_COLOURS: [[u8; 3]; 6] =
    [[66, 135, 245], [245, 166, 35], [155, 89, 182], [26, 188, 156], [236, 240, 241], [241, 196, 15]];
const MONSTER: [u8; 3] = [200, 45, 45];
const NPC: [u8; 3] = [250, 128, 170];
const ITEM: [u8; 3] = [212, 175, 55];
const FEATURE: [u8; 3] = [214, 51, 132];
const FALLEN: [u8; 3] = [110, 110, 110];

/// How one thing on the map is drawn, and named in the key.
#[derive(Debug, Clone, PartialEq)]
struct Marker {
    label: String,
    colour: [u8; 3],
    shape: Shape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Token,
    Square,
    Diamond,
}

/// The full map for the game room, and a smaller one for the starting room's thread.
pub fn render(state: &GameState, round: u32) -> Result<(ReplyImage, ReplyImage)> {
    let font = FontRef::try_from_slice(crate::pp::FONT).context("bundled font is invalid")?;
    let markers = markers(state);
    Ok((draw(state, round, &markers, FULL, &font, "map.png")?, draw(state, round, &markers, SMALL, &font, "map-small.png")?))
}

/// A label for everything on the map: a creature's initial, numbered when others
/// share it, like W1 and W2 for two wolves, and numbers for items and for places.
fn markers(state: &GameState) -> Vec<Marker> {
    let creature = |entity: &Entity| matches!(entity.kind, Kind::Player | Kind::Monster | Kind::Npc);
    let initial = |entity: &Entity| entity.name.chars().find(|c| c.is_alphanumeric()).unwrap_or('?').to_uppercase().to_string();
    let (mut players, mut items, mut places) = (0, 0, 0);
    state
        .entities
        .iter()
        .enumerate()
        .map(|(index, entity)| {
            let fallen = entity.hp == Some(0);
            let colour = |colour| if fallen { FALLEN } else { colour };
            match entity.kind {
                Kind::Player | Kind::Monster | Kind::Npc => {
                    let letter = initial(entity);
                    let sharing: Vec<usize> = state
                        .entities
                        .iter()
                        .enumerate()
                        .filter(|(_, other)| creature(other) && initial(other) == letter)
                        .map(|(other_index, _)| other_index)
                        .collect();
                    let label = match sharing.iter().position(|&other_index| other_index == index) {
                        Some(number) if sharing.len() > 1 => format!("{letter}{}", number + 1),
                        _ => letter,
                    };
                    let base = match entity.kind {
                        Kind::Player => {
                            players += 1;
                            PLAYER_COLOURS[(players - 1) % PLAYER_COLOURS.len()]
                        }
                        Kind::Monster => MONSTER,
                        _ => NPC,
                    };
                    Marker { label, colour: colour(base), shape: Shape::Token }
                }
                Kind::Item => {
                    items += 1;
                    Marker { label: items.to_string(), colour: ITEM, shape: Shape::Square }
                }
                Kind::Feature => {
                    places += 1;
                    Marker { label: places.to_string(), colour: FEATURE, shape: Shape::Diamond }
                }
            }
        })
        .collect()
}

fn draw(state: &GameState, round: u32, markers: &[Marker], scale: Scale, font: &FontRef, filename: &str) -> Result<ReplyImage> {
    let map_width = state.width as u32 * scale.square;
    let map_height = state.height as u32 * scale.square;
    let key = key_lines(state);
    let key_height = KEY_PADDING * 2 + key.len() as u32 * KEY_LINE;
    let width = map_width + KEY_WIDTH;
    let height = map_height.max(key_height) + STRIP_HEIGHT;
    let mut image = RgbImage::from_pixel(width, height, BACKGROUND);

    for y in 0..state.height {
        for x in 0..state.width {
            draw_square(&mut image, x as u32 * scale.square, y as u32 * scale.square, scale.square, state.tile(x, y));
        }
    }
    // Places and items under creatures, so a creature standing on one is still seen.
    let mut order: Vec<usize> = (0..state.entities.len()).collect();
    order.sort_by_key(|&index| markers[index].shape == Shape::Token);
    for index in order {
        let entity = &state.entities[index];
        let (cx, cy) = (
            (entity.x as u32 * scale.square + scale.square / 2) as i32,
            (entity.y as u32 * scale.square + scale.square / 2) as i32,
        );
        draw_marker(&mut image, font, &markers[index], cx, cy, scale.token_radius, scale.token_font);
    }

    let text = |image: &mut RgbImage, x: u32, baseline: u32, line: &str, ink: [u8; 3]| {
        crate::cast_sheet::draw_text(image, font, PxScale::from(KEY_FONT), x as f32, baseline as f32, line, ink);
    };
    let key_left = map_width + KEY_PADDING;
    let fits = chars_that_fit(font, KEY_WIDTH - KEY_PADDING * 2 - 30);
    for (line_number, line) in key.iter().enumerate() {
        let top = KEY_PADDING + line_number as u32 * KEY_LINE;
        let baseline = top + KEY_LINE / 2 + 5;
        match line {
            KeyLine::Heading(heading) => text(&mut image, key_left, baseline, heading, HEADING),
            KeyLine::Marker(index, description) => {
                let marker = &markers[*index];
                draw_marker(&mut image, font, marker, (key_left + 10) as i32, (top + KEY_LINE / 2) as i32, 9, 11.0);
                text(&mut image, key_left + 26, baseline, &fit(description, fits), TEXT);
            }
            KeyLine::Ground(tile, name) => {
                draw_square(&mut image, key_left + 1, top + 2, KEY_LINE - 4, *tile);
                text(&mut image, key_left + 26, baseline, &fit(name, fits), TEXT);
            }
        }
    }

    let strip_top = height - STRIP_HEIGHT;
    let quest = if state.quest.is_empty() { String::new() } else { format!(" · Goal: {}", state.quest) };
    let strip = fit(&format!("Round {round}{quest}"), chars_that_fit(font, width - 16));
    text(&mut image, 8, strip_top + 20, &strip, MUTED);

    png(&image, filename)
}

enum KeyLine {
    Heading(&'static str),
    /// An entity, by index, and what the key says about it.
    Marker(usize, String),
    Ground(char, &'static str),
}

/// The key: creatures with their health, then items, places, and the ground shown.
fn key_lines(state: &GameState) -> Vec<KeyLine> {
    let mut lines = Vec::new();
    let section = |heading: &'static str, kinds: &[Kind], lines: &mut Vec<KeyLine>| {
        let entries: Vec<KeyLine> = state
            .entities
            .iter()
            .enumerate()
            .filter(|(_, entity)| kinds.contains(&entity.kind))
            .map(|(index, entity)| {
                let health = match (entity.hp, entity.max_hp) {
                    (Some(0), _) => " (fallen)".to_string(),
                    (Some(hp), Some(max)) => format!(" {hp}/{max} hp"),
                    (Some(hp), None) => format!(" {hp} hp"),
                    _ => String::new(),
                };
                KeyLine::Marker(index, format!("{}{health}", entity.name))
            })
            .collect();
        if !entries.is_empty() {
            lines.push(KeyLine::Heading(heading));
            lines.extend(entries);
        }
    };
    section("Creatures", &[Kind::Player, Kind::Npc, Kind::Monster], &mut lines);
    section("Items", &[Kind::Item], &mut lines);
    section("Places", &[Kind::Feature], &mut lines);

    let shown: Vec<(char, &'static str)> =
        TILES.iter().copied().filter(|(tile, _)| state.rows.iter().any(|row| row.contains(*tile))).collect();
    if !shown.is_empty() {
        lines.push(KeyLine::Heading("Ground"));
        lines.extend(shown.into_iter().map(|(tile, name)| KeyLine::Ground(tile, name)));
    }
    lines
}

fn draw_marker(image: &mut RgbImage, font: &FontRef, marker: &Marker, cx: i32, cy: i32, radius: i32, font_size: f32) {
    match marker.shape {
        Shape::Token => {
            circle(image, cx, cy, radius + 2, OUTLINE);
            circle(image, cx, cy, radius, marker.colour);
        }
        Shape::Square => {
            rectangle(image, cx - radius - 1, cy - radius - 1, cx + radius + 1, cy + radius + 1, OUTLINE);
            rectangle(image, cx - radius + 1, cy - radius + 1, cx + radius - 1, cy + radius - 1, marker.colour);
        }
        Shape::Diamond => {
            diamond(image, cx, cy, radius + 3, OUTLINE);
            diamond(image, cx, cy, radius + 1, marker.colour);
        }
    }
    // Two-character labels are drawn smaller to fit.
    let size = if marker.label.chars().count() > 1 { font_size * 0.8 } else { font_size };
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let width: f32 = marker.label.chars().map(|c| scaled.h_advance(font.glyph_id(c))).sum();
    let ink = if marker.colour.iter().map(|&c| u32::from(c)).sum::<u32>() > 450 { OUTLINE } else { TEXT };
    crate::cast_sheet::draw_text(image, font, scale, cx as f32 - width / 2.0, cy as f32 + size * 0.36, &marker.label, ink);
}

fn chars_that_fit(font: &FontRef, width: u32) -> usize {
    let scale = PxScale::from(KEY_FONT);
    (width as f32 / font.as_scaled(scale).h_advance(font.glyph_id('M'))) as usize
}

fn fit(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(chars.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

fn png(image: &RgbImage, filename: &str) -> Result<ReplyImage> {
    let mut data = Vec::new();
    image.write_to(&mut Cursor::new(&mut data), ImageFormat::Png).context("failed to encode the map")?;
    Ok(ReplyImage { filename: filename.to_string(), mime: mime::IMAGE_PNG, data, width: image.width(), height: image.height(), animated: false })
}

const GRASS: [u8; 3] = [86, 125, 70];
const WATER: [u8; 3] = [52, 98, 160];

/// One square of ground, `size` pixels across, with a little drawing for what it is.
fn draw_square(image: &mut RgbImage, left: u32, top: u32, size: u32, tile: char) {
    let base = match tile {
        '.' | 'T' | 'c' | '*' => GRASS,
        ',' => [139, 115, 85],
        '_' => [118, 118, 124],
        '#' | '+' => [64, 64, 70],
        '^' => [98, 92, 86],
        '~' => WATER,
        ' ' => [14, 14, 16],
        _ => [92, 70, 92],
    };
    for y in top..top + size {
        for x in left..left + size {
            // A faint grid, so squares can be counted.
            let shade = if x == left || y == top { 0.82 } else { 1.0 };
            image.put_pixel(x, y, Rgb(base.map(|c| (f32::from(c) * shade) as u8)));
        }
    }
    let (cx, cy, s) = ((left + size / 2) as i32, (top + size / 2) as i32, size as i32);
    match tile {
        'T' => {
            circle(image, cx, cy, s * 3 / 8, [34, 78, 38]);
            circle(image, cx - s / 10, cy - s / 10, s / 6, [52, 104, 54]);
        }
        'c' => ellipse(image, cx, cy + s / 10, s * 3 / 8, s / 4, [10, 10, 12]),
        '*' => {
            circle(image, cx, cy, s * 5 / 16, [250, 232, 140]);
            circle(image, cx, cy, s / 6, [255, 250, 220]);
        }
        '~' => {
            for x in left + size / 5..left + size - size / 5 {
                image.put_pixel(x, top + size * 3 / 8, Rgb([92, 140, 200]));
                image.put_pixel(x, top + size * 5 / 8, Rgb([92, 140, 200]));
            }
        }
        '#' => {
            for x in left..left + size {
                image.put_pixel(x, top + size / 2, Rgb([84, 84, 92]));
            }
            for y in top..top + size / 2 {
                image.put_pixel(left + size / 2, y, Rgb([84, 84, 92]));
            }
        }
        '+' => rectangle(image, left as i32 + s / 4, top as i32 + s / 8, left as i32 + s - s / 4 - 1, top as i32 + s - s / 16 - 1, [128, 84, 44]),
        '^' => {
            let peak = s * 7 / 16;
            for row in 0..peak {
                for x in (cx - row)..=(cx + row) {
                    image.put_pixel(x as u32, (top as i32 + s / 4 + row) as u32, Rgb([130, 122, 112]));
                }
            }
        }
        _ => {}
    }
}

fn put(image: &mut RgbImage, x: i32, y: i32, colour: [u8; 3]) {
    if x >= 0 && y >= 0 && (x as u32) < image.width() && (y as u32) < image.height() {
        image.put_pixel(x as u32, y as u32, Rgb(colour));
    }
}

fn circle(image: &mut RgbImage, cx: i32, cy: i32, radius: i32, colour: [u8; 3]) {
    ellipse(image, cx, cy, radius, radius, colour);
}

fn ellipse(image: &mut RgbImage, cx: i32, cy: i32, rx: i32, ry: i32, colour: [u8; 3]) {
    for y in -ry..=ry {
        for x in -rx..=rx {
            if (x * x) as f32 / (rx * rx).max(1) as f32 + (y * y) as f32 / (ry * ry).max(1) as f32 <= 1.0 {
                put(image, cx + x, cy + y, colour);
            }
        }
    }
}

fn rectangle(image: &mut RgbImage, left: i32, top: i32, right: i32, bottom: i32, colour: [u8; 3]) {
    for y in top..=bottom {
        for x in left..=right {
            put(image, x, y, colour);
        }
    }
}

fn diamond(image: &mut RgbImage, cx: i32, cy: i32, size: i32, colour: [u8; 3]) {
    for y in -size..=size {
        let reach = size - y.abs();
        for x in -reach..=reach {
            put(image, cx + x, cy + y, colour);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dm::state::Change;

    fn scene() -> GameState {
        let mut state = GameState::new(&[
            ("@a:x".to_string(), "Alice".to_string(), "alice".to_string()),
            ("@w:x".to_string(), "Will".to_string(), "will".to_string()),
        ]);
        let changes: Vec<Change> = serde_json::from_str(
            r#"[
            {"op": "set_map", "rows": ["~~~~~~~~~~~~", "............", "......T.....", "...........c"]},
            {"op": "move", "id": "alice", "x": 1, "y": 1},
            {"op": "move", "id": "will", "x": 2, "y": 1},
            {"op": "add", "id": "wolf-1", "kind": "monster", "name": "Wolf", "x": 4, "y": 2, "hp": 5},
            {"op": "add", "id": "wolf-2", "kind": "monster", "name": "Wolf", "x": 5, "y": 2, "hp": 5},
            {"op": "set_hp", "id": "wolf-2", "hp": 0},
            {"op": "add", "id": "bones", "kind": "item", "name": "Old bones", "x": 8, "y": 1},
            {"op": "add", "id": "glow", "kind": "feature", "name": "Faint glow", "x": 11, "y": 3},
            {"op": "set_quest", "quest": "Find the glow"}
        ]"#,
        )
        .unwrap();
        state.apply(&changes).unwrap();
        state
    }

    #[test]
    fn labels_everything_so_the_key_can_name_it() {
        let labels: Vec<String> = markers(&scene()).into_iter().map(|marker| marker.label).collect();
        // Alice stands alone; Will and the two wolves share W, so they're numbered.
        assert_eq!(labels, ["A", "W1", "W2", "W3", "1", "1"]);
        let markers = markers(&scene());
        assert_eq!(markers[3].colour, FALLEN, "the fallen wolf is greyed out");
        assert_eq!((markers[4].shape, markers[5].shape), (Shape::Square, Shape::Diamond));
    }

    #[test]
    fn the_key_names_creatures_with_health_items_places_and_ground() {
        let state = scene();
        let key: Vec<String> = key_lines(&state)
            .into_iter()
            .map(|line| match line {
                KeyLine::Heading(heading) => format!("# {heading}"),
                KeyLine::Marker(_, text) => text,
                KeyLine::Ground(tile, name) => format!("{tile} {name}"),
            })
            .collect();
        assert_eq!(
            key,
            [
                "# Creatures",
                "Alice 12/12 hp",
                "Will 12/12 hp",
                "Wolf 5/5 hp",
                "Wolf (fallen)",
                "# Items",
                "Old bones",
                "# Places",
                "Faint glow",
                "# Ground",
                ". grass",
                "T tree",
                "~ water",
                "c cave mouth",
            ]
        );
    }

    #[test]
    fn draws_the_map_and_its_key() {
        let state = scene();
        let (full, small) = render(&state, 3).unwrap();
        let key_height = KEY_PADDING * 2 + 14 * KEY_LINE;
        assert_eq!((full.width, full.height), (12 * FULL.square + KEY_WIDTH, key_height + STRIP_HEIGHT));
        assert_eq!(small.width, 12 * SMALL.square + KEY_WIDTH, "the smaller map keeps a full-size key");

        let image = image::load_from_memory(&full.data).unwrap().to_rgb8();
        let square = FULL.square;
        let at = |x: u32, y: u32, dx: u32, dy: u32| image.get_pixel(x * square + dx, y * square + dy).0;
        assert_eq!(at(0, 0, 23, 23), WATER);
        assert_eq!(at(3, 1, 23, 23), GRASS);
        // Alice's token, the first wolf's, and the bones' square, each in its colour.
        assert_eq!(at(1, 1, 16 + 9, 16), PLAYER_COLOURS[0]);
        assert_eq!(at(4, 2, 16 + 9, 16), MONSTER);
        assert_eq!(at(8, 1, 16 + 9, 16 + 9), ITEM);
        // The key has its first creature's token beside the map.
        let key_token = image.get_pixel(12 * square + KEY_PADDING + 10 + 6, KEY_PADDING + KEY_LINE + KEY_LINE / 2).0;
        assert_eq!(key_token, PLAYER_COLOURS[0]);
    }

    #[test]
    fn a_map_taller_than_its_key_sets_the_height() {
        let mut state = GameState::new(&[]);
        let rows = vec!["....".to_string(); 16];
        state.apply(&[Change::SetMap { rows }]).unwrap();
        let (full, _) = render(&state, 1).unwrap();
        assert_eq!((full.width, full.height), (4 * FULL.square + KEY_WIDTH, 16 * FULL.square + STRIP_HEIGHT));
    }

    #[test]
    fn shortens_text_that_doesnt_fit() {
        assert_eq!(fit("short", 10), "short");
        assert_eq!(fit("a much longer line", 8), "a much…");
    }
}
