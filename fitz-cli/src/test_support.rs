//! Minimal test helpers for the CLI's own tests: locating bundled test data and
//! synthesizing a tiny frame. The bulk of the fixture *generators* live in
//! `libfitz`'s test support, since the domain logic that consumes them moved
//! there.

use std::path::{Path, PathBuf};

use libfitz::fitskit::{FitsFile, HeaderValue, ImageData, PixelData};

/// Absolute path to a file under the workspace's `test-data/` directory.
pub(crate) fn test_data(filename: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("test-data")
        .join(filename)
}

/// Write a tiny 4x4 FITS frame carrying a `FILTER` (optional), `EXPTIME`
/// (seconds) and a `DATE-LOC` timestamp string.
pub(crate) fn write_frame_fits(
    path: &Path,
    filter: Option<&str>,
    exptime: f64,
    date_loc: Option<&str>,
) {
    let img = ImageData::new(vec![4, 4], PixelData::I16(vec![0; 16]));
    let mut fits = FitsFile::with_primary_image(img);
    let header = &mut fits.primary_mut().header;
    header.set("EXPTIME", HeaderValue::Float(exptime), None);
    for (key, value) in [("FILTER", filter), ("DATE-LOC", date_loc)] {
        if let Some(value) = value {
            header.set(key, HeaderValue::String(value.to_string()), None);
        }
    }
    fits.to_file(path).unwrap();
}
