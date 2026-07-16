//! Rendering a loaded document into the window's widgets — pure presentation,
//! with no state or threading. Data in, Slint properties out; the controller
//! owns *when* to call these, this owns *how* the data maps to the UI.

use fitz_core::info::SummaryField;
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

/// The labeled statistics for the panel, or empty for an already-debayered RGB
/// cube (where single-channel pixel stats aren't meaningful).
fn stat_items(stats: &Option<StatSummary>) -> ModelRc<StatItem> {
    let items = match stats {
        Some(s) => vec![
            stat("min", format_stat(s.min)),
            stat("max", format_stat(s.max)),
            stat("mean", format_stat(s.mean)),
            stat("median", format_stat(s.median)),
            stat("zeros", s.zeros.to_string()),
        ],
        None => Vec::new(),
    };
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
