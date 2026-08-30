//! A loaded FITS document's resident metadata: the header cards, curated
//! info summary, pixel statistics and star metrics the Headers tab and stats
//! panel need. All of it is derived on the worker thread (where the decoded
//! `Image` is already in hand) and cached as one unit, so switching tabs or
//! toggling the stats panel never re-reads the file. For a CFA source the
//! statistics follow the debayer toggle — the mosaic and its demosaiced RGB
//! have different pixel statistics and star shapes — so the metadata records
//! which state it was built under ([`FileMeta::debayered`]) and the
//! controller caches it per state, recomputing only for a state it hasn't
//! measured yet.
//!
//! Kept free of Slint types so the whole thing is `Send` (it crosses from the
//! worker back to the UI thread) and the formatting logic stays unit-testable
//! without an event loop. Pixel statistics and star metrics are stored
//! exactly as `libfitz` computes them (`ImageStats`, `StarStats`) rather than
//! copied into a parallel struct — only the header cards are genuinely
//! reshaped for display.

use libfitz::data::Image;
use libfitz::fitskit::{HeaderValue, Keyword};
use libfitz::stars::{Star, StarDetectOptions, StarStats};
use libfitz::stats::ImageStats;
use libfitz::summary::SummaryField;

/// One FITS header card, pre-formatted into the three display columns.
pub struct HeaderCard {
    pub name: String,
    pub value: String,
    pub comment: String,
}

/// Everything the UI needs about one loaded file, cached as a unit and kept
/// resident for as long as the file stays in the working set. Independent of
/// the stretch toggle; for a CFA source it depends on the debayer toggle
/// ([`FileMeta::debayered`]) and is cached per state by the controller.
pub struct FileMeta {
    pub headers: Vec<HeaderCard>,
    /// The curated metadata summary (label/value pairs), the same fields the
    /// `fitz info` command reports, for the docked info panel.
    pub info: Vec<SummaryField>,
    /// Pixel statistics, one channel for grayscale/CFA, three (R, G, B) for an
    /// already-debayered RGB cube — straight from `Image::stats()`.
    pub stats: ImageStats,
    /// The frame's global percentile contrast — a single whole-frame value,
    /// unlike `stats`, straight from `Image::contrast()`.
    pub contrast: f32,
    /// The frame's star metrics. `None` only when [`Image::detection_plane`]
    /// can't build a detection plane from this image.
    pub stars: Option<StarStats>,
    /// Every detected star's centroid and shape, for the View ▸ Show stars
    /// overlay. Empty exactly when `stars` is `None` or reports zero stars.
    /// Derived from the same detection pass as `stars` (via
    /// [`libfitz::stars::aggregate`]) rather than a second one.
    pub star_list: Vec<Star>,
    /// The debayer toggle state this metadata was computed under, when it
    /// matters: `Some(state)` for a CFA source, `None` for anything else
    /// (where the toggle is a no-op and one measurement serves both states).
    pub debayered: Option<bool>,
}

impl FileMeta {
    /// Build the resident metadata from a decoded image — after the debayer
    /// stage, so a demosaiced frame's statistics describe what's on screen.
    /// `debayered` is the state to record: `Some(toggle)` when the source was
    /// a CFA mosaic, `None` otherwise. Runs on the worker thread.
    pub fn build(img: &Image, debayered: Option<bool>) -> Self {
        let headers = img.header.iter().map(header_card).collect();
        let info = libfitz::summary::info_summary(img);
        let stats = img.stats();
        let contrast = img.contrast()*100.0;
        let star_list = img.star_list(&StarDetectOptions::default()).ok();
        let stars = star_list.as_ref().map(|s| libfitz::stars::aggregate(s));
        FileMeta {
            headers,
            info,
            stats,
            contrast,
            stars,
            star_list: star_list.unwrap_or_default(),
            debayered,
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

        let meta = FileMeta::build(&img, None);
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
        let meta = FileMeta::build(&img, None);
        assert_eq!(meta.stats.channels.len(), 1);
    }

    #[test]
    fn build_computes_a_finite_contrast() {
        let raw =
            libfitz::fits_file::load_fits(&crate::controller::test_data("cfa_orion.fits")).unwrap();
        let meta = FileMeta::build(&raw, Some(false));
        assert!(meta.contrast.is_finite());
    }

    #[test]
    fn cfa_meta_differs_per_debayer_state() {
        let raw =
            libfitz::fits_file::load_fits(&crate::controller::test_data("cfa_orion.fits")).unwrap();

        // Built from the mosaic itself: one channel, recorded as the
        // toggle-off measurement.
        let mosaic = FileMeta::build(&raw, Some(false));
        assert_eq!(mosaic.stats.channels.len(), 1);
        assert_eq!(mosaic.debayered, Some(false));

        // Built from the demosaiced frame: three channels, recorded as the
        // toggle-on measurement.
        let rgb_img = raw.debayer().unwrap().unwrap();
        let rgb = FileMeta::build(&rgb_img, Some(true));
        assert_eq!(rgb.stats.channels.len(), 3);
        assert_eq!(rgb.debayered, Some(true));
    }
}
