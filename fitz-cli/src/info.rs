//! The `info` command: print a human-readable summary of a FITS image —
//! resolution, bit depth, channel count and sky coordinates. With `--pixel`
//! it additionally reads the (possibly tile-compressed) pixel data and reports
//! basic pixel statistics, one column per colour channel for an
//! already-debayered RGB cube. `--stars` metrics are always measured on a
//! single detection plane — the frame's green channel for an RGB cube.

use std::fmt::Write as _;
use std::path::Path;

use crate::io_prompt::print_step;
use crate::options::InfoOptions;
use crate::terminal::terminal_dimensions;
use anyhow::Result;
use libfitz::stars::{StarDetectOptions, StarStats};
use libfitz::stats::{ImageStats, Stats};

/// Height of the rendered histogram in terminal character rows.
const HISTOGRAM_ROWS: usize = 10;

/// Width each channel's value is right-aligned into in a [`print_table`] row,
/// so a table's columns line up across every row printed for it regardless of
/// how many digits any one row's values happen to have.
const STATS_COLUMN_WIDTH: usize = 10;

/// Write one row of an aligned table: `caption` in the label column (padded
/// to [`FIELD_LABEL_WIDTH`] like the header summary above), then each of
/// `columns` right-aligned into [`STATS_COLUMN_WIDTH`] and separated by `|`,
/// so every column lines up across every row of the same table regardless of
/// how many digits any one row's values happen to have. The shared layout
/// behind [`print_table`], [`print_table_header`] and [`print_star_stats`].
fn print_row(out: &mut String, caption: &str, columns: &[String]) {
    let value = columns
        .iter()
        .map(|c| format!("{c:>STATS_COLUMN_WIDTH$}"))
        .collect::<Vec<_>>()
        .join(" | ");
    let _ = writeln!(out, "  {:>1$}{value}", caption, FIELD_LABEL_WIDTH);
}

/// Print one row of a per-channel statistics table: `caption` in the label
/// column, then one value per channel in `stats` — one for a single-channel
/// frame, three (R, G, B) for an RGB cube. `extractor` pulls the value to
/// display out of each channel's [`Stats`].
fn print_table(
    out: &mut String,
    caption: &str,
    stats: &[Stats],
    extractor: impl Fn(&Stats) -> String,
) {
    let columns: Vec<String> = stats.iter().map(extractor).collect();
    print_row(out, &format!("{caption}:"), &columns);
}

/// Print the column header for a [`print_table`]/[`print_star_stats`] report:
/// `caption` in the label column, then each of `columns` — e.g. `R`, `G`, `B`
/// for a per-channel table — so they sit directly over the value columns the
/// following rows print.
fn print_table_header(out: &mut String, caption: &str, columns: &[&str]) {
    let columns: Vec<String> = columns.iter().map(|c| c.to_string()).collect();
    print_row(out, caption, &columns);
}

pub fn info_file(input: &Path, opts: &InfoOptions) -> Result<()> {
    print_step(opts.verbose, "reading");

    let image = libfitz::fits_file::load_fits(input)?;

    // `--headers` is a distinct mode: dump the image HDU's raw header cards
    // instead of the formatted summary. For a tile-compressed input this is the
    // compressed HDU's header as stored, so the binary-table container and `Z*`
    // keywords appear alongside the carried-over original image keywords.
    if opts.headers {
        let mut out = String::new();
        let _ = writeln!(out, "{}", input.display());
        push_raw_headers(&mut out, &image.header);
        print!("{out}");
        return Ok(());
    }

    // Build the whole report in a buffer and print it with a single write, so
    // reports for different files don't interleave when `process_files` runs the
    // batch in parallel. Writing to a `String` is infallible, so the formatting
    // `Result`s are discarded.
    let mut out = String::new();
    let _ = writeln!(out, "{}", input.display());
    // The curated metadata fields come from `libfitz` so the CLI report and
    // the GUI info panel stay in sync; the CLI just pads the labels into a column.
    for field in libfitz::summary::info_summary(&image) {
        let _ = writeln!(
            out,
            "  {:<1$}{value}",
            format!("{}:", field.label),
            FIELD_LABEL_WIDTH,
            value = field.value,
        );
    }

    // Pixel statistics are only computed on request (`--pixel`), since they
    // require reading and decompressing the full pixel array. For an
    // already-debayered RGB cube `Image::stats()` reports one column per
    // colour channel rather than blending the R/G/B planes into a
    // meaningless figure.
    if opts.pixel {
        let stats = image.stats();
        print_stats(&mut out, &stats, opts.log);
    }

    // Star metrics are their own request (`--stars`), independent of `--pixel`:
    // detection builds its threshold from the detection plane's own background,
    // never from the frame's PixelStats, so neither flag implies the other.
    if opts.stars {
        let star_stats = image
            .detection_plane()?
            .detect_stars(&StarDetectOptions::default());
        print_star_stats(&mut out, &star_stats);
    }

    print!("{out}");
    Ok(())
}

