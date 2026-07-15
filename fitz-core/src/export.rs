//! Encode an in-memory RGB image to an export file format — FITS, TIFF, JPEG or
//! PNG — with per-format options, and a one-call [`export_file`] that renders a
//! FITS input through the debayer/stretch preview pipeline and writes it out.
//!
//! Reusable by any frontend: performs no path derivation, prompting, or
//! progress reporting (a caller drives those). The pixel path mirrors the live
//! preview, so an exported file matches what the viewer shows for the same
//! debayer/stretch settings.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use anyhow::{Context, Result};
use fitskit::{CompressionType, FitsFile, Header, PixelData};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use rayon::prelude::*;
use tiff::encoder::{Compression, DeflateLevel, TiffEncoder, colortype};

use crate::compress::{CompressOptions, compress_fits};
use crate::debayer::{OutputSamples, to_output_samples};
use crate::fits_image::{
    CFA_KEYWORDS, RgbBuffer, build_pixel_fits, deinterleave_to_planes, high_byte,
};
use crate::preview::{PreviewParams, render_export_rgb};

/// FITS pixel storage requested for an exported image, mapping to a FITS
/// `BITPIX`: `I8` → byte (8), `I16` → signed short (16, using the unsigned-16
/// convention so 0..=65535 round-trips), `F32` → IEEE float (-32).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FitsBitpix {
    I8,
    I16,
    F32,
}

/// Per-format options for a FITS export.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FitsExportOptions {
    pub bitpix: FitsBitpix,
    /// Tile-compress the output with this algorithm; `None` writes it plain.
    pub compression: Option<CompressionType>,
}

/// Per-format options for a TIFF export.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TiffExportOptions {
    /// Bits per sample: 8, 16 or 32.
    pub bpp: u32,
    /// Apply DEFLATE (zip) compression to the image data.
    pub deflate: bool,
}

/// Per-format options for a JPEG export.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct JpegExportOptions {
    /// Encoder quality, 1..=100 (higher is better, larger).
    pub quality: u8,
}

/// The output format an image is exported to, carrying that format's options.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExportFormat {
    Fits(FitsExportOptions),
    Tiff(TiffExportOptions),
    Jpeg(JpegExportOptions),
    /// PNG (8-bit RGB); no options.
    Png,
}

impl ExportFormat {
    /// The default file extension (without a leading dot) for this format.
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Fits(_) => "fits",
            ExportFormat::Tiff(_) => "tiff",
            ExportFormat::Jpeg(_) => "jpg",
            ExportFormat::Png => "png",
        }
    }
}

/// Render a FITS `input` through the debayer/stretch preview pipeline (per
/// `params`) and write it to `output` in `format`. The one-call entry point a
/// frontend uses per file; a failure names the offending input.
pub fn export_file(
    input: &Path,
    output: &Path,
    params: &PreviewParams,
    format: &ExportFormat,
) -> Result<()> {
    let fits =
        FitsFile::from_file(input).with_context(|| format!("cannot read {}", input.display()))?;
    let (header, img) = crate::fits_image::find_image_hdu(&fits, input)?;
    let (width, height, rgb) = render_export_rgb(header, img.as_ref(), params)?;
    export_rgb(output, width, height, rgb, Some(header), format)
}

/// Encode an already-rendered interleaved RGB buffer to `output` in `format`.
/// `src_header`, when given, seeds a FITS export's metadata (its CFA keywords
/// are dropped, since the output is a debayered RGB cube); it is ignored by the
/// TIFF/JPEG/PNG paths.
pub fn export_rgb(
    output: &Path,
    width: usize,
    height: usize,
    rgb: RgbBuffer,
    src_header: Option<&Header>,
    format: &ExportFormat,
) -> Result<()> {
    match format {
        ExportFormat::Fits(opts) => write_fits(output, width, height, rgb, src_header, opts),
        ExportFormat::Tiff(opts) => {
            let samples = to_output_samples(rgb, opts.bpp);
            write_tiff(output, width, height, samples, opts.deflate)
        }
        ExportFormat::Jpeg(opts) => {
            write_jpeg(output, width, height, &rgb_to_rgb8(rgb), opts.quality)
        }
        ExportFormat::Png => write_png(output, width, height, &rgb_to_rgb8(rgb)),
    }
}

/// Narrow an interleaved RGB buffer to 8-bit RGB samples (the input JPEG/PNG
/// consume), keeping each 16-bit sample's high byte.
fn rgb_to_rgb8(rgb: RgbBuffer) -> Vec<u8> {
    match to_output_samples(rgb, 8) {
        OutputSamples::U8(v) => v,
        _ => unreachable!("bpp 8 always yields 8-bit samples"),
    }
}

