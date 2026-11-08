//! A contact sheet of cast and crew photos, each with the person's name and role
//! underneath, so `!rt` can show who's in something as one image instead of a run of
//! separate ones. Text is drawn with the font bundled for `!pp`.

use std::io::Cursor;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use anyhow::{Context, Result, bail};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{self, FilterType};
use image::{Rgb, RgbImage};

use crate::message_log::ReplyImage;

/// The size Rotten Tomatoes serves headshots at.
const PHOTO_WIDTH: u32 = 100;
const PHOTO_HEIGHT: u32 = 120;
const TILE_WIDTH: u32 = 160;
const COLUMNS: u32 = 3;
const GAP: u32 = 16;
const PADDING: u32 = 20;
const FONT_SIZE: f32 = 15.0;
const LINE_HEIGHT: u32 = 18;
/// Below the photo: up to two lines of name, then up to two of role.
const NAME_LINES: usize = 2;
const ROLE_LINES: usize = 2;
const TEXT_GAP: u32 = 8;
const QUALITY: u8 = 90;

/// Matches the `!pp` GIF, so the bot's images look like a set.
const BACKGROUND: Rgb<u8> = Rgb([30, 31, 34]);
const NAME_INK: [u8; 3] = [235, 235, 235];
const ROLE_INK: [u8; 3] = [160, 162, 168];
/// Shown in place of a photo that couldn't be loaded.
const NO_PHOTO: Rgb<u8> = Rgb([58, 60, 66]);

pub struct Person {
    pub name: String,
    pub role: Option<String>,
    /// The headshot's encoded bytes, or `None` to draw a blank in its place.
    pub photo: Option<Vec<u8>>,
}

/// Draws the sheet as a JPEG. Fails if there's nobody to show, or if no photo could be
/// read, since a sheet of blanks isn't worth posting.
pub fn render(people: &[Person]) -> Result<ReplyImage> {
    if people.is_empty() {
        bail!("nobody to show");
    }
    let photos: Vec<Option<RgbImage>> = people.iter().map(|person| person.photo.as_deref().and_then(decode_photo)).collect();
    if photos.iter().all(Option::is_none) {
        bail!("no photo could be read");
    }

    let font = FontRef::try_from_slice(crate::pp::FONT).context("bundled font is invalid")?;
    let scale = PxScale::from(FONT_SIZE);
    let advance = font.as_scaled(scale).h_advance(font.glyph_id('M'));
    let chars_per_line = ((TILE_WIDTH as f32) / advance).floor().max(1.0) as usize;

    let count = people.len() as u32;
    let columns = count.min(COLUMNS);
    let rows = count.div_ceil(COLUMNS);
    let tile_height = PHOTO_HEIGHT + TEXT_GAP + LINE_HEIGHT * (NAME_LINES + ROLE_LINES) as u32;
    let width = 2 * PADDING + columns * TILE_WIDTH + (columns - 1) * GAP;
    let height = 2 * PADDING + rows * tile_height + (rows - 1) * GAP;
    let mut sheet = RgbImage::from_pixel(width, height, BACKGROUND);

    for (index, (person, photo)) in people.iter().zip(&photos).enumerate() {
        let index = index as u32;
        let left = PADDING + (index % COLUMNS) * (TILE_WIDTH + GAP);
        let top = PADDING + (index / COLUMNS) * (tile_height + GAP);

        let photo_left = left + (TILE_WIDTH - PHOTO_WIDTH) / 2;
        match photo {
            Some(photo) => imageops::replace(&mut sheet, photo, i64::from(photo_left), i64::from(top)),
            None => {
                let blank = RgbImage::from_pixel(PHOTO_WIDTH, PHOTO_HEIGHT, NO_PHOTO);
                imageops::replace(&mut sheet, &blank, i64::from(photo_left), i64::from(top));
            }
        }

        let mut line_top = top + PHOTO_HEIGHT + TEXT_GAP;
        let name_lines = wrap(&person.name, chars_per_line, NAME_LINES);
        let role_lines = person.role.as_deref().map(|role| wrap(role, chars_per_line, ROLE_LINES)).unwrap_or_default();
        for (lines, ink) in [(name_lines, NAME_INK), (role_lines, ROLE_INK)] {
            for line in lines {
                let line_width = advance * line.chars().count() as f32;
                let x = left as f32 + (TILE_WIDTH as f32 - line_width) / 2.0;
                let baseline = (line_top + LINE_HEIGHT) as f32 - 4.0;
                draw_text(&mut sheet, &font, scale, x, baseline, &line, ink);
                line_top += LINE_HEIGHT;
            }
        }
    }

    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(Cursor::new(&mut jpeg), QUALITY).encode_image(&sheet).context("failed to encode the cast sheet")?;
    Ok(ReplyImage {
        filename: "cast.jpg".to_string(),
        mime: mime::IMAGE_JPEG,
        data: jpeg,
        width,
        height,
        animated: false,
    })
}

