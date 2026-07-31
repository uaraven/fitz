//! Build a display-ready preview of an in-memory FITS [`Image`]: round-trip it
//! through [`image_to_fits`] and read the first image HDU back as an `image`
//! crate [`DynamicImage`](image::DynamicImage), ready for a GUI or terminal
//! renderer to paint.

use crate::data::Image;
use crate::errors::FitsError;
use crate::fits_file::{SaveOptions, image_to_fits};
use crate::fits_image::find_image_hdu_index;
use fitskit::HduData;

pub fn render_preview(src: &Image) -> Result<image::DynamicImage, FitsError> {
    let fits = image_to_fits(src, SaveOptions::default())?;
    let image_hdu_index = find_image_hdu_index(&fits);
    if let Some(image_hdu) = image_hdu_index {
        if let HduData::Image(img) = &fits.hdus[image_hdu].data {
            return Ok(img.to_dynamic_image(1.0, 0.0)?);
        }
    }
    Err(FitsError::ConversionError)
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
