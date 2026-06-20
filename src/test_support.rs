//! Helpers shared by the per-module unit tests: locating bundled test data,
//! copying it into a temp dir, and synthesizing small FITS fixtures.

use std::path::{Path, PathBuf};

use fitskit::{FitsFile, HeaderValue, ImageData, PixelData};
use tempfile::TempDir;

use crate::fits_image::BAYERPAT;

/// Absolute path to a file under the crate's `test-data/` directory.
pub(crate) fn test_data(filename: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test-data")
        .join(filename)
}

/// Copy a bundled test-data file into `tmp`, returning the destination path.
pub(crate) fn copy_to_temp(filename: &str, tmp: &TempDir) -> PathBuf {
    let dst = tmp.path().join(filename);
    std::fs::copy(test_data(filename), &dst).unwrap();
    dst
}

/// Write a 2D single-plane I16 mosaic, optionally tagged with a BAYERPAT.
pub(crate) fn write_mosaic_fits(path: &Path, width: usize, height: usize, pattern: Option<&str>) {
    let pixels: Vec<i16> = (0..(width * height) as i16).collect();
    let img = ImageData::new(vec![width, height], PixelData::I16(pixels));
    let mut fits = FitsFile::with_primary_image(img);
    if let Some(p) = pattern {
        fits.primary_mut()
            .header
            .set(BAYERPAT, HeaderValue::String(p.to_string()), None);
    }
    fits.to_file(path).unwrap();
}

/// Write a 3-plane (R, G, B) I16 RGB cube with sequential pixel values.
pub(crate) fn write_rgb_cube_fits(path: &Path, width: usize, height: usize) {
    let n = width * height;
    let mut pixels = Vec::with_capacity(n * 3);
    for c in 0..3 {
        for i in 0..n {
            pixels.push((c * n + i) as i16);
        }
    }
    let img = ImageData::new(vec![width, height, 3], PixelData::I16(pixels));
    let fits = FitsFile::with_primary_image(img);
    fits.to_file(path).unwrap();
}
