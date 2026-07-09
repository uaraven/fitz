//! Compute a structured summary of a FITS image: resolution, bit depth,
//! channel count, sky coordinates and other header-derived metadata, plus
//! (optionally expensive) pixel statistics and a value histogram. Formatting
//! this into a human-readable report is left to the caller (e.g. the CLI's
//! terminal report, or a GUI's header panel).

use std::path::Path;

use anyhow::{Context, Result};
use fitskit::{FitsFile, Header, ImageData};
use rayon::prelude::*;

use crate::fits_image::{find_image_hdu, get_bayerpat, is_debayered_rgb_cube, scaled_pixels};

/// Number of buckets in the pixel-value histogram.
pub const HISTOGRAM_BUCKETS: usize = 256;

/// Min, max, mean and median of a single-channel image's physical pixel values,
/// plus the count of pixels whose physical value is exactly zero and a
/// [`HISTOGRAM_BUCKETS`]-bin histogram of the values over the `[min, max]` range.
pub struct PixelStats {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub median: f64,
    pub zeros: usize,
    /// Pixel counts per bucket, evenly spanning `[min, max]`. Bucket `i` covers
    /// values in `[min + i*w, min + (i+1)*w)` where `w = (max - min) / BUCKETS`,
    /// with `max` itself folded into the last bucket.
    pub histogram: Vec<u64>,
}

/// A structured summary of a FITS image's header metadata and (optionally)
/// its pixel statistics.
pub struct HeaderInfo {
    pub width: usize,
    pub height: usize,
    /// `3` for an already-debayered RGB cube, `1` otherwise (a raw mosaic, or
    /// an already-debayered monochrome frame with no `BAYERPAT`).
    pub channels: usize,
    pub bitpix: i64,
    /// Whether the source is an unsigned-16 image (`BITPIX 16`, `BZERO 32768`).
    pub is_unsigned16: bool,
    pub bayerpat: Option<String>,
    pub object: Option<String>,
    pub ra_deg: Option<f64>,
    pub ra_sexagesimal: Option<String>,
    pub dec_deg: Option<f64>,
    pub dec_sexagesimal: Option<String>,
    pub rotation_deg: Option<f64>,
    pub exptime_s: Option<f64>,
    pub gain: Option<f64>,
    pub offset: Option<f64>,
    pub binning: Option<(i64, i64)>,
    pub filter: Option<String>,
    pub instrument: Option<String>,
    pub telescope: Option<String>,
    pub focal_len_mm: Option<f64>,
    pub focal_ratio: Option<f64>,
    pub date_obs: Option<String>,
    pub header: Header,
    /// Only computed when the caller opts into it via [`header_info_with_pixels`].
    pub pixel_stats: Option<PixelStats>,
}

/// Read `input`'s header and build a [`HeaderInfo`] summary, without reading
/// (or decompressing) pixel data. Use [`header_info_with_pixels`] to also get
/// [`PixelStats`].
pub fn header_info(input: &Path) -> Result<HeaderInfo> {
    header_info_impl(input, false)
}

/// Like [`header_info`], but also reads (transparently decompressing if
/// needed) the pixel data and computes [`PixelStats`] — meaningless for an
/// already-debayered RGB cube, in which case `pixel_stats` stays `None`.
pub fn header_info_with_pixels(input: &Path) -> Result<HeaderInfo> {
    header_info_impl(input, true)
}

fn header_info_impl(input: &Path, with_pixels: bool) -> Result<HeaderInfo> {
    let fits =
        FitsFile::from_file(input).with_context(|| format!("cannot read {}", input.display()))?;

    let (header, img) = find_image_hdu(&fits, input)?;
    let img = img.as_ref();

    let is_rgb_cube = is_debayered_rgb_cube(header, img);
    let channels = if is_rgb_cube { 3 } else { 1 };

    let width = img.axes.first().copied().unwrap_or(0);
    let height = img.axes.get(1).copied().unwrap_or(0);
    let bitpix = img.bitpix().to_i64();
    let is_unsigned16 = bitpix == 16 && header.get_float("BZERO").unwrap_or(0.0) == 32768.0;

    let pixel_stats = if with_pixels && !is_rgb_cube {
        Some(pixel_stats(header, img))
    } else {
        None
    };

    Ok(HeaderInfo {
        width,
        height,
        channels,
        bitpix,
        is_unsigned16,
        bayerpat: get_bayerpat(header).map(str::trim).map(str::to_string),
        object: header.get_string("OBJECT").map(str::to_string),
        ra_deg: header.get_float("OBJCTRA"),
        ra_sexagesimal: header.get_string("OBJCTRA").map(str::to_string),
        dec_deg: header.get_float("OBJCTDEC"),
        dec_sexagesimal: header.get_string("OBJCTDEC").map(str::to_string),
        rotation_deg: header.get_float("OBJCTROT"),
        exptime_s: header.get_float("EXPTIME"),
        gain: header.get_float("GAIN"),
        offset: header.get_float("OFFSET"),
        binning: header
            .get_int("XBINNING")
            .zip(header.get_int("YBINNING")),
        filter: header.get_string("FILTER").map(str::to_string),
        instrument: header.get_string("INSTRUME").map(str::to_string),
        telescope: header.get_string("TELESCOP").map(str::to_string),
        focal_len_mm: header.get_float("FOCALLEN"),
        focal_ratio: header.get_float("FOCRATIO"),
        date_obs: header.get_string("DATE-OBS").map(str::to_string),
        header: header.clone(),
        pixel_stats,
    })
}