/// Write the RGB buffer as a 3-plane FITS cube at the requested BITPIX, optionally
/// tile-compressed. Values flow through the 16-bit interleaved form and are then
/// deinterleaved into planes and cast to the target pixel type.
fn write_fits(
    output: &Path,
    width: usize,
    height: usize,
    rgb: RgbBuffer,
    src_header: Option<&Header>,
    opts: &FitsExportOptions,
) -> Result<()> {
    let interleaved = match to_output_samples(rgb, 16) {
        OutputSamples::U16(v) => v,
        _ => unreachable!("bpp 16 always yields 16-bit samples"),
    };
    let planes = deinterleave_to_planes(&interleaved);

    // Map the common 0..=65535 plane values to the requested storage type. I16
    // uses the FITS unsigned-16 convention (BZERO 32768); the others store the
    // value directly with no scaling.
    let (pixels, bscale, bzero) = match opts.bitpix {
        FitsBitpix::I8 => (
            PixelData::U8(planes.par_iter().map(|&v| high_byte(v)).collect()),
            1.0,
            0.0,
        ),
        FitsBitpix::I16 => (
            PixelData::I16(
                planes
                    .par_iter()
                    .map(|&v| (v as i32 - 32768) as i16)
                    .collect(),
            ),
            1.0,
            32768.0,
        ),
        FitsBitpix::F32 => (
            PixelData::F32(planes.par_iter().map(|&v| v as f32).collect()),
            1.0,
            0.0,
        ),
    };

    let fits = build_pixel_fits(
        vec![width, height, 3],
        pixels,
        bscale,
        bzero,
        src_header,
        CFA_KEYWORDS,
        Some("exported by fitz"),
    );
    let fits = match opts.compression {
        Some(algorithm) => compress_fits(&fits, &CompressOptions { algorithm })?,
        None => fits,
    };
    fits.to_file(output)
        .with_context(|| format!("cannot write {}", output.display()))?;
    Ok(())
}

/// Write interleaved RGB samples as an RGB TIFF at their bit depth, optionally
/// DEFLATE-compressed.
fn write_tiff(
    output: &Path,
    width: usize,
    height: usize,
    samples: OutputSamples,
    deflate: bool,
) -> Result<()> {
    let file =
        File::create(output).with_context(|| format!("cannot create {}", output.display()))?;
    let compression = if deflate {
        Compression::Deflate(DeflateLevel::Balanced)
    } else {
        Compression::Uncompressed
    };
    let mut enc = TiffEncoder::new(file)
        .with_context(|| format!("cannot create TIFF encoder for {}", output.display()))?
        .with_compression(compression);

    let (w, h) = (width as u32, height as u32);
    match samples {
        OutputSamples::U8(v) => enc.write_image::<colortype::RGB8>(w, h, &v),
        OutputSamples::U16(v) => enc.write_image::<colortype::RGB16>(w, h, &v),
        OutputSamples::U32(v) => enc.write_image::<colortype::RGB32>(w, h, &v),
    }
    .with_context(|| format!("cannot write {}", output.display()))?;
    Ok(())
}

/// Write 8-bit interleaved RGB samples as a JPEG at the given quality.
fn write_jpeg(output: &Path, width: usize, height: usize, rgb8: &[u8], quality: u8) -> Result<()> {
    let file =
        File::create(output).with_context(|| format!("cannot create {}", output.display()))?;
    JpegEncoder::new_with_quality(BufWriter::new(file), quality)
        .write_image(rgb8, width as u32, height as u32, ExtendedColorType::Rgb8)
        .with_context(|| format!("cannot write {}", output.display()))?;
    Ok(())
}

