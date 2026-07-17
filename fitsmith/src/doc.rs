//! A loaded FITS document in display-ready form: the rendered preview plus the
//! header cards and pixel statistics the Headers tab and stats panel need. All
//! of it is derived on the worker thread (where the decoded `(header, img)` is
//! already in hand) and cached as one unit, so switching tabs or toggling the
//! stats panel never re-reads the file.
//!
//! Kept free of Slint types so the whole thing is `Send` (it crosses from the
//! worker back to the UI thread) and the formatting logic stays unit-testable
//! without an event loop.

use libfitz::fits_image::is_debayered_rgb_cube;
use libfitz::fitskit::{Header, HeaderValue, ImageData, Keyword};
use libfitz::info::{
    HISTOGRAM_BUCKETS, InfoRequest, StarReport, SummaryField, header_info_from, pixel_stats,
};
use libfitz::preview::PreviewImage;

/// One FITS header card, pre-formatted into the three display columns.
pub struct HeaderCard {
    pub name: String,
    pub value: String,
    pub comment: String,
}

/// Pixel statistics for the stats panel: the numeric summary plus a normalized
/// (0..1) histogram ready to drive the bar heights.
pub struct StatSummary {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub median: f64,
    /// Population standard deviation of the pixel values — noise inflated by
    /// stars and hot pixels. Compare against `mad`.
    pub sigma: f64,
    /// Median absolute deviation (scaled to estimate σ), the robust noise
    /// measure that ignores stars.
    pub mad: f64,
    pub zeros: usize,
    /// [`HISTOGRAM_BUCKETS`] bar heights in `[0, 1]`.
    pub histogram: Vec<f32>,
    /// The frame's star metrics, or `None` when [`detection_plane`] can't build a
    /// plane from this image (the same shapes that also carry no `PixelStats`).
    ///
    /// [`detection_plane`]: libfitz::fits_image::detection_plane
    pub stars: Option<StarSummary>,
}

/// The frame's star metrics for the stats panel: how many stars were accepted
/// and the median of each shape measurement across them. The shape medians are
/// `None` when detection found no stars (a starless frame still has a count — it
/// is zero).
pub struct StarSummary {
    pub count: usize,
    pub hfr: Option<f64>,
    pub fwhm: Option<f64>,
    pub eccentricity: Option<f64>,
}

/// Everything the UI needs about one loaded file, cached as a unit.
pub struct LoadedDoc {
    pub preview: PreviewImage,
    pub headers: Vec<HeaderCard>,
    /// The curated metadata summary (label/value pairs), the same fields the
    /// `fitz info` command reports, for the docked info panel.
    pub info: Vec<SummaryField>,
    /// `None` for an already-debayered RGB cube, where per-pixel stats over a
    /// single channel aren't meaningful (mirrors `header_info_with`).
    pub stats: Option<StatSummary>,
}

impl LoadedDoc {
    /// Build the display-ready document from a decoded image and its rendered
    /// preview. Runs on the worker thread.
    pub fn build(header: &Header, img: &ImageData, preview: PreviewImage) -> Self {
        let headers = header.iter().map(header_card).collect();
        // Request star detection so the stats panel can show star metrics: the
        // one pass here (on a cached, worker-thread build) feeds both the info
        // summary and the panel's star column. Pixel statistics stay a separate
        // call below. The info panel itself still shows metadata only.
        let hi = header_info_from(
            header,
            img,
            InfoRequest {
                stars: true,
                ..Default::default()
            },
        );
        let info = hi.summary();
        let stats = (!is_debayered_rgb_cube(header, img)).then(|| {
            let s = pixel_stats(header, img);
            StatSummary {
                min: s.min,
                max: s.max,
                mean: s.mean,
                median: s.median,
                sigma: s.sigma,
                mad: s.mad,
                zeros: s.zeros,
                histogram: normalize_histogram(&s.histogram),
                stars: hi.stars.as_ref().map(star_summary),
            }
        });
        LoadedDoc {
            preview,
            headers,
            info,
            stats,
        }
    }
}

/// Extract the panel's star metrics from a [`StarReport`]. The plane dimensions
/// the report also carries aren't shown here; the panel reports the numbers.
fn star_summary(report: &StarReport) -> StarSummary {
    let s = &report.stats;
    StarSummary {
        count: s.count,
        hfr: s.hfr,
        fwhm: s.fwhm,
        eccentricity: s.eccentricity,
    }
}

/// Pre-format one keyword into name / value / comment display columns.
fn header_card(kw: &Keyword) -> HeaderCard {
    HeaderCard {
        name: kw.name.clone(),
        value: format_value(kw.value.as_ref()),
        comment: kw.comment.clone().unwrap_or_default(),
    }
}

/// Render a header value for display: strings without their FITS quoting,
/// everything else via its natural formatting, and a valueless (commentary or
/// blank) card as an empty cell.
fn format_value(value: Option<&HeaderValue>) -> String {
    match value {
        Some(HeaderValue::String(s)) => s.trim().to_string(),
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

/// Normalize raw histogram counts to bar heights in `[0, 1]`. A logarithmic
/// scale keeps the long tail of an astronomical frame visible instead of a
/// single spike swamping every other bucket. An empty image yields all zeros.
fn normalize_histogram(counts: &[u64]) -> Vec<f32> {
    debug_assert_eq!(counts.len(), HISTOGRAM_BUCKETS);
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

    fn kw(name: &str, value: Option<HeaderValue>, comment: Option<&str>) -> Keyword {
        Keyword {
            name: name.to_string(),
            value,
            comment: comment.map(str::to_string),
        }
    }

    #[test]
    fn string_values_are_unquoted_and_trimmed() {
        let c = header_card(&kw(
            "OBJECT",
            Some(HeaderValue::String("  M31  ".to_string())),
            Some("target"),
        ));
        assert_eq!(c.name, "OBJECT");
        assert_eq!(c.value, "M31");
        assert_eq!(c.comment, "target");
    }

    #[test]
    fn numeric_values_use_natural_formatting() {
        assert_eq!(format_value(Some(&HeaderValue::Integer(300))), "300");
        assert_eq!(format_value(Some(&HeaderValue::Logical(true))), "T");
    }

    #[test]
    fn commentary_card_has_empty_value() {
        let c = header_card(&kw("COMMENT", None, Some("a note")));
        assert_eq!(c.value, "");
        assert_eq!(c.comment, "a note");
    }

    #[test]
    fn histogram_normalizes_peak_to_one() {
        let mut counts = vec![0u64; HISTOGRAM_BUCKETS];
        counts[0] = 1;
        counts[1] = 1000; // the peak bucket
        let norm = normalize_histogram(&counts);
        assert_eq!(norm.len(), HISTOGRAM_BUCKETS);
        // The peak reaches the top; every bar is within range and the empty
        // buckets stay flat.
        assert!((norm[1] - 1.0).abs() < 1e-6);
        assert!(norm.iter().all(|&h| (0.0..=1.0).contains(&h)));
        assert_eq!(norm[2], 0.0);
        // Log scale keeps the lone-count bucket visible (a linear scale would
        // crush it to ~0.001).
        assert!(norm[0] > 0.0 && norm[0] < norm[1]);
    }

    #[test]
    fn empty_histogram_is_all_zero() {
        let norm = normalize_histogram(&vec![0u64; HISTOGRAM_BUCKETS]);
        assert!(norm.iter().all(|&h| h == 0.0));
    }
}
