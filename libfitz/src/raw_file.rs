use crate::data::{Image, ImageType, PixelBuffer};
use crate::fits_bayer::cfa_str;
use crate::keywords::{BAYERPAT, add_history};
use crate::xtrans;
use anyhow::{Result, bail};
use bayer::CFA;
use fitskit::{Header, HeaderValue};
use rayon::prelude::*;
use rsraw::{FullRawInfo, GpsInfo, RawImage};
use rsraw_sys as sys;
use std::path::Path;

/// Per-raw-colour-index (`0..=3`, the same index `libraw_COLOR`/`idata.xtrans`
/// cells use, *before* it's resolved into an `R`/`G`/`B` letter via `cdesc`)
/// black level, falling back to the scalar `black` when the camera didn't
/// populate the per-channel array.
fn channel_black_levels(color: &sys::libraw_colordata_t) -> [u32; 4] {
    if color.cblack[0..4].iter().all(|&b| b == 0) {
        [color.black; 4]
    } else {
        [
            color.cblack[0],
            color.cblack[1],
            color.cblack[2],
            color.cblack[3],
        ]
    }
}

/// Subtract `black` and rescale so `white` maps to 65535, clamping both ends.
/// A camera's raw ADU counts sit on a non-zero pedestal (`black`) and rarely
/// fill the full 16-bit container (`white` is the sensor's real saturation
/// point) — skipping this leaves every downstream consumer (stats, stretch,
/// demosaic) working against the wrong dynamic range.
fn normalize_sample(raw: u16, black: u32, white: u32) -> u16 {
    let span = white.saturating_sub(black).max(1);
    let v = (raw as u32).saturating_sub(black).min(span);
    (v as u64 * 65535 / span as u64) as u16
}

pub fn load_raw_image(source: &Path) -> Result<Image> {
    let data = std::fs::read(source)?;
    let mut raw_image = RawImage::open(&data)?;
    raw_image.unpack()?;

    let raw_data_ref: &sys::libraw_data_t = raw_image.as_ref();
    let sizes = &raw_data_ref.sizes;
    let rd = &raw_data_ref.rawdata;

    // Only true single-plane CFA data lives in raw_image.
    if rd.raw_image.is_null() || raw_data_ref.idata.filters == 0 {
        bail!("Unsupported RAW file: not a Bayer CFA raw");
    }
    // if raw_data_ref.idata.filters < 1000 {
    //     bail!("Unsupported RAW file: non-2x2 CFA");
    // }

    let (w, h) = (sizes.width as usize, sizes.height as usize);
    let (top, left) = (sizes.top_margin as usize, sizes.left_margin as usize);
    let pitch = sizes.raw_pitch as usize / 2; // raw_pitch is in bytes
    let full = unsafe {
        std::slice::from_raw_parts(
            rd.raw_image as *const u16,
            pitch * sizes.raw_height as usize,
        )
    };

    let black_levels = channel_black_levels(&rd.color);
    let white = if rd.color.maximum != 0 {
        rd.color.maximum
    } else {
        rd.color.data_maximum
    };

    // Sensor type, colour pattern, and the per-position black level (indexed
    // the same way as the pattern itself, so both a 2x2 CFA phase and a 6x6
    // X-Trans phase are expressed as one 6x6 table — a 2x2 pattern repeats
    // exactly across a 6-cell period too).
    let (image_type, black_by_position): (ImageType, [[u32; 6]; 6]) =
        if raw_data_ref.idata.filters == 9 {
            let table: [[i8; 6]; 6] = raw_data_ref.idata.xtrans;
            let black6x6 = table.map(|row| row.map(|idx| black_levels[idx as usize]));
            (
                ImageType::XTrans(xtrans::pattern_from_libraw(table, raw_data_ref.idata.cdesc)?),
                black6x6,
            )
        } else if raw_data_ref.idata.filters >= 1000 {
            // CFA pattern at the visible origin; libraw_COLOR uses visible-area coordinates.
            let p = raw_data_ref as *const _ as *mut sys::libraw_data_t;
            let idx_at = |r, c| unsafe { sys::libraw_COLOR(p, r, c) } as usize;
            let idxs = [[idx_at(0, 0), idx_at(0, 1)], [idx_at(1, 0), idx_at(1, 1)]];
            let chars = idxs.map(|row| row.map(|idx| raw_data_ref.idata.cdesc[idx] as u8 as char)); // cdesc is e.g. "RGBG"
            let pattern = match [chars[0][0], chars[0][1], chars[1][0], chars[1][1]] {
                ['R', 'G', 'G', 'B'] => CFA::RGGB,
                ['B', 'G', 'G', 'R'] => CFA::BGGR,
                ['G', 'R', 'B', 'G'] => CFA::GRBG,
                ['G', 'B', 'R', 'G'] => CFA::GBRG,
                other => bail!("Unsupported CFA pattern: {:?}", other),
            };
            let black2x2 = idxs.map(|row| row.map(|idx| black_levels[idx]));
            let black6x6 = std::array::from_fn(|r: usize| {
                std::array::from_fn(|c: usize| black2x2[r % 2][c % 2])
            });
            (ImageType::CFA(pattern), black6x6)
        } else {
            bail!("Unsupported RAW file: non-2x2 CFA");
        };

    // Crop the visible area out of the full sensor buffer (drops masked
    // border pixels), subtracting each pixel's own black level and rescaling
    // so the sensor's saturation point maps to 65535 in the same pass.
    let data: Vec<u16> = (0..h)
        .into_par_iter()
        .flat_map_iter(|y| {
            let off = (y + top) * pitch + left;
            let row = &full[off..off + w];
            let black_row = &black_by_position[y % 6];
            (0..w).map(move |x| normalize_sample(row[x], black_row[x % 6], white))
        })
        .collect();

    let headers = metadata_to_headers(raw_image.full_info(), image_type, w, h);

    Ok(Image::new(
        image_type,
        headers,
        w,
        h,
        PixelBuffer::U16(data),
    ))
}

