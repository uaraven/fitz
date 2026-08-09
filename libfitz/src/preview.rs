//! Build a display-ready preview of an in-memory FITS [`Image`]: round-trip it
//! through [`image_to_fits`] and read the first image HDU back as an `image`
//! crate [`DynamicImage`](image::DynamicImage), ready for a GUI or terminal
//! renderer to paint.

use crate::data::Image;
use crate::fits_file::{SaveOptions, find_image_hdu_index, image_to_fits};
use crate::keywords::{BSCALE, BZERO};
use anyhow::{Result, anyhow};
use fitskit::HduData;

pub fn render_preview(src: &Image) -> Result<image::DynamicImage> {
    let fits = image_to_fits(src, SaveOptions::default())?;
    let image_hdu = find_image_hdu_index(&fits).ok_or_else(|| anyhow!("conversion error"))?;
    let header = &fits.hdus[image_hdu].header;
    // `image_to_fits` always writes the BSCALE/BZERO that actually apply to the
    // encoded samples (32768 for the unsigned-16 convention); hardcoding (1.0,
    // 0.0) here instead clamped every negative-offset sample to 0 and washed
    // out half the display range.
    let bscale = header.get_float(BSCALE).unwrap_or(1.0);
    let bzero = header.get_float(BZERO).unwrap_or(0.0);
    match &fits.hdus[image_hdu].data {
        HduData::Image(img) => Ok(img.to_dynamic_image(bscale, bzero)?),
        _ => Err(anyhow!("conversion error")),
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
}
