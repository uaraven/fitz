//! Build a display-ready preview of an in-memory [`Image`]: its own pixel
//! buffer, narrowed to 16-bit samples, wrapped directly in an `image` crate
//! [`DynamicImage`](image::DynamicImage) ready for a GUI or terminal renderer
//! to paint. Built straight from `Image`, not via a FITS round-trip:
//! `fitskit`'s FITS→`DynamicImage` conversion only understands a single 2D
//! plane, so encoding an already-debayered `RGB` image and reading it back
//! would silently mis-shape the 3-plane cube into a grayscale buffer.

use crate::data::{Image, ImageType};
use anyhow::{Result, anyhow};
use image::{DynamicImage, ImageBuffer, Luma, Rgb};

pub fn render_preview(src: &Image) -> Result<DynamicImage> {
    let (width, height) = (src.width as u32, src.height as u32);
    let samples = src.pixels.as_u16();
    match src.image_type {
        ImageType::RGB => ImageBuffer::<Rgb<u16>, _>::from_raw(width, height, samples)
            .map(DynamicImage::ImageRgb16)
            .ok_or_else(|| anyhow!("conversion error")),
        ImageType::Grayscale | ImageType::CFA(_) => {
            ImageBuffer::<Luma<u16>, _>::from_raw(width, height, samples)
                .map(DynamicImage::ImageLuma16)
                .ok_or_else(|| anyhow!("conversion error"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fits_file::load_fits;
    use crate::test_support::write_mosaic_fits;
    use tempfile::TempDir;

    #[test]
    fn render_preview_converts_image_to_dynamic_image() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("mosaic.fit");
        write_mosaic_fits(&path, 8, 6, None);

        let image = load_fits(&path).unwrap();
        let dynamic = render_preview(&image).unwrap();

        assert_eq!(dynamic.width(), 8);
        assert_eq!(dynamic.height(), 6);
    }

    /// Regression test: `render_preview` used to hardcode `(bscale, bzero) =
    /// (1.0, 0.0)` instead of the `(1.0, 32768.0)` the unsigned-16 convention
    /// actually writes, which clamped every below-median sample to 0 and made
    /// a stretched (brighter, wider-range) image look no different on screen
    /// from an unstretched one.
    #[test]
    fn render_preview_reflects_a_stretch() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("mosaic.fit");
        write_mosaic_fits(&path, 32, 32, None);

        let image = load_fits(&path).unwrap();
        let flat = render_preview(&image).unwrap().to_luma16();
        let stretched_image = image.stretch(true, crate::stretch::DEFAULT_BRIGHTNESS);
        let stretched = render_preview(&stretched_image).unwrap().to_luma16();

        let mean = |buf: &image::ImageBuffer<image::Luma<u16>, Vec<u16>>| {
            buf.as_raw().iter().map(|&v| v as f64).sum::<f64>() / buf.as_raw().len() as f64
        };
        assert_ne!(mean(&flat), mean(&stretched));
    }

    /// Regression test: a debayered (`ImageType::RGB`) frame used to be
    /// rendered by round-tripping through a FITS-encoded 3-plane cube and
    /// `fitskit::to_dynamic_image`, which only understands a single 2D plane
    /// and either errors or mis-shapes the buffer — visibly, a "debayered"
    /// preview that stayed grayscale. `render_preview` must return an actual
    /// RGB16 image, not a Luma16 one, for an already-debayered source.
    #[test]
    fn render_preview_keeps_debayered_frames_in_color() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("mosaic.fit");
        write_mosaic_fits(&path, 16, 16, Some("RGGB"));

        let mosaic = load_fits(&path).unwrap();
        let rgb = mosaic.debayer().unwrap().unwrap();
        let dynamic = render_preview(&rgb).unwrap();

        assert!(matches!(dynamic, DynamicImage::ImageRgb16(_)));
        assert_eq!(dynamic.width(), 16);
        assert_eq!(dynamic.height(), 16);
    }
}
