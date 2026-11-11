use std::path::Path;

use libfitz::loader::load_header_from_file;
use libfitz::session::{FrameType, exposure_of};

/// Read just the header of the FITS or RAW file at `path` (no pixels) and
/// return what the file list shows from it: the exposure in seconds
/// (`EXPTIME`; 0 when missing or the file cannot be read) and the frame type
/// its `IMAGETYP` names (`None` when it names none, or for a RAW file).
pub fn load_frame_header(path: &Path) -> (f32, Option<FrameType>) {
    match load_header_from_file(path) {
        Ok(header) => (exposure_of(&header) as f32, FrameType::from_header(&header)),
        Err(_) => (0.0, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_data;

    #[test]
    fn load_frame_header_reads_the_real_exptime_and_imagetyp() {
        // uncompressed.fit carries `EXPTIME = 180.` and `IMAGETYP= 'LIGHT'`;
        // the compressed fixture's header carries the same frame type.
        assert_eq!(
            load_frame_header(&test_data("uncompressed.fit")),
            (180.0, Some(FrameType::Light))
        );
        assert_eq!(
            load_frame_header(&test_data("compressed.fits.fz")).1,
            Some(FrameType::Light)
        );
    }

    #[test]
    fn a_raw_frame_has_an_exposure_but_no_frame_type() {
        let (exposure, frame_type) = load_frame_header(&test_data("comet.ARW"));
        assert!(exposure > 0.0);
        assert_eq!(frame_type, None);
    }

    #[test]
    fn load_frame_header_is_empty_when_the_file_cannot_be_read() {
        assert_eq!(
            load_frame_header(Path::new("/nonexistent/path.fit")),
            (0.0, None)
        );
    }

    #[test]
    fn load_frame_header_is_empty_when_exptime_and_imagetyp_are_absent() {
        use libfitz::fitskit::{FitsFile, ImageData, PixelData};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("no_exptime.fit");
        let img = ImageData::new(vec![2, 2], PixelData::I16(vec![0, 0, 0, 0]));
        FitsFile::with_primary_image(img).to_file(&path).unwrap();

        assert_eq!(load_frame_header(&path), (0.0, None));
    }
}
