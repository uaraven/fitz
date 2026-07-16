//! Compute a structured summary of a FITS image: resolution, bit depth,
//! channel count, sky coordinates and other header-derived metadata, plus
//! (optionally expensive) pixel statistics and a value histogram. Formatting
//! this into a human-readable report is left to the caller (e.g. the CLI's
//! terminal report, or a GUI's header panel).

use std::path::Path;

use anyhow::{Context, Result};
use fitskit::{FitsFile, Header, ImageData, PixelData};
use rayon::prelude::*;

use crate::fits_image::{
    bscale_bzero, find_image_hdu, get_bayerpat, is_debayered_rgb_cube, scaled_pixels,
};

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
    /// Count of pixels whose value equals `min`.
    pub min_count: usize,
    /// Count of pixels whose value equals `max`.
    pub max_count: usize,
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
    Ok(header_info_from(header, img.as_ref(), with_pixels))
}

/// Build a [`HeaderInfo`] from an already-loaded image and header, without
/// touching the filesystem. This is the shared core of [`header_info`] and lets
/// callers that already hold a decoded `(header, img)` (e.g. the GUI's loader)
/// reuse the same summary without a second read. `with_pixels` computes
/// [`PixelStats`] (meaningless, and skipped, for an already-debayered RGB cube).
pub fn header_info_from(header: &Header, img: &ImageData, with_pixels: bool) -> HeaderInfo {
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

    HeaderInfo {
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
        binning: header.get_int("XBINNING").zip(header.get_int("YBINNING")),
        filter: header.get_string("FILTER").map(str::to_string),
        instrument: header.get_string("INSTRUME").map(str::to_string),
        telescope: header.get_string("TELESCOP").map(str::to_string),
        focal_len_mm: header.get_float("FOCALLEN"),
        focal_ratio: header.get_float("FOCRATIO"),
        date_obs: header.get_string("DATE-OBS").map(str::to_string),
        header: header.clone(),
        pixel_stats,
    }
}

/// One labeled field in a [`HeaderInfo::summary`] — a display label and its
/// already-formatted value (e.g. `"Resolution"` / `"3008 x 3008"`).
pub struct SummaryField {
    pub label: String,
    pub value: String,
}

impl HeaderInfo {
    /// A curated, ordered list of the most useful header fields as label/value
    /// pairs. The CLI `info` report and the GUI info panel both build on this so
    /// the two stay in sync. Resolution, bit depth and channels are always
    /// present; every other field appears only when the header carries it (and
    /// is non-blank). Pixel statistics are deliberately excluded — they belong
    /// in their own panel/section.
    pub fn summary(&self) -> Vec<SummaryField> {
        let mut fields = Vec::new();
        push(
            &mut fields,
            "Resolution",
            format!("{} x {}", self.width, self.height),
        );
        push(&mut fields, "Bit depth", bit_depth_label(self));
        push(
            &mut fields,
            "Channels",
            format!("{} ({})", self.channels, channel_label(self)),
        );
        push_str(&mut fields, "Bayer", self.bayerpat.as_deref());
        push_str(&mut fields, "Object", self.object.as_deref());
        push_coordinate(
            &mut fields,
            Axis::Ra,
            self.ra_deg,
            self.ra_sexagesimal.as_deref(),
        );
        push_coordinate(
            &mut fields,
            Axis::Dec,
            self.dec_deg,
            self.dec_sexagesimal.as_deref(),
        );
        if let Some(rot) = self.rotation_deg {
            push(&mut fields, "Rotation", format!("{}°", trim_float(rot)));
        }
        if let Some(exptime) = self.exptime_s {
            push(
                &mut fields,
                "Exposure",
                format!("{} s", trim_float(exptime)),
            );
        }
        if let Some(gain) = self.gain {
            push(&mut fields, "Gain", trim_float(gain));
        }
        if let Some(offset) = self.offset {
            push(&mut fields, "Offset", trim_float(offset));
        }
        if let Some((xbin, ybin)) = self.binning {
            push(&mut fields, "Binning", format!("{xbin}x{ybin}"));
        }
        push_str(&mut fields, "Filter", self.filter.as_deref());
        push_str(&mut fields, "Instrument", self.instrument.as_deref());
        if let Some(telescope) = telescope_label(self) {
            push(&mut fields, "Telescope", telescope);
        }
        push_str(&mut fields, "Date-obs", self.date_obs.as_deref());
        fields
    }
}

