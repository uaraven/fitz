//! The `info` command: print a human-readable summary of a FITS image —
//! resolution, bit depth, channel count and sky coordinates. With `--pixel`
//! it additionally reads the (possibly tile-compressed) pixel data and reports
//! basic pixel statistics, which are only meaningful for single-channel,
//! non-debayered data.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use fitz_core::info::{header_info, header_info_with_pixels, trim_float};

use crate::io_prompt::print_step;
use crate::options::InfoOptions;
use crate::terminal::terminal_dimensions;

/// Height of the rendered histogram in terminal character rows.
const HISTOGRAM_ROWS: usize = 10;

pub fn info_file(input: &Path, opts: &InfoOptions) -> Result<()> {
    print_step(opts.verbose, "reading");

    let info = if opts.pixel {
        header_info_with_pixels(input)?
    } else {
        header_info(input)?
    };

    // `--headers` is a distinct mode: dump the image HDU's raw header cards
    // instead of the formatted summary. For a tile-compressed input this is the
    // compressed HDU's header as stored, so the binary-table container and `Z*`
    // keywords appear alongside the carried-over original image keywords.
    if opts.headers {
        let mut out = String::new();
        let _ = writeln!(out, "{}", input.display());
        push_raw_headers(&mut out, &info.header);
        print!("{out}");
        return Ok(());
    }

    // Build the whole report in a buffer and print it with a single write, so
    // reports for different files don't interleave when `process_files` runs the
    // batch in parallel. Writing to a `String` is infallible, so the formatting
    // `Result`s are discarded.
    let mut out = String::new();
    let _ = writeln!(out, "{}", input.display());
    // The curated metadata fields come from `fitz-core` so the CLI report and
    // the GUI info panel stay in sync; the CLI just pads the labels into a column.
    for field in info.summary() {
        let _ = writeln!(
            out,
            "  {:<1$}{value}",
            format!("{}:", field.label),
            FIELD_LABEL_WIDTH,
            value = field.value,
        );
    }

    // Pixel statistics are only computed on request (`--pixel`), since they
    // require reading and decompressing the full pixel array. They only make
    // sense for a single, non-debayered channel: mixing the R/G/B planes of an
    // RGB cube would give a meaningless figure.
    if opts.pixel {
        match &info.pixel_stats {
            None => {
                let _ = writeln!(
                    out,
                    "  Pixels:      pixel statistics are not supported for debayered images"
                );
            }
            Some(stats) => {
                // Split across lines by meaning rather than crowding everything
                // onto `Pixels:`; each label pads into the same column as the
                // metadata fields above.
                let _ = writeln!(
                    out,
                    "  Pixels:      min={} max={} mean={} median={} zeros={}",
                    trim_float(stats.min),
                    trim_float(stats.max),
                    trim_float(stats.mean),
                    trim_float(stats.median),
                    stats.zeros,
                );
                let _ = writeln!(
                    out,
                    "  Noise:       sigma={} mad={}",
                    trim_float(stats.sigma),
                    trim_float(stats.mad),
                );
                let _ = writeln!(out, "  Background:  mode={}", trim_float(stats.mode));
                // The fraction comes from the stats' own sample count, so it
                // stays right for any future per-plane statistics.
                let percent = if stats.count > 0 {
                    stats.saturated as f64 / stats.count as f64 * 100.0
                } else {
                    0.0
                };
                let _ = writeln!(
                    out,
                    "  Saturated:   {} of {} ({}%)",
                    stats.saturated,
                    stats.count,
                    trim_float((percent * 1000.0).round() / 1000.0),
                );
                // The histogram is the last thing in the report: a title aligned
                // with the other fields, then the bar chart centered horizontally.
                // The width is chosen so each column maps to a whole number of
                // buckets: the largest of 16/32/64/128/256 whose drawn box (`width
                // + 2` for the `|` borders) fits the terminal.
                let (cols, _) = terminal_dimensions();
                let _ = writeln!(out, "  Histogram:");
                let width = histogram_width(cols as usize);
                // The drawn box adds a `|` on each side, so center the full
                // `width + 2` box within the terminal.
                let boxed = width + 2;
                let left_pad = (cols as usize).saturating_sub(boxed) / 2;
                push_histogram(&mut out, &stats.histogram, width, left_pad, opts.log);
            }
        }
    }

    print!("{out}");
    Ok(())
}

/// Append the header's raw FITS cards to `out`, one card per line with trailing
/// padding trimmed. Each keyword is serialized back to its 80-column card image
/// (so commentary cards and CONTINUE-split long strings are shown as they appear
/// in the file), giving an unformatted dump rather than the curated summary.
fn push_raw_headers(out: &mut String, header: &fitz_core::fitskit::Header) {
    for keyword in header.iter() {
        for card in keyword.to_cards() {
            // Cards are fixed-width ASCII; `from_utf8_lossy` is only a guard
            // against a malformed card and won't allocate for valid ones.
            let line = String::from_utf8_lossy(&card);
            let _ = writeln!(out, "{}", line.trim_end());
        }
    }
}

/// Column width (including the trailing colon) reserved for a field's label, so
/// values across different fields line up (e.g. `"  Resolution:  1024 x 768"`).
const FIELD_LABEL_WIDTH: usize = 13;

/// Pick the drawn histogram width for a terminal `cols` wide.
///
/// The width is the largest power-of-two divisor of the histogram bucket count
/// (256) in `16..=256` whose box (`width + 2` for the `|` borders) fits within
/// `cols`, so every column maps to a whole number of buckets. Falls back to the
/// smallest candidate (16) on a very narrow terminal.
fn histogram_width(cols: usize) -> usize {
    const CANDIDATES: [usize; 5] = [256, 128, 64, 32, 16];
    CANDIDATES
        .into_iter()
        .find(|&w| w + 2 <= cols)
        .unwrap_or(16)
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
    let max_weight = if log {
        ((max + 1) as f64).ln()
    } else {
        max as f64
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_data;

    #[test]
    fn render_histogram_shape_and_scaling() {
        // Two columns: the tallest fills the full height, the half-height one
        // reaches halfway. 4 rows => 16 quarter-levels total.
        let rows = 4;
        let out = render_histogram(&[8, 4], 2, rows, false);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), rows);

        // Column 0 (max) is a full block on every row.
        assert!(lines.iter().all(|l| l.starts_with('█')));
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
    fn histogram_width_picks_power_of_two_fitting_terminal() {
        // Each candidate is chosen once the terminal is wide enough for its
        // box (`width + 2`); every candidate evenly divides the bucket count.
        assert_eq!(histogram_width(300), 256);
        assert_eq!(histogram_width(258), 256);
        assert_eq!(histogram_width(257), 128);
        assert_eq!(histogram_width(130), 128);
        assert_eq!(histogram_width(129), 64);
        assert_eq!(histogram_width(66), 64);
        assert_eq!(histogram_width(33), 16);
        // Narrower than the smallest box still falls back to 16.
        assert_eq!(histogram_width(0), 16);
        for w in [256, 128, 64, 32, 16] {
            assert_eq!(256usize % w, 0);
        }
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
}
