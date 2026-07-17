//! Helpers shared by the per-module unit tests: locating bundled test data,
//! copying it into a temp dir, and synthesizing small FITS fixtures.

use std::path::{Path, PathBuf};

use fitskit::{FitsFile, Header, HeaderValue, ImageData, PixelData};
use tempfile::TempDir;

use crate::fits_image::{BAYERPAT, BZERO, round_to_u16};

/// Absolute path to a file under the workspace's `test-data/` directory.
pub(crate) fn test_data(filename: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("test-data")
        .join(filename)
}

/// Read back the primary HDU's header of a FITS file written by a test, for
/// asserting on metadata a command's output should carry over or drop.
pub(crate) fn output_header(path: &Path) -> Header {
    FitsFile::from_file(path).unwrap().primary().header.clone()
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

/// Like [`write_mosaic_fits`] but also stamps representative metadata (OBJECT,
/// DATE-OBS, a WCS pair, and a COMMENT card) so header-preservation tests have
/// something to assert survives the processing commands.
pub(crate) fn write_mosaic_fits_with_metadata(
    path: &Path,
    width: usize,
    height: usize,
    pattern: Option<&str>,
) {
    let pixels: Vec<i16> = (0..(width * height) as i16).collect();
    let img = ImageData::new(vec![width, height], PixelData::I16(pixels));
    let mut fits = FitsFile::with_primary_image(img);
    let header = &mut fits.primary_mut().header;
    if let Some(p) = pattern {
        header.set(BAYERPAT, HeaderValue::String(p.to_string()), None);
    }
    header.set("OBJECT", HeaderValue::String("M31".to_string()), None);
    header.set(
        "DATE-OBS",
        HeaderValue::String("2026-06-22T00:00:00".to_string()),
        None,
    );
    header.set("CRVAL1", HeaderValue::Float(10.68), None);
    header.set("CRVAL2", HeaderValue::Float(41.27), None);
    header.push(fitskit::Keyword::commentary(
        "COMMENT",
        "captured by fitz test suite",
    ));
    fits.to_file(path).unwrap();
}

/// Write a 2D mono frame in the unsigned-16 convention (I16 samples with
/// BZERO 32768, so physical values span 0..=65535) holding a synthetic star
/// field: a flat `background`, a fixed low-amplitude ripple standing in for
/// noise, and one 2D Gaussian per `(x, y, sigma_x, sigma_y, peak)`. Values above
/// the 16-bit ceiling clip there, exactly as a sensor's would.
///
/// The ripple is deterministic rather than random: the background's MAD sets the
/// detection threshold, so a seeded RNG would let a star-detection test flake on
/// its seed.
///
/// Carries a DATE-OBS so an analytics batch will key it onto the time axis
/// rather than skipping it; a test wanting several frames in an order overrides
/// [`FileMetrics::time`](crate::analytics::FileMetrics) itself.
pub(crate) fn write_star_field_fits(
    path: &Path,
    width: usize,
    height: usize,
    background: f64,
    stars: &[(f64, f64, f64, f64, f64)],
) {
    let mut pixels = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let ripple = ((x * 7 + y * 13) % 5) as f64;
            let mut v = background + ripple;
            for &(sx, sy, sigma_x, sigma_y, peak) in stars {
                let dx = (x as f64 - sx) / sigma_x;
                let dy = (y as f64 - sy) / sigma_y;
                v += peak * (-0.5 * (dx * dx + dy * dy)).exp();
            }
            // Physical value to raw sample under BZERO 32768.
            pixels.push((round_to_u16(v) as i32 - 32768) as i16);
        }
    }

    let img = ImageData::new(vec![width, height], PixelData::I16(pixels));
    let mut fits = FitsFile::with_primary_image(img);
    let header = &mut fits.primary_mut().header;
    header.set(BZERO, HeaderValue::Float(32768.0), None);
    header.set(
        "DATE-OBS",
        HeaderValue::String("2026-06-22T00:00:00".to_string()),
        None,
    );
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

/// Write a 2D single-plane F32 monochrome FITS with values scaled to [0, 1].
/// Simulates drizzle-processed output, which typically uses float pixels in a
/// small range rather than the [0, 65535] range that `round_to_u16` assumes.
pub(crate) fn write_mono_f32_fits(path: &Path, width: usize, height: usize) {
    let n = width * height;
    let pixels: Vec<f32> = (0..n).map(|i| i as f32 / n as f32).collect();
    let img = ImageData::new(vec![width, height], PixelData::F32(pixels));
    let fits = FitsFile::with_primary_image(img);
    fits.to_file(path).unwrap();
}

/// Write a 3-plane F32 RGB cube with values scaled to [0, 1] per channel.
pub(crate) fn write_rgb_cube_f32_fits(path: &Path, width: usize, height: usize) {
    let n = width * height;
    let mut pixels = Vec::with_capacity(n * 3);
    for c in 0..3usize {
        for i in 0..n {
            pixels.push((c * n + i) as f32 / (3 * n) as f32);
        }
    }
    let img = ImageData::new(vec![width, height, 3], PixelData::F32(pixels));
    let fits = FitsFile::with_primary_image(img);
    fits.to_file(path).unwrap();
}
