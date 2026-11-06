//! A grid of photos posted as one image, so `!strain` shows several pictures without
//! filling the room with separate messages.

use std::io::Cursor;

use anyhow::{Context, Result, bail};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{self, FilterType};
use image::{Rgb, RgbImage};

use crate::message_log::ReplyImage;

const TILE: u32 = 220;
const COLUMNS: u32 = 3;
const GAP: u32 = 8;
const PADDING: u32 = 8;
const QUALITY: u8 = 85;

/// Matches the `!pp` GIF and the `!rt` cast sheet, so the bot's images look like a set.
const BACKGROUND: [u8; 3] = [30, 31, 34];

/// Draws the photos that decode as square tiles, in rows of three, and encodes the
/// sheet as a JPEG. Fails if none of them decode.
pub fn render(photos: &[Vec<u8>], filename: &str) -> Result<ReplyImage> {
    let tiles: Vec<RgbImage> = photos.iter().filter_map(|data| tile(data)).collect();
    if tiles.is_empty() {
        bail!("no photo could be read");
    }

    let count = tiles.len() as u32;
    let columns = count.min(COLUMNS);
    let rows = count.div_ceil(COLUMNS);
    let width = 2 * PADDING + columns * TILE + (columns - 1) * GAP;
    let height = 2 * PADDING + rows * TILE + (rows - 1) * GAP;
    let mut sheet = RgbImage::from_pixel(width, height, Rgb(BACKGROUND));
    for (index, tile) in tiles.iter().enumerate() {
        let index = index as u32;
        let left = PADDING + (index % COLUMNS) * (TILE + GAP);
        let top = PADDING + (index / COLUMNS) * (TILE + GAP);
        imageops::replace(&mut sheet, tile, i64::from(left), i64::from(top));
    }

    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(Cursor::new(&mut jpeg), QUALITY).encode_image(&sheet).context("failed to encode the photo sheet")?;
    Ok(ReplyImage { filename: filename.to_string(), mime: mime::IMAGE_JPEG, data: jpeg, width, height, animated: false })
}

/// A photo cropped to fill a square tile. Leafly's product shots are PNGs with
/// transparent backgrounds, so those are laid over the sheet's colour rather than
/// whatever the transparent pixels happen to hold.
fn tile(data: &[u8]) -> Option<RgbImage> {
    let image = image::load_from_memory(data).ok()?.resize_to_fill(TILE, TILE, FilterType::Lanczos3).to_rgba8();
    Some(RgbImage::from_fn(TILE, TILE, |x, y| {
        let [red, green, blue, alpha] = image.get_pixel(x, y).0;
        let alpha = u16::from(alpha);
        let blend = |ink: u8, background: u8| ((u16::from(ink) * alpha + u16::from(background) * (255 - alpha)) / 255) as u8;
        Rgb([blend(red, BACKGROUND[0]), blend(green, BACKGROUND[1]), blend(blue, BACKGROUND[2])])
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};

    fn png(width: u32, height: u32, color: Rgba<u8>) -> Vec<u8> {
        let mut data = Vec::new();
        RgbaImage::from_pixel(width, height, color).write_to(&mut Cursor::new(&mut data), ImageFormat::Png).unwrap();
        data
    }

    #[test]
    fn lays_photos_out_in_rows_of_three() {
        let photos: Vec<Vec<u8>> = (0..4).map(|_| png(300, 200, Rgba([200, 120, 60, 255]))).collect();
        let sheet = render(&photos, "strain.jpg").unwrap();
        assert_eq!((sheet.width, sheet.height), (2 * PADDING + 3 * TILE + 2 * GAP, 2 * PADDING + 2 * TILE + GAP));
        assert_eq!(sheet.filename, "strain.jpg");

        let decoded = image::load_from_memory(&sheet.data).unwrap().to_rgb8();
        let photo = decoded.get_pixel(PADDING + TILE / 2, PADDING + TILE / 2);
        assert!(photo[0] > 170 && photo[2] < 90, "{photo:?}");
        // The fifth and sixth places are left empty.
        let empty = decoded.get_pixel(PADDING + TILE + GAP + TILE / 2, PADDING + TILE + GAP + TILE / 2);
        assert!(empty.0.iter().zip(BACKGROUND).all(|(&got, want)| got.abs_diff(want) < 12), "{empty:?}");

        let one = render(&[png(10, 10, Rgba([0, 0, 0, 255]))], "strain.jpg").unwrap();
        assert_eq!((one.width, one.height), (2 * PADDING + TILE, 2 * PADDING + TILE));
    }

    #[test]
    fn transparent_pixels_show_the_background() {
        let sheet = render(&[png(50, 50, Rgba([255, 255, 255, 0]))], "strain.jpg").unwrap();
        let decoded = image::load_from_memory(&sheet.data).unwrap().to_rgb8();
        let pixel = decoded.get_pixel(PADDING + TILE / 2, PADDING + TILE / 2);
        assert!(pixel.0.iter().zip(BACKGROUND).all(|(&got, want)| got.abs_diff(want) < 12), "{pixel:?}");
    }

    #[test]
    fn skips_photos_that_dont_decode() {
        assert!(render(&[], "strain.jpg").is_err());
        assert!(render(&[b"junk".to_vec()], "strain.jpg").is_err());
        let sheet = render(&[b"junk".to_vec(), png(10, 10, Rgba([0, 0, 0, 255]))], "strain.jpg").unwrap();
        assert_eq!(sheet.width, 2 * PADDING + TILE);
    }
}
