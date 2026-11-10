//! A tiny synthetic frame for tests that group images by their headers. Public
//! behind the `test-support` feature so the CLI's tests can use the same
//! fixture as `libfitz`'s own.

use fitskit::{FitsFile, HeaderValue, ImageData, PixelData};
use std::path::Path;

/// Write a tiny 4x4 frame carrying only the keywords session grouping reads:
/// an optional `FILTER`, `EXPTIME` (seconds), and optional `DATE-LOC` /
/// `DATE-OBS` timestamp strings. Panics if the file cannot be written.
pub fn write_frame_fits(
    path: &Path,
    filter: Option<&str>,
    exptime: f64,
    date_loc: Option<&str>,
    date_obs: Option<&str>,
) {
    let img = ImageData::new(vec![4, 4], PixelData::I16(vec![0; 16]));
    let mut fits = FitsFile::with_primary_image(img);
    let header = &mut fits.primary_mut().header;
    header.set("EXPTIME", HeaderValue::Float(exptime), None);
    for (key, value) in [
        ("FILTER", filter),
        ("DATE-LOC", date_loc),
        ("DATE-OBS", date_obs),
    ] {
        if let Some(value) = value {
            header.set(key, HeaderValue::String(value.to_string()), None);
        }
    }
    fits.to_file(path).unwrap();
}
