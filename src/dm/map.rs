//! Draws a game's map: coloured squares for the ground, a token for everyone and
//! everything on it, and a strip with the round, the goal, and the party's health.

use std::io::Cursor;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use anyhow::{Context, Result};
use image::imageops::{self, FilterType};
use image::{ImageFormat, Rgb, RgbImage};

use super::state::{GameState, Kind};
use crate::message_log::ReplyImage;

const SQUARE: u32 = 32;
/// Narrow maps still get room for the strip's text.
const MIN_WIDTH: u32 = 360;
const STRIP_HEIGHT: u32 = 46;
const FONT_SIZE: f32 = 15.0;
const TOKEN_FONT_SIZE: f32 = 17.0;

const BACKGROUND: Rgb<u8> = Rgb([30, 31, 34]);
const TEXT: [u8; 3] = [235, 235, 235];
const MUTED: [u8; 3] = [160, 162, 168];
const PLAYER_COLOURS: [[u8; 3]; 6] =
    [[66, 135, 245], [245, 166, 35], [155, 89, 182], [26, 188, 156], [236, 240, 241], [241, 196, 15]];
const MONSTER: [u8; 3] = [200, 45, 45];
const NPC: [u8; 3] = [240, 200, 60];
const ITEM: [u8; 3] = [212, 175, 55];
const FALLEN: [u8; 3] = [110, 110, 110];

