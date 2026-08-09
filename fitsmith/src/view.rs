//! Rendering a loaded document into the window's widgets — pure presentation,
//! with no state or threading. Data in, Slint properties out; the controller
//! owns *when* to call these, this owns *how* the data maps to the UI.

use libfitz::stars::StarStats;
use libfitz::stats::{ImageStats, Stats};
use libfitz::summary::SummaryField;
use slint::{Image, ModelRc, VecModel};

use crate::doc::FileMeta;
use crate::image::preview_to_image;
use crate::{AppWindow, HeaderRow, StatItem};

/// Show a loaded document: its image (plus natural size for fit/zoom), the
/// header table, and the pixel statistics + histogram.
pub fn show_doc(app: &AppWindow, meta: &FileMeta, preview: &image::RgbImage) {
    app.set_preview_image(preview_to_image(preview));
    app.set_image_width(preview.width() as f32);
    app.set_image_height(preview.height() as f32);
    app.set_header_rows(header_rows(meta));
    app.set_info(info_items(&meta.info));
    app.set_stats(stat_items(&meta.stats));
    app.set_star_stats(star_items(&meta.stars));
    app.set_histogram(histogram(&meta.stats));
}

/// Reset every data-driven view to empty (no document, or a failed load).
pub fn clear(app: &AppWindow) {
    app.set_preview_image(Image::default());
    app.set_image_width(0.0);
    app.set_image_height(0.0);
    app.set_header_rows(ModelRc::new(VecModel::<HeaderRow>::default()));
    app.set_info(ModelRc::new(VecModel::<StatItem>::default()));
    app.set_stats(ModelRc::new(VecModel::<StatItem>::default()));
    app.set_star_stats(ModelRc::new(VecModel::<StatItem>::default()));
    app.set_histogram(ModelRc::new(VecModel::<f32>::default()));
}

