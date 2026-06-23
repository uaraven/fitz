//! The `info` command: print a human-readable summary of a FITS image —
//! resolution, bit depth, channel count and sky coordinates. With `--pixel`
//! it additionally reads the (possibly tile-compressed) pixel data and reports
//! basic pixel statistics, which are only meaningful for single-channel,
//! non-debayered data.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use fitskit::{FitsFile, Header, ImageData};
use rayon::prelude::*;

use crate::fits_image::{find_image_hdu, get_bayerpat, print_step, scaled_pixels};
use crate::options::InfoOptions;
use crate::terminal::terminal_dimensions;

/// Number of buckets in the pixel-value histogram.
const HISTOGRAM_BUCKETS: usize = 256;

/// Height of the rendered histogram in terminal character rows.
const HISTOGRAM_ROWS: usize = 10;

/// Min, max, mean and median of a single-channel image's physical pixel values,
/// plus the count of pixels whose physical value is exactly zero and a
/// `HISTOGRAM_BUCKETS`-bin histogram of the values over the `[min, max]` range.
struct PixelStats {
    min: f64,
    max: f64,
    mean: f64,
    median: f64,
    zeros: usize,
    /// Pixel counts per bucket, evenly spanning `[min, max]`. Bucket `i` covers
    /// values in `[min + i*w, min + (i+1)*w)` where `w = (max - min) / BUCKETS`,
    /// with `max` itself folded into the last bucket.
    histogram: Vec<u64>,
}

pub fn info_file(input: &Path, opts: &InfoOptions) -> Result<()> {
    print_step(opts.verbose, "reading");
    let fits =
        FitsFile::from_file(input).with_context(|| format!("cannot read {}", input.display()))?;

    let (header, img) = find_image_hdu(&fits, input, opts.verbose)?;
    let img = img.as_ref();

    // `--headers` is a distinct mode: dump the image HDU's raw header cards
    // instead of the formatted summary. For a tile-compressed input this is the
    // compressed HDU's header as stored, so the binary-table container and `Z*`
    // keywords appear alongside the carried-over original image keywords.
    if opts.headers {
        let mut out = String::new();
        let _ = writeln!(out, "{}", input.display());
        push_raw_headers(&mut out, header);
        print!("{out}");
        return Ok(());
    }

    // A 3-plane cube with no BAYERPAT is an already-debayered RGB image (3
    // channels); anything else is treated as a single-channel frame, matching
    // the detection used by the debayer/stretch/split commands.
    let is_rgb_cube = get_bayerpat(header).is_none() && img.axes.len() == 3 && img.axes[2] == 3;
    let channels = if is_rgb_cube { 3 } else { 1 };

    let width = img.axes.first().copied().unwrap_or(0);
    let height = img.axes.get(1).copied().unwrap_or(0);

    // Build the whole report in a buffer and print it with a single write, so
    // reports for different files don't interleave when `process_files` runs the
    // batch in parallel. Writing to a `String` is infallible, so the formatting
    // `Result`s are discarded.
    let mut out = String::new();
    let _ = writeln!(out, "{}", input.display());
    let _ = writeln!(out, "  Resolution:  {width} x {height}");
    let _ = writeln!(out, "  Bit depth:   {}", bit_depth_label(header, img));
    let _ = writeln!(
        out,
        "  Channels:    {channels} ({})",
        channel_label(channels, header)
    );

    if let Some(pat) = get_bayerpat(header) {
        let pat = pat.trim();
        if !pat.is_empty() {
            let _ = writeln!(out, "  Bayer:       {pat}");
        }
    }

    if let Some(object) = header.get_string("OBJECT") {
        let object = object.trim();
        if !object.is_empty() {
            let _ = writeln!(out, "  Object:      {object}");
        }
    }

    push_coordinate(&mut out, header, Axis::Ra, "RA", "OBJCTRA");
    push_coordinate(&mut out, header, Axis::Dec, "DEC", "OBJCTDEC");

    if let Some(exptime) = header.get_float("EXPTIME") {
        let _ = writeln!(out, "  Exposure:    {} s", trim_float(exptime));
    }
    if let Some(filter) = header.get_string("FILTER") {
        let filter = filter.trim();
        if !filter.is_empty() {
            let _ = writeln!(out, "  Filter:      {filter}");
        }
    }
    if let Some(instrument) = header.get_string("INSTRUME") {
        let instrument = instrument.trim();
        if !instrument.is_empty() {
            let _ = writeln!(out, "  Instrument:  {instrument}");
        }
    }
    if let Some(date) = header.get_string("DATE-OBS") {
        let date = date.trim();
        if !date.is_empty() {
            let _ = writeln!(out, "  Date-obs:    {date}");
        }
    }

    // Pixel statistics are only computed on request (`--pixel`), since they
    // require reading and decompressing the full pixel array. They only make
    // sense for a single, non-debayered channel: mixing the R/G/B planes of an
    // RGB cube would give a meaningless figure.
    if opts.pixel {
        if is_rgb_cube {
            let _ = writeln!(
                out,
                "  Pixels:      pixel statistics are not supported for debayered images"
            );
        } else {
            let stats = pixel_stats(header, img);
            let _ = writeln!(
                out,
                "  Pixels:      min={} max={} mean={} median={} zeros={}",
                trim_float(stats.min),
                trim_float(stats.max),
                trim_float(stats.mean),
                trim_float(stats.median),
                stats.zeros,
            );
            // The histogram is the last thing in the report: a title aligned
            // with the other fields, then the bar chart spanning 80% of the
            // terminal width, centered horizontally.
            let (cols, _) = terminal_dimensions();
            let _ = writeln!(out, "  Histogram:");
            let width = (cols as usize * 80) / 100;
            // The drawn box adds a `|` on each side, so center the full
            // `width + 2` box within the terminal.
            let boxed = (width + 2).min(cols as usize);
            let left_pad = (cols as usize - boxed) / 2;
            push_histogram(&mut out, &stats.histogram, width, left_pad, opts.log);
        }
    }

    print!("{out}");
    Ok(())
}

