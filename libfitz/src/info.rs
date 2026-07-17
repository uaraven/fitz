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
    bscale_bzero, detection_plane, find_image_hdu, get_bayerpat, green_plane,
    is_debayered_rgb_cube, scaled_pixels,
};
use crate::stars::{StarDetectOptions, StarStats, detect_stars, plane_background};

/// Number of buckets in the pixel-value histogram.
pub const HISTOGRAM_BUCKETS: usize = 256;

/// Multiplier turning a median absolute deviation into an estimate of the
/// standard deviation of normally distributed data.
const MAD_TO_SIGMA: f64 = 1.4826;

/// Min, max, mean and median of a single-channel image's physical pixel values,
/// plus robust noise/background statistics, the count of pixels whose physical
/// value is exactly zero and a [`HISTOGRAM_BUCKETS`]-bin histogram of the values
/// over the `[min, max]` range.
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
    /// Number of samples these statistics were computed over.
    pub count: usize,
    /// Population standard deviation of the physical pixel values. Sensitive to
    /// stars and hot pixels by construction — compare against `mad`.
    pub sigma: f64,
    /// Median absolute deviation from the median, scaled by [`MAD_TO_SIGMA`] so
    /// it estimates σ for Gaussian noise while ignoring stars entirely.
    pub mad: f64,
    /// The most common physical pixel value — the sky background level. Ties
    /// resolve to the lowest such value. Approximated to the center of the
    /// largest histogram bucket for float samples, where no exact mode exists.
    pub mode: f64,
    /// Pixels at the sample type's saturation level. Anything above it is
    /// unrepresentable, so "at" and "at or above" are the same set.
    pub saturated: usize,
    /// The saturation level itself: the physical value of the largest
    /// representable raw sample (65535 for the unsigned-16 convention, 255 for
    /// `U8`), or the observed maximum for float samples — where `saturated` is
    /// therefore definitionally `max_count`.
    pub saturation: f64,
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
    /// Only computed when the caller asks via [`InfoRequest::pixel_stats`]. For
    /// an already-debayered RGB cube these describe its green channel.
    pub pixel_stats: Option<PixelStats>,
    /// Only computed when the caller asks via [`InfoRequest::stars`]. For an
    /// already-debayered RGB cube detection runs on its green channel.
    pub stars: Option<StarReport>,
}

/// What to compute beyond the header-derived metadata. Each field costs a pass
/// over the pixels, so the caller asks for what it will actually print.
///
/// A request struct rather than one entry point per combination: the two flags
/// are independent in both directions, and a `header_info_with_stars` plus a
/// fourth for the pair is exactly the API sprawl this prevents.
#[derive(Clone, Copy, Default, Debug)]
pub struct InfoRequest {
    pub pixel_stats: bool,
    pub stars: bool,
}

/// A frame's star metrics, plus the plane they were measured on.
///
/// The dimensions travel with the numbers because a CFA frame detects on a green
/// super-pixel plane at half the frame's size, and its HFR/FWHM are in *that
/// plane's* pixels — a report that omits this is actively misleading. A caller
/// compares `plane_width` against the frame's width rather than re-deriving the
/// "is it a super-pixel plane, and did an odd width drop a column" rule, which
/// lives in [`detection_plane`] and must not drift.
pub struct StarReport {
    pub stats: StarStats,
    pub plane_width: usize,
    pub plane_height: usize,
}

/// Read `input`'s header and build a [`HeaderInfo`] summary, without reading
/// (or decompressing) pixel data. Use [`header_info_with`] to also get the
/// statistics that need the pixels.
pub fn header_info(input: &Path) -> Result<HeaderInfo> {
    header_info_with(input, InfoRequest::default())
}

/// Like [`header_info`], but also reads (transparently decompressing if needed)
/// the pixel data and computes whatever `req` asks for. One read serves both
/// requests: a caller wanting pixel statistics *and* star metrics must not open
/// and decompress the frame twice.
pub fn header_info_with(input: &Path, req: InfoRequest) -> Result<HeaderInfo> {
    let fits =
        FitsFile::from_file(input).with_context(|| format!("cannot read {}", input.display()))?;

    let (header, img) = find_image_hdu(&fits, input)?;
    Ok(header_info_from(header, img.as_ref(), req))
}