/// Append a field with an already-formatted value.
fn push(fields: &mut Vec<SummaryField>, label: &str, value: String) {
    fields.push(SummaryField {
        label: label.to_string(),
        value,
    });
}

/// Append a string field only when present and non-blank once trimmed.
fn push_str(fields: &mut Vec<SummaryField>, label: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|s| !s.is_empty()) {
        push(fields, label, value.to_string());
    }
}

/// Describe the pixel storage format. The bit depth comes from the (possibly
/// decompressed) image's own pixel type, so it's correct for tile-compressed
/// images whose container `BITPIX` describes the binary table, not the image.
fn bit_depth_label(info: &HeaderInfo) -> String {
    match info.bitpix {
        // An unsigned 16-bit image is stored as signed 16 with BZERO=32768.
        16 if info.is_unsigned16 => "16-bit unsigned integer".to_string(),
        8 => "8-bit unsigned integer".to_string(),
        16 => "16-bit integer".to_string(),
        32 => "32-bit integer".to_string(),
        64 => "64-bit integer".to_string(),
        -32 => "32-bit float".to_string(),
        -64 => "64-bit float".to_string(),
        other => format!("BITPIX {other}"),
    }
}

/// Describe the channel layout. The Bayer pattern itself is reported on its own
/// `Bayer` field, so the raw-mosaic case just notes that it is a mosaic.
fn channel_label(info: &HeaderInfo) -> String {
    if info.channels == 3 {
        return "debayered RGB".to_string();
    }
    match info.bayerpat {
        Some(_) => "mosaic".to_string(),
        None => "monochrome (debayered)".to_string(),
    }
}

/// Describe the imaging telescope: its name (`TELESCOP`) optionally followed by
/// its optical figure derived from focal length (`FOCALLEN`, mm) and focal ratio
/// (`FOCRATIO`), e.g. `My Scope (203mm F/4.5)`. Returns `None` when no telescope
/// keyword carries usable information.
fn telescope_label(info: &HeaderInfo) -> Option<String> {
    let name = info
        .telescope
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut optics = String::new();
    if let Some(focal) = info.focal_len_mm {
        optics.push_str(&format!("{}mm", trim_float(focal)));
    }
    if let Some(ratio) = info.focal_ratio {
        if !optics.is_empty() {
            optics.push(' ');
        }
        optics.push_str(&format!("F/{}", trim_float(ratio)));
    }

    match (name, optics.is_empty()) {
        (Some(name), false) => Some(format!("{name} ({optics})")),
        (Some(name), true) => Some(name.to_string()),
        (None, false) => Some(optics),
        (None, true) => None,
    }
}

/// Which sky axis a coordinate is, selecting its sexagesimal convention: right
/// ascension is expressed in hours (`h m s`, 360° = 24h), declination in signed
/// degrees (`° ' "`).
#[derive(Clone, Copy)]
enum Axis {
    Ra,
    Dec,
}

/// Append a sky coordinate. When the decimal-degree value is present it is
/// rendered in sexagesimal form (hours for RA, degrees for DEC) with the decimal
/// value in parentheses; otherwise the raw sexagesimal header string is shown
/// verbatim. Absent on both counts, nothing is appended.
fn push_coordinate(
    fields: &mut Vec<SummaryField>,
    axis: Axis,
    deg: Option<f64>,
    sexagesimal: Option<&str>,
) {
    let label = match axis {
        Axis::Ra => "RA",
        Axis::Dec => "DEC",
    };
    let sexagesimal = sexagesimal.map(str::trim).filter(|s| !s.is_empty());

    let value = match (deg, sexagesimal) {
        (Some(d), _) => Some(format_coordinate(axis, d)),
        (None, Some(s)) => Some(s.to_string()),
        (None, None) => None,
    };
    if let Some(value) = value {
        push(fields, label, value);
    }
}