/// The full map for the game room, and a half-size one for the main channel's thread.
pub fn render(state: &GameState, round: u32) -> Result<(ReplyImage, ReplyImage)> {
    let font = FontRef::try_from_slice(crate::pp::FONT).context("bundled font is invalid")?;
    let map_width = state.width as u32 * SQUARE;
    let width = map_width.max(MIN_WIDTH);
    let height = state.height as u32 * SQUARE + STRIP_HEIGHT;
    let mut image = RgbImage::from_pixel(width, height, BACKGROUND);

    for y in 0..state.height {
        for x in 0..state.width {
            draw_square(&mut image, x as u32 * SQUARE, y as u32 * SQUARE, state.tile(x, y));
        }
    }

    let mut players = 0;
    for entity in &state.entities {
        let fallen = entity.hp == Some(0);
        let colour = match entity.kind {
            _ if fallen => FALLEN,
            Kind::Player => {
                players += 1;
                PLAYER_COLOURS[(players - 1) % PLAYER_COLOURS.len()]
            }
            Kind::Monster => MONSTER,
            Kind::Npc => NPC,
            Kind::Item => ITEM,
        };
        let (cx, cy) = ((entity.x as u32 * SQUARE + SQUARE / 2) as i32, (entity.y as u32 * SQUARE + SQUARE / 2) as i32);
        if entity.kind == Kind::Item {
            diamond(&mut image, cx, cy, 8, colour);
            continue;
        }
        circle(&mut image, cx, cy, 13, [20, 20, 22]);
        circle(&mut image, cx, cy, 11, colour);
        let initial: String = entity.name.chars().find(|c| c.is_alphanumeric()).unwrap_or('?').to_uppercase().collect();
        let ink = if colour.iter().map(|&c| u32::from(c)).sum::<u32>() > 450 { [20, 20, 22] } else { TEXT };
        let scale = PxScale::from(TOKEN_FONT_SIZE);
        let advance = font.as_scaled(scale).h_advance(font.glyph_id(initial.chars().next().unwrap_or('?')));
        crate::cast_sheet::draw_text(&mut image, &font, scale, cx as f32 - advance / 2.0, cy as f32 + 6.0, &initial, ink);
    }

    let top = state.height as u32 * SQUARE;
    let scale = PxScale::from(FONT_SIZE);
    let fits = (width as f32 / font.as_scaled(scale).h_advance(font.glyph_id('M'))) as usize;
    let quest = if state.quest.is_empty() { String::new() } else { format!(" · {}", state.quest) };
    let first = fit(&format!("Round {round}{quest}"), fits);
    crate::cast_sheet::draw_text(&mut image, &font, scale, 8.0, (top + 19) as f32, &first, TEXT);
    let party: Vec<String> = state
        .entities
        .iter()
        .filter(|entity| entity.kind == Kind::Player)
        .map(|entity| match (entity.hp, entity.max_hp) {
            (Some(hp), Some(max)) => format!("{} {hp}/{max}", entity.name),
            _ => entity.name.clone(),
        })
        .collect();
    crate::cast_sheet::draw_text(&mut image, &font, scale, 8.0, (top + 39) as f32, &fit(&party.join(" · "), fits), MUTED);

    // The thread's caption says what the strip does, and its text would be too small
    // to read at half size.
    let map_only = imageops::crop_imm(&image, 0, 0, width, top).to_image();
    let small = imageops::resize(&map_only, width / 2, top / 2, FilterType::Triangle);
    Ok((png(&image, "map.png")?, png(&small, "map-small.png")?))
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

fn draw_square(image: &mut RgbImage, left: u32, top: u32, tile: char) {
    let base = match tile {
        '.' | 'T' | 'c' | '*' => GRASS,
        ',' => [139, 115, 85],
        '_' => [118, 118, 124],
        '#' | '+' => [64, 64, 70],
        '^' => [98, 92, 86],
        '~' => [52, 98, 160],
        ' ' => [14, 14, 16],
        _ => [92, 70, 92],
    };
    for y in top..top + SQUARE {
        for x in left..left + SQUARE {
            // A faint grid, so squares can be counted.
            let edge = x == left || y == top;
            let shade = if edge { 0.82 } else { 1.0 };
            image.put_pixel(x, y, Rgb(base.map(|c| (f32::from(c) * shade) as u8)));
        }
    }
    let (cx, cy) = ((left + SQUARE / 2) as i32, (top + SQUARE / 2) as i32);
    match tile {
        'T' => {
            circle(image, cx, cy, 12, [34, 78, 38]);
            circle(image, cx - 3, cy - 3, 5, [52, 104, 54]);
        }
        'c' => ellipse(image, cx, cy + 3, 12, 8, [10, 10, 12]),
        '*' => {
            circle(image, cx, cy, 10, [250, 232, 140]);
            circle(image, cx, cy, 5, [255, 250, 220]);
        }
        '~' => {
            for x in left + 6..left + SQUARE - 6 {
                image.put_pixel(x, top + 12, Rgb([92, 140, 200]));
                image.put_pixel(x, top + 21, Rgb([92, 140, 200]));
            }
        }
        '#' => {
            for x in left..left + SQUARE {
                image.put_pixel(x, top + SQUARE / 2, Rgb([84, 84, 92]));
            }
            for y in top..top + SQUARE / 2 {
                image.put_pixel(left + SQUARE / 2, y, Rgb([84, 84, 92]));
            }
        }
        '+' => {
            for y in top + 4..top + SQUARE - 2 {
                for x in left + 8..left + SQUARE - 8 {
                    image.put_pixel(x, y, Rgb([128, 84, 44]));
                }
            }
        }
        '^' => {
            for row in 0..14 {
                for x in (cx - row)..=(cx + row) {
                    image.put_pixel(x as u32, top + 8 + row as u32, Rgb([130, 122, 112]));
                }
            }
        }
        _ => {}
    }
}

fn circle(image: &mut RgbImage, cx: i32, cy: i32, radius: i32, colour: [u8; 3]) {
    ellipse(image, cx, cy, radius, radius, colour);
}

fn ellipse(image: &mut RgbImage, cx: i32, cy: i32, rx: i32, ry: i32, colour: [u8; 3]) {
    for y in -ry..=ry {
        for x in -rx..=rx {
            let inside = (x * x) as f32 / (rx * rx) as f32 + (y * y) as f32 / (ry * ry) as f32 <= 1.0;
            let (px, py) = (cx + x, cy + y);
            if inside && px >= 0 && py >= 0 && (px as u32) < image.width() && (py as u32) < image.height() {
                image.put_pixel(px as u32, py as u32, Rgb(colour));
            }
        }
    }
}

fn diamond(image: &mut RgbImage, cx: i32, cy: i32, size: i32, colour: [u8; 3]) {
    for y in -size..=size {
        let reach = size - y.abs();
        for x in -reach..=reach {
            image.put_pixel((cx + x) as u32, (cy + y) as u32, Rgb(colour));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dm::state::Change;

    #[test]
    fn draws_the_ground_tokens_and_strip() {
        let mut state = GameState::new(&[("@a:x".to_string(), "Alice".to_string(), "alice".to_string())]);
        let changes: Vec<Change> = serde_json::from_str(
            r#"[
            {"op": "set_map", "rows": ["~~~~~~~~~~~~", "............", "......T.....", "...........c"]},
            {"op": "move", "id": "alice", "x": 1, "y": 1},
            {"op": "add", "id": "wolf", "kind": "monster", "name": "Wolf", "x": 4, "y": 2, "hp": 5},
            {"op": "set_quest", "quest": "Find the glow"}
        ]"#,
        )
        .unwrap();
        state.apply(&changes).unwrap();
        let (full, small) = render(&state, 3).unwrap();
        assert_eq!((full.width, full.height), (12 * SQUARE, 4 * SQUARE + STRIP_HEIGHT));
        assert_eq!((small.width, small.height), (full.width / 2, 4 * SQUARE / 2));

        let image = image::load_from_memory(&full.data).unwrap().to_rgb8();
        let at = |x: u32, y: u32| image.get_pixel(x * SQUARE + SQUARE / 2 + 7, y * SQUARE + SQUARE / 2 + 7).0;
        assert_eq!(at(0, 0), [52, 98, 160], "water");
        assert_eq!(at(2, 1), GRASS);
        // Tokens are drawn over the ground, the player's in its colour and the wolf's in red.
        let token = |x: u32, y: u32| image.get_pixel(x * SQUARE + SQUARE / 2 + 8, y * SQUARE + SQUARE / 2).0;
        assert_eq!(token(1, 1), PLAYER_COLOURS[0]);
        assert_eq!(token(4, 2), MONSTER);
        // The strip has text in it.
        let strip = (0..full.width).any(|x| image.get_pixel(x, 4 * SQUARE + 14).0 != BACKGROUND.0);
        assert!(strip);
    }

    #[test]
    fn narrow_maps_leave_room_for_the_strip() {
        let mut state = GameState::new(&[]);
        let changes: Vec<Change> = serde_json::from_str(r#"[{"op": "set_map", "rows": ["....", "....", "....", "...."]}]"#).unwrap();
        state.apply(&changes).unwrap();
        assert_eq!(render(&state, 1).unwrap().0.width, MIN_WIDTH);
    }

    #[test]
    fn shortens_text_that_doesnt_fit() {
        assert_eq!(fit("short", 10), "short");
        assert_eq!(fit("a much longer line", 8), "a much…");
    }
}
