//! Draws a game's map, with a key beside it naming everything on it: each creature
//! with its health, each item and place by number, and each kind of ground shown. A
//! strip under the map gives the round and the party's goal.
//!
//! Ground and anything with a look are drawn from the Kenney tile sheets (see
//! `sprites`). On the full-size map each thing also wears a badge with its label from
//! the key, so two wolves can be told apart.

use std::io::Cursor;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use anyhow::{Context, Result};
use image::imageops::{self, FilterType};
use image::{ImageFormat, Rgb, RgbImage, RgbaImage};

use super::sprites;
use super::state::{Entity, GameState, Kind, TILES};
use crate::message_log::ReplyImage;

/// Sizes for one drawing of the map.
#[derive(Debug, Clone, Copy)]
struct Scale {
    square: u32,
    token_radius: i32,
    token_font: f32,
    /// Labels on the map itself. At the small size they'd hide the sprites.
    badges: bool,
}

/// For the game room: the tiles at twice their size.
const FULL: Scale = Scale { square: 32, token_radius: 12, token_font: 15.0, badges: true };
/// For the thread in the room the game started from: the tiles at their own size,
/// with the key still full size.
const SMALL: Scale = Scale { square: 16, token_radius: 6, token_font: 9.0, badges: false };

const KEY_WIDTH: u32 = 270;
const KEY_LINE: u32 = 22;
const KEY_PADDING: u32 = 12;
const KEY_FONT: f32 = 14.0;
const BADGE_FONT: f32 = 10.0;
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
    /// From its look. Without one, it's drawn as `shape` with its label inside.
    sprite: Option<RgbaImage>,
    /// The top half of a look two squares tall, drawn in the square above.
    top: Option<RgbaImage>,
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
            let look = |default: Option<&str>| {
                let (sprite, top) = entity.look.as_deref().or(default).and_then(sprites::look).unzip();
                let top = top.flatten();
                if fallen { (sprite.map(|sprite| greyed(&sprite)), top.map(|top| greyed(&top))) } else { (sprite, top) }
            };
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
                    let (base, default_look) = match entity.kind {
                        Kind::Player => {
                            players += 1;
                            let order = (players - 1) % PLAYER_COLOURS.len();
                            (PLAYER_COLOURS[order], Some(sprites::PLAYER_LOOKS[order]))
                        }
                        Kind::Monster => (MONSTER, None),
                        _ => (NPC, None),
                    };
                    let (sprite, top) = look(default_look);
                    Marker { label, colour: colour(base), shape: Shape::Token, sprite, top }
                }
                Kind::Item => {
                    items += 1;
                    let (sprite, top) = look(None);
                    Marker { label: items.to_string(), colour: ITEM, shape: Shape::Square, sprite, top }
                }
                Kind::Feature => {
                    places += 1;
                    let (sprite, top) = look(None);
                    Marker { label: places.to_string(), colour: FEATURE, shape: Shape::Diamond, sprite, top }
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
            draw_ground(&mut image, x as u32 * scale.square, y as u32 * scale.square, scale.square, state.tile(x, y), x, y);
        }
    }
    // Places and items under creatures, so a creature standing on one is still seen.
    let mut order: Vec<usize> = (0..state.entities.len()).collect();
    order.sort_by_key(|&index| markers[index].shape == Shape::Token);
    for &index in &order {
        let entity = &state.entities[index];
        let (left, top) = (entity.x as u32 * scale.square, entity.y as u32 * scale.square);
        let marker = &markers[index];
        match &marker.sprite {
            Some(sprite) => {
                overlay(&mut image, &imageops::resize(sprite, scale.square, scale.square, FilterType::Nearest), left, top);
                // A tall look's top goes in the square above, unless it's the map's top row.
                if let (Some(upper), Some(above)) = (&marker.top, top.checked_sub(scale.square)) {
                    overlay(&mut image, &imageops::resize(upper, scale.square, scale.square, FilterType::Nearest), left, above);
                }
            }
            None => {
                let (cx, cy) = ((left + scale.square / 2) as i32, (top + scale.square / 2) as i32);
                draw_marker(&mut image, font, marker, cx, cy, scale.token_radius, scale.token_font);
            }
        }
    }
    // Badges last, so none is hidden under a neighbour's sprite.
    if scale.badges {
        for &index in &order {
            let entity = &state.entities[index];
            if markers[index].sprite.is_some() {
                draw_badge(&mut image, font, &markers[index], entity.x as u32 * scale.square, entity.y as u32 * scale.square);
            }
        }
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
                match &marker.sprite {
                    Some(sprite) => overlay(&mut image, sprite, key_left + 2, top + (KEY_LINE - sprites::SIZE) / 2),
                    None => draw_marker(&mut image, font, marker, (key_left + 10) as i32, (top + KEY_LINE / 2) as i32, 9, 11.0),
                }
                let line = format!("{} {description}", marker.label);
                text(&mut image, key_left + 26, baseline, &fit(&line, fits), TEXT);
            }
            KeyLine::Ground(tile, name) => {
                draw_ground(&mut image, key_left + 1, top + 2, KEY_LINE - 4, *tile, 0, 1);
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

/// A lettered or numbered token, for things without a look.
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
    let width = text_width(font, scale, &marker.label);
    crate::cast_sheet::draw_text(image, font, scale, cx as f32 - width / 2.0, cy as f32 + size * 0.36, &marker.label, ink_for(marker.colour));
}