/// Format a decimal-degree coordinate in sexagesimal form with the decimal value
/// echoed in parentheses, e.g. `20h 30m 00.00s (20.5h)` for RA or
/// `-12° 30' 00.00" (-12.5°)` for DEC.
fn format_coordinate(axis: Axis, deg: f64) -> String {
    match axis {
        Axis::Ra => {
            // 360 degrees of RA span 24 hours, so hours = degrees / 15.
            let hours = deg / 15.0;
            let (h, m, s) = to_sexagesimal(hours.abs());
            let sign = if hours < 0.0 { "-" } else { "" };
            format!("{sign}{h}h {m:02}m {s:05.2}s ({}h)", trim_float(hours))
        }
        Axis::Dec => {
            let (d, m, s) = to_sexagesimal(deg.abs());
            let sign = if deg < 0.0 { "-" } else { "" };
            format!("{sign}{d}° {m:02}' {s:05.2}\" ({}°)", trim_float(deg))
        }
    }
}

/// Split a non-negative decimal value into whole units, minutes and seconds.
/// Rounding is done on the total seconds first so any carry propagates and the
/// returned minutes/seconds stay in `[0, 60)`.
fn to_sexagesimal(value: f64) -> (u64, u64, f64) {
    let total_seconds = (value * 3600.0 * 100.0).round() / 100.0;
    let whole = (total_seconds / 3600.0).trunc();
    let rem = total_seconds - whole * 3600.0;
    let minutes = (rem / 60.0).trunc();
    let seconds = rem - minutes * 60.0;
    (whole as u64, minutes as u64, seconds)
}

