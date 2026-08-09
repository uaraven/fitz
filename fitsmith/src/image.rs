//! Bridge a rendered RGB8 preview buffer to a Slint [`Image`]. This is the
//! one conversion point every on-screen image goes through, so the rest of the
//! GUI never touches raw pixel buffers.

use libfitz::inspect::Tile;
use slint::{Image, Rgb8Pixel, SharedPixelBuffer};

/// Wrap a rendered preview's interleaved RGB8 bytes in a Slint [`Image`].
pub fn preview_to_image(preview: &image::RgbImage) -> Image {
    rgb8_to_image(preview.width(), preview.height(), preview.as_raw())
}

/// Wrap an aberration-inspector [`Tile`]'s square RGB8 crop in a Slint
/// [`Image`]. `crop_rgb8` guarantees `rgb8.len() == size * size * 3`.
pub fn tile_to_image(tile: &Tile) -> Image {
    rgb8_to_image(tile.size as u32, tile.size as u32, &tile.rgb8)
}

/// Copy a `width × height` interleaved RGB8 buffer into a Slint [`Image`]. The
/// caller guarantees `rgb8.len() == width * height * 3`.
fn rgb8_to_image(width: u32, height: u32, rgb8: &[u8]) -> Image {
    let mut buffer = SharedPixelBuffer::<Rgb8Pixel>::new(width, height);
    buffer.make_mut_bytes().copy_from_slice(rgb8);
    Image::from_rgb8(buffer)
}