/// Build a [`HeaderInfo`] from an already-loaded image and header, without
/// touching the filesystem. This is the shared core of [`header_info`] and lets
/// callers that already hold a decoded `(header, img)` (e.g. the GUI's loader)
/// reuse the same summary without a second read.
///
/// For an already-debayered RGB cube, both statistics and star metrics are
/// measured on the frame's green channel ([`green_plane`]) — green carries the
/// most signal on a Bayer-derived frame — rather than being skipped.
pub fn header_info_from(header: &Header, img: &ImageData, req: InfoRequest) -> HeaderInfo {
    let is_rgb_cube = is_debayered_rgb_cube(header, img);
    let channels = if is_rgb_cube { 3 } else { 1 };

    let width = img.axes.first().copied().unwrap_or(0);
    let height = img.axes.get(1).copied().unwrap_or(0);
    let bitpix = img.bitpix().to_i64();
    let is_unsigned16 = bitpix == 16 && header.get_float("BZERO").unwrap_or(0.0) == 32768.0;

    let pixel_stats = req.pixel_stats.then(|| pixel_stats(header, img));
    let stars = req.stars.then(|| star_report(header, img)).flatten();

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
        stars,
    }
}

/// Detect and measure the frame's stars. `None` for an image
/// [`detection_plane`] can't build a plane from — a shape this summary reports
/// but has nothing to say about, which is not an error the way an unreadable
/// file is.
fn star_report(header: &Header, img: &ImageData) -> Option<StarReport> {
    let plane = detection_plane(header, img).ok()?;
    let bg = plane_background(&plane);
    Some(StarReport {
        stats: detect_stars(&plane, &bg, &StarDetectOptions::default()),
        plane_width: plane.width,
        plane_height: plane.height,
    })
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
    // An already-debayered RGB cube has three interleaved planes; the fast
    // value-count path below would blend them into one meaningless statistic.
    // Reduce to the green channel (the plane star detection also measures on)
    // and take the general `f64` path over just those samples.
    if is_debayered_rgb_cube(header, img) {
        return stats_from_values(&mut green_plane(header, img).values);
    }
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

/// … and in the 8-bit one. Sizing the array to the sample domain rather than
/// always to 65536 is what makes `counts.len() - 1` mean "the largest
/// representable raw sample" for both types — the saturation level.
const U8_COUNT_SLOTS: usize = 1 << 8;

/// Count occurrences of every raw sample value in one parallel pass, for
/// sample types whose values index into a small array (`U8`, `I16`). Returns
/// the counts plus the `offset` mapping an index back to its raw sample value
/// (`raw = index - offset`), or `None` for wider/float samples.
fn value_counts(img: &ImageData) -> Option<(Vec<u64>, f64)> {
    fn count<T: Sync>(v: &[T], slots: usize, idx: impl Fn(&T) -> usize + Sync + Send) -> Vec<u64> {
        // Every chunk costs a `slots`-slot allocation to zero and a `slots`-element
        // add to merge, so a chunk must count enough samples to earn that back:
        // spread the work one chunk per thread, but never split so fine that the
        // bookkeeping dominates. A small frame stays on a single chunk.
        let chunk = v
            .len()
            .div_ceil(rayon::current_num_threads())
            .max(4 * slots);
        v.par_chunks(chunk)
            .fold(
                || vec![0u64; slots],
                |mut c, chunk| {
                    for x in chunk {
                        c[idx(x)] += 1;
                    }
                    c
                },
            )
            .reduce(|| vec![0u64; slots], add_counts)
    }

    match &img.pixels {
        PixelData::U8(v) => Some((count(v, U8_COUNT_SLOTS, |&x| x as usize), 0.0)),
        PixelData::I16(v) => Some((
            count(v, VALUE_COUNT_SLOTS, |&x| (x as i32 + 32768) as usize),
            32768.0,
        )),
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
    // The saturation level is a property of the sample type, so it is known
    // even with no data: the largest raw sample the array can represent.
    let saturation = physical(counts.len() - 1);
    if n == 0 {
        return PixelStats {
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            mean: 0.0,
            median: 0.0,
            zeros: 0,
            min_count: 0,
            max_count: 0,
            count: 0,
            sigma: 0.0,
            mad: 0.0,
            mode: 0.0,
            saturated: 0,
            saturation,
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
    let (mut best_count, mut best_idx) = (0u64, min_idx);
    for (i, &c) in counts.iter().enumerate().take(max_idx + 1).skip(min_idx) {
        if c == 0 {
            continue;
        }
        let v = physical(i);
        sum += v * c as f64;
        zeros += if v == 0.0 { c as usize } else { 0 };
        buckets[bucket_index(v, min, scale)] += c;
        // Strict `>` so the *first* (lowest) value wins a tie: `max_by_key`
        // would keep the last maximum, and on a bimodal amp-glow histogram the
        // lower of the two peaks is the sky background.
        if c > best_count {
            (best_count, best_idx) = (c, i);
        }
    }
    let mean = sum / n as f64;

    // A second walk for σ: sum the squared deviations around the known mean
    // rather than using Σv² − (Σv)²/n, which cancels catastrophically at a sky
    // level of 20000 ADU with noise of 10.
    let mut sq_sum = 0.0;
    for (i, &c) in counts.iter().enumerate().take(max_idx + 1).skip(min_idx) {
        if c != 0 {
            let d = physical(i) - mean;
            sq_sum += d * d * c as f64;
        }
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
        mean,
        median,
        zeros,
        min_count: counts[min_idx] as usize,
        max_count: counts[max_idx] as usize,
        count: n as usize,
        sigma: (sq_sum / n as f64).sqrt(),
        mad: mad_from_counts(counts, n, median, &physical),
        mode: physical(best_idx),
        saturated: *counts.last().unwrap() as usize,
        saturation,
        histogram: buckets,
    }
}

/// The scaled median absolute deviation from `median`, read off the same
/// value-count array the median came from.
///
/// A deviation `|physical(i) − median|` grows monotonically as `i` moves away
/// from the median in either direction, so two cursors walking outward from the
/// central slot — always advancing whichever has the smaller deviation — visit
/// the deviations in sorted order. That makes this an exact selection with no
/// allocation and no sort. The even-count convention matches
/// [`median_in_place`]: the mean of the deviations at ranks `n/2 - 1` and `n/2`
/// (which coincide for an odd `n`), so the fast and general paths agree
/// exactly.
fn mad_from_counts(counts: &[u64], n: u64, median: f64, physical: &impl Fn(usize) -> f64) -> f64 {
    // The lower central slot: `physical(lo) <= median` by construction, so
    // every slot at or below it deviates downward and every slot above it
    // upward.
    let lo_start = index_at_rank(counts, (n - 1) / 2);
    let mut lo = Some(lo_start);
    let mut hi = next_occupied(counts, lo_start + 1);

    let (r1, r2) = ((n - 1) / 2, n / 2);
    let mut d1 = 0.0;
    let mut cumulative = 0u64;
    let d2 = loop {
        let dev_lo = lo.map(|i| median - physical(i));
        let dev_hi = hi.map(|i| physical(i) - median);
        let take_lo = match (dev_lo, dev_hi) {
            (Some(a), Some(b)) => a <= b,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => unreachable!("cursors exhausted before rank {r2} of {n}"),
        };
        let (i, dev) = if take_lo {
            (lo.unwrap(), dev_lo.unwrap())
        } else {
            (hi.unwrap(), dev_hi.unwrap())
        };

        // This slot covers ranks `cumulative .. cumulative + counts[i]`.
        if (cumulative..cumulative + counts[i]).contains(&r1) {
            d1 = dev;
        }
        if (cumulative..cumulative + counts[i]).contains(&r2) {
            // r2 >= r1, so d1 is already pinned by the time we get here.
            break dev;
        }
        cumulative += counts[i];
        if take_lo {
            lo = prev_occupied(counts, i);
        } else {
            hi = next_occupied(counts, i + 1);
        }
    };

    MAD_TO_SIGMA * (d1 + d2) / 2.0
}

/// The first occupied slot at or after `from`, or `None` past the end.
fn next_occupied(counts: &[u64], from: usize) -> Option<usize> {
    counts
        .get(from..)?
        .iter()
        .position(|&c| c > 0)
        .map(|i| i + from)
}

/// The last occupied slot strictly before `before`, or `None` past the start.
fn prev_occupied(counts: &[u64], before: usize) -> Option<usize> {
    counts[..before].iter().rposition(|&c| c > 0)
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
    stats_from_values(&mut scaled_pixels(header, img))
}

/// Every [`PixelStats`] field from already-scaled physical values, for callers
/// that hold a value buffer rather than a `(header, img)` pair (e.g. a star
/// detection plane). Reorders `values` — selection for the median, then
/// overwritten with deviations for the MAD — so the caller keeps the multiset,
/// not the order.
pub(crate) fn stats_from_values(values: &mut Vec<f64>) -> PixelStats {
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
    let histogram = histogram(values, min, max);

    // The center of the largest histogram bucket stands in for the mode: for
    // continuous float values, "the most common value" has no exact answer.
    // A degenerate range collapses every value into bucket 0, hence `min`.
    let mode_idx = argmax_first(&histogram);
    let mode = if max > min {
        min + (mode_idx as f64 + 0.5) * (max - min) / HISTOGRAM_BUCKETS as f64
    } else {
        min
    };

    let n = values.len();
    let (mean, sigma, median, mad) = if n == 0 {
        (0.0, 0.0, 0.0, 0.0)
    } else {
        // Both the sum and the squared-deviation sum stay sequential, for the
        // reason the min/max comment gives above: a parallel reduction would
        // let the reported value drift with thread scheduling.
        let sum: f64 = values.iter().sum();
        let mean = sum / n as f64;
        let sq_sum: f64 = values.iter().map(|v| (v - mean).powi(2)).sum();
        let sigma = (sq_sum / n as f64).sqrt();

        // σ must be computed before this point: `median_in_place` only reorders
        // `values`, but the MAD step overwrites them with their deviations.
        let median = median_in_place(values);
        values.iter_mut().for_each(|v| *v = (*v - median).abs());
        let mad = MAD_TO_SIGMA * median_in_place(values);
        (mean, sigma, median, mad)
    };

    PixelStats {
        min,
        max,
        mean,
        median,
        zeros,
        min_count,
        max_count,
        count: n,
        sigma,
        mad,
        mode,
        // Float samples have no representable ceiling to saturate against, so
        // the observed maximum is the best available stand-in — which makes the
        // saturated count definitionally `max_count`.
        saturated: max_count,
        saturation: max,
        histogram,
    }
}

/// Index of the largest element, resolving ties to the lowest index. Unlike
/// `max_by_key`, which keeps the *last* maximum — on a bimodal histogram the
/// lower peak is the sky background, so the first one is the one we want.
fn argmax_first(counts: &[u64]) -> usize {
    let mut best = 0;
    for (i, &c) in counts.iter().enumerate() {
        if c > counts[best] {
            best = i;
        }
    }
    best
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
pub(crate) fn median_in_place(values: &mut [f64]) -> f64 {
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
    use crate::stars::tests::REAL_MOSAIC_STAR_COUNT;
    use crate::test_support::{
        test_data, write_mosaic_fits, write_rgb_cube_f32_fits, write_rgb_cube_fits,
        write_star_field_fits,
    };
    use fitskit::HeaderValue;
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
        let info = header_info_with(
            &input,
            InfoRequest {
                pixel_stats: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(info.pixel_stats.is_some());
        // --pixel doesn't imply --stars: it stays exactly as fast as it was.
        assert!(info.stars.is_none());
    }

    #[test]
    fn header_info_with_pixels_on_rgb_cube_measures_the_green_channel() {
        // An already-debayered RGB cube reports pixel stats over its green
        // channel only. The fixture's planes are sequential: for a 4x3 frame
        // (n=12) the green plane holds values 12..=23, so min=12, max=23 and the
        // count is one plane's worth (12), not all three (36).
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 4, 3);
        let info = header_info_with(
            &input,
            InfoRequest {
                pixel_stats: true,
                ..Default::default()
            },
        )
        .unwrap();
        let stats = info.pixel_stats.expect("green-channel stats");
        assert_eq!((stats.min, stats.max), (12.0, 23.0));
        assert_eq!(stats.count, 12);
    }

    #[test]
    fn header_info_with_pixels_on_float_rgb_cube_uses_the_unsigned16_range() {
        // A float cube's green channel is quantized by the fixed full-scale map
        // (1.0 -> 65535), so its ADU numbers are comparable with a 16-bit CFA
        // frame's, not the sub-1.0 floats the fixture stores. The fixture's green
        // values run 12/36..23/36 (~0.33..0.64), so they map into the
        // ~21845..41870 range — large ADU values, but well short of 65535, which
        // is what tells the fixed map apart from a per-frame max-normalization.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb_f32.fits");
        write_rgb_cube_f32_fits(&input, 4, 3);
        let info = header_info_with(
            &input,
            InfoRequest {
                pixel_stats: true,
                ..Default::default()
            },
        )
        .unwrap();
        let stats = info.pixel_stats.expect("green-channel stats");
        assert!(
            stats.min > 21000.0 && stats.min < 22000.0,
            "min {}",
            stats.min
        );
        assert!(
            stats.max > 41000.0 && stats.max < 42000.0,
            "max {}",
            stats.max
        );
        assert_eq!(stats.count, 12);
    }

    #[test]
    fn header_info_with_stars_reads_star_metrics_on_real_data() {
        // The same frame stars.rs pins, reached through a different entry
        // point: uncompressed.fit is a 3008x3008 GRBG mosaic, so detection runs
        // on its 1504x1504 green super-pixel plane.
        let input = test_data("uncompressed.fit");
        let info = header_info_with(
            &input,
            InfoRequest {
                stars: true,
                ..Default::default()
            },
        )
        .unwrap();

        let report = info.stars.expect("stars measured");
        assert_eq!(report.stats.count, REAL_MOSAIC_STAR_COUNT);
        assert_eq!((report.plane_width, report.plane_height), (1504, 1504));
        // The plane is not the frame, which is exactly what tells a report that
        // its HFR needs the half-resolution caveat.
        assert_ne!(report.plane_width, info.width);
        // --stars doesn't imply --pixel: the value-count pass is skipped whole.
        assert!(info.pixel_stats.is_none());
    }

    #[test]
    fn header_info_with_stars_on_a_mono_frame_detects_on_the_frame_itself() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mono.fits");
        write_star_field_fits(&input, 60, 60, 1000.0, &[(30.0, 30.0, 2.0, 2.0, 5000.0)]);
        let info = header_info_with(
            &input,
            InfoRequest {
                stars: true,
                ..Default::default()
            },
        )
        .unwrap();

        let report = info.stars.expect("stars measured");
        assert_eq!(report.stats.count, 1);
        // No super-pixel plane, so no caveat to report.
        assert_eq!((report.plane_width, report.plane_height), (60, 60));
    }

    #[test]
    fn header_info_with_stars_on_an_rgb_cube_detects_on_the_full_res_green_plane() {
        // Star detection on an already-debayered cube runs on its green channel
        // at the frame's full resolution — the plane *is* the frame (no
        // super-pixel halving), so no half-resolution caveat applies.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 4, 3);
        let info = header_info_with(
            &input,
            InfoRequest {
                pixel_stats: true,
                stars: true,
            },
        )
        .unwrap();
        let report = info.stars.expect("stars measured");
        assert_eq!((report.plane_width, report.plane_height), (4, 3));
        assert!(info.pixel_stats.is_some());
    }

    #[test]
    fn default_info_request_computes_neither() {
        // The cheap path stays cheap: header_info reads no pixels at all.
        let input = test_data("uncompressed.fit");
        let info = header_info(&input).unwrap();
        assert!(info.pixel_stats.is_none());
        assert!(info.stars.is_none());
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
        // No spread and a single value: both noise estimates vanish and the
        // mode is that value.
        assert_eq!((stats.sigma, stats.mad, stats.mode), (0.0, 0.0, 0.0));
        assert_eq!(stats.count, 6);
        assert_eq!(stats.histogram[0], 6);
        assert_eq!(stats.histogram.iter().sum::<u64>(), 6);
    }

    /// An unsigned-16 header (BITPIX 16 with BZERO 32768), the convention every
    /// real frame here uses: raw sample `r` carries the physical value
    /// `r + 32768`, so the I16 ceiling of 32767 is a physical 65535.
    fn unsigned16_header() -> Header {
        let mut header = Header::default();
        header.set("BSCALE", HeaderValue::Float(1.0), None);
        header.set("BZERO", HeaderValue::Float(32768.0), None);
        header
    }

    #[test]
    fn robust_stats_match_hand_computed_values() {
        // Values 10,10,10,10,20,20,30,100 with no BSCALE/BZERO, so physical
        // values are the raw samples. By hand: mean = 210/8 = 26.25; median =
        // (10+20)/2 = 15 (ranks 3 and 4); mode = 10 (four of them); the squared
        // deviations about the mean sum to 6587.5, so sigma = sqrt(823.4375);
        // the deviations about the median are 5,5,5,5,5,5,15,85, whose central
        // pair averages to 5, so mad = 1.4826 * 5.
        let img = ImageData::new(
            vec![4, 2],
            PixelData::I16(vec![10, 10, 10, 10, 20, 20, 30, 100]),
        );
        let stats = pixel_stats(&Header::default(), &img);

        assert_eq!(stats.count, 8);
        assert_eq!(stats.mean, 26.25);
        assert_eq!(stats.median, 15.0);
        assert_eq!(stats.mode, 10.0);
        assert!(
            (stats.sigma - 823.4375_f64.sqrt()).abs() < 1e-9,
            "{}",
            stats.sigma
        );
        assert!(
            (stats.mad - MAD_TO_SIGMA * 5.0).abs() < 1e-9,
            "{}",
            stats.mad
        );
        // Nothing near the I16 ceiling, which with no BZERO is a physical 32767.
        assert_eq!(stats.saturation, 32767.0);
        assert_eq!(stats.saturated, 0);
    }

    #[test]
    fn mode_breaks_ties_to_the_lowest_value() {
        // Two values, two pixels each: the lower one wins.
        let img = ImageData::new(vec![2, 2], PixelData::I16(vec![3, 7, 3, 7]));
        assert_eq!(pixel_stats(&Header::default(), &img).mode, 3.0);
    }

    #[test]
    fn mad_averages_the_two_central_deviations_for_an_even_count() {
        // 0,1,2,10: median = 1.5, deviations sort to 0.5, 0.5, 1.5, 8.5, whose
        // two central values (ranks 1 and 2) average to 1.0 — the same
        // even-count convention `median_in_place` uses.
        let img = ImageData::new(vec![2, 2], PixelData::I16(vec![0, 1, 2, 10]));
        let stats = pixel_stats(&Header::default(), &img);
        assert_eq!(stats.median, 1.5);
        assert_eq!(stats.mad, MAD_TO_SIGMA);
        // The general path must land on exactly the same value.
        assert_eq!(pixel_stats_general(&Header::default(), &img).mad, stats.mad);
    }

    #[test]
    fn mad_is_robust_to_outliers_that_inflate_sigma() {
        // A flat background of 1000±1 (33 pixels at each of 999/1000/1001) with
        // four stars at 30000. The stars drag sigma up by orders of magnitude;
        // the MAD never notices them. This is the entire reason MAD exists.
        let mut pixels: Vec<i16> = (0..99).map(|i| 999 + (i % 3)).collect();
        pixels.extend([30000; 4]);
        let img = ImageData::new(vec![103, 1], PixelData::I16(pixels));
        let stats = pixel_stats(&Header::default(), &img);

        // The median is 1000 and the median deviation from it is 1 ADU, so the
        // stars leave mad at 1.4826 — as if they weren't in the frame at all.
        assert_eq!(stats.median, 1000.0);
        assert!((stats.mad - MAD_TO_SIGMA).abs() < 1e-9, "{}", stats.mad);
        assert!(
            stats.sigma > 100.0 * stats.mad,
            "{} vs {}",
            stats.sigma,
            stats.mad
        );
    }

    #[test]
    fn saturated_counts_pixels_at_the_sample_maximum() {
        // Three pixels at the unsigned-16 ceiling (raw 32767 = physical 65535).
        let img = ImageData::new(
            vec![2, 3],
            PixelData::I16(vec![0, 100, 32767, 32767, 32767, 5]),
        );
        let stats = pixel_stats(&unsigned16_header(), &img);
        assert_eq!(stats.saturation, 65535.0);
        assert_eq!(stats.saturated, 3);
        assert_eq!(stats.max, 65535.0);
    }

    #[test]
    fn saturation_level_follows_the_sample_type() {
        // The same shape as an 8-bit frame: saturation is 255, not the 65535 a
        // fixed-size count array would report. Nothing here is saturated.
        let img = ImageData::new(vec![2, 3], PixelData::U8(vec![0, 100, 200, 200, 200, 5]));
        let stats = pixel_stats(&Header::default(), &img);
        assert_eq!(stats.saturation, 255.0);
        assert_eq!(stats.saturated, 0);

        // …and pixels at 255 are counted against it.
        let img = ImageData::new(vec![2, 2], PixelData::U8(vec![255, 255, 1, 2]));
        assert_eq!(pixel_stats(&Header::default(), &img).saturated, 2);
    }

    #[test]
    fn robust_stats_hold_their_invariants_on_real_data() {
        for name in ["uncompressed.fit", "compressed.fits.fz"] {
            let input = test_data(name);
            let fits = FitsFile::from_file(&input).unwrap();
            let (header, img) = find_image_hdu(&fits, &input).unwrap();
            let stats = pixel_stats(header, img.as_ref());

            // Real sky frames are star-sparse, so the robust noise estimate must
            // come in below the outlier-sensitive one.
            assert!(
                stats.mad <= stats.sigma,
                "{name}: {} {}",
                stats.mad,
                stats.sigma
            );
            assert!(stats.min <= stats.mode && stats.mode <= stats.max, "{name}");
            assert_eq!(stats.count, 3008 * 3008);
            // Both frames are unsigned-16.
            assert_eq!(stats.saturation, 65535.0);
        }
    }

    #[test]
    fn mode_agrees_between_paths_only_as_a_background_estimate() {
        // The fast path reports the exact most-common value; the general path
        // can only report the center of the largest histogram bucket — and that
        // bucket need not even contain the exact mode, since the distribution
        // within a bucket is skewed. So the two agree on the background *level*
        // (here, to well under 1% of the frame's value range) rather than on a
        // value, which is why `fast_path_matches_general_path_on_real_data`
        // deliberately leaves `mode` out.
        let input = test_data("uncompressed.fit");
        let fits = FitsFile::from_file(&input).unwrap();
        let (header, img) = find_image_hdu(&fits, &input).unwrap();
        let img = img.as_ref();

        let fast = pixel_stats(header, img);
        let general = pixel_stats_general(header, img);
        let range = fast.max - fast.min;
        assert!(
            (fast.mode - general.mode).abs() < 0.01 * range,
            "{} vs {} (range {range})",
            fast.mode,
            general.mode
        );
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
        assert_eq!(fast.count, general.count);
        // `saturated`/`saturation` are deliberately not compared: only the fast
        // path knows the sample type's ceiling, while the general path can only
        // fall back on the observed maximum.
        // MAD is a selection over the same multiset of deviations, with the
        // same even-count convention on both paths, so there is no arithmetic
        // to drift: it must match exactly.
        assert_eq!(fast.mad, general.mad);
        // The fast path sums value*count over ≤65536 slots instead of every
        // pixel individually, so the mean — and sigma, which is summed the same
        // way around it — may differ in the last few ulps.
        assert!((fast.mean - general.mean).abs() < 1e-9 * general.mean.abs().max(1.0));
        assert!((fast.sigma - general.sigma).abs() < 1e-9 * general.sigma.abs().max(1.0));
        // `mode` is deliberately not compared here: the general path can only
        // approximate it to a histogram bucket. See
        // `mode_agrees_between_paths_only_to_histogram_bucket_width`.
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
