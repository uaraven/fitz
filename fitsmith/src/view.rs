//! Rendering a loaded document into the window's widgets — pure presentation,
//! with no state or threading. Data in, Slint properties out; the controller
//! owns *when* to call these, this owns *how* the data maps to the UI.

use libfitz::info::SummaryField;
use slint::{Image, ModelRc, VecModel};

use crate::doc::{LoadedDoc, StatSummary};
use crate::image::preview_to_image;
use crate::{AppWindow, HeaderRow, StatItem};

/// Show a loaded document: its image (plus natural size for fit/zoom), the
/// header table, and the pixel statistics + histogram.
pub fn show_doc(app: &AppWindow, doc: &LoadedDoc) {
    app.set_preview_image(preview_to_image(&doc.preview));
    app.set_image_width(doc.preview.width as f32);
    app.set_image_height(doc.preview.height as f32);
    app.set_header_rows(header_rows(doc));
    app.set_info(info_items(&doc.info));
    app.set_stats(stat_items(&doc.stats));
    app.set_star_stats(star_items(&doc.stats));
    app.set_histogram(histogram(&doc.stats));
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
fn header_rows(doc: &LoadedDoc) -> ModelRc<HeaderRow> {
    let rows: Vec<HeaderRow> = doc
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
            label: f.label.as_str().into(),
            value: f.value.as_str().into(),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// The labeled statistics for the panel, or empty when there are no stats. For
/// an already-debayered RGB cube the numbers are its green channel's, so each
/// ADU label carries a `(G)` suffix ([`StatSummary::channel`]) — otherwise a
/// frame's numbers would read as if measured across all channels.
fn stat_items(stats: &Option<StatSummary>) -> ModelRc<StatItem> {
    let items = match stats {
        Some(s) => {
            // The channel suffix disambiguates an RGB cube's per-channel numbers;
            // for a single-channel frame it's absent and the label is unchanged.
            let adu = |name: &str| match s.channel {
                Some(ch) => format!("{name} ADU ({ch})"),
                None => format!("{name} ADU"),
            };
            vec![
                stat(&adu("Min"), format_stat(s.min)),
                stat(&adu("Max"), format_stat(s.max)),
                stat(&adu("Mean"), format_stat(s.mean)),
                stat(&adu("Median"), format_stat(s.median)),
                stat(&adu("Sigma"), format_stat(s.sigma)),
                stat(&adu("MAD"), format_stat(s.mad)),
                stat("Zeros", s.zeros.to_string()),
            ]
        }
        None => Vec::new(),
    };
    ModelRc::new(VecModel::from(items))
}

/// The star metrics for the panel's second column: always the star count, plus
/// the shape medians when detection found any stars. Empty when the frame has no
/// star metrics (an already-debayered RGB cube, or a shape detection can't run
/// on) — the panel then hides the column.
fn star_items(stats: &Option<StatSummary>) -> ModelRc<StatItem> {
    let mut items = Vec::new();
    if let Some(stars) = stats.as_ref().and_then(|s| s.stars.as_ref()) {
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

/// The normalized histogram bar heights, or empty when there are no stats.
fn histogram(stats: &Option<StatSummary>) -> ModelRc<f32> {
    let heights = stats
        .as_ref()
        .map(|s| s.histogram.clone())
        .unwrap_or_default();
    ModelRc::new(VecModel::from(heights))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::StarSummary;
    use slint::Model;

    /// Flatten a `StatItem` model back into `"label: value"` strings for
    /// assertions.
    fn rows(model: &ModelRc<StatItem>) -> Vec<String> {
        model
            .iter()
            .map(|s| format!("{}: {}", s.label, s.value))
            .collect()
    }

    fn summary() -> StatSummary {
        StatSummary {
            min: 0.0,
            max: 65535.0,
            mean: 1234.5,
            median: 1000.0,
            sigma: 12.75,
            mad: 8.0,
            zeros: 7,
            histogram: Vec::new(),
            stars: None,
            channel: None,
        }
    }

    #[test]
    fn pixel_stats_carry_adu_labels() {
        // The renamed labels: Max ADU etc., and whole numbers print without a
        // decimal point while fractional means keep three places.
        let items = stat_items(&Some(summary()));
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
    fn rgb_cube_stats_label_the_green_channel() {
        // An RGB cube's numbers are its green channel's, so each ADU label gains
        // a `(G)` suffix; the channel-free Zeros row is untouched.
        let stats = StatSummary {
            channel: Some("G"),
            ..summary()
        };
        let labels: Vec<String> = stat_items(&Some(stats))
            .iter()
            .map(|s| s.label.to_string())
            .collect();
        assert_eq!(
            labels,
            [
                "Min ADU (G)",
                "Max ADU (G)",
                "Mean ADU (G)",
                "Median ADU (G)",
                "Sigma ADU (G)",
                "MAD ADU (G)",
                "Zeros",
            ]
        );
    }

    #[test]
    fn no_pixel_stats_is_an_empty_column() {
        assert!(rows(&stat_items(&None)).is_empty());
    }

    #[test]
    fn star_column_lists_count_and_present_shapes() {
        // A frame with detected stars: count plus the three shapes, each rounded
        // to two places.
        let stats = StatSummary {
            stars: Some(StarSummary {
                count: 42,
                hfr: Some(2.418),
                fwhm: Some(3.001),
                eccentricity: Some(0.5),
            }),
            ..summary()
        };
        assert_eq!(
            rows(&star_items(&Some(stats))),
            ["Stars: 42", "HFR: 2.42", "FWHM: 3.00", "Eccentricity: 0.50"]
        );
    }

    #[test]
    fn starless_frame_shows_only_the_count() {
        // Detection ran but found nothing: the count (zero) is a real
        // measurement, but there are no shapes to report.
        let stats = StatSummary {
            stars: Some(StarSummary {
                count: 0,
                hfr: None,
                fwhm: None,
                eccentricity: None,
            }),
            ..summary()
        };
        assert_eq!(rows(&star_items(&Some(stats))), ["Stars: 0"]);
    }

    #[test]
    fn no_star_metrics_is_an_empty_column() {
        // No StarSummary at all (an RGB cube, or a shape detection can't run on):
        // the panel hides the column.
        assert!(rows(&star_items(&Some(summary()))).is_empty());
        assert!(rows(&star_items(&None)).is_empty());
    }
}
