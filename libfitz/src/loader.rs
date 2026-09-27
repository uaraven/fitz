use crate::data::Image;
use crate::fits_file::load_fits;
use crate::raw_file::load_raw_image;
use anyhow::Result;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Determines if the file is in FITS format
fn is_fits_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    };
    // Every FITS file's first header card is the mandatory `SIMPLE` keyword,
    // so its first 6 bytes always spell "SIMPLE".
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 6];
    file.read_exact(&mut magic).is_ok() && &magic == b"SIMPLE"
}

pub fn load_image_from_file(source: &Path) -> Result<Image> {
    if is_fits_file(source) {
        load_fits(source)
    } else {
        // try to load as a RAW file
        load_raw_image(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_data;

    #[test]
    fn recognizes_a_real_fits_file() {
        assert!(is_fits_file(&test_data("uncompressed.fit")));
    }

    #[test]
    fn rejects_a_non_fits_file() {
        assert!(!is_fits_file(&test_data("comet.ARW")));
    }

    #[test]
    fn rejects_a_missing_path() {
        assert!(!is_fits_file(Path::new("/no/such/file.fits")));
    }

    #[test]
    fn load_image_from_file_dispatches_a_fits_file_to_the_fits_loader() {
        let path = test_data("uncompressed.fit");

        let via_loader = load_image_from_file(&path).unwrap();
        let via_fits = load_fits(&path).unwrap();

        assert_eq!(via_loader.image_type, via_fits.image_type);
        assert_eq!(
            (via_loader.width, via_loader.height),
            (via_fits.width, via_fits.height)
        );
        assert_eq!(via_loader.pixels, via_fits.pixels);
    }

    #[test]
    fn load_image_from_file_dispatches_a_raw_file_to_the_raw_loader() {
        let path = test_data("comet.ARW");

        let via_loader = load_image_from_file(&path).unwrap();
        let via_raw = load_raw_image(&path).unwrap();

        assert_eq!(via_loader.image_type, via_raw.image_type);
        assert_eq!(
            (via_loader.width, via_loader.height),
            (via_raw.width, via_raw.height)
        );
        assert_eq!(via_loader.pixels, via_raw.pixels);
    }

    #[test]
    fn load_image_from_file_fails_on_a_missing_path() {
        assert!(load_image_from_file(Path::new("/no/such/file.fits")).is_err());
    }
}
