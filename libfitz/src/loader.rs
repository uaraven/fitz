use crate::data::Image;
use crate::fits_file::{load_fits, load_header};
use crate::raw_file::{load_raw_header, load_raw_image};
use anyhow::Result;
use fitskit::Header;
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

/// Loads a supported image from file
/// Image can be either in FITS format
/// or in one of RAW formats supported by libraw
pub fn load_image_from_file(source: &Path) -> Result<Image> {
    if is_fits_file(source) {
        load_fits(source)
    } else {
        // try to load as a RAW file
        load_raw_image(source)
    }
}

/// Reads just the header of a supported image — FITS or camera RAW — without
/// decoding its pixels (a FITS file's pixel data is skipped unread; a RAW file
/// is read whole, since LibRaw parses from memory). A RAW file's EXIF is translated to FITS-style keywords
/// (`DATE-OBS`, `EXPTIME`, …), so callers see one header shape for both
/// formats. The header-only counterpart of [`load_image_from_file`].
pub fn load_header_from_file(source: &Path) -> Result<Header> {
    if is_fits_file(source) {
        Ok(load_header(source)?.header)
    } else {
        load_raw_header(source)
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
    fn load_header_from_file_reads_a_fits_header() {
        let path = test_data("uncompressed.fit");

        let header = load_header_from_file(&path).unwrap();

        assert_eq!(header.get_string("IMAGETYP").map(str::trim), Some("LIGHT"));
        assert_eq!(
            header.get_string("FILTER").map(str::trim),
            Some("Narrowband 7nm")
        );
        assert_eq!(
            header.get_string("DATE-OBS"),
            load_fits(&path).unwrap().header.get_string("DATE-OBS")
        );
    }

    #[test]
    fn load_header_from_file_reads_raw_exif_as_fits_keywords() {
        let path = test_data("comet.ARW");

        let header = load_header_from_file(&path).unwrap();
        let full = load_raw_image(&path).unwrap().header;

        // The EXIF-derived fields match what a full RAW load produces.
        assert!(header.get_string("DATE-OBS").is_some());
        assert!(header.get_float("EXPTIME").is_some_and(|e| e > 0.0));
        assert!(header.get_string("INSTRUME").is_some());
        for key in ["DATE-OBS", "INSTRUME", "TELESCOP", "OBJECT"] {
            assert_eq!(header.get_string(key), full.get_string(key), "{key}");
        }
        assert_eq!(header.get_float("EXPTIME"), full.get_float("EXPTIME"));
        assert_eq!(header.get_int("ISOSPEED"), full.get_int("ISOSPEED"));
        // Pixel-dependent cards are not part of the header-only read.
        assert_eq!(header.get_string("BAYERPAT"), None);
        assert!(header.find("HISTORY").is_none());
        // ...but a FILTER never exists in RAW EXIF.
        assert_eq!(header.get_string("FILTER"), None);
    }

    #[test]
    fn load_header_from_file_fails_on_a_missing_path() {
        assert!(load_header_from_file(Path::new("/no/such/file.fits")).is_err());
    }

    #[test]
    fn load_image_from_file_fails_on_a_missing_path() {
        assert!(load_image_from_file(Path::new("/no/such/file.fits")).is_err());
    }
}