/// Compute min/max/mean/median, the zero count and the value histogram of the
/// image's physical (BSCALE/BZERO-applied) pixel values.
pub fn pixel_stats(header: &Header, img: &ImageData) -> PixelStats {
    let mut values = scaled_pixels(header, img);

    // min/max and the zero count reduce in parallel (all associative, so the
    // result is independent of how the work is split). The sum is kept
    // sequential so the reported mean doesn't drift with thread scheduling from
    // reordered floating-point adds.
    let (min, max, zeros) = values
        .par_iter()
        .fold(
            || (f64::INFINITY, f64::NEG_INFINITY, 0usize),
            |(mn, mx, z), &v| (mn.min(v), mx.max(v), z + (v == 0.0) as usize),
        )
        .reduce(
            || (f64::INFINITY, f64::NEG_INFINITY, 0usize),
            |a, b| (a.0.min(b.0), a.1.max(b.1), a.2 + b.2),
        );

    // Bin into the histogram once min/max are known. Done as a separate pass
    // (the data is already in memory) so the bucket edges can span the actual
    // value range. Per-thread bucket arrays are summed element-wise in reduce.
    let histogram = histogram(&values, min, max);

    let n = values.len();
    let (mean, median) = if n == 0 {
        (0.0, 0.0)
    } else {
        let sum: f64 = values.iter().sum();
        (sum / n as f64, median_in_place(&mut values))
    };

    PixelStats {
        min,
        max,
        mean,
        median,
        zeros,
        histogram,
    }
}

