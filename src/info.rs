//! The `info` command: print a human-readable summary of a FITS image —
//! resolution, bit depth, channel count, sky coordinates, and (for
//! single-channel, non-debayered data) basic pixel statistics.

use std::path::Path;

use anyhow::{Context, Result};
use fitskit::{FitsFile, Header, ImageData};

use crate::fits_image::{find_image_hdu, get_bayerpat, print_step, scaled_pixels};
use crate::options::InfoOptions;

/// Min, max, mean and median of a single-channel image's physical pixel values.
struct PixelStats {
    min: f64,
    max: f64,
    mean: f64,
    median: f64,
}

pub fn info_file(input: &Path, opts: &InfoOptions) -> Result<()> {
    print_step(opts.verbose, "reading");
    let fits =
        FitsFile::from_file(input).with_context(|| format!("cannot read {}", input.display()))?;

    let (header, img) = find_image_hdu(&fits, input, opts.verbose)?;
    let img = img.as_ref();

    // A 3-plane cube with no BAYERPAT is an already-debayered RGB image (3
    // channels); anything else is treated as a single-channel frame, matching
    // the detection used by the debayer/stretch/split commands.
    let is_rgb_cube = get_bayerpat(header).is_none() && img.axes.len() == 3 && img.axes[2] == 3;
    let channels = if is_rgb_cube { 3 } else { 1 };

    let width = img.axes.first().copied().unwrap_or(0);
    let height = img.axes.get(1).copied().unwrap_or(0);

    println!("{}", input.display());
    println!("  Resolution:  {width} x {height}");
    println!("  Bit depth:   {}", bit_depth_label(header, img));
    println!(
        "  Channels:    {channels} ({})",
        channel_label(channels, header)
    );

    if let Some(object) = header.get_string("OBJECT") {
        let object = object.trim();
        if !object.is_empty() {
            println!("  Object:      {object}");
        }
    }

    print_coordinate(header, "RA", "RA", "OBJCTRA");
    print_coordinate(header, "DEC", "DEC", "OBJCTDEC");

    if let Some(exptime) = header.get_float("EXPTIME") {
        println!("  Exposure:    {} s", trim_float(exptime));
    }
    if let Some(filter) = header.get_string("FILTER") {
        let filter = filter.trim();
        if !filter.is_empty() {
            println!("  Filter:      {filter}");
        }
    }
    if let Some(instrument) = header.get_string("INSTRUME") {
        let instrument = instrument.trim();
        if !instrument.is_empty() {
            println!("  Instrument:  {instrument}");
        }
    }
    if let Some(date) = header.get_string("DATE-OBS") {
        let date = date.trim();
        if !date.is_empty() {
            println!("  Date-obs:    {date}");
        }
    }

    // Pixel statistics only make sense for a single, non-debayered channel:
    // mixing the R/G/B planes of an RGB cube would give a meaningless figure.
    if channels == 1 {
        let stats = pixel_stats(header, img);
        println!(
            "  Pixels:      min={} max={} mean={} median={}",
            trim_float(stats.min),
            trim_float(stats.max),
            trim_float(stats.mean),
            trim_float(stats.median),
        );
    }

    Ok(())
}

/// Describe the pixel storage format. The bit depth comes from the (possibly
/// decompressed) image's own pixel type, so it's correct for tile-compressed
/// images whose container `BITPIX` describes the binary table, not the image.
fn bit_depth_label(header: &Header, img: &ImageData) -> String {
    let bitpix = img.bitpix().to_i64();
    match bitpix {
        // An unsigned 16-bit image is stored as signed 16 with BZERO=32768.
        16 if header.get_float("BZERO").unwrap_or(0.0) == 32768.0 => {
            "16-bit unsigned integer".to_string()
        }
        8 => "8-bit unsigned integer".to_string(),
        16 => "16-bit integer".to_string(),
        32 => "32-bit integer".to_string(),
        64 => "64-bit integer".to_string(),
        -32 => "32-bit float".to_string(),
        -64 => "64-bit float".to_string(),
        other => format!("BITPIX {other}"),
    }
}

/// Describe the channel layout, noting the Bayer pattern for raw mosaics.
fn channel_label(channels: usize, header: &Header) -> String {
    if channels == 3 {
        return "debayered RGB".to_string();
    }
    match get_bayerpat(header) {
        Some(pat) => format!("mosaic, BAYERPAT={}", pat.trim()),
        None => "monochrome / undebayered".to_string(),
    }
}

/// Print a sky coordinate, preferring the decimal-degree keyword and appending
/// the sexagesimal string form when present.
fn print_coordinate(header: &Header, label: &str, deg_key: &str, sexagesimal_key: &str) {
    let deg = header.get_float(deg_key);
    let sexagesimal = header
        .get_string(sexagesimal_key)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match (deg, sexagesimal) {
        (Some(d), Some(s)) => println!("  {label}:{}{} deg ({s})", pad(label), trim_float(d)),
        (Some(d), None) => println!("  {label}:{}{} deg", pad(label), trim_float(d)),
        (None, Some(s)) => println!("  {label}:{}{s}", pad(label)),
        (None, None) => {}
    }
}

