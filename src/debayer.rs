use std::fs::File;
use std::path::Path;

use anyhow::{bail, Context, Result};
use fitskit::{Bitpix, FitsFile, Header, ImageData, PixelData};
use tiff::encoder::{colortype, TiffEncoder};

use crate::fits_image::{
    bscale_bzero, demosaic_to_rgb, ensure_can_write, find_image_hdu, get_bayerpat, print_progress,
    resolve_cfa, round_to_u16, scaled_pixels, write_pixel_fits, RgbBuffer,
};
use crate::options::DebayerOptions;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Tiff,
    Fits,
}

impl OutputFormat {
    pub fn extension(self) -> &'static str {
        match self {
            OutputFormat::Tiff => "tiff",
            OutputFormat::Fits => "fits",
        }
    }
}

pub fn parse_output_format(s: &str) -> Result<OutputFormat, String> {
    match s.to_ascii_lowercase().as_str() {
        "tiff" => Ok(OutputFormat::Tiff),
        "fits" => Ok(OutputFormat::Fits),
        _ => Err("format must be one of: TIFF, FITS".to_string()),
    }
}

enum OutputSamples {
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
}

/// The pixel storage format of a source FITS image, captured so a debayered
/// FITS can be written back using the same BITPIX and BSCALE/BZERO scaling
/// instead of a fixed bit depth.
struct SourceFormat {
    bitpix: Bitpix,
    bscale: f64,
    bzero: f64,
}

impl SourceFormat {
    fn from_image(header: &Header, img: &ImageData) -> Self {
        let (bscale, bzero) = bscale_bzero(header);
        SourceFormat {
            bitpix: img.pixels.bitpix(),
            bscale,
            bzero,
        }
    }
}

pub fn debayer_file(input: &Path, output: &Path, opts: &DebayerOptions) -> Result<()> {
    ensure_can_write(output, opts.force)?;
    print_progress(opts.verbose, input, output);

    let fits =
        FitsFile::from_file(input).with_context(|| format!("cannot read {}", input.display()))?;

    let (header, img) = find_image_hdu(&fits, input)?;

    let source = SourceFormat::from_image(header, img);

    let already_debayered = !opts.force_demosaic
        && get_bayerpat(header).is_none()
        && img.axes.len() == 3
        && img.axes[2] == 3;

    let (width, height, rgb) = if already_debayered {
        println!(
            "{}: already debayered (no BAYERPAT header, found a 3-plane RGB cube) — skipping debayer step",
            input.display()
        );
        let width = img.axes[0];
        let height = img.axes[1];
        let rgb = rgb_from_cube(header, img, width, height);
        (width, height, rgb)
    } else {
        if img.axes.len() != 2 {
            bail!(
                "{}: expected a 2D mosaic image, found {} axes",
                input.display(),
                img.axes.len()
            );
        }

        let cfa = resolve_cfa(header, opts.pattern)
            .with_context(|| format!("{}: cannot determine Bayer pattern", input.display()))?;

        demosaic_to_rgb(header, img, cfa)
            .with_context(|| format!("{}: debayering failed", input.display()))?
    };

    match opts.format {
        OutputFormat::Tiff => {
            let samples = to_output_samples(rgb, opts.bpp);
            write_tiff(output, width, height, samples)?;
        }
        OutputFormat::Fits => write_fits(output, width, height, rgb, &source)?,
    }

    Ok(())
}

/// Interleave an already-debayered 3-plane RGB cube into an `RgbBuffer`,
/// without running it through the demosaic algorithm.
fn rgb_from_cube(header: &Header, img: &ImageData, width: usize, height: usize) -> RgbBuffer {
    let plane_len = width * height;

    if let PixelData::U8(v) = &img.pixels {
        let mut out = vec![0u8; plane_len * 3];
        for i in 0..plane_len {
            out[i * 3] = v[i];
            out[i * 3 + 1] = v[plane_len + i];
            out[i * 3 + 2] = v[2 * plane_len + i];
        }
        return RgbBuffer::U8(out);
    }

    let scaled = scaled_pixels(header, img);

    let mut out = vec![0u16; plane_len * 3];
    for i in 0..plane_len {
        out[i * 3] = round_to_u16(scaled[i]);
        out[i * 3 + 1] = round_to_u16(scaled[plane_len + i]);
        out[i * 3 + 2] = round_to_u16(scaled[2 * plane_len + i]);
    }
    RgbBuffer::U16(out)
}

