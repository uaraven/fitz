//! A loaded FITS document's resident metadata: the header cards, curated
//! info summary, pixel statistics and star metrics the Headers tab and stats
//! panel need. All of it is derived on the worker thread (where the decoded
//! `Image` is already in hand) and cached as one unit, so switching tabs or
//! toggling the stats panel never re-reads the file.
//!
//! Kept free of Slint types so the whole thing is `Send` (it crosses from the
//! worker back to the UI thread) and the formatting logic stays unit-testable
//! without an event loop. Pixel statistics and star metrics are stored
//! exactly as `libfitz` computes them (`ImageStats`, `StarStats`) rather than
//! copied into a parallel struct — only the header cards are genuinely
//! reshaped for display.

use libfitz::data::Image;
use libfitz::fitskit::{HeaderValue, Keyword};
use libfitz::stars::{StarDetectOptions, StarStats};
use libfitz::stats::ImageStats;
use libfitz::summary::SummaryField;

/// One FITS header card, pre-formatted into the three display columns.
pub struct HeaderCard {
    pub name: String,
    pub value: String,
    pub comment: String,
}

/// Everything the UI needs about one loaded file that doesn't depend on the
/// debayer/stretch preview toggles, cached as a unit and kept resident for as
/// long as the file stays in the working set.
pub struct FileMeta {
    pub headers: Vec<HeaderCard>,
    /// The curated metadata summary (label/value pairs), the same fields the
    /// `fitz info` command reports, for the docked info panel.
    pub info: Vec<SummaryField>,
    /// Pixel statistics, one channel for grayscale/CFA, three (R, G, B) for an
    /// already-debayered RGB cube — straight from `Image::stats()`.
    pub stats: ImageStats,
    /// The frame's star metrics. `None` only when [`Image::detection_plane`]
    /// can't build a detection plane from this image.
    pub stars: Option<StarStats>,
}

impl FileMeta {
    /// Build the resident metadata from a decoded image. Runs on the worker
    /// thread; computed once per file for the life of the working set.
    pub fn build(img: &Image) -> Self {
        let headers = img.header.iter().map(header_card).collect();
        let info = libfitz::summary::info_summary(img);
        let stats = img.stats();
        let stars = img
            .detection_plane()
            .ok()
            .map(|plane| plane.detect_stars(&StarDetectOptions::default()));
        FileMeta {
            headers,
            info,
            stats,
            stars,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use libfitz::fitskit::Header;

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
    fn build_reports_per_channel_stats_for_a_debayered_rgb_cube() {
        use libfitz::data::{ImageType, PixelBuffer};

        // A 4x3 RGB cube with sequential planes: R = 0..=11, G = 12..=23,
        // B = 24..=35, interleaved (as `Image` always stores RGB in memory).
        let (w, h) = (4usize, 3usize);
        let n = w * h;
        let planar: Vec<u16> = (0..3 * n).map(|i| i as u16).collect();
        let interleaved: Vec<u16> = (0..n)
            .flat_map(|i| [planar[i], planar[n + i], planar[2 * n + i]])
            .collect();
        let img = Image::new(
            ImageType::RGB,
            Header::new(),
            w,
            h,
            PixelBuffer::U16(interleaved),
        );

        let meta = FileMeta::build(&img);
        assert_eq!(meta.stats.channels.len(), 3);
        assert_eq!(
            (meta.stats.channels[0].min, meta.stats.channels[0].max),
            (0, 11)
        );
        assert_eq!(
            (meta.stats.channels[1].min, meta.stats.channels[1].max),
            (12, 23)
        );
        assert_eq!(
            (meta.stats.channels[2].min, meta.stats.channels[2].max),
            (24, 35)
        );
    }

    #[test]
    fn build_reports_a_single_channel_for_grayscale() {
        use libfitz::data::{ImageType, PixelBuffer};

        let img = Image::new(
            ImageType::Grayscale,
            Header::new(),
            4,
            2,
            PixelBuffer::U16(vec![0; 8]),
        );
        let meta = FileMeta::build(&img);
        assert_eq!(meta.stats.channels.len(), 1);
    }
}
