//! Bridge `fitz-core`'s RGBA8 preview buffer to a Slint [`Image`]. This is the
//! one conversion point every on-screen image goes through, so the rest of the
//! GUI never touches raw pixel buffers.

use fitz_core::preview::PreviewImage;
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

/// A small diagonal gradient used when there's no real image to show yet (e.g.
/// the sample frame can't be loaded). Purely a visible "the renderer works"
/// placeholder.
pub fn placeholder_image() -> Image {
    const W: u32 = 320;
    const H: u32 = 240;
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(W, H);
    let pixels = buffer.make_mut_slice();
    for y in 0..H {
        for x in 0..W {
            let i = (y * W + x) as usize;
            pixels[i] = Rgba8Pixel {
                r: (x * 255 / W) as u8,
                g: (y * 255 / H) as u8,
                b: 128,
                a: 255,
            };
        }
    }
    Image::from_rgba8(buffer)
}
