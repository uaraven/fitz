//! Curated FITS header summary: the label/value fields both `fitz info` and
//! the `fitsmith` GUI show for a loaded image — resolution, bit depth,
//! channel count, sky coordinates, telescope, exposure, and the rest of the
//! commonly useful header keywords, each shown only when the header actually
//! carries it.

use crate::data::{Image, ImageType, PixelBuffer};
use fitskit::Header;

/// One labeled field in [`info_summary`]'s report — a display label and its
/// already-formatted value (e.g. `"Resolution"` / `"3008 x 3008"`).
pub struct SummaryField {
    pub label: &'static str,
    pub value: String,
}

/// A curated, ordered list of the most useful header fields as label/value
/// pairs. Resolution, bit depth and channels are always present; every other
/// field appears only when the header carries it (and is non-blank). Pixel
/// statistics are deliberately excluded — callers report those separately.
pub fn info_summary(image: &Image) -> Vec<SummaryField> {
    let header = &image.header;
    let mut fields = Vec::new();

    push(
        &mut fields,
        "Resolution",
        format!("{} x {}", image.width, image.height),
    );
    push(&mut fields, "Bit depth", bit_depth_label(image));
    push(
        &mut fields,
        "Channels",
        format!("{} ({})", image.channels(), channel_label(image)),
    );
    push_str(&mut fields, "Bayer", header.get_string("BAYERPAT"));
    push_str(&mut fields, "Object", header.get_string("OBJECT"));
    push_coordinate(
        &mut fields,
        Axis::Ra,
        header.get_float("OBJCTRA"),
        header.get_string("OBJCTRA"),
    );
    push_coordinate(
        &mut fields,
        Axis::Dec,
        header.get_float("OBJCTDEC"),
        header.get_string("OBJCTDEC"),
    );
    if let Some(rot) = header.get_float("OBJCTROT") {
        push(&mut fields, "Rotation", format!("{}°", trim(rot)));
    }
    if let Some(exptime) = header.get_float("EXPTIME") {
        push(&mut fields, "Exposure", format!("{} s", trim(exptime)));
    }
    if let Some(gain) = header.get_float("GAIN") {
        push(&mut fields, "Gain", trim(gain));
    }
    if let Some(offset) = header.get_float("OFFSET") {
        push(&mut fields, "Offset", trim(offset));
    }
    if let Some((xbin, ybin)) = header.get_int("XBINNING").zip(header.get_int("YBINNING")) {
        push(&mut fields, "Binning", format!("{xbin}x{ybin}"));
    }
    push_str(&mut fields, "Filter", header.get_string("FILTER"));
    push_str(&mut fields, "Instrument", header.get_string("INSTRUME"));
    if let Some(telescope) = telescope_label(header) {
        push(&mut fields, "Telescope", telescope);
    }
    push_str(&mut fields, "Date-obs", header.get_string("DATE-OBS"));

    fields
}

/// Append a field with an already-formatted value.
fn push(fields: &mut Vec<SummaryField>, label: &'static str, value: String) {
    fields.push(SummaryField { label, value });
}

/// Append a string field only when present and non-blank once trimmed.
fn push_str(fields: &mut Vec<SummaryField>, label: &'static str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|s| !s.is_empty()) {
        push(fields, label, value.to_string());
    }
}

/// Describe the pixel storage format from the decoded buffer's own type,
/// which is correct for a tile-compressed source too — unlike the header's
/// own `BITPIX`, which for a compressed HDU describes its binary table, not
/// the image.
fn bit_depth_label(image: &Image) -> String {
    match &image.pixels {
        PixelBuffer::U16(_) => "16-bit unsigned integer".to_string(),
        PixelBuffer::F32(_) => "32-bit float".to_string(),
    }
}

/// Describe the channel layout. The Bayer pattern itself is reported on its
/// own `Bayer` field, so the raw-mosaic case just notes that it is a mosaic.
fn channel_label(image: &Image) -> &'static str {
    match image.image_type {
        ImageType::RGB => "debayered RGB",
        ImageType::CFA(_) => "mosaic",
        ImageType::Grayscale => "monochrome (debayered)",
    }
}