/// Build a FITS header from a RAW file's embedded metadata: the Bayer
/// pattern, whatever EXIF-derived acquisition/optics/site fields LibRaw
/// managed to parse, and a HISTORY card noting the RAW->FITS conversion.
/// Structural keywords (`NAXIS*`, `BITPIX`, …) are not written here — they
/// are regenerated by the FITS writer from the `Image` itself.
fn metadata_to_headers(raw_info: FullRawInfo, image_type: ImageType, w: usize, h: usize) -> Header {
    let mut header = Header::new();
    let cfa = match image_type {
        ImageType::XTrans(pattern) => Some(HeaderValue::String(xtrans::pattern_str(&pattern))),
        ImageType::CFA(pattern) => Some(HeaderValue::String(cfa_str(pattern).to_string())),
        _ => None,
    };

    if let Some(pattern) = cfa {
        header.set(
            BAYERPAT,
            pattern,
            Some("Bayer color pattern"),
        );
    }

    if let Some(datetime) = raw_info.datetime {
        header.set(
            "DATE-OBS",
            HeaderValue::String(datetime.format("%Y-%m-%dT%H:%M:%S").to_string()),
            Some("Date/time of observation, UT"),
        );
    }

    if raw_info.shutter > 0.0 {
        header.set(
            "EXPTIME",
            HeaderValue::Float(raw_info.shutter as f64),
            Some("[s] Exposure duration"),
        );
    }

    if raw_info.iso_speed > 0 {
        header.set(
            "ISOSPEED",
            HeaderValue::Integer(raw_info.iso_speed as i64),
            Some("ISO camera sensitivity"),
        );
    }

    if raw_info.aperture > 0.0 {
        header.set(
            "FOCRATIO",
            HeaderValue::Float(raw_info.aperture as f64),
            Some("Focal ratio (f-number)"),
        );
    }

    if raw_info.focal_len > 0.0 {
        header.set(
            "FOCALLEN",
            HeaderValue::Float(raw_info.focal_len as f64),
            Some("[mm] Focal length"),
        );
    }

    if let Some(camera) = non_empty(&join_words(&[
        &raw_info.normalized_make,
        &raw_info.normalized_model,
    ])) {
        header.set(
            "INSTRUME",
            HeaderValue::String(camera),
            Some("Camera make and model"),
        );
    }

    if let Some(lens) = non_empty(&raw_info.lens_info.lens_name) {
        header.set("TELESCOP", HeaderValue::String(lens), Some("Lens used"));
    }

    if let Some(artist) = non_empty(&raw_info.artist) {
        header.set("OBSERVER", HeaderValue::String(artist), None);
    }

    if let Some(object) = non_empty(&raw_info.desc) {
        header.set("OBJECT", HeaderValue::String(object), None);
    }

    if let Some(software) = non_empty(&raw_info.software) {
        header.set(
            "SWCREATE",
            HeaderValue::String(software),
            Some("Camera firmware/software"),
        );
    }

    if raw_info.gps != GpsInfo::default() {
        header.set(
            "SITELAT",
            HeaderValue::Float(dms_to_decimal(raw_info.gps.latitude) as f64),
            Some("[deg] Observation site latitude"),
        );
        header.set(
            "SITELONG",
            HeaderValue::Float(dms_to_decimal(raw_info.gps.longitude) as f64),
            Some("[deg] Observation site longitude"),
        );
        if raw_info.gps.altitude != 0.0 {
            header.set(
                "SITEELEV",
                HeaderValue::Float(raw_info.gps.altitude as f64),
                Some("[m] Observation site elevation"),
            );
        }
    }

    add_history(
        &mut header,
        &format!("Converted from RAW file ({w}x{h} sensor crop)"),
    );
    add_history(
        &mut header,
        "Black level subtracted, white point scaled to full range",
    );

    header
}