/// Format a float without a trailing `.0` for whole numbers, keeping a compact
/// representation otherwise. Shared with the CLI's pixel-stats report so numbers
/// are rendered the same way everywhere.
pub fn trim_float(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Parse a FITS `DATE-OBS` timestamp (`YYYY-MM-DDTHH:MM:SS[.sss]`, UTC by
/// convention; a trailing `Z` is tolerated) into seconds since the Unix epoch,
/// preserving fractional seconds. The result is both a sortable key and a
/// numeric X value for time-series plots. Returns `None` for anything
/// unparseable (including a bare date with no time part).
pub fn parse_date_obs(s: &str) -> Option<f64> {
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;

    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut parts = time.split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    let second: f64 = parts.next()?.parse().ok()?;
    // Allow 60 for a leap second, per the FITS convention of UTC timestamps.
    if parts.next().is_some() || hour >= 24 || minute >= 60 || !(0.0..61.0).contains(&second) {
        return None;
    }

    let days = days_from_civil(year, month, day) as f64;
    Some(days * 86400.0 + f64::from(hour) * 3600.0 + f64::from(minute) * 60.0 + second)
}

/// Days from the Unix epoch (1970-01-01) to the given civil date, negative for
/// earlier dates. Howard Hinnant's `days_from_civil` algorithm — exact for the
/// entire proleptic Gregorian calendar, no external date crate needed.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = i64::from((m + 9) % 12); // March-based month, [0, 11]
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Compute min/max/mean/median, the min/max pixel counts, the zero count and
/// the value histogram of the image's physical (BSCALE/BZERO-applied) pixel
/// values.
///
/// Integer 16-bit samples (`U8`/`I16` storage — including the common
/// unsigned-16 convention, BITPIX 16 with BZERO 32768) take a single-pass
/// fast path: one parallel scan builds a full-resolution value-count array
/// (65536 slots, one per raw sample value), from which every statistic falls
/// out with no sort and no second pass over the pixels. BSCALE/BZERO is an
/// affine, monotonic map (for the usual `BSCALE > 0`), so ordering and equal
/// counts on raw samples translate exactly to physical values. Wider or float
/// samples fall back to the general fold+sort path.
pub fn pixel_stats(header: &Header, img: &ImageData) -> PixelStats {
    let (bscale, bzero) = bscale_bzero(header);
    if bscale > 0.0
        && let Some((counts, offset)) = value_counts(img)
    {
        return stats_from_counts(&counts, |i| bzero + bscale * (i as f64 - offset));
    }
    pixel_stats_general(header, img)
}

/// Number of distinct raw sample values in the 16-bit fast path.
const VALUE_COUNT_SLOTS: usize = 1 << 16;

/// Smallest sample count worth giving its own parallel chunk (and hence its own
/// [`VALUE_COUNT_SLOTS`]-slot array): four times the per-chunk overhead, so that
/// bookkeeping stays a fraction of the counting.
const MIN_COUNT_CHUNK: usize = 4 * VALUE_COUNT_SLOTS;

/// Count occurrences of every raw sample value in one parallel pass, for
/// sample types whose values index into a 65536-slot array (`U8`, `I16`).
/// Returns the counts plus the `offset` mapping an index back to its raw
/// sample value (`raw = index - offset`), or `None` for wider/float samples.
fn value_counts(img: &ImageData) -> Option<(Vec<u64>, f64)> {
    fn count<T: Sync>(v: &[T], idx: impl Fn(&T) -> usize + Sync + Send) -> Vec<u64> {
        // Every chunk costs a 65536-slot allocation to zero and a 65536-element
        // add to merge, so a chunk must count enough samples to earn that back:
        // spread the work one chunk per thread, but never split so fine that the
        // bookkeeping dominates. A small frame stays on a single chunk.
        let chunk = v
            .len()
            .div_ceil(rayon::current_num_threads())
            .max(MIN_COUNT_CHUNK);
        v.par_chunks(chunk)
            .fold(
                || vec![0u64; VALUE_COUNT_SLOTS],
                |mut c, chunk| {
                    for x in chunk {
                        c[idx(x)] += 1;
                    }
                    c
                },
            )
            .reduce(|| vec![0u64; VALUE_COUNT_SLOTS], add_counts)
    }

    match &img.pixels {
        PixelData::U8(v) => Some((count(v, |&x| x as usize), 0.0)),
        PixelData::I16(v) => Some((count(v, |&x| (x as i32 + 32768) as usize), 32768.0)),
        _ => None,
    }
}

/// Sum two equal-length count arrays element-wise, reusing `a`'s allocation.
/// The `reduce` step of every parallel counting fold in this module.
fn add_counts(mut a: Vec<u64>, b: Vec<u64>) -> Vec<u64> {
    for (slot, c) in a.iter_mut().zip(b) {
        *slot += c;
    }
    a
}

/// Multiplier turning a value's distance from `min` into a bucket index. A
/// degenerate range (`max <= min`, e.g. a constant image) yields `0.0`, which
/// collapses every value into bucket 0.
fn bucket_scale(min: f64, max: f64) -> f64 {
    let range = max - min;
    if range > 0.0 {
        HISTOGRAM_BUCKETS as f64 / range
    } else {
        0.0
    }
}

/// Bucket index for a value, given `min` and the [`bucket_scale`] for the
/// range. `max` (and anything rounding past the upper edge) folds into the last
/// bucket. Shared so that binning distinct values by their counts lands them in
/// exactly the buckets that binning every pixel individually would.
fn bucket_index(v: f64, min: f64, scale: f64) -> usize {
    (((v - min) * scale) as usize).min(HISTOGRAM_BUCKETS - 1)
}

/// Derive every [`PixelStats`] field from a full-resolution value-count array.
/// `physical` maps a count index to its physical (BSCALE/BZERO-applied) value
/// and must be monotonically increasing in the index.
fn stats_from_counts(counts: &[u64], physical: impl Fn(usize) -> f64) -> PixelStats {
    let n: u64 = counts.iter().sum();
    if n == 0 {
        return PixelStats {
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            mean: 0.0,
            median: 0.0,
            zeros: 0,
            min_count: 0,
            max_count: 0,
            histogram: vec![0; HISTOGRAM_BUCKETS],
        };
    }

    let min_idx = counts.iter().position(|&c| c > 0).unwrap();
    let max_idx = counts.iter().rposition(|&c| c > 0).unwrap();
    let min = physical(min_idx);
    let max = physical(max_idx);

    // One walk over the (at most 65536) occupied slots covers the sum for the
    // mean, the zero count, and the 256-bin display histogram; binning each
    // distinct value with the same formula as `histogram` keeps the output
    // bit-identical to binning every pixel individually.
    let mut sum = 0.0;
    let mut zeros = 0usize;
    let mut buckets = vec![0u64; HISTOGRAM_BUCKETS];
    let scale = bucket_scale(min, max);
    for (i, &c) in counts.iter().enumerate().take(max_idx + 1).skip(min_idx) {
        if c == 0 {
            continue;
        }
        let v = physical(i);
        sum += v * c as f64;
        zeros += if v == 0.0 { c as usize } else { 0 };
        buckets[bucket_index(v, min, scale)] += c;
    }

    // Median from the cumulative counts: the value at rank n/2, averaged with
    // the one at rank n/2 - 1 for an even count.
    let mid = n / 2;
    let median = if n % 2 == 1 {
        physical(index_at_rank(counts, mid))
    } else {
        (physical(index_at_rank(counts, mid - 1)) + physical(index_at_rank(counts, mid))) / 2.0
    };

    PixelStats {
        min,
        max,
        mean: sum / n as f64,
        median,
        zeros,
        min_count: counts[min_idx] as usize,
        max_count: counts[max_idx] as usize,
        histogram: buckets,
    }
}

/// Index of the value-count slot holding the sample of (0-based) `rank` in the
/// sorted order of all samples. `rank` must be less than the total count.
fn index_at_rank(counts: &[u64], rank: u64) -> usize {
    let mut cumulative = 0u64;
    for (i, &c) in counts.iter().enumerate() {
        cumulative += c;
        if cumulative > rank {
            return i;
        }
    }
    unreachable!("rank {rank} beyond total sample count");
}

/// The general-purpose fallback for samples that don't fit the 16-bit
/// value-count fast path: parallel fold for min/max/zeros, a counting pass for
/// the min/max pixel counts, a histogram pass, and an in-place selection for
/// the median.
fn pixel_stats_general(header: &Header, img: &ImageData) -> PixelStats {
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

    // Count the pixels sitting exactly at the extremes (needs min/max known,
    // so it can't fold into the pass above; the data is already in memory).
    let (min_count, max_count) = values
        .par_iter()
        .fold(
            || (0usize, 0usize),
            |(mnc, mxc), &v| (mnc + (v == min) as usize, mxc + (v == max) as usize),
        )
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));

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
        min_count,
        max_count,
        histogram,
    }
}

