//! Build a display-ready preview of an in-memory FITS [`Image`]: round-trip it
//! through [`image_to_fits`] and read the first image HDU back as an `image`
//! crate [`DynamicImage`](image::DynamicImage), ready for a GUI or terminal
//! renderer to paint.

use crate::data::Image;
use crate::fits_file::{SaveOptions, find_image_hdu_index, image_to_fits};
use anyhow::{Result, anyhow};
use fitskit::HduData;

pub fn render_preview(src: &Image) -> Result<image::DynamicImage> {
    let fits = image_to_fits(src, SaveOptions::default())?;
    let image_hdu = find_image_hdu_index(&fits).ok_or_else(|| anyhow!("conversion error"))?;
    match &fits.hdus[image_hdu].data {
        HduData::Image(img) => Ok(img.to_dynamic_image(1.0, 0.0)?),
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
}