/// One table row per header card, so the full header is shown (as
/// `fitz info --headers` prints it).
fn header_rows(meta: &FileMeta) -> ModelRc<HeaderRow> {
    let rows: Vec<HeaderRow> = meta
        .headers
        .iter()
        .map(|c| HeaderRow {
            keyword: c.name.as_str().into(),
            value: c.value.as_str().into(),
            comment: c.comment.as_str().into(),
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

/// The curated metadata summary as label/value rows for the info panel (reusing
/// the generic [`StatItem`] label/value pair).
fn info_items(fields: &[SummaryField]) -> ModelRc<StatItem> {
    let items: Vec<StatItem> = fields
        .iter()
        .map(|f| StatItem {
            label: f.label.into(),
            value: f.value.as_str().into(),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// The labeled pixel statistics for the panel, one row per measurement. A
/// single-channel source (grayscale/CFA) shows one number; an already-
/// debayered RGB source shows all three channels on the same row, mirroring
/// `fitz info`'s per-channel table.
fn stat_items(stats: &ImageStats) -> ModelRc<StatItem> {
    let row = |label: &str, extract: fn(&Stats) -> f64| {
        stat(label, join_channels(&stats.channels, extract))
    };
    let items = vec![
        row("Min ADU", |s| s.min as f64),
        row("Max ADU", |s| s.max as f64),
        row("Mean ADU", |s| s.mean as f64),
        row("Median ADU", |s| s.median as f64),
        row("Sigma ADU", |s| s.sigma as f64),
        row("MAD ADU", |s| s.mad as f64),
        row("Zeros", |s| s.zero_count as f64),
    ];
    ModelRc::new(VecModel::from(items))
}

/// Format one measurement across every channel: a bare number for a single
/// channel, or `"R <v>  G <v>  B <v>"` for three.
fn join_channels(channels: &[Stats], extract: fn(&Stats) -> f64) -> String {
    const LABELS: [&str; 3] = ["R", "G", "B"];
    if channels.len() == 3 {
        LABELS
            .iter()
            .zip(channels)
            .map(|(label, s)| format!("{label} {}", format_stat(extract(s))))
            .collect::<Vec<_>>()
            .join("  ")
    } else {
        channels
            .first()
            .map(|s| format_stat(extract(s)))
            .unwrap_or_default()
    }
}

/// The star metrics for the panel's second column: always the star count, plus
/// the shape medians when detection found any stars. Empty when the frame has
/// no star metrics at all (detection couldn't build a plane for it).
fn star_items(stars: &Option<StarStats>) -> ModelRc<StatItem> {
    let mut items = Vec::new();
    if let Some(stars) = stars {
        items.push(stat("Stars", stars.count.to_string()));
        if let Some(hfr) = stars.hfr {
            items.push(stat("HFR", format_star(hfr)));
        }
        if let Some(fwhm) = stars.fwhm {
            items.push(stat("FWHM", format_star(fwhm)));
        }
        if let Some(ecc) = stars.eccentricity {
            items.push(stat("Eccentricity", format_star(ecc)));
        }
    }
    ModelRc::new(VecModel::from(items))
}

/// The normalized histogram bar heights.
fn histogram(stats: &ImageStats) -> ModelRc<f32> {
    ModelRc::new(VecModel::from(normalize_histogram(&stats.histogram)))
}

fn stat(label: &str, value: String) -> StatItem {
    StatItem {
        label: label.into(),
        value: value.into(),
    }
}

/// Whole numbers without a decimal point, fractional ones to three places.
/// Shared with the analytics chart's axis/tooltip labels so a metric reads the
/// same in the stats panel and on the plot.
pub fn format_stat(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.3}")
    }
}

/// Star shapes (HFR/FWHM/eccentricity) to two decimal places — they are
/// measurements good to a couple of digits, matching what `fitz info --stars`
/// prints; more would be reporting noise.
fn format_star(v: f64) -> String {
    format!("{v:.2}")
}

/// Normalize raw histogram counts to bar heights in `[0, 1]`. A logarithmic
/// scale keeps the long tail of an astronomical frame visible instead of a
/// single spike swamping every other bucket. An empty image yields all zeros.
fn normalize_histogram(counts: &[u64]) -> Vec<f32> {
    let max = counts.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return vec![0.0; counts.len()];
    }
    let denom = ((max + 1) as f64).ln();
    counts
        .iter()
        .map(|&c| (((c + 1) as f64).ln() / denom) as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model;

    /// Flatten a `StatItem` model back into `"label: value"` strings for
    /// assertions.
    fn rows(model: &ModelRc<StatItem>) -> Vec<String> {
        model
            .iter()
            .map(|s| format!("{}: {}", s.label, s.value))
            .collect()
    }

    fn mono_stats(
        min: u16,
        max: u16,
        mean: f32,
        median: f32,
        sigma: f32,
        mad: f32,
        zeros: u32,
    ) -> ImageStats {
        ImageStats {
            channels: vec![Stats {
                min,
                max,
                mean,
                median,
                sigma,
                mad,
                zero_count: zeros,
                ..Stats::default()
            }],
            histogram: [0u64; 256],
        }
    }

    #[test]
    fn pixel_stats_report_a_single_channel() {
        let stats = mono_stats(0, 65535, 1234.5, 1000.0, 12.75, 8.0, 7);
        let items = stat_items(&stats);
        assert_eq!(
            rows(&items),
            [
                "Min ADU: 0",
                "Max ADU: 65535",
                "Mean ADU: 1234.500",
                "Median ADU: 1000",
                "Sigma ADU: 12.750",
                "MAD ADU: 8",
                "Zeros: 7",
            ]
        );
    }

    #[test]
    fn pixel_stats_report_all_three_channels_for_rgb() {
        let stats = ImageStats {
            channels: vec![
                Stats {
                    min: 10,
                    max: 100,
                    mean: 50.0,
                    ..Stats::default()
                },
                Stats {
                    min: 20,
                    max: 200,
                    mean: 60.0,
                    ..Stats::default()
                },
                Stats {
                    min: 30,
                    max: 300,
                    mean: 70.0,
                    ..Stats::default()
                },
            ],
            histogram: [0u64; 256],
        };
        let items = stat_items(&stats);
        let min_row = rows(&items)
            .into_iter()
            .find(|r| r.starts_with("Min ADU"))
            .unwrap();
        assert_eq!(min_row, "Min ADU: R 10  G 20  B 30");
    }

    #[test]
    fn star_column_lists_count_and_present_shapes() {
        let stats = Some(StarStats {
            count: 42,
            hfr: Some(2.418),
            fwhm: Some(3.001),
            eccentricity: Some(0.5),
        });
        assert_eq!(
            rows(&star_items(&stats)),
            ["Stars: 42", "HFR: 2.42", "FWHM: 3.00", "Eccentricity: 0.50"]
        );
    }

    #[test]
    fn starless_frame_shows_only_the_count() {
        let stats = Some(StarStats {
            count: 0,
            hfr: None,
            fwhm: None,
            eccentricity: None,
        });
        assert_eq!(rows(&star_items(&stats)), ["Stars: 0"]);
    }

    #[test]
    fn no_star_metrics_is_an_empty_column() {
        assert!(rows(&star_items(&None)).is_empty());
    }

    #[test]
    fn histogram_normalizes_peak_to_one() {
        let mut counts = [0u64; 256];
        counts[0] = 1;
        counts[1] = 1000; // the peak bucket
        let norm = normalize_histogram(&counts);
        assert_eq!(norm.len(), 256);
        assert!((norm[1] - 1.0).abs() < 1e-6);
        assert!(norm.iter().all(|&h| (0.0..=1.0).contains(&h)));
        assert_eq!(norm[2], 0.0);
        assert!(norm[0] > 0.0 && norm[0] < norm[1]);
    }

    #[test]
    fn empty_histogram_is_all_zero() {
        let norm = normalize_histogram(&[0u64; 256]);
        assert!(norm.iter().all(|&h| h == 0.0));
    }
}
