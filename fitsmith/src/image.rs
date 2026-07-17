//! Bridge `libfitz`'s RGBA8 preview buffer to a Slint [`Image`]. This is the
//! one conversion point every on-screen image goes through, so the rest of the
//! GUI never touches raw pixel buffers.

use libfitz::preview::PreviewImage;
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Wrap a [`PreviewImage`]'s interleaved RGBA8 bytes in a Slint [`Image`].
///
/// `render_preview` already guarantees `rgba8.len() == width * height * 4`, so
/// the copy into the pixel buffer lines up exactly.
pub fn preview_to_image(preview: &PreviewImage) -> Image {
    let mut buffer =
        SharedPixelBuffer::<Rgba8Pixel>::new(preview.width as u32, preview.height as u32);
    buffer.make_mut_bytes().copy_from_slice(&preview.rgba8);
    Image::from_rgba8(buffer)
}