/// Bin `values` into a [`HISTOGRAM_BUCKETS`]-bin histogram spanning `[min, max]`.
/// `max` (and anything rounding to the upper edge) folds into the last bucket.
/// A degenerate range (`max <= min`, e.g. a constant image) puts every value in
/// bucket 0. The work is split across threads, each filling a local bucket
/// array that is then summed element-wise.
pub fn histogram(values: &[f64], min: f64, max: f64) -> Vec<u64> {
    let scale = bucket_scale(min, max);
    values
        .par_iter()
        .fold(
            || vec![0u64; HISTOGRAM_BUCKETS],
            |mut buckets, &v| {
                buckets[bucket_index(v, min, scale)] += 1;
                buckets
            },
        )
        .reduce(|| vec![0u64; HISTOGRAM_BUCKETS], add_counts)
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
        assert_eq!(stats.min_count, 1); // sequential values: one pixel per value
        assert_eq!(stats.max_count, 1);

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
    fn min_max_counts_on_repeated_extremes() {
        // 2x2 mosaic with two pixels at the minimum and one at the maximum.
        let img = ImageData::new(vec![2, 2], PixelData::I16(vec![3, 3, 7, 5]));
        let stats = pixel_stats(&Header::default(), &img);
        assert_eq!(stats.min, 3.0);
        assert_eq!(stats.max, 7.0);
        assert_eq!(stats.min_count, 2);
        assert_eq!(stats.max_count, 1);
        assert_eq!(stats.zeros, 0);
        assert_eq!(stats.median, 4.0); // mean of 3 and 5
    }

    #[test]
    fn constant_image_degenerate_min_equals_max() {
        // min == max: both counts cover every pixel, histogram collapses to
        // bucket 0, and zeros counts everything when the constant is 0.
        let img = ImageData::new(vec![3, 2], PixelData::I16(vec![0; 6]));
        let stats = pixel_stats(&Header::default(), &img);
        assert_eq!((stats.min, stats.max), (0.0, 0.0));
        assert_eq!(stats.min_count, 6);
        assert_eq!(stats.max_count, 6);
        assert_eq!(stats.zeros, 6);
        assert_eq!((stats.mean, stats.median), (0.0, 0.0));
        assert_eq!(stats.histogram[0], 6);
        assert_eq!(stats.histogram.iter().sum::<u64>(), 6);
    }

    #[test]
    fn fast_path_matches_general_path_on_real_data() {
        // The bundled frame is 16-bit unsigned (I16 + BZERO 32768), so
        // `pixel_stats` takes the value-count fast path; it must agree with
        // the general fold+sort path on every field.
        let input = test_data("uncompressed.fit");
        let fits = FitsFile::from_file(&input).unwrap();
        let (header, img) = find_image_hdu(&fits, &input).unwrap();
        let img = img.as_ref();

        let fast = pixel_stats(header, img);
        let general = pixel_stats_general(header, img);

        assert_eq!(fast.min, general.min);
        assert_eq!(fast.max, general.max);
        assert_eq!(fast.median, general.median);
        assert_eq!(fast.zeros, general.zeros);
        assert_eq!(fast.min_count, general.min_count);
        assert_eq!(fast.max_count, general.max_count);
        assert_eq!(fast.histogram, general.histogram);
        // The fast path sums value*count over ≤65536 slots instead of every
        // pixel individually, so the mean may differ in the last few ulps.
        assert!((fast.mean - general.mean).abs() < 1e-9 * general.mean.abs().max(1.0));
    }

    #[test]
    fn general_path_reports_min_max_counts_for_float_samples() {
        // F32 samples skip the value-count fast path; the fallback must still
        // fill in the new count fields. Values are i/n, all distinct.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mono_f32.fits");
        crate::test_support::write_mono_f32_fits(&input, 4, 4);

        let fits = FitsFile::from_file(&input).unwrap();
        let (header, img) = find_image_hdu(&fits, &input).unwrap();
        let stats = pixel_stats(header, img.as_ref());
        assert_eq!(stats.min_count, 1);
        assert_eq!(stats.max_count, 1);
    }

    #[test]
    fn parse_date_obs_handles_valid_timestamps() {
        assert_eq!(parse_date_obs("1970-01-01T00:00:00"), Some(0.0));
        // 2000-01-01T12:00:00 UTC is a well-known epoch value (J2000).
        assert_eq!(parse_date_obs("2000-01-01T12:00:00"), Some(946728000.0));
        // Fractional seconds are preserved; trailing Z and whitespace tolerated.
        assert_eq!(parse_date_obs("1970-01-01T00:00:00.25"), Some(0.25));
        assert_eq!(parse_date_obs(" 1970-01-02T00:00:00Z "), Some(86400.0));
    }

    #[test]
    fn parse_date_obs_rejects_invalid_input() {
        for s in [
            "",
            "2026-05-31",              // bare date, no time part
            "2026-05-31 04:57:09",     // space instead of T
            "2026-13-01T00:00:00",     // month out of range
            "2026-05-32T00:00:00",     // day out of range
            "2026-05-31T24:00:00",     // hour out of range
            "2026-05-31T00:60:00",     // minute out of range
            "2026-05-31T00:00:61",     // second out of range
            "2026-05-31T00:00",        // missing seconds
            "2026-05-31T00:00:00:00",  // extra time field
            "2026-05-31-01T00:00:00",  // extra date field
            "not-a-date-at-allT00:00", // garbage
        ] {
            assert_eq!(parse_date_obs(s), None, "{s:?} should not parse");
        }
    }

    #[test]
    fn parse_date_obs_orders_chronologically() {
        // Across a day boundary, a month boundary and a leap year.
        let times = [
            "2023-12-31T23:59:59.9",
            "2024-01-01T00:00:00",
            "2024-02-29T12:00:00",
            "2024-03-01T00:00:00",
        ];
        let parsed: Vec<f64> = times.iter().map(|s| parse_date_obs(s).unwrap()).collect();
        assert!(parsed.windows(2).all(|w| w[0] < w[1]), "{parsed:?}");
    }

    #[test]
    fn parse_date_obs_reads_real_fixture_header() {
        let input = test_data("uncompressed.fit");
        let info = header_info(&input).unwrap();
        let t = parse_date_obs(info.date_obs.as_deref().unwrap()).unwrap();
        // 2026-05-31T04:57:09.004664 — sanity-check the epoch conversion.
        assert_eq!(t, 1780203429.004664);
    }

    #[test]
    fn trim_float_drops_trailing_zeros() {
        assert_eq!(trim_float(180.0), "180");
        assert_eq!(trim_float(312.866739069469), "312.866739");
        assert_eq!(trim_float(30.5), "30.5");
    }

    #[test]
    fn ra_formats_as_hours() {
        // 307.5° / 15 = 20.5h = 20h 30m 00s.
        assert_eq!(format_coordinate(Axis::Ra, 307.5), "20h 30m 00.00s (20.5h)");
        // 0° is 00h 00m 00s.
        assert_eq!(format_coordinate(Axis::Ra, 0.0), "0h 00m 00.00s (0h)");
    }

    #[test]
    fn dec_formats_as_signed_degrees() {
        assert_eq!(
            format_coordinate(Axis::Dec, 12.5),
            "12° 30' 00.00\" (12.5°)"
        );
        // Declination is signed.
        assert_eq!(
            format_coordinate(Axis::Dec, -12.5),
            "-12° 30' 00.00\" (-12.5°)"
        );
    }

    #[test]
    fn sexagesimal_carries_rounding() {
        // 1.0 - a hair: should round cleanly to 1h 00m 00s, not 0h 59m 60s.
        let (h, m, s) = to_sexagesimal(0.9999999);
        assert_eq!((h, m), (1, 0));
        assert_eq!(s, 0.0);
    }

    #[test]
    fn summary_lists_core_fields_and_present_metadata() {
        // A real mosaic frame always yields Resolution / Bit depth / Channels,
        // plus its Bayer pattern, in that order and without any pixel stats.
        let input = test_data("uncompressed.fit");
        let info = header_info(&input).unwrap();
        let summary = info.summary();

        let labels: Vec<&str> = summary.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(&labels[..3], &["Resolution", "Bit depth", "Channels"]);
        assert!(labels.contains(&"Bayer"));

        let by = |label: &str| {
            summary
                .iter()
                .find(|f| f.label == label)
                .map(|f| f.value.as_str())
        };
        assert_eq!(by("Resolution"), Some("3008 x 3008"));
        assert_eq!(by("Channels"), Some("1 (mosaic)"));
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