/// A sprite's label from the key, in the top-left corner of its square, in the
/// colour that says what it is: a player's own colour, red for monsters, and so on.
fn draw_badge(image: &mut RgbImage, font: &FontRef, marker: &Marker, left: u32, top: u32) {
    let scale = PxScale::from(BADGE_FONT);
    let width = text_width(font, scale, &marker.label).ceil() as i32 + 4;
    let (left, top) = (left as i32, top as i32);
    rectangle(image, left, top, left + width + 1, top + 12, OUTLINE);
    rectangle(image, left + 1, top + 1, left + width, top + 11, marker.colour);
    crate::cast_sheet::draw_text(image, font, scale, (left + 3) as f32, (top + 9) as f32, &marker.label, ink_for(marker.colour));
}

fn ink_for(colour: [u8; 3]) -> [u8; 3] {
    if colour.iter().map(|&c| u32::from(c)).sum::<u32>() > 450 { OUTLINE } else { TEXT }
}

fn text_width(font: &FontRef, scale: PxScale, text: &str) -> f32 {
    let scaled = font.as_scaled(scale);
    text.chars().map(|c| scaled.h_advance(font.glyph_id(c))).sum()
}

fn chars_that_fit(font: &FontRef, width: u32) -> usize {
    (width as f32 / text_width(font, PxScale::from(KEY_FONT), "M")) as usize
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

/// Draws a sprite over the image, keeping what's under its transparent pixels.
fn overlay(image: &mut RgbImage, sprite: &RgbaImage, left: u32, top: u32) {
    for (x, y, pixel) in sprite.enumerate_pixels() {
        let (px, py) = (left + x, top + y);
        if px >= image.width() || py >= image.height() || pixel[3] == 0 {
            continue;
        }
        let alpha = f32::from(pixel[3]) / 255.0;
        let under = image.get_pixel_mut(px, py);
        for channel in 0..3 {
            under[channel] = (f32::from(pixel[channel]) * alpha + f32::from(under[channel]) * (1.0 - alpha)).round() as u8;
        }
    }
}

/// A fallen creature, faded to grey.
fn greyed(sprite: &RgbaImage) -> RgbaImage {
    let mut grey = sprite.clone();
    for pixel in grey.pixels_mut() {
        let luma = (u32::from(pixel[0]) * 3 + u32::from(pixel[1]) * 6 + u32::from(pixel[2])) / 10;
        let faded = (luma * 7 / 10) as u8;
        (pixel[0], pixel[1], pixel[2]) = (faded, faded, faded);
    }
    grey
}

const GRASS: [u8; 3] = [86, 125, 70];
const WATER: [u8; 3] = [56, 128, 196];
const RIPPLE: [u8; 3] = [120, 178, 228];

/// One square of ground, `size` pixels across: a tile from the sheets where the packs
/// have one, otherwise drawn here in the same flat style.
fn draw_ground(image: &mut RgbImage, left: u32, top: u32, size: u32, tile: char, x: usize, y: usize) {
    let grass = || sprites::ground('.', x, y).map(|(grass, _)| grass);
    if let Some((ground, standing)) = sprites::ground(tile, x, y) {
        // Some tiles, like the cave's doorway, have see-through corners.
        if let Some(grass) = grass() {
            overlay(image, &imageops::resize(&grass, size, size, FilterType::Nearest), left, top);
        }
        overlay(image, &imageops::resize(&ground, size, size, FilterType::Nearest), left, top);
        if let Some(standing) = standing {
            overlay(image, &imageops::resize(&standing, size, size, FilterType::Nearest), left, top);
        }
        return;
    }

    let base = match tile {
        '~' => WATER,
        '^' => [112, 104, 96],
        ' ' => [14, 14, 16],
        '*' => GRASS,
        _ => [92, 70, 92],
    };
    rectangle(image, left as i32, top as i32, (left + size) as i32 - 1, (top + size) as i32 - 1, base);
    let (cx, cy, s) = ((left + size / 2) as i32, (top + size / 2) as i32, size as i32);
    // In whole steps of the tiles' own pixels, so drawn ground matches the sprites.
    let pixel = (s / 16).max(1);
    match tile {
        '~' => {
            for (row, offset) in [(5, 2), (11, 8)] {
                let ripple_top = top as i32 + row * s / 16;
                rectangle(image, left as i32 + offset * s / 16, ripple_top, left as i32 + (offset + 5) * s / 16, ripple_top + pixel - 1, RIPPLE);
            }
        }
        '^' => {
            let peak = s * 7 / 16;
            for row in 0..peak {
                let shade = if row < peak / 3 { [236, 236, 240] } else { [150, 142, 132] };
                rectangle(image, cx - row, top as i32 + s / 4 + row, cx + row, top as i32 + s / 4 + row, shade);
            }
        }
        '*' => {
            if let Some(grass) = grass() {
                overlay(image, &imageops::resize(&grass, size, size, FilterType::Nearest), left, top);
            }
            circle(image, cx, cy, s * 5 / 16, [250, 232, 140]);
            circle(image, cx, cy, s / 6, [255, 250, 220]);
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
    for y in -radius..=radius {
        for x in -radius..=radius {
            if x * x + y * y <= radius * radius {
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
            {"op": "add", "id": "wolf-1", "kind": "monster", "name": "Wolf", "x": 4, "y": 2, "hp": 5, "look": "wolf"},
            {"op": "add", "id": "wolf-2", "kind": "monster", "name": "Wolf", "x": 5, "y": 2, "hp": 5},
            {"op": "set_hp", "id": "wolf-2", "hp": 0},
            {"op": "add", "id": "bones", "kind": "item", "name": "Old bones", "x": 8, "y": 1},
            {"op": "add", "id": "glow", "kind": "feature", "name": "Faint glow", "x": 11, "y": 3, "look": "crystal"},
            {"op": "set_quest", "quest": "Find the glow"}
        ]"#,
        )
        .unwrap();
        state.apply(&changes).unwrap();
        state
    }

    #[test]
    fn labels_everything_so_the_key_can_name_it() {
        let markers = markers(&scene());
        let labels: Vec<&str> = markers.iter().map(|marker| marker.label.as_str()).collect();
        // Alice stands alone; Will and the two wolves share W, so they're numbered.
        assert_eq!(labels, ["A", "W1", "W2", "W3", "1", "1"]);
        // Players look like adventurers by default; things given a look use it, and
        // others are tokens.
        let drawn: Vec<bool> = markers.iter().map(|marker| marker.sprite.is_some()).collect();
        assert_eq!(drawn, [true, true, true, false, false, true]);
        assert_eq!(markers[3].colour, FALLEN, "the fallen wolf is greyed out");
        assert_eq!((markers[4].shape, markers[5].shape), (Shape::Square, Shape::Diamond));
    }

    #[test]
    fn the_key_names_creatures_with_health_items_places_and_ground() {
        let key: Vec<String> = key_lines(&scene())
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
        assert_eq!(at(0, 0, 1, 1), WATER);
        // Alice's badge, in her colour, in the corner of her square.
        assert_eq!(at(1, 1, 3, 3), PLAYER_COLOURS[0]);
        // The fallen wolf has no look, so it's a grey token.
        assert_eq!(at(5, 2, 16 + 9, 16), FALLEN);
        // The bones' numbered square.
        assert_eq!(at(8, 1, 16 + 9, 16 + 9), ITEM);
        // Grass comes from the tile sheet, so it isn't the drawn colour.
        assert_ne!(at(3, 3, 8, 8), GRASS);
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