/// Join non-blank, trimmed words with a single space (e.g. camera make +
/// model), skipping any that are empty.
fn join_words(words: &[&str]) -> String {
    words
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `Some(trimmed)` unless `s` is empty once trimmed — RAW/EXIF string fields
/// come back as `""` rather than `None` when LibRaw found nothing.
fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Convert a `[degrees, minutes, seconds]` GPS coordinate (LibRaw's
/// convention: sign folded into every component, all matching) to decimal
/// degrees.
fn dms_to_decimal(dms: [f32; 3]) -> f32 {
    let sign = if dms.iter().any(|&v| v < 0.0) {
        -1.0
    } else {
        1.0
    };
    sign * (dms[0].abs() + dms[1].abs() / 60.0 + dms[2].abs() / 3600.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};
    use rsraw::{FocusType, LensInfo};

    fn lens_info(lens_name: &str) -> LensInfo {
        LensInfo {
            min_focal: 105.0,
            max_focal: 105.0,
            max_aperture_at_min_focal: 2.8,
            max_aperture_at_max_focal: 2.8,
            lens_make: "NIKON".into(),
            lens_name: lens_name.into(),
            lens_serial: "".into(),
            internal_lens_serial: "".into(),
            focal_length_in_35mm_format: 105,
            mounts: "".into(),
            focus_type: FocusType::Prime,
            feture_pre: "".into(),
            feture_suf: "".into(),
        }
    }

    fn full_raw_info() -> FullRawInfo {
        FullRawInfo {
            width: 8280,
            height: 5520,
            colors: 3,
            iso_speed: 250,
            shutter: 1.0 / 100.0,
            aperture: 3.5,
            focal_len: 105.0,
            datetime: Local.with_ymd_and_hms(2024, 11, 4, 20, 11, 38).single(),
            gps: GpsInfo {
                latitude: [47.0, 36.0, 22.0],
                longitude: [-122.0, -19.0, -55.0],
                gpstimestamp: [12.0, 30.0, 45.0],
                altitude: 56.0,
            },
            artist: "HEXILEE".into(),
            desc: "M31".into(),
            make: "Nikon".into(),
            model: "Z 8".into(),
            normalized_make: "Nikon".into(),
            normalized_model: "Z 8".into(),
            software: "Ver.02.00".into(),
            raw_count: 1,
            dng_version: 0,
            lens_info: lens_info("NIKKOR Z MC 105mm f/2.8 VR S"),
        }
    }

    #[test]
    fn metadata_to_headers_fills_in_known_fields() {
        let header = metadata_to_headers(full_raw_info(), ImageType::CFA(CFA::RGGB), 8280, 5520);

        assert_eq!(header.get_string(BAYERPAT), Some("RGGB"));
        assert_eq!(header.get_string("DATE-OBS"), Some("2024-11-04T20:11:38"));
        assert!((header.get_float("EXPTIME").unwrap() - 1.0 / 100.0).abs() < 1e-6);
        assert_eq!(header.get_int("ISOSPEED"), Some(250));
        assert_eq!(header.get_float("FOCRATIO"), Some(3.5));
        assert_eq!(header.get_float("FOCALLEN"), Some(105.0));
        assert_eq!(header.get_string("INSTRUME"), Some("Nikon Z 8"));
        assert_eq!(
            header.get_string("TELESCOP"),
            Some("NIKKOR Z MC 105mm f/2.8 VR S")
        );
        assert_eq!(header.get_string("OBSERVER"), Some("HEXILEE"));
        assert_eq!(header.get_string("OBJECT"), Some("M31"));
        assert_eq!(header.get_string("SWCREATE"), Some("Ver.02.00"));
        assert!((header.get_float("SITELAT").unwrap() - 47.606_11).abs() < 1e-4);
        assert!((header.get_float("SITELONG").unwrap() - -122.331_95).abs() < 1e-4);
        assert_eq!(header.get_float("SITEELEV"), Some(56.0));
        assert!(
            header
                .find("HISTORY")
                .and_then(|k| k.comment.as_deref())
                .is_some_and(|c| c.contains("8280x5520"))
        );
    }

    #[test]
    fn metadata_to_headers_writes_the_xtrans_pattern_string_as_bayerpat() {
        use crate::xtrans::parse_pattern;

        const PATTERN: &str = "GGRGGBGGBGGRBRGRBGGGBGGRGGRGGBRBGBRG";
        let pattern = parse_pattern(PATTERN).unwrap();
        let header = metadata_to_headers(full_raw_info(), ImageType::XTrans(pattern), 6032, 4028);

        assert_eq!(header.get_string(BAYERPAT), Some(PATTERN));
    }

    #[test]
    fn metadata_to_headers_omits_unavailable_fields() {
        let mut raw_info = full_raw_info();
        raw_info.datetime = None;
        raw_info.gps = GpsInfo::default();
        raw_info.artist = "".into();
        raw_info.desc = "  ".into();
        raw_info.software = "".into();
        raw_info.normalized_make = "".into();
        raw_info.normalized_model = "".into();
        raw_info.lens_info.lens_name = "".into();

        let header = metadata_to_headers(raw_info, ImageType::CFA(CFA::BGGR), 100, 100);

        assert_eq!(header.get_string("DATE-OBS"), None);
        assert_eq!(header.get_string("SITELAT"), None);
        assert_eq!(header.get_string("SITELONG"), None);
        assert_eq!(header.get_string("SITEELEV"), None);
        assert_eq!(header.get_string("OBSERVER"), None);
        assert_eq!(header.get_string("OBJECT"), None);
        assert_eq!(header.get_string("SWCREATE"), None);
        assert_eq!(header.get_string("INSTRUME"), None);
        assert_eq!(header.get_string("TELESCOP"), None);
        // Bayer pattern is always known, regardless of what EXIF carried.
        assert_eq!(header.get_string(BAYERPAT), Some("BGGR"));
    }

    #[test]
    fn dms_to_decimal_handles_negative_and_positive_coordinates() {
        assert!((dms_to_decimal([47.0, 36.0, 22.0]) - 47.606_11).abs() < 1e-4);
        assert!((dms_to_decimal([-122.0, -19.0, -55.0]) - -122.331_95).abs() < 1e-4);
    }

    #[test]
    fn channel_black_levels_reads_the_per_channel_array() {
        let mut color: sys::libraw_colordata_t = unsafe { std::mem::zeroed() };
        color.cblack[0] = 128;
        color.cblack[1] = 130;
        color.cblack[2] = 132;
        color.cblack[3] = 130;
        assert_eq!(channel_black_levels(&color), [128, 130, 132, 130]);
    }

    #[test]
    fn channel_black_levels_falls_back_to_the_scalar_black() {
        let mut color: sys::libraw_colordata_t = unsafe { std::mem::zeroed() };
        color.black = 64;
        assert_eq!(channel_black_levels(&color), [64, 64, 64, 64]);
    }

    #[test]
    fn normalize_sample_clamps_and_scales_linearly() {
        assert_eq!(normalize_sample(0, 512, 16383), 0);
        assert_eq!(normalize_sample(512, 512, 16383), 0);
        assert_eq!(normalize_sample(16383, 512, 16383), 65535);
        // Above the sensor's declared saturation point: still clamps, doesn't overflow/wrap.
        assert_eq!(normalize_sample(u16::MAX, 512, 16383), 65535);

        let mid = 512 + (16383 - 512) / 2;
        let expected = ((mid - 512) as u64 * 65535 / (16383 - 512) as u64) as u16;
        assert_eq!(normalize_sample(mid, 512, 16383), expected);
    }

    #[test]
    fn normalize_sample_does_not_divide_by_zero_on_a_degenerate_span() {
        // A white point at or below black shouldn't happen in practice (LibRaw
        // always derives `maximum` from the bit depth), but must not panic.
        assert_eq!(normalize_sample(1000, 2000, 1000), 0);
    }
}