fn print_stats(out: &mut String, img_stats: &ImageStats, log_scale: bool) {
    let stats = &img_stats.channels;
    if stats.len() == 3 {
        print_table_header(out, "Channels", &["R", "G", "B"]);
    }
    print_table(out, "Min", stats, |s: &Stats| s.min.to_string());
    print_table(out, "Max", stats, |s: &Stats| s.max.to_string());
    print_table(out, "Mean", stats, |s: &Stats| {
        round_to(s.mean as f64, 2).to_string()
    });
    print_table(out, "Median", stats, |s: &Stats| {
        round_to(s.median as f64, 2).to_string()
    });
    print_table(out, "Mode", stats, |s: &Stats| {
        round_to(s.mode as f64, 2).to_string()
    });
    print_table(out, "Avg Dev", stats, |s: &Stats| {
        round_to(s.avg_dev as f64, 2).to_string()
    });
    print_table(out, "MAD", stats, |s: &Stats| {
        round_to(s.mad as f64, 2).to_string()
    });
    print_table(out, "σ", stats, |s: &Stats| {
        round_to(s.sigma as f64, 2).to_string()
    });
    print_table(out, "Bit-depth (est)", stats, |s: &Stats| {
        s.estimated_bit_depth.to_string()
    });
    print_table(out, "Zeros", stats, |s: &Stats| s.zero_count.to_string());
    print_table(out, "Saturated", stats, |s: &Stats| {
        s.saturated_count.to_string()
    });

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
    print_histogram(out, &img_stats.histogram, width, left_pad, log_scale);
}

/// Print the `--stars` report: a header naming each metric, then one data row
/// of their values — the same aligned-table layout [`print_stats`] uses for
/// its per-channel rows, transposed. Detection runs once per frame rather
/// than once per channel, so here the metrics (`Count`, `HFR`, `FWHM`,
/// `Eccentricity`) are the columns and there is only ever one row.
fn print_star_stats(out: &mut String, stats: &StarStats) {
    // An outcome, not an error — and a cloud indicator in its own right, so it
    // must be reported rather than silently printing a table of dashes.
    if stats.hfr.is_none() && stats.fwhm.is_none() && stats.eccentricity.is_none() {
        let _ = writeln!(out, "  Stars:       none detected");
        return;
    }

    let _ = writeln!(out, "Stars");
    let _ = writeln!(out, "        Count: {}", stats.count);
    let _ = writeln!(
        out,
        "          HFR: {}",
        round_to(stats.hfr.unwrap_or(0.0), 2)
    );
    let _ = writeln!(
        out,
        "         FWHM: {}",
        round_to(stats.fwhm.unwrap_or(0.0), 2)
    );
    let _ = writeln!(
        out,
        " Eccentricity: {}",
        round_to(stats.eccentricity.unwrap_or(0.0), 2)
    );
}

/// Round to `places` decimal places. Star shapes are measurements good to a
/// couple of digits; `trim_float`'s six would be reporting noise.
fn round_to(v: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (v * scale).round() / scale
}

