//! Bridge `libfitz`'s RGBA8 preview buffer to a Slint [`Image`]. This is the
//! one conversion point every on-screen image goes through, so the rest of the
//! GUI never touches raw pixel buffers.

use libfitz::inspect::Tile;
use libfitz::preview::PreviewImage;
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Wrap a [`PreviewImage`]'s interleaved RGBA8 bytes in a Slint [`Image`].
///
/// `render_preview` already guarantees `rgba8.len() == width * height * 4`, so
/// the copy into the pixel buffer lines up exactly.
pub fn preview_to_image(preview: &PreviewImage) -> Image {
    rgba8_to_image(preview.width as u32, preview.height as u32, &preview.rgba8)
}

/// Wrap an aberration-inspector [`Tile`]'s square RGBA8 crop in a Slint
/// [`Image`]. `crop_rgba8` guarantees `rgba8.len() == size * size * 4`.
pub fn tile_to_image(tile: &Tile) -> Image {
    rgba8_to_image(tile.size as u32, tile.size as u32, &tile.rgba8)
}

/// Copy a `width × height` interleaved RGBA8 buffer into a Slint [`Image`]. The
/// caller guarantees `rgba8.len() == width * height * 4`.
fn rgba8_to_image(width: u32, height: u32, rgba8: &[u8]) -> Image {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    buffer.make_mut_bytes().copy_from_slice(rgba8);
    Image::from_rgba8(buffer)
}
