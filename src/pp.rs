//! `!pp`: an animated GIF of a randomly sized pp, carried over from the old Python bot.
//! The hand strokes down the shaft and back, then it drips. Text is drawn with a
//! bundled monospace font, and the two emoji with bundled Twemoji images, since the
//! font has no emoji.

use std::io::Cursor;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use anyhow::{Context, Result};
use image::codecs::gif::{GifEncoder, Repeat};
use image::imageops::{self, FilterType};
use image::{Delay, Frame, Rgba, RgbaImage};

/// Also used for the `!rt` cast sheet.
pub(crate) const FONT: &[u8] = include_bytes!("../assets/pp/Inconsolata.ttf");
const FIST: &[u8] = include_bytes!("../assets/pp/270a.png");
const DROPS: &[u8] = include_bytes!("../assets/pp/1f4a6.png");

const FIST_CHAR: char = '✊';
const DROP_CHAR: char = '💦';

const FONT_SIZE: f32 = 50.0;
const EMOJI_SIZE: u32 = 48;
const PADDING: u32 = 24;
/// Opaque, so it reads the same in light and dark themes, and no frame shows through
/// the next.
const BACKGROUND: Rgba<u8> = Rgba([30, 31, 34, 255]);
const INK: [u8; 3] = [235, 235, 235];

const PER_FRAME_MS: u32 = 75;
const END_FRAME_MS: u32 = 500;

/// A finished animation.
pub struct PpGif {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The last frame as text, for the log and history.
    pub text: String,
}

/// Sizes picked the same way as the original: a sack of 2 to 5, a shaft of 2 to 10,
/// and 1 to 5 drops.
pub fn random() -> Result<PpGif> {
    render(rand::random_range(2..=5), rand::random_range(2..=10), rand::random_range(1..=5))
}

/// One frame: `(___)==✊===D💦`. `hand` is how many `=` come before the fist.
fn frame(sack: usize, shaft: usize, hand: usize, drops: usize) -> String {
    let mut rod = "=".repeat(shaft);
    rod.insert(hand.min(shaft), FIST_CHAR);
    format!("({}){rod}D{}", "_".repeat(sack), DROP_CHAR.to_string().repeat(drops))
}

/// The hand strokes from the tip to the base and back, then each drop shrinks the sack.
fn frames(sack: usize, shaft: usize, drops: usize) -> Vec<String> {
    let mut frames = Vec::new();
    for hand in (0..shaft).rev() {
        frames.push(frame(sack, shaft, hand, 0));
    }
    for hand in 0..=shaft {
        frames.push(frame(sack, shaft, hand, 0));
    }
    for drop in 1..=drops {
        frames.push(frame(sack.saturating_sub(drop).max(1), shaft, shaft, drop));
    }
    frames
}