/// Append the header's raw FITS cards to `out`, one card per line with trailing
/// padding trimmed. Each keyword is serialized back to its 80-column card image
/// (so commentary cards and CONTINUE-split long strings are shown as they appear
/// in the file), giving an unformatted dump rather than the curated summary.
fn push_raw_headers(out: &mut String, header: &libfitz::fitskit::Header) {
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
const FIELD_LABEL_WIDTH: usize = 16;

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
fn print_histogram(out: &mut String, hist: &[u64], width: usize, left_pad: usize, log: bool) {
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
    use libfitz::fitskit::{Header, HeaderValue};
    use libfitz::stars::StarStats;

    fn stats_fixture(min: u16, max: u16, mean: f32) -> Stats {
        Stats {
            min,
            max,
            mean,
            median: mean,
            mode: mean as u16,
            avg_dev: 1.0,
            mad: 1.0,
            sigma: 1.0,
            estimated_bit_depth: 16,
            zero_count: 0,
            saturated_count: 0,
            ..Stats::default()
        }
    }

    #[test]
    fn round_to_keeps_two_places() {
        // Star shapes are good to a couple of digits, not six.
        assert_eq!(round_to(2.41379, 2), 2.41);
        assert_eq!(round_to(3.0, 2), 3.0);
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
        print_histogram(&mut out, &[1, 2, 3], width, pad, false);
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

    #[test]
    fn info_file_reads_pixels_and_stars_on_an_rgb_cube() {
        // The bundled debayered frame is a 3-plane RGB cube: `--pixel`/`--stars`
        // now succeed on it (measuring the green channel) rather than printing
        // an "unsupported" notice.
        let input = test_data("uncompressed_debayer.fits");
        info_file(
            &input,
            &InfoOptions {
                pixel: true,
                stars: true,
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn stars_are_detected_on_the_detection_plane_not_raw_pixels() {
        // `Image::detect_stars` is documented as requiring
        // `Image::detection_plane()` first: on this debayered RGB cube,
        // detecting directly on the raw interleaved samples finds nothing at
        // all (the interleaving reads as noise), while the green-channel
        // detection plane finds the frame's actual stars. `info --stars`
        // (`info_file` above) must go through the plane, not the raw image.
        let image = libfitz::fits_file::load_fits(&test_data("uncompressed_debayer.fits")).unwrap();

        let raw = image.detect_stars(&StarDetectOptions::default());
        assert_eq!(raw.count, 0);

        let via_plane = image
            .detection_plane()
            .unwrap()
            .detect_stars(&StarDetectOptions::default());
        assert!(via_plane.count > 0);
        assert!(via_plane.hfr.is_some());
    }

    #[test]
    fn push_raw_headers_renders_trimmed_cards() {
        let mut header = Header::new();
        header.set("OBJECT", HeaderValue::String("M31".to_string()), None);
        let mut out = String::new();
        push_raw_headers(&mut out, &header);
        assert!(out.contains("OBJECT"));
        assert!(out.contains("M31"));
        // Cards are trimmed, so no line carries trailing padding.
        assert!(out.lines().all(|l| l == l.trim_end()));
    }

    #[test]
    fn print_stats_reports_single_channel_without_header_row() {
        let img_stats = ImageStats {
            channels: vec![stats_fixture(0, 65535, 100.0)],
            histogram: [0u64; 256],
        };
        let mut out = String::new();
        print_stats(&mut out, &img_stats, false);
        assert!(!out.contains("Channels"));
        assert!(out.contains("Min"));
        assert!(out.contains("Max"));
        assert!(out.contains("65535"));
    }

    #[test]
    fn print_stats_reports_rgb_channel_header() {
        let img_stats = ImageStats {
            channels: vec![
                stats_fixture(0, 100, 10.0),
                stats_fixture(0, 200, 20.0),
                stats_fixture(0, 300, 30.0),
            ],
            histogram: [0u64; 256],
        };
        let mut out = String::new();
        print_stats(&mut out, &img_stats, false);
        assert!(out.contains("Channels"));
        // The R/G/B header row and every stats row have three value columns.
        let min_line = out
            .lines()
            .find(|l| l.trim_start().starts_with("Min:"))
            .unwrap();
        assert_eq!(min_line.matches('|').count(), 2);
    }

    #[test]
    fn print_star_stats_reports_none_detected() {
        let stats = StarStats {
            count: 0,
            hfr: None,
            fwhm: None,
            eccentricity: None,
        };
        let mut out = String::new();
        print_star_stats(&mut out, &stats);
        assert!(out.contains("none detected"));
    }

    #[test]
    fn print_star_stats_reports_measured_values() {
        let stats = StarStats {
            count: 12,
            hfr: Some(2.4137),
            fwhm: Some(3.1),
            eccentricity: Some(0.2),
        };
        let mut out = String::new();
        print_star_stats(&mut out, &stats);
        assert!(out.contains("Count:"));
        assert!(out.contains('2'));
        // Each metric line pairs its label with the rounded value.
        let has_line =
            |label: &str, value: &str| out.lines().any(|l| l.contains(label) && l.contains(value));
        assert!(has_line("Count:", "12"));
        assert!(has_line("HFR:", "2.41"));
        assert!(has_line("FWHM:", "3.1"));
        assert!(has_line("Eccentricity:", "0.2"));
    }

    #[test]
    fn print_row_aligns_columns_by_width() {
        let mut out = String::new();
        print_row(&mut out, "Label:", &["1".to_string(), "22".to_string()]);
        let line = out.lines().next().unwrap();
        // Each column is right-aligned into STATS_COLUMN_WIDTH and `|`-separated.
        let value_part = line.split_once(':').unwrap().1;
        let columns: Vec<&str> = value_part.split('|').collect();
        assert_eq!(columns.len(), 2);
        assert!(columns.iter().all(|c| c.len() == STATS_COLUMN_WIDTH + 1));
    }
}