/// Write 8-bit interleaved RGB samples as a PNG.
fn write_png(output: &Path, width: usize, height: usize, rgb8: &[u8]) -> Result<()> {
    let file =
        File::create(output).with_context(|| format!("cannot create {}", output.display()))?;
    PngEncoder::new(BufWriter::new(file))
        .write_image(rgb8, width as u32, height as u32, ExtendedColorType::Rgb8)
        .with_context(|| format!("cannot write {}", output.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{test_data, write_mosaic_fits, write_rgb_cube_fits};
    use fitskit::{Bitpix, HduData};
    use tempfile::TempDir;

    fn default_params() -> PreviewParams {
        PreviewParams::default()
    }

    /// The BITPIX of the primary image in a written FITS file.
    fn fits_bitpix(path: &Path) -> Bitpix {
        let fits = FitsFile::from_file(path).unwrap();
        match &fits.primary().data {
            HduData::Image(img) => img.pixels.bitpix(),
            _ => panic!("expected primary image data"),
        }
    }

    #[test]
    fn export_fits_writes_three_plane_cube_at_requested_bitpix() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fit");
        write_mosaic_fits(&input, 8, 6, Some("RGGB"));

        for (bitpix, expected) in [
            (FitsBitpix::I8, Bitpix::U8),
            (FitsBitpix::I16, Bitpix::I16),
            (FitsBitpix::F32, Bitpix::F32),
        ] {
            let output = tmp.path().join(format!("out-{bitpix:?}.fits"));
            let format = ExportFormat::Fits(FitsExportOptions {
                bitpix,
                compression: None,
            });
            export_file(&input, &output, &default_params(), &format).unwrap();

            assert_eq!(fits_bitpix(&output), expected);
            let fits = FitsFile::from_file(&output).unwrap();
            match &fits.primary().data {
                HduData::Image(img) => assert_eq!(img.axes, vec![8, 6, 3]),
                _ => panic!("expected image data"),
            }
            // The CFA keyword must be dropped so the RGB cube reads back cleanly.
            assert!(fits.primary().header.find("BAYERPAT").is_none());
        }
    }

    #[test]
    fn export_fits_compressed_round_trips() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fit");
        write_mosaic_fits(&input, 16, 12, Some("RGGB"));

        let output = tmp.path().join("out.fits");
        let format = ExportFormat::Fits(FitsExportOptions {
            bitpix: FitsBitpix::I16,
            compression: Some(CompressionType::Rice1),
        });
        export_file(&input, &output, &default_params(), &format).unwrap();

        // The written file is a tile-compressed FITS: it has a compressed HDU,
        // and decompressing it yields the 3-plane RGB cube.
        let fits = FitsFile::from_file(&output).unwrap();
        assert!(fits.hdus.iter().any(|h| h.as_compressed_image().is_some()));
        let restored = crate::decompress::decompress(&output).unwrap();
        match &restored.primary().data {
            HduData::Image(img) => assert_eq!(img.axes, vec![16, 12, 3]),
            _ => panic!("expected decompressed image data"),
        }
    }

    #[test]
    fn export_tiff_bpp_and_deflate_affect_size() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fit");
        write_mosaic_fits(&input, 16, 16, Some("RGGB"));

        let write = |bpp: u32, deflate: bool, name: &str| {
            let output = tmp.path().join(name);
            let format = ExportFormat::Tiff(TiffExportOptions { bpp, deflate });
            export_file(&input, &output, &default_params(), &format).unwrap();
            let data = std::fs::read(&output).unwrap();
            assert!(
                data.starts_with(b"II") || data.starts_with(b"MM"),
                "not a TIFF"
            );
            std::fs::metadata(&output).unwrap().len()
        };

        let len8 = write(8, false, "out8.tiff");
        let len16 = write(16, false, "out16.tiff");
        let len16z = write(16, true, "out16z.tiff");
        // 8bpp is smaller than 16bpp; deflate shrinks the 16bpp output further.
        assert!(len8 < len16);
        assert!(len16z < len16);
    }

    #[test]
    fn export_jpeg_and_png_write_recognizable_files() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fit");
        write_rgb_cube_fits(&input, 12, 9);

        let jpg = tmp.path().join("out.jpg");
        export_file(
            &input,
            &jpg,
            &default_params(),
            &ExportFormat::Jpeg(JpegExportOptions { quality: 85 }),
        )
        .unwrap();
        let jdata = std::fs::read(&jpg).unwrap();
        assert_eq!(&jdata[0..2], &[0xFF, 0xD8], "JPEG SOI marker");

        let png = tmp.path().join("out.png");
        export_file(&input, &png, &default_params(), &ExportFormat::Png).unwrap();
        let pdata = std::fs::read(&png).unwrap();
        assert_eq!(&pdata[0..8], b"\x89PNG\r\n\x1a\n", "PNG signature");
    }

    #[test]
    fn export_jpeg_quality_changes_output_size() {
        let tmp = TempDir::new().unwrap();
        let low = tmp.path().join("low.jpg");
        let high = tmp.path().join("high.jpg");
        let input = test_data("uncompressed.fit");

        export_file(
            &input,
            &low,
            &default_params(),
            &ExportFormat::Jpeg(JpegExportOptions { quality: 20 }),
        )
        .unwrap();
        export_file(
            &input,
            &high,
            &default_params(),
            &ExportFormat::Jpeg(JpegExportOptions { quality: 95 }),
        )
        .unwrap();

        let low_len = std::fs::metadata(&low).unwrap().len();
        let high_len = std::fs::metadata(&high).unwrap().len();
        assert!(high_len > low_len, "higher quality should be larger");
    }

    #[test]
    fn extension_matches_format() {
        assert_eq!(
            ExportFormat::Fits(FitsExportOptions {
                bitpix: FitsBitpix::I16,
                compression: None
            })
            .extension(),
            "fits"
        );
        assert_eq!(
            ExportFormat::Tiff(TiffExportOptions {
                bpp: 16,
                deflate: false
            })
            .extension(),
            "tiff"
        );
        assert_eq!(
            ExportFormat::Jpeg(JpegExportOptions { quality: 90 }).extension(),
            "jpg"
        );
        assert_eq!(ExportFormat::Png.extension(), "png");
    }
}