/// Pad after a coordinate label so values line up with the other fields, which
/// use a 12-column label area (`"  Resolution:  "`).
fn pad(label: &str) -> &'static str {
    match label.len() {
        2 => "          ", // "RA"
        _ => "         ",  // "DEC"
    }
}

/// Format a float without a trailing `.0` for whole numbers, keeping a compact
/// representation otherwise.
fn trim_float(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Compute min/max/mean/median of the image's physical (BSCALE/BZERO-applied)
/// pixel values.
fn pixel_stats(header: &Header, img: &ImageData) -> PixelStats {
    let mut values = scaled_pixels(header, img);

    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut sum = 0.0;
    for &v in &values {
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
        sum += v;
    }

    let n = values.len();
    let (mean, median) = if n == 0 {
        (0.0, 0.0)
    } else {
        (sum / n as f64, median_in_place(&mut values))
    };

    PixelStats { min, max, mean, median }
}

/// Median via in-place selection; averages the two central values for an even
/// count. Assumes a non-empty slice of finite values.
fn median_in_place(values: &mut [f64]) -> f64 {
    let n = values.len();
    let mid = n / 2;
    let (_, hi, _) = values.select_nth_unstable_by(mid, f64::total_cmp);
    let hi = *hi;
    if n % 2 == 1 {
        hi
    } else {
        let (_, lo, _) = values.select_nth_unstable_by(mid - 1, f64::total_cmp);
        (*lo + hi) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{test_data, write_mosaic_fits, write_rgb_cube_fits};
    use tempfile::TempDir;

    #[test]
    fn trim_float_drops_trailing_zeros() {
        assert_eq!(trim_float(180.0), "180");
        assert_eq!(trim_float(312.866739069469), "312.866739");
        assert_eq!(trim_float(30.5), "30.5");
    }

    #[test]
    fn pixel_stats_match_known_values() {
        // write_mosaic_fits stores sequential i16 values 0..(w*h) with no
        // BSCALE/BZERO, so physical values equal the raw pixel indices.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 4, 4, Some("RGGB"));

        let fits = FitsFile::from_file(&input).unwrap();
        let (header, img) = find_image_hdu(&fits, &input, false).unwrap();
        let img = img.as_ref();
        let stats = pixel_stats(header, img);

        assert_eq!(stats.min, 0.0);
        assert_eq!(stats.max, 15.0);
        assert_eq!(stats.mean, 7.5);
        assert_eq!(stats.median, 7.5); // mean of 7 and 8
    }

    #[test]
    fn median_handles_odd_count() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        // 3x3 = 9 pixels, values 0..8, median is 4.
        write_mosaic_fits(&input, 3, 3, None);

        let fits = FitsFile::from_file(&input).unwrap();
        let (header, img) = find_image_hdu(&fits, &input, false).unwrap();
        let img = img.as_ref();
        let stats = pixel_stats(header, img);
        assert_eq!(stats.median, 4.0);
    }

    #[test]
    fn mosaic_reports_single_channel() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 4, 4, Some("RGGB"));

        let fits = FitsFile::from_file(&input).unwrap();
        let (header, img) = find_image_hdu(&fits, &input, false).unwrap();
        let img = img.as_ref();
        let is_rgb_cube = get_bayerpat(header).is_none() && img.axes.len() == 3 && img.axes[2] == 3;
        assert!(!is_rgb_cube);
        assert_eq!(channel_label(1, header), "mosaic, BAYERPAT=RGGB");
    }

    #[test]
    fn rgb_cube_reports_three_channels() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 4, 3);

        let fits = FitsFile::from_file(&input).unwrap();
        let (header, img) = find_image_hdu(&fits, &input, false).unwrap();
        let img = img.as_ref();
        let is_rgb_cube = get_bayerpat(header).is_none() && img.axes.len() == 3 && img.axes[2] == 3;
        assert!(is_rgb_cube);
        assert_eq!(channel_label(3, header), "debayered RGB");
    }

    #[test]
    fn info_file_runs_on_real_data() {
        // The bundled frame is a 3008x3008 GRBG mosaic; info should succeed and
        // treat it as a single channel.
        let input = test_data("uncompressed.fit");
        info_file(&input, &InfoOptions::default()).unwrap();
    }

    #[test]
    fn info_runs_on_tile_compressed_image() {
        // The bundled .fz holds the image in a tile-compressed extension HDU;
        // info must decompress it and report the original geometry/bit depth.
        let input = test_data("compressed.fits.fz");
        info_file(&input, &InfoOptions::default()).unwrap();

        let fits = FitsFile::from_file(&input).unwrap();
        let (header, img) = find_image_hdu(&fits, &input, false).unwrap();
        let img = img.as_ref();
        assert_eq!(img.axes, vec![3008, 3008]);
        assert_eq!(bit_depth_label(header, img), "16-bit unsigned integer");
        assert_eq!(channel_label(1, header), "mosaic, BAYERPAT=GRBG");
    }

    #[test]
    fn bit_depth_label_recognizes_unsigned_16() {
        let input = test_data("uncompressed.fit");
        let fits = FitsFile::from_file(&input).unwrap();
        let (header, img) = find_image_hdu(&fits, &input, false).unwrap();
        let img = img.as_ref();
        assert_eq!(bit_depth_label(header, img), "16-bit unsigned integer");
    }
}