/// A headshot at the size its tile has room for, cropped rather than squashed if it
/// isn't the usual shape.
fn decode_photo(data: &[u8]) -> Option<RgbImage> {
    let image = image::load_from_memory(data).ok()?;
    Some(image.resize_to_fill(PHOTO_WIDTH, PHOTO_HEIGHT, FilterType::Lanczos3).to_rgb8())
}

/// Breaks text into at most `max_lines` lines of `width` characters, between words
/// where it can, ending with an ellipsis if it doesn't all fit.
fn wrap(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        loop {
            let used = current.chars().count();
            let space = usize::from(used > 0);
            if used + space + word.len() <= width {
                if space == 1 {
                    current.push(' ');
                }
                current.extend(word.iter());
                break;
            }
            if used > 0 {
                lines.push(std::mem::take(&mut current));
                continue;
            }
            // A word longer than a whole line is split.
            current = word.drain(..width).collect();
            lines.push(std::mem::take(&mut current));
            if word.is_empty() {
                break;
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }

    if lines.len() > max_lines {
        lines.truncate(max_lines);
        let last = lines.last_mut().expect("max_lines is at least one");
        let mut kept: Vec<char> = last.chars().collect();
        kept.truncate(width.saturating_sub(1));
        *last = format!("{}…", kept.into_iter().collect::<String>().trim_end());
    }
    lines
}

/// Also used for the `!dm` map's labels.
pub(crate) fn draw_text(image: &mut RgbImage, font: &FontRef, scale: PxScale, x: f32, baseline: f32, text: &str, ink: [u8; 3]) {
    let scaled = font.as_scaled(scale);
    let (width, height) = image.dimensions();
    let mut x = x;
    for c in text.chars() {
        let id = font.glyph_id(c);
        if let Some(outlined) = font.outline_glyph(id.with_scale_and_position(scale, point(x, baseline))) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                let (px, py) = (bounds.min.x as i64 + i64::from(gx), bounds.min.y as i64 + i64::from(gy));
                if px >= 0 && py >= 0 && (px as u32) < width && (py as u32) < height {
                    let pixel = image.get_pixel_mut(px as u32, py as u32);
                    let coverage = coverage.clamp(0.0, 1.0);
                    for channel in 0..3 {
                        let background = f32::from(pixel[channel]);
                        pixel[channel] = (background + (f32::from(ink[channel]) - background) * coverage).round() as u8;
                    }
                }
            });
        }
        x += scaled.h_advance(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo() -> Vec<u8> {
        let mut jpeg = Vec::new();
        JpegEncoder::new(Cursor::new(&mut jpeg)).encode_image(&RgbImage::from_pixel(100, 120, Rgb([200, 120, 60]))).unwrap();
        jpeg
    }

    fn person(name: &str, role: Option<&str>, photo: Option<Vec<u8>>) -> Person {
        Person { name: name.to_string(), role: role.map(String::from), photo }
    }

    #[test]
    fn wraps_between_words_and_ends_with_an_ellipsis() {
        assert_eq!(wrap("Timothée Chalamet", 20, 2), ["Timothée Chalamet"]);
        assert_eq!(wrap("Carmen \"Carmy\" Berzatto", 20, 2), ["Carmen \"Carmy\"", "Berzatto"]);
        assert_eq!(wrap("one two three four five six", 9, 2), ["one two", "three…"]);
        assert_eq!(wrap("Supercalifragilistic", 8, 3), ["Supercal", "ifragili", "stic"]);
        assert!(wrap("", 10, 2).is_empty());
    }

    #[test]
    fn the_font_has_an_ellipsis() {
        let font = FontRef::try_from_slice(crate::pp::FONT).unwrap();
        assert_ne!(font.glyph_id('…').0, 0);
    }

    #[test]
    fn lays_people_out_in_rows_of_three() {
        let people: Vec<Person> =
            (0..5).map(|index| person(&format!("Person {index}"), Some("Role"), (index != 2).then(photo))).collect();
        let sheet = render(&people).unwrap();
        let tile_height = PHOTO_HEIGHT + TEXT_GAP + LINE_HEIGHT * 4;
        assert_eq!((sheet.width, sheet.height), (2 * PADDING + 3 * TILE_WIDTH + 2 * GAP, 2 * PADDING + 2 * tile_height + GAP));

        let decoded = image::load_from_memory(&sheet.data).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (sheet.width, sheet.height));
        // The first photo is drawn where its tile puts it.
        let pixel = decoded.get_pixel(PADDING + (TILE_WIDTH - PHOTO_WIDTH) / 2 + 50, PADDING + 60);
        assert!(pixel[0] > 150 && pixel[2] < 110, "{pixel:?}");

        let one = render(&[person("Solo", None, Some(photo()))]).unwrap();
        assert_eq!(one.width, 2 * PADDING + TILE_WIDTH);
    }

    #[test]
    fn needs_at_least_one_photo() {
        assert!(render(&[]).is_err());
        assert!(render(&[person("No Photo", None, None), person("Bad", None, Some(b"junk".to_vec()))]).is_err());
    }
}