/// Bin `values` into a [`HISTOGRAM_BUCKETS`]-bin histogram spanning `[min, max]`.
/// `max` (and anything rounding to the upper edge) folds into the last bucket.
/// A degenerate range (`max <= min`, e.g. a constant image) puts every value in
/// bucket 0. The work is split across threads, each filling a local bucket
/// array that is then summed element-wise.
pub fn histogram(values: &[f64], min: f64, max: f64) -> Vec<u64> {
    let range = max - min;
    if range <= 0.0 {
        let mut buckets = vec![0u64; HISTOGRAM_BUCKETS];
        buckets[0] = values.len() as u64;
        return buckets;
    }

    let scale = HISTOGRAM_BUCKETS as f64 / range;
    values
        .par_iter()
        .fold(
            || vec![0u64; HISTOGRAM_BUCKETS],
            |mut buckets, &v| {
                let idx = (((v - min) * scale) as usize).min(HISTOGRAM_BUCKETS - 1);
                buckets[idx] += 1;
                buckets
            },
        )
        .reduce(
            || vec![0u64; HISTOGRAM_BUCKETS],
            |mut a, b| {
                for (slot, count) in a.iter_mut().zip(b) {
                    *slot += count;
                }
                a
            },
        )
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
    fn pixel_stats_match_known_values() {
        // write_mosaic_fits stores sequential i16 values 0..(w*h) with no
        // BSCALE/BZERO, so physical values equal the raw pixel indices.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 4, 4, Some("RGGB"));

        let fits = FitsFile::from_file(&input).unwrap();
        let (header, img) = find_image_hdu(&fits, &input).unwrap();
        let img = img.as_ref();
        let stats = pixel_stats(header, img);

        assert_eq!(stats.min, 0.0);
        assert_eq!(stats.max, 15.0);
        assert_eq!(stats.mean, 7.5);
        assert_eq!(stats.median, 7.5); // mean of 7 and 8
        assert_eq!(stats.zeros, 1); // only the single 0 pixel

        // 16 values (0..15) over 256 buckets: every value lands in a distinct
        // bucket, the total is conserved, and 15 (the max) folds into bucket 255.
        assert_eq!(stats.histogram.len(), HISTOGRAM_BUCKETS);
        assert_eq!(stats.histogram.iter().sum::<u64>(), 16);
        assert_eq!(stats.histogram[0], 1); // value 0
        assert_eq!(stats.histogram[HISTOGRAM_BUCKETS - 1], 1); // value 15 (max)
    }

    #[test]
    fn histogram_distributes_values_across_buckets() {
        // Values 0..255 over a [0, 255] range with 256 buckets put exactly one
        // value in each bucket.
        let values: Vec<f64> = (0..256).map(|v| v as f64).collect();
        let h = histogram(&values, 0.0, 255.0);
        assert_eq!(h.len(), HISTOGRAM_BUCKETS);
        assert!(h.iter().all(|&c| c == 1));
        assert_eq!(h.iter().sum::<u64>(), 256);
    }

    #[test]
    fn histogram_handles_constant_image() {
        // A degenerate (zero) range dumps every pixel into the first bucket.
        let values = vec![42.0; 100];
        let h = histogram(&values, 42.0, 42.0);
        assert_eq!(h[0], 100);
        assert_eq!(h.iter().sum::<u64>(), 100);
    }

    #[test]
    fn median_handles_odd_count() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        // 3x3 = 9 pixels, values 0..8, median is 4.
        write_mosaic_fits(&input, 3, 3, None);

        let fits = FitsFile::from_file(&input).unwrap();
        let (header, img) = find_image_hdu(&fits, &input).unwrap();
        let img = img.as_ref();
        let stats = pixel_stats(header, img);
        assert_eq!(stats.median, 4.0);
    }

    #[test]
    fn mosaic_reports_single_channel() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 4, 4, Some("RGGB"));

        let info = header_info(&input).unwrap();
        assert_eq!(info.channels, 1);
        assert_eq!(info.bayerpat.as_deref(), Some("RGGB"));
    }

    #[test]
    fn mono_without_bayerpat_reports_single_channel() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mono.fits");
        write_mosaic_fits(&input, 4, 4, None);

        let info = header_info(&input).unwrap();
        assert_eq!(info.channels, 1);
        assert!(info.bayerpat.is_none());
    }

    #[test]
    fn rgb_cube_reports_three_channels() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 4, 3);

        let info = header_info(&input).unwrap();
        assert_eq!(info.channels, 3);
    }

    #[test]
    fn header_info_runs_on_real_data() {
        // The bundled frame is a 3008x3008 GRBG mosaic.
        let input = test_data("uncompressed.fit");
        let info = header_info(&input).unwrap();
        assert_eq!(info.width, 3008);
        assert_eq!(info.height, 3008);
        assert_eq!(info.channels, 1);
    }

    #[test]
    fn header_info_with_pixels_reads_pixel_stats_on_real_data() {
        let input = test_data("uncompressed.fit");
        let info = header_info_with_pixels(&input).unwrap();
        assert!(info.pixel_stats.is_some());
    }

    #[test]
    fn header_info_with_pixels_on_rgb_cube_has_no_stats() {
        // Pixel stats aren't meaningful for an already-debayered RGB cube.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 4, 3);
        let info = header_info_with_pixels(&input).unwrap();
        assert!(info.pixel_stats.is_none());
    }

    #[test]
    fn zero_pixels_are_counted() {
        // A 3x3 mosaic stores values 0..8, so exactly one pixel is zero.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 3, 3, None);

        let fits = FitsFile::from_file(&input).unwrap();
        let (header, img) = find_image_hdu(&fits, &input).unwrap();
        let img = img.as_ref();
        let stats = pixel_stats(header, img);
        assert_eq!(stats.zeros, 1);
    }

    #[test]
    fn header_info_runs_on_tile_compressed_image() {
        // The bundled .fz holds the image in a tile-compressed extension HDU;
        // info must decompress it and report the original geometry/bit depth.
        let input = test_data("compressed.fits.fz");
        let info = header_info(&input).unwrap();
        assert_eq!(info.width, 3008);
        assert_eq!(info.height, 3008);
        assert_eq!(info.channels, 1);
        assert!(info.is_unsigned16);
        assert_eq!(info.bayerpat.as_deref(), Some("GRBG"));
    }
}