/// Describe the imaging telescope: its name (`TELESCOP`) optionally followed by
/// its optical figure derived from focal length (`FOCALLEN`, mm) and focal ratio
/// (`FOCRATIO`), e.g. `My Scope (203mm F/4.5)`. Returns `None` when no telescope
/// keyword carries usable information.
fn telescope_label(header: &Header) -> Option<String> {
    let name = header
        .get_string("TELESCOP")
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut optics = String::new();
    if let Some(focal) = header.get_float("FOCALLEN") {
        optics.push_str(&format!("{}mm", trim(focal)));
    }
    if let Some(ratio) = header.get_float("FOCRATIO") {
        if !optics.is_empty() {
            optics.push(' ');
        }
        optics.push_str(&format!("F/{}", trim(ratio)));
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
            format!("{sign}{h}h {m:02}m {s:05.2}s ({}h)", trim(hours))
        }
        Axis::Dec => {
            let (d, m, s) = to_sexagesimal(deg.abs());
            let sign = if deg < 0.0 { "-" } else { "" };
            format!("{sign}{d}° {m:02}' {s:05.2}\" ({}°)", trim(deg))
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
/// representation otherwise.
fn trim(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::PixelBuffer;
    use bayer::CFA;
    use fitskit::HeaderValue;

    /// Build a minimal grayscale [`Image`] with an otherwise-empty header, for
    /// tests that only care about the header-driven fields of [`info_summary`].
    fn image_with_header(header: Header) -> Image {
        Image::new(
            ImageType::Grayscale,
            header,
            4,
            2,
            PixelBuffer::U16(vec![0; 8]),
        )
    }

    #[test]
    fn info_summary_always_reports_resolution_bit_depth_and_channels() {
        // With an otherwise-empty header, only the three always-present fields
        // show up — every other field is conditional on a header keyword.
        let image = image_with_header(Header::new());
        let fields = info_summary(&image);
        let labels: Vec<&str> = fields.iter().map(|f| f.label).collect();
        assert_eq!(labels, vec!["Resolution", "Bit depth", "Channels"]);
        assert_eq!(fields[0].value, "4 x 2");
        assert_eq!(fields[1].value, "16-bit unsigned integer");
        assert_eq!(fields[2].value, "1 (monochrome (debayered))");
    }

    #[test]
    fn info_summary_includes_present_header_fields() {
        let mut header = Header::new();
        header.set("OBJECT", HeaderValue::String("M31".to_string()), None);
        header.set("BAYERPAT", HeaderValue::String("RGGB".to_string()), None);
        header.set("OBJCTROT", HeaderValue::Float(90.0), None);
        header.set("EXPTIME", HeaderValue::Float(30.0), None);
        header.set("GAIN", HeaderValue::Float(100.0), None);
        header.set("OFFSET", HeaderValue::Float(10.0), None);
        header.set("XBINNING", HeaderValue::Integer(2), None);
        header.set("YBINNING", HeaderValue::Integer(2), None);
        header.set("FILTER", HeaderValue::String("L".to_string()), None);
        header.set("INSTRUME", HeaderValue::String("ZWO".to_string()), None);
        header.set(
            "DATE-OBS",
            HeaderValue::String("2026-06-22".to_string()),
            None,
        );

        let image = image_with_header(header);
        let fields = info_summary(&image);
        let find = |label: &str| {
            fields
                .iter()
                .find(|f| f.label == label)
                .map(|f| f.value.as_str())
        };

        assert_eq!(find("Object"), Some("M31"));
        assert_eq!(find("Bayer"), Some("RGGB"));
        assert_eq!(find("Rotation"), Some("90°"));
        assert_eq!(find("Exposure"), Some("30 s"));
        assert_eq!(find("Gain"), Some("100"));
        assert_eq!(find("Offset"), Some("10"));
        assert_eq!(find("Binning"), Some("2x2"));
        assert_eq!(find("Filter"), Some("L"));
        assert_eq!(find("Instrument"), Some("ZWO"));
        assert_eq!(find("Date-obs"), Some("2026-06-22"));
    }

    #[test]
    fn info_summary_omits_blank_string_fields() {
        // A present-but-whitespace-only keyword must not produce an empty row.
        let mut header = Header::new();
        header.set("OBJECT", HeaderValue::String("   ".to_string()), None);
        let image = image_with_header(header);
        let fields = info_summary(&image);
        assert!(fields.iter().all(|f| f.label != "Object"));
    }

    #[test]
    fn bit_depth_label_reports_buffer_type() {
        let u16_image = image_with_header(Header::new());
        assert_eq!(bit_depth_label(&u16_image), "16-bit unsigned integer");

        let f32_image = Image::new(
            ImageType::Grayscale,
            Header::new(),
            1,
            1,
            PixelBuffer::F32(vec![0.0]),
        );
        assert_eq!(bit_depth_label(&f32_image), "32-bit float");
    }

    #[test]
    fn channel_label_matches_image_type() {
        let make = |t: ImageType| Image::new(t, Header::new(), 1, 1, PixelBuffer::U16(vec![0]));
        assert_eq!(channel_label(&make(ImageType::RGB)), "debayered RGB");
        assert_eq!(channel_label(&make(ImageType::CFA(CFA::RGGB))), "mosaic");
        assert_eq!(
            channel_label(&make(ImageType::Grayscale)),
            "monochrome (debayered)"
        );
    }

    #[test]
    fn telescope_label_combines_name_and_optics() {
        let mut header = Header::new();
        header.set(
            "TELESCOP",
            HeaderValue::String("My Scope".to_string()),
            None,
        );
        header.set("FOCALLEN", HeaderValue::Float(203.0), None);
        header.set("FOCRATIO", HeaderValue::Float(4.5), None);
        assert_eq!(
            telescope_label(&header),
            Some("My Scope (203mm F/4.5)".to_string())
        );
    }

    #[test]
    fn telescope_label_falls_back_to_name_or_optics_alone() {
        let mut name_only = Header::new();
        name_only.set(
            "TELESCOP",
            HeaderValue::String("My Scope".to_string()),
            None,
        );
        assert_eq!(telescope_label(&name_only), Some("My Scope".to_string()));

        let mut optics_only = Header::new();
        optics_only.set("FOCALLEN", HeaderValue::Float(203.0), None);
        assert_eq!(telescope_label(&optics_only), Some("203mm".to_string()));

        assert_eq!(telescope_label(&Header::new()), None);
    }

    #[test]
    fn push_coordinate_prefers_decimal_over_sexagesimal_string() {
        let mut fields = Vec::new();
        // RA 307.5° = 20.5h -> 20h 30m 00.00s.
        push_coordinate(
            &mut fields,
            Axis::Ra,
            Some(307.5),
            Some("ignored raw string"),
        );
        assert_eq!(fields[0].label, "RA");
        assert_eq!(fields[0].value, "20h 30m 00.00s (20.5h)");
    }

    #[test]
    fn push_coordinate_falls_back_to_raw_string_without_decimal() {
        let mut fields = Vec::new();
        push_coordinate(&mut fields, Axis::Dec, None, Some(" -12 30 00 "));
        assert_eq!(fields[0].value, "-12 30 00");
    }

    #[test]
    fn push_coordinate_omits_field_when_absent() {
        let mut fields = Vec::new();
        push_coordinate(&mut fields, Axis::Ra, None, None);
        assert!(fields.is_empty());
    }

    #[test]
    fn format_coordinate_renders_ra_and_dec() {
        assert_eq!(format_coordinate(Axis::Ra, 307.5), "20h 30m 00.00s (20.5h)");
        assert_eq!(
            format_coordinate(Axis::Dec, -12.5),
            "-12° 30' 00.00\" (-12.5°)"
        );
    }

    #[test]
    fn to_sexagesimal_splits_and_carries_rounding() {
        assert_eq!(to_sexagesimal(20.5), (20, 30, 0.0));
        // 59.9999 seconds rounds up to 60.00 and carries into the minute.
        let (h, m, s) = to_sexagesimal(1.0 + 59.9999 / 3600.0);
        assert_eq!((h, m), (1, 1));
        assert!(s.abs() < 1e-9);
    }

    #[test]
    fn trim_drops_trailing_zeros() {
        assert_eq!(trim(3.0), "3");
        assert_eq!(trim(3.5), "3.5");
        assert_eq!(trim(3.125), "3.125");
        assert_eq!(trim(-4.0), "-4");
    }
}