/// Scale a demosaiced RGB buffer to the requested output bit depth by bit
/// replication (promoting) or truncation (demoting), so 0 and the maximum
/// value always map to 0 and the new maximum.
fn to_output_samples(buf: RgbBuffer, bpp: u32) -> OutputSamples {
    match (buf, bpp) {
        (RgbBuffer::U8(v), 8) => OutputSamples::U8(v),
        (RgbBuffer::U8(v), 16) => {
            OutputSamples::U16(v.iter().map(|&x| x as u16 * 257).collect())
        }
        (RgbBuffer::U8(v), 32) => {
            OutputSamples::U32(v.iter().map(|&x| x as u32 * 16843009).collect())
        }
        (RgbBuffer::U16(v), 8) => OutputSamples::U8(v.iter().map(|&x| (x >> 8) as u8).collect()),
        (RgbBuffer::U16(v), 16) => OutputSamples::U16(v),
        (RgbBuffer::U16(v), 32) => {
            OutputSamples::U32(v.iter().map(|&x| x as u32 * 65537).collect())
        }
        (_, other) => unreachable!("bpp {other} should have been rejected by the CLI parser"),
    }
}

fn write_tiff(output: &Path, width: usize, height: usize, samples: OutputSamples) -> Result<()> {
    let file =
        File::create(output).with_context(|| format!("cannot create {}", output.display()))?;
    let mut enc = TiffEncoder::new(file)
        .with_context(|| format!("cannot create TIFF encoder for {}", output.display()))?;

    let result = match samples {
        OutputSamples::U8(v) => enc.write_image::<colortype::RGB8>(width as u32, height as u32, &v),
        OutputSamples::U16(v) => {
            enc.write_image::<colortype::RGB16>(width as u32, height as u32, &v)
        }
        OutputSamples::U32(v) => {
            enc.write_image::<colortype::RGB32>(width as u32, height as u32, &v)
        }
    };

    result.with_context(|| format!("cannot write {}", output.display()))?;

    Ok(())
}

/// Split an interleaved RGB sample buffer into concatenated R, G, B planes
/// (the same plane order [`rgb_from_cube`] expects when reading one back).
fn deinterleave_to_planes<T: Copy>(v: &[T]) -> Vec<T> {
    let n = v.len() / 3;
    let mut out = Vec::with_capacity(v.len());
    for i in 0..n {
        out.push(v[i * 3]);
    }
    for i in 0..n {
        out.push(v[i * 3 + 1]);
    }
    for i in 0..n {
        out.push(v[i * 3 + 2]);
    }
    out
}

/// Write the debayered RGB cube as FITS using the same pixel format
/// (BITPIX and BSCALE/BZERO scaling) as the source image, rather than a
/// fixed bit depth — the `--bpp` option only governs TIFF output.
fn write_fits(
    output: &Path,
    width: usize,
    height: usize,
    rgb: RgbBuffer,
    source: &SourceFormat,
) -> Result<()> {
    let pixels = encode_rgb_as_source(rgb, source);
    write_pixel_fits(
        output,
        vec![width, height, 3],
        pixels,
        source.bscale,
        source.bzero,
    )
}

/// Convert a demosaiced RGB buffer back into the source image's pixel format.
fn encode_rgb_as_source(rgb: RgbBuffer, source: &SourceFormat) -> PixelData {
    match rgb {
        // An 8-bit source is demosaiced in raw-sample space (no scaling
        // applied), so its samples can be stored back unchanged.
        RgbBuffer::U8(v) => PixelData::U8(deinterleave_to_planes(&v)),
        // Wider sources are demosaiced in physical-value space; invert the
        // BSCALE/BZERO scaling to recover raw samples of the source type.
        RgbBuffer::U16(v) => encode_physical_as_source(&deinterleave_to_planes(&v), source),
    }
}