fn render(sack: usize, shaft: usize, drops: usize) -> Result<PpGif> {
    let font = FontRef::try_from_slice(FONT).context("bundled font is invalid")?;
    let scaled = font.as_scaled(PxScale::from(FONT_SIZE));
    let fist = sprite(FIST)?;
    let drop = sprite(DROPS)?;

    let texts = frames(sack, shaft, drops);
    let advance = scaled.h_advance(font.glyph_id('='));
    let line_width = |text: &str| -> f32 {
        text.chars().map(|c| if c == FIST_CHAR || c == DROP_CHAR { EMOJI_SIZE as f32 } else { advance }).sum()
    };
    let content_width = texts.iter().map(|text| line_width(text)).fold(0.0, f32::max).ceil() as u32;
    let line_height = (scaled.ascent() - scaled.descent()).ceil().max(EMOJI_SIZE as f32) as u32;
    let (width, height) = (content_width + 2 * PADDING, line_height + 2 * PADDING);

    let mut images = Vec::with_capacity(texts.len());
    for text in &texts {
        let mut image = RgbaImage::from_pixel(width, height, BACKGROUND);
        let baseline = PADDING as f32 + (line_height as f32 + scaled.ascent() + scaled.descent()) / 2.0;
        let mut x = PADDING as f32;
        for c in text.chars() {
            if c == FIST_CHAR || c == DROP_CHAR {
                let emoji = if c == FIST_CHAR { &fist } else { &drop };
                let top = PADDING + (line_height - EMOJI_SIZE) / 2;
                imageops::overlay(&mut image, emoji, x as i64, i64::from(top));
                x += EMOJI_SIZE as f32;
                continue;
            }
            let glyph = font.glyph_id(c).with_scale_and_position(scaled.scale(), point(x, baseline));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, coverage| {
                    let (px, py) = (bounds.min.x as i64 + i64::from(gx), bounds.min.y as i64 + i64::from(gy));
                    if px >= 0 && py >= 0 && (px as u32) < width && (py as u32) < height {
                        blend(image.get_pixel_mut(px as u32, py as u32), coverage);
                    }
                });
            }
            x += advance;
        }
        images.push(image);
    }

    let last = images.len() - 1;
    let mut data = Vec::new();
    {
        let mut encoder = GifEncoder::new_with_speed(Cursor::new(&mut data), 10);
        encoder.set_repeat(Repeat::Infinite)?;
        let frames = images.into_iter().enumerate().map(|(index, image)| {
            let ms = if index == last { END_FRAME_MS } else { PER_FRAME_MS };
            Frame::from_parts(image, 0, 0, Delay::from_numer_denom_ms(ms, 1))
        });
        encoder.encode_frames(frames)?;
    }

    Ok(PpGif { data, width, height, text: texts.last().cloned().unwrap_or_default() })
}

fn sprite(png: &[u8]) -> Result<RgbaImage> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).context("bundled emoji is invalid")?;
    Ok(imageops::resize(&image.to_rgba8(), EMOJI_SIZE, EMOJI_SIZE, FilterType::Lanczos3))
}

fn blend(pixel: &mut Rgba<u8>, coverage: f32) {
    let coverage = coverage.clamp(0.0, 1.0);
    for channel in 0..3 {
        let background = f32::from(pixel[channel]);
        pixel[channel] = (background + (f32::from(INK[channel]) - background) * coverage).round() as u8;
    }
}

#[cfg(test)]
mod tests {
    use image::AnimationDecoder;
    use image::codecs::gif::GifDecoder;

    use super::*;

    #[test]
    fn the_hand_strokes_down_and_back_then_it_drips() {
        let frames = frames(3, 4, 2);
        assert_eq!(frames.first().unwrap(), "(___)===✊=D");
        assert_eq!(frames[3], "(___)✊====D");
        assert_eq!(frames[4], "(___)✊====D");
        assert_eq!(frames[8], "(___)====✊D");
        assert_eq!(frames[9], "(__)====✊D💦");
        assert_eq!(frames.last().unwrap(), "(_)====✊D💦💦");
        assert_eq!(frames.len(), 4 + 5 + 2);
    }

    #[test]
    fn the_sack_never_empties() {
        assert!(frames(2, 2, 5).last().unwrap().starts_with("(_)"));
    }

    #[test]
    fn renders_an_animated_gif_with_one_frame_per_step() {
        let pp = render(5, 10, 5).unwrap();
        assert!(pp.data.starts_with(b"GIF89a"));
        assert_eq!(pp.text, "(_)==========✊D💦💦💦💦💦");

        let decoded = GifDecoder::new(Cursor::new(&pp.data)).unwrap().into_frames().collect_frames().unwrap();
        assert_eq!(decoded.len(), 10 + 11 + 5);
        assert_eq!(decoded[0].buffer().dimensions(), (pp.width, pp.height));
        assert_eq!(decoded.last().unwrap().delay(), Delay::from_numer_denom_ms(END_FRAME_MS, 1));
    }
}