/// Append the header's raw FITS cards to `out`, one card per line with trailing
/// padding trimmed. Each keyword is serialized back to its 80-column card image
/// (so commentary cards and CONTINUE-split long strings are shown as they appear
/// in the file), giving an unformatted dump rather than the curated summary.
fn push_raw_headers(out: &mut String, header: &Header) {
    for keyword in header.iter() {
        for card in keyword.to_cards() {
            // Cards are fixed-width ASCII; `from_utf8_lossy` is only a guard
            // against a malformed card and won't allocate for valid ones.
            let line = String::from_utf8_lossy(&card);
            let _ = writeln!(out, "{}", line.trim_end());
        }
    }
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

/// Describe the channel layout. The Bayer pattern itself is reported on its own
/// `Bayer:` line, so the raw-mosaic case just notes that it is a mosaic.
fn channel_label(channels: usize, header: &Header) -> String {
    if channels == 3 {
        return "debayered RGB".to_string();
    }
    match get_bayerpat(header) {
        Some(_) => "mosaic".to_string(),
        None => "monochrome / undebayered".to_string(),
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

/// Append a sky coordinate to `out`. When the decimal-degree keyword is present
/// it is rendered in sexagesimal form (hours for RA, degrees for DEC) with the
/// decimal value in parentheses; otherwise the raw sexagesimal header string is
/// shown verbatim.
fn push_coordinate(
    out: &mut String,
    header: &Header,
    axis: Axis,
    deg_key: &str,
    sexagesimal_key: &str,
) {
    let label = match axis {
        Axis::Ra => "RA",
        Axis::Dec => "DEC",
    };
    let deg = header.get_float(deg_key);
    let sexagesimal = header
        .get_string(sexagesimal_key)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let _ = match (deg, sexagesimal) {
        (Some(d), _) => writeln!(
            out,
            "  {label}:{}{}",
            pad(label),
            format_coordinate(axis, d)
        ),
        (None, Some(s)) => writeln!(out, "  {label}:{}{s}", pad(label)),
        (None, None) => Ok(()),
    };
}

/// Format a decimal-degree coordinate in sexagesimal form with the decimal value
/// echoed in parentheses, e.g. `20h 30m 00.00s (20.5h)` for RA or
/// `-12° 30' 00.00" (-12.5°)` for DEC.
fn format_coordinate(axis: Axis, deg: f64) -> String {
    match axis {
        Axis::Ra => {
            // 360 degrees of RA span 24 hours, so hours = degrees / 15.
            let hours = deg / 15.0;
            let (h, m, s) = sexagesimal(hours.abs());
            let sign = if hours < 0.0 { "-" } else { "" };
            format!("{sign}{h}h {m:02}m {s:05.2}s ({}h)", trim_float(hours))
        }
        Axis::Dec => {
            let (d, m, s) = sexagesimal(deg.abs());
            let sign = if deg < 0.0 { "-" } else { "" };
            format!("{sign}{d}° {m:02}' {s:05.2}\" ({}°)", trim_float(deg))
        }
    }
}

/// Split a non-negative decimal value into whole units, minutes and seconds.
/// Rounding is done on the total seconds first so any carry propagates and the
/// returned minutes/seconds stay in `[0, 60)`.
fn sexagesimal(value: f64) -> (u64, u64, f64) {
    let total_seconds = (value * 3600.0 * 100.0).round() / 100.0;
    let whole = (total_seconds / 3600.0).trunc();
    let rem = total_seconds - whole * 3600.0;
    let minutes = (rem / 60.0).trunc();
    let seconds = rem - minutes * 60.0;
    (whole as u64, minutes as u64, seconds)
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

/// Compute min/max/mean/median, the zero count and the value histogram of the
/// image's physical (BSCALE/BZERO-applied) pixel values.
fn pixel_stats(header: &Header, img: &ImageData) -> PixelStats {
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

/// Bin `values` into a `HISTOGRAM_BUCKETS`-bin histogram spanning `[min, max]`.
/// `max` (and anything rounding to the upper edge) folds into the last bucket.
/// A degenerate range (`max <= min`, e.g. a constant image) puts every value in
/// bucket 0. The work is split across threads, each filling a local bucket
/// array that is then summed element-wise.
fn histogram(values: &[f64], min: f64, max: f64) -> Vec<u64> {
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

/// Append the rendered histogram to `out`, enclosed in a `+`/`-`/`|` box and
/// indented by `left_pad` spaces so the box is centered under the report.
/// Delegates the chart shape to [`render_histogram`] and uses [`HISTOGRAM_ROWS`]
/// for the height.
fn push_histogram(out: &mut String, hist: &[u64], width: usize, left_pad: usize, log: bool) {
    let chart = render_histogram(hist, width, HISTOGRAM_ROWS, log);
    let pad = " ".repeat(left_pad);
    let border = format!("{pad}+{}+\n", "-".repeat(width));
    out.push_str(&border);
    for line in chart.lines() {
        out.push_str(&pad);
        out.push('|');
        out.push_str(line);
        out.push('|');
        out.push('\n');
    }
    out.push_str(&border);
}

/// Render `hist` as a text bar chart `rows` characters tall and `width`
/// characters wide. Unicode block elements give sub-cell vertical resolution:
/// each character row is split into quarters (`▂ ▄ ▆ █`), so the effective
/// height is `rows * 4` levels. Bars are scaled so the tallest column reaches
/// the full height; any non-empty column shows at least one quarter so it stays
/// visible. With `log`, the bar heights scale by `log(count + 1)` instead of
/// linearly, which keeps a tall low-value spike from flattening the rest of the
/// distribution. The result is `rows` newline-terminated lines, drawn
/// top-to-bottom.
fn render_histogram(hist: &[u64], width: usize, rows: usize, log: bool) -> String {
    /// Vertical sub-divisions per character cell (quarter-height blocks).
    const LEVELS_PER_ROW: u64 = 4;
    /// Block glyphs indexed by how many quarters of the cell are filled (0..=4).
    const BLOCKS: [char; 5] = [' ', '▂', '▄', '▆', '█'];

    if width == 0 || rows == 0 {
        return String::new();
    }

    // Resample the buckets onto `width` columns: each column sums the buckets
    // falling in its slice of the range. `max(start + 1)` guarantees every
    // column maps to at least one bucket, so a display wider than the bucket
    // count stretches (rather than leaving gaps in) the histogram.
    let n = hist.len();
    let mut columns = vec![0u64; width];
    if n > 0 {
        for (j, slot) in columns.iter_mut().enumerate() {
            let start = j * n / width;
            let end = ((j + 1) * n / width).max(start + 1).min(n);
            *slot = hist[start..end].iter().sum();
        }
    }

    let max = columns.iter().copied().max().unwrap_or(0);
    let total_levels = rows as u64 * LEVELS_PER_ROW;
    // `weight` maps a count onto the 0..=1 axis. The log axis uses `ln(c + 1)`
    // (so an empty column still weighs 0) normalised by the tallest column.
    let max_weight = if log { ((max + 1) as f64).ln() } else { max as f64 };
    let weight = |c: u64| if log { ((c + 1) as f64).ln() } else { c as f64 };
    let heights: Vec<u64> = columns
        .iter()
        .map(|&c| {
            if max == 0 || c == 0 {
                0
            } else {
                ((weight(c) / max_weight) * total_levels as f64)
                    .round()
                    .max(1.0) as u64
            }
        })
        .collect();

    let mut out = String::with_capacity(rows * (width + 1));
    for row in (0..rows as u64).rev() {
        for &h in &heights {
            let filled = h.saturating_sub(row * LEVELS_PER_ROW).min(LEVELS_PER_ROW);
            out.push(BLOCKS[filled as usize]);
        }
        out.push('\n');
    }
    out
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
        let (h, m, s) = sexagesimal(0.9999999);
        assert_eq!((h, m), (1, 0));
        assert_eq!(s, 0.0);
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
    fn render_histogram_shape_and_scaling() {
        // Two columns: the tallest fills the full height, the half-height one
        // reaches halfway. 4 rows => 16 quarter-levels total.
        let rows = 4;
        let out = render_histogram(&[8, 4], 2, rows, false);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), rows);

        // Column 0 (max) is a full block on every row.
        assert!(lines.iter().all(|l| l.chars().next() == Some('█')));
        // Column 1 (half) is empty on the top two rows and full on the bottom two.
        let col1: Vec<char> = lines.iter().map(|l| l.chars().nth(1).unwrap()).collect();
        assert_eq!(col1, vec![' ', ' ', '█', '█']);
    }

    #[test]
    fn render_histogram_keeps_tiny_bars_visible() {
        // A column far below the max must still render at least one quarter so
        // it doesn't vanish; an all-zero column stays blank.
        let out = render_histogram(&[1000, 1, 0], 3, 10, false);
        let bottom = out.lines().last().unwrap();
        let chars: Vec<char> = bottom.chars().collect();
        assert_eq!(chars[0], '█'); // the max column
        assert_eq!(chars[1], '▂'); // tiny but present
        assert_eq!(chars[2], ' '); // genuinely empty
    }

    #[test]
    fn render_histogram_fits_requested_geometry() {
        // Output is exactly `rows` lines, each `width` characters wide.
        let width = 50;
        let rows = 10;
        let hist: Vec<u64> = (0..256).collect();
        let out = render_histogram(&hist, width, rows, false);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), rows);
        assert!(lines.iter().all(|l| l.chars().count() == width));
    }

    #[test]
    fn render_histogram_log_axis_lifts_small_columns() {
        // A column 1000x smaller than the max is invisible on a linear axis
        // (rounds to a single quarter) but is lifted well above the floor on a
        // log axis, where ln(1000+1)/ln(1_000_000+1) ≈ 0.5 of full height.
        let hist = [1_000_000u64, 1000];
        let linear = render_histogram(&hist, 2, 10, false);
        let log = render_histogram(&hist, 2, 10, true);

        // Count filled (non-space) cells in the second column for each axis.
        let filled = |chart: &str| {
            chart
                .lines()
                .filter(|l| l.chars().nth(1).is_some_and(|c| c != ' '))
                .count()
        };
        assert!(filled(&log) > filled(&linear));
    }

    #[test]
    fn render_histogram_handles_degenerate_geometry() {
        assert_eq!(render_histogram(&[1, 2, 3], 0, 10, false), "");
        assert_eq!(render_histogram(&[1, 2, 3], 10, 0, false), "");
    }

    #[test]
    fn push_histogram_draws_centered_box() {
        // The chart is wrapped in a `+`/`-`/`|` box, and every line is prefixed
        // by `left_pad` spaces so the box sits centered under the report.
        let width = 6;
        let pad = 4;
        let mut out = String::new();
        push_histogram(&mut out, &[1, 2, 3], width, pad, false);
        let lines: Vec<&str> = out.lines().collect();

        // HISTOGRAM_ROWS chart rows plus the top and bottom borders.
        assert_eq!(lines.len(), HISTOGRAM_ROWS + 2);
        assert!(lines.iter().all(|l| l.starts_with("    ")));
        // pad spaces + box border (`|` + width + `|`).
        assert!(lines.iter().all(|l| l.chars().count() == pad + width + 2));

        let border = format!("{}+{}+", " ".repeat(pad), "-".repeat(width));
        assert_eq!(*lines.first().unwrap(), border);
        assert_eq!(*lines.last().unwrap(), border);
        // Interior rows are bounded by `|` on both sides.
        for line in &lines[1..lines.len() - 1] {
            let trimmed = line.trim_start();
            assert!(trimmed.starts_with('|') && trimmed.ends_with('|'));
        }
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
        assert_eq!(channel_label(1, header), "mosaic");
        assert_eq!(get_bayerpat(header).map(str::trim), Some("RGGB"));
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
    fn info_file_dumps_headers_on_real_data() {
        // `--headers` must succeed on a real frame, reading the HDU header.
        let input = test_data("uncompressed.fit");
        info_file(
            &input,
            &InfoOptions {
                headers: true,
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn raw_headers_are_dumped_verbatim() {
        // The raw dump emits one trimmed card per keyword, in order, and each
        // line re-parses to the same keyword it came from.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 4, 4, Some("RGGB"));

        let fits = FitsFile::from_file(&input).unwrap();
        let (header, _img) = find_image_hdu(&fits, &input, false).unwrap();

        let mut out = String::new();
        push_raw_headers(&mut out, header);
        let lines: Vec<&str> = out.lines().collect();

        // Standard mandatory cards appear in their raw card form, in order.
        assert!(lines.iter().any(|l| l.starts_with("SIMPLE  =")));
        assert!(lines.iter().any(|l| l.starts_with("BITPIX  =")));
        assert!(lines.iter().any(|l| l.starts_with("NAXIS1  =")));
        assert!(lines.iter().any(|l| l.contains("RGGB")));
        // No card exceeds the 80-column FITS card width, and none is END
        // (the terminator isn't stored as a keyword).
        assert!(lines.iter().all(|l| l.chars().count() <= 80));
        assert!(!lines.iter().any(|l| l.starts_with("END")));
        // One emitted line per stored keyword (these fixtures use no
        // CONTINUE-split long strings).
        assert_eq!(lines.len(), header.iter().count());
    }

    #[test]
    fn info_file_reads_pixels_on_real_data() {
        // With `--pixel` the command must read the pixel data and succeed on a
        // single-channel mosaic frame.
        let input = test_data("uncompressed.fit");
        info_file(
            &input,
            &InfoOptions {
                pixel: true,
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn info_file_pixels_on_rgb_cube_does_not_fail() {
        // Pixel stats aren't supported for debayered RGB cubes; the command
        // reports that in the output rather than erroring out.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 4, 3);
        info_file(
            &input,
            &InfoOptions {
                pixel: true,
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn zero_pixels_are_counted() {
        // A 3x3 mosaic stores values 0..8, so exactly one pixel is zero.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 3, 3, None);

        let fits = FitsFile::from_file(&input).unwrap();
        let (header, img) = find_image_hdu(&fits, &input, false).unwrap();
        let img = img.as_ref();
        let stats = pixel_stats(header, img);
        assert_eq!(stats.zeros, 1);
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
        assert_eq!(channel_label(1, header), "mosaic");
        assert_eq!(get_bayerpat(header).map(str::trim), Some("GRBG"));
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
