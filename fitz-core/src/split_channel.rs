//! Debayer a FITS mosaic image (or reuse an already-debayered RGB cube) and
//! split it into three independent per-channel pixel buffers.

use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use fitskit::{FitsFile, Header};
use rayon::prelude::*;

use crate::fits_image::{
    RgbBuffer, demosaic_to_rgb, find_image_hdu, get_bayerpat, is_rgb_cube_shape, resolve_cfa,
    scaled_pixels,
};

/// Pixel sample type used when writing each split-out channel to FITS.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChannelFormat {
    I8,
    I16,
    I32,
    F32,
    F64,
}

impl FromStr for ChannelFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "i8" => Ok(ChannelFormat::I8),
            "i16" => Ok(ChannelFormat::I16),
            "i32" => Ok(ChannelFormat::I32),
            "f32" => Ok(ChannelFormat::F32),
            "f64" => Ok(ChannelFormat::F64),
            _ => Err("format must be one of: i8, i16, i32, f32, f64".to_string()),
        }
    }
}

/// Domain options controlling how an image is split into channels.
#[derive(Default)]
pub struct SplitChannelOptions {
    /// Bayer pattern override; takes precedence over the FITS headers.
    pub pattern: Option<bayer::CFA>,
    /// Always demosaic, even if the input looks like an already-debayered
    /// RGB image.
    pub force_demosaic: bool,
}

/// The three per-channel physical pixel buffers, plus the shared geometry and
/// source header a caller needs to write them out.
pub struct SplitChannels {
    pub width: usize,
    pub height: usize,
    pub header: Header,
    pub r: Vec<f64>,
    pub g: Vec<f64>,
    pub b: Vec<f64>,
}

/// Debayer (or reinterleave) `input` and split it into three independent
/// per-channel physical pixel buffers. Performs no path derivation or writing
/// — callers decide the output format/paths.
pub fn split_channels(input: &Path, opts: &SplitChannelOptions) -> Result<SplitChannels> {
    let fits =
        FitsFile::from_file(input).with_context(|| format!("cannot read {}", input.display()))?;

    let (header, img) = find_image_hdu(&fits, input)?;
    let img = img.as_ref();

    let try_demosaic = opts.force_demosaic || get_bayerpat(header).is_some();

    let (width, height, r, g, b) = if try_demosaic {
        if img.axes.len() != 2 {
            bail!("expected a 2D mosaic image, found {} axes", img.axes.len());
        }

        let cfa = resolve_cfa(header, opts.pattern)
            .with_context(|| "cannot determine Bayer pattern".to_string())?;

        let (width, height, rgb) =
            demosaic_to_rgb(header, img, cfa).with_context(|| "debayering failed".to_string())?;

        let (r, g, b) = deinterleave(rgb);
        (width, height, r, g, b)
    } else {
        if !is_rgb_cube_shape(img) {
            bail!("no BAYERPAT header and image is not a 3-plane RGB cube (NAXIS3=3)");
        }

        let width = img.axes[0];
        let height = img.axes[1];
        let scaled = scaled_pixels(header, img);

        let plane_len = width * height;
        let r = scaled[0..plane_len].to_vec();
        let g = scaled[plane_len..2 * plane_len].to_vec();
        let b = scaled[2 * plane_len..3 * plane_len].to_vec();
        (width, height, r, g, b)
    };

    Ok(SplitChannels {
        width,
        height,
        header: header.clone(),
        r,
        g,
        b,
    })
}

fn deinterleave(rgb: RgbBuffer) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
    match rgb {
        RgbBuffer::U8(v) => deinterleave_channels(&v),
        RgbBuffer::U16(v) => deinterleave_channels(&v),
    }
}

fn deinterleave_channels<T: Copy + Into<f64> + Send + Sync>(
    v: &[T],
) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
    let n = v.len() / 3;
    let r = (0..n).into_par_iter().map(|i| v[i * 3].into()).collect();
    let g = (0..n)
        .into_par_iter()
        .map(|i| v[i * 3 + 1].into())
        .collect();
    let b = (0..n)
        .into_par_iter()
        .map(|i| v[i * 3 + 2].into())
        .collect();
    (r, g, b)
}

