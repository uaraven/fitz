use std::path::Path;

/// Loads the headers of the fits file at `path` and extracts EXPTIME
/// header
/// Returns 0 if file failed to load or the EXPTIME header is missing
pub fn load_exposure(path: &Path) -> f32 {
    let header = libfitz::fits_file::load_header(&path);
    let exposure = header.map(|h| h.header.get_float("EXPTIME")).unwrap_or(None).unwrap_or(0.0);
    exposure as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_data;

    #[test]
    fn load_exposure_reads_the_real_exptime_header() {
        // uncompressed.fit carries `EXPTIME = 180. / [s] Exposure time duration`.
        assert_eq!(load_exposure(&test_data("uncompressed.fit")), 180.0);
    }

    #[test]
    fn load_exposure_is_zero_when_the_file_cannot_be_read() {
        assert_eq!(load_exposure(Path::new("/nonexistent/path.fit")), 0.0);
    }

    #[test]
    fn load_exposure_is_zero_when_exptime_is_absent() {
        use libfitz::fitskit::{FitsFile, ImageData, PixelData};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("no_exptime.fit");
        let img = ImageData::new(vec![2, 2], PixelData::I16(vec![0, 0, 0, 0]));
        FitsFile::with_primary_image(img).to_file(&path).unwrap();

        assert_eq!(load_exposure(&path), 0.0);
    }
}