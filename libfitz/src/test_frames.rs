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
    write_keywords_fits(
        path,
        exptime,
        &[
            ("FILTER", filter),
            ("DATE-LOC", date_loc),
            ("DATE-OBS", date_obs),
        ],
    );
}

/// [`write_frame_fits`] for a frame whose `IMAGETYP` header names its type
/// (`"DARK"`, `"Flat Field"`, …), with an optional `DATE-LOC` and no filter.
pub fn write_typed_frame_fits(path: &Path, imagetyp: &str, exptime: f64, date_loc: Option<&str>) {
    write_keywords_fits(
        path,
        exptime,
        &[("IMAGETYP", Some(imagetyp)), ("DATE-LOC", date_loc)],
    );
}

/// A tiny 4x4 frame with `EXPTIME` and every string keyword in `strings` that
/// has a value.
fn write_keywords_fits(path: &Path, exptime: f64, strings: &[(&str, Option<&str>)]) {
    let img = ImageData::new(vec![4, 4], PixelData::I16(vec![0; 16]));
    let mut fits = FitsFile::with_primary_image(img);
    let header = &mut fits.primary_mut().header;
    header.set("EXPTIME", HeaderValue::Float(exptime), None);
    for &(key, value) in strings {
        if let Some(value) = value {
            header.set(key, HeaderValue::String(value.to_string()), None);
        }
    }
    fits.to_file(path).unwrap();
}