/// Map physical (BSCALE/BZERO-applied) pixel values back to raw samples of the
/// source's BITPIX, clamping integer types to their representable range.
fn encode_physical_as_source(planes: &[u16], source: &SourceFormat) -> PixelData {
    let raw = |p: u16| (p as f64 - source.bzero) / source.bscale;
    let raw_int = |p: u16, lo: f64, hi: f64| raw(p).round().clamp(lo, hi);

    match source.bitpix {
        Bitpix::U8 => {
            PixelData::U8(planes.iter().map(|&p| raw_int(p, 0.0, u8::MAX as f64) as u8).collect())
        }
        Bitpix::I16 => PixelData::I16(
            planes
                .iter()
                .map(|&p| raw_int(p, i16::MIN as f64, i16::MAX as f64) as i16)
                .collect(),
        ),
        Bitpix::I32 => PixelData::I32(
            planes
                .iter()
                .map(|&p| raw_int(p, i32::MIN as f64, i32::MAX as f64) as i32)
                .collect(),
        ),
        Bitpix::I64 => PixelData::I64(
            planes
                .iter()
                .map(|&p| raw_int(p, i64::MIN as f64, i64::MAX as f64) as i64)
                .collect(),
        ),
        Bitpix::F32 => PixelData::F32(planes.iter().map(|&p| raw(p) as f32).collect()),
        Bitpix::F64 => PixelData::F64(planes.iter().map(|&p| raw(p)).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{test_data, write_mosaic_fits, write_rgb_cube_fits};
    use bayer::CFA;
    use fitskit::{HduData, HeaderValue};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    #[test]
    fn debayer_produces_tiff_of_expected_size() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 8, 6, Some("RGGB"));

        let output = tmp.path().join("raw.tiff");
        let opts = DebayerOptions {
            format: OutputFormat::Tiff,
            ..DebayerOptions::default()
        };
        debayer_file(&input, &output, &opts).unwrap();

        let data = std::fs::read(&output).unwrap();
        // Cheap sanity check: a real TIFF file, and bigger than the raw pixel data
        // (it's now 3 channels at 16bpp instead of 1 channel at 16bpp).
        assert!(data.starts_with(b"II") || data.starts_with(b"MM"));
        assert!(data.len() as usize > 8 * 6 * 2);
    }

    #[test]
    fn debayer_default_format_is_fits() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 8, 6, Some("RGGB"));

        let output = tmp.path().join("raw_debayer.fits");
        debayer_file(&input, &output, &DebayerOptions::default()).unwrap();

        let fits = FitsFile::from_file(&output).unwrap();
        if let HduData::Image(img) = &fits.primary().data {
            assert_eq!(img.axes, vec![8, 6, 3]);
        } else {
            panic!("expected image data");
        }
    }

    fn write_typed_fits(path: &Path, width: usize, height: usize, pixels: PixelData) {
        let img = ImageData::new(vec![width, height], pixels);
        let mut fits = FitsFile::with_primary_image(img);
        fits.primary_mut()
            .header
            .set("BAYERPAT", HeaderValue::String("RGGB".to_string()), None);
        fits.to_file(path).unwrap();
    }

    fn debayer_to_fits_bitpix(input: &Path) -> Bitpix {
        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("out.fits");
        debayer_file(input, &output, &DebayerOptions::default()).unwrap();

        let fits = FitsFile::from_file(&output).unwrap();
        match &fits.primary().data {
            HduData::Image(img) => img.pixels.bitpix(),
            _ => panic!("expected image data"),
        }
    }

    #[test]
    fn debayer_fits_preserves_f32_source_format() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        let pixels = (0..(8 * 6)).map(|x| x as f32).collect();
        write_typed_fits(&input, 8, 6, PixelData::F32(pixels));

        // Default bpp is 16, but FITS output must keep the source's float format.
        assert_eq!(debayer_to_fits_bitpix(&input), Bitpix::F32);
    }

    #[test]
    fn debayer_fits_preserves_u8_source_format() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        let pixels = (0..(8 * 6)).map(|x| x as u8).collect();
        write_typed_fits(&input, 8, 6, PixelData::U8(pixels));

        assert_eq!(debayer_to_fits_bitpix(&input), Bitpix::U8);
    }

    #[test]
    fn debayer_fits_preserves_i32_source_format() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        let pixels: Vec<i32> = (0..(8 * 6)).collect();
        write_typed_fits(&input, 8, 6, PixelData::I32(pixels));

        assert_eq!(debayer_to_fits_bitpix(&input), Bitpix::I32);
    }

    #[test]
    fn debayer_errors_without_pattern() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 4, 4, None);

        let output = tmp.path().join("raw.tiff");
        let err = debayer_file(&input, &output, &DebayerOptions::default()).unwrap_err();
        assert!(err.to_string().contains("Bayer pattern"));
    }

    #[test]
    fn debayer_uses_cli_pattern_when_header_missing() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 4, 4, None);

        let output = tmp.path().join("raw.tiff");
        let opts = DebayerOptions {
            pattern: Some(CFA::BGGR),
            ..DebayerOptions::default()
        };
        debayer_file(&input, &output, &opts).unwrap();
        assert!(output.exists());
    }

    #[test]
    fn debayer_errors_if_output_exists_without_force() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 4, 4, Some("RGGB"));

        let output = tmp.path().join("raw.tiff");
        std::fs::write(&output, b"dummy").unwrap();

        let err = debayer_file(&input, &output, &DebayerOptions::default()).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn debayer_force_overwrites_existing_output() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 4, 4, Some("RGGB"));

        let output = tmp.path().join("raw.tiff");
        std::fs::write(&output, b"dummy").unwrap();

        let opts = DebayerOptions {
            force: true,
            ..DebayerOptions::default()
        };
        debayer_file(&input, &output, &opts).unwrap();
        assert!(output.metadata().unwrap().len() > 5);
    }

    #[test]
    fn debayer_skips_demosaic_for_already_debayered_cube() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 4, 3);

        let output = tmp.path().join("rgb.tiff");
        let opts = DebayerOptions {
            format: OutputFormat::Tiff,
            ..DebayerOptions::default()
        };
        debayer_file(&input, &output, &opts).unwrap();

        let data = std::fs::read(&output).unwrap();
        assert!(data.starts_with(b"II") || data.starts_with(b"MM"));
    }

    #[test]
    fn debayer_force_demosaic_rejects_3_plane_cube_instead_of_guessing() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 4, 3);

        let output = tmp.path().join("rgb.tiff");
        let opts = DebayerOptions {
            format: OutputFormat::Tiff,
            force_demosaic: true,
            ..DebayerOptions::default()
        };
        let err = debayer_file(&input, &output, &opts).unwrap_err();
        assert!(err.to_string().contains("2D mosaic image"));
    }

    #[test]
    fn debayer_bpp_8_produces_smaller_output_than_bpp_16() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 16, 16, Some("RGGB"));

        let out8 = tmp.path().join("out8.tiff");
        let out16 = tmp.path().join("out16.tiff");

        debayer_file(
            &input,
            &out8,
            &DebayerOptions {
                bpp: 8,
                format: OutputFormat::Tiff,
                ..DebayerOptions::default()
            },
        )
        .unwrap();
        debayer_file(
            &input,
            &out16,
            &DebayerOptions {
                bpp: 16,
                format: OutputFormat::Tiff,
                ..DebayerOptions::default()
            },
        )
        .unwrap();

        let len8 = std::fs::metadata(&out8).unwrap().len();
        let len16 = std::fs::metadata(&out16).unwrap().len();
        assert!(len8 < len16);
    }

    fn assert_debayer_matches_known_hash(bpp: u32, hash_file: &str) {
        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("uncompressed.tiff");

        let opts = DebayerOptions {
            bpp,
            format: OutputFormat::Tiff,
            ..DebayerOptions::default()
        };
        debayer_file(&test_data("uncompressed.fit"), &output, &opts).unwrap();

        let expected = std::fs::read_to_string(test_data(hash_file))
            .unwrap()
            .trim()
            .to_string();

        let actual = format!("{:x}", Sha256::digest(std::fs::read(&output).unwrap()));

        assert_eq!(actual, expected);
    }

    #[test]
    fn debayer_uncompressed_fit_matches_known_hash() {
        assert_debayer_matches_known_hash(16, "debayer/uncompressed.sha256");
    }

    #[test]
    fn debayer_uncompressed_fit_bpp8_matches_known_hash() {
        assert_debayer_matches_known_hash(8, "debayer/uncompressed-bpp8.sha256");
    }

    #[test]
    fn debayer_uncompressed_fit_bpp32_matches_known_hash() {
        assert_debayer_matches_known_hash(32, "debayer/uncompressed-bpp32.sha256");
    }
}