/// Encode a single channel's physical pixel values into `PixelData` of the
/// requested [`ChannelFormat`], returning the pixels and the `BZERO` needed to
/// round-trip them (nonzero only for the unsigned-integer conventions).
pub fn encode_channel(values: &[f64], format: ChannelFormat) -> (fitskit::PixelData, f64) {
    use fitskit::PixelData;
    match format {
        ChannelFormat::I8 => (
            PixelData::U8(
                values
                    .par_iter()
                    .map(|&v| v.round().clamp(0.0, 255.0) as u8)
                    .collect(),
            ),
            0.0,
        ),
        ChannelFormat::I16 => (
            PixelData::I16(
                values
                    .par_iter()
                    .map(|&v| (v.round().clamp(0.0, 65535.0) - 32768.0) as i16)
                    .collect(),
            ),
            32768.0,
        ),
        ChannelFormat::I32 => (
            PixelData::I32(
                values
                    .par_iter()
                    .map(|&v| (v.round().clamp(0.0, 4294967295.0) - 2147483648.0) as i32)
                    .collect(),
            ),
            2147483648.0,
        ),
        ChannelFormat::F32 => (
            PixelData::F32(values.par_iter().map(|&v| v as f32).collect()),
            0.0,
        ),
        ChannelFormat::F64 => (PixelData::F64(values.to_vec()), 0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fits_image::CFA_KEYWORDS;
    use crate::test_support::{
        copy_to_temp, test_data, write_mosaic_fits, write_mosaic_fits_with_metadata,
        write_rgb_cube_fits,
    };
    use fitskit::HduData;
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    fn write_channel_fits(
        output: &Path,
        width: usize,
        height: usize,
        values: &[f64],
        format: ChannelFormat,
        src_header: &Header,
        channel: &str,
    ) {
        let (pixels, bzero) = encode_channel(values, format);
        let history = format!("split channel {channel} by fitz-core tests");
        crate::fits_image::write_pixel_fits(
            output,
            vec![width, height],
            pixels,
            1.0,
            bzero,
            Some(src_header),
            CFA_KEYWORDS,
            Some(&history),
        )
        .unwrap();
    }

    fn split_to_files(input: &Path, dir: &Path, format: ChannelFormat) {
        let s = split_channels(input, &SplitChannelOptions::default()).unwrap();
        let filename = input.file_name().unwrap();
        for (channel, values) in [("R", &s.r), ("G", &s.g), ("B", &s.b)] {
            let output = dir.join(format!("{channel}-{}", filename.to_str().unwrap()));
            write_channel_fits(
                &output, s.width, s.height, values, format, &s.header, channel,
            );
        }
    }

    #[test]
    fn split_channel_preserves_metadata_and_drops_bayerpat() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits_with_metadata(&input, 8, 6, Some("RGGB"));

        split_to_files(&input, tmp.path(), ChannelFormat::I16);

        for (channel, file) in [
            ("R", "R-raw.fits"),
            ("G", "G-raw.fits"),
            ("B", "B-raw.fits"),
        ] {
            let header = FitsFile::from_file(tmp.path().join(file))
                .unwrap()
                .primary()
                .header
                .clone();
            assert_eq!(header.get_string("OBJECT"), Some("M31"), "{channel}");
            assert_eq!(header.get_float("CRVAL1"), Some(10.68), "{channel}");
            assert!(header.find("BAYERPAT").is_none(), "{channel}");
        }
    }

    #[test]
    fn split_channel_fz_output_does_not_leak_container_keywords() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);

        split_to_files(&input, tmp.path(), ChannelFormat::I16);

        let header = FitsFile::from_file(tmp.path().join("R-compressed.fits.fz"))
            .unwrap()
            .primary()
            .header
            .clone();
        for kw in [
            "TFORM1", "TFIELDS", "ZIMAGE", "ZCMPTYPE", "ZNAXIS1", "XTENSION", "EXTNAME", "BAYERPAT",
        ] {
            assert!(header.find(kw).is_none(), "{kw} leaked into split output");
        }
    }

    #[test]
    fn split_channel_skips_debayer_for_existing_rgb_cube() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 4, 3);

        let s = split_channels(&input, &SplitChannelOptions::default()).unwrap();
        assert_eq!(s.r, (0..12).map(|x| x as f64).collect::<Vec<_>>());
    }

    #[test]
    fn split_channel_force_demosaic_rejects_3_plane_cube_instead_of_guessing() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 4, 3);

        let opts = SplitChannelOptions {
            force_demosaic: true,
            pattern: Some(bayer::CFA::RGGB),
        };
        let err = match split_channels(&input, &opts) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(err.to_string().contains("2D mosaic image"));
    }

    #[test]
    fn split_channel_errors_without_bayerpat_or_rgb_cube() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 4, 4, None);

        let err = match split_channels(&input, &SplitChannelOptions::default()) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(err.to_string().contains("3-plane RGB cube"));
    }

    #[test]
    fn split_channel_handles_tile_compressed_input() {
        // A compressed .fz input must be decompressed before debayering and
        // splitting into the three per-channel files.
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);

        let s = split_channels(&input, &SplitChannelOptions::default()).unwrap();
        assert_eq!(s.width, 3008);
        assert_eq!(s.height, 3008);
    }

    fn assert_split_channel_matches_known_hashes(format: ChannelFormat, suffix: &str) {
        let tmp = TempDir::new().unwrap();
        let input = test_data("uncompressed.fit");

        let s = split_channels(&input, &SplitChannelOptions::default()).unwrap();
        for (channel, values) in [("r", &s.r), ("g", &s.g), ("b", &s.b)] {
            let output = tmp.path().join(format!("{channel}.fits"));
            write_channel_fits(
                &output, s.width, s.height, values, format, &s.header, channel,
            );

            let hash_file = format!("split/uncompressed-{suffix}-{channel}.sha256");
            let expected = std::fs::read_to_string(test_data(&hash_file))
                .unwrap()
                .trim()
                .to_string();

            let fits = FitsFile::from_file(&output).unwrap();
            let (_, img) = find_image_hdu(&fits, &output).unwrap();
            let actual = format!("{:x}", Sha256::digest(img.pixels.to_bytes()));
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn split_channel_uncompressed_fit_i16_matches_known_hash() {
        assert_split_channel_matches_known_hashes(ChannelFormat::I16, "i16");
    }

    #[test]
    fn split_channel_reports_rgb_cube_shape() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 4, 3);
        let fits = FitsFile::from_file(&input).unwrap();
        let (_, img) = find_image_hdu(&fits, &input).unwrap();
        assert!(is_rgb_cube_shape(img.as_ref()));
    }

    #[test]
    fn split_channel_matches_hduimage() {
        // Sanity: HduData::Image is reachable from fitskit for callers building
        // their own assertions against split output.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 4, 3);
        let fits = FitsFile::from_file(&input).unwrap();
        assert!(matches!(fits.primary().data, HduData::Image(_)));
    }
}
