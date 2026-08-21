use std::path::Path;

use crate::io_prompt::{ensure_can_write, print_progress, print_step};
use crate::options::DebayerOptions;
use anyhow::{Result, bail};
use libfitz::data::ImageType;
use libfitz::export::{ExportFormat, FitsOptions, TiffOptions};
use libfitz::fits_file::load_fits;

#[derive(Clone, Copy, Debug, Default)]
pub enum OutputFormat {
    #[default]
    Fits,
    Tiff,
    Jpeg,
    Png,
}

impl OutputFormat {
    pub(crate) fn extension(&self) -> &'static str {
        match self {
            OutputFormat::Fits => "fits",
            OutputFormat::Tiff => "tiff",
            OutputFormat::Jpeg => "jpg",
            OutputFormat::Png => "png",
        }
    }
}

pub(crate) fn parse_output_format(s: &str) -> Result<OutputFormat, String> {
    match s.to_lowercase().trim() {
        "tiff" => Ok(OutputFormat::Tiff),
        "jpeg" => Ok(OutputFormat::Jpeg),
        "fits" => Ok(OutputFormat::Fits),
        "png" => Ok(OutputFormat::Png),
        _ => Err("Unsupported output format".to_string()),
    }
}

pub fn debayer_file(input: &Path, output: &Path, opts: &DebayerOptions) -> Result<()> {
    ensure_can_write(output, opts.yes)?;
    print_progress(input, output);

    print_step(opts.verbose, "reading");
    let image = load_fits(input)?;

    let d = match image.image_type {
        ImageType::CFA(_) => {
            print_step(opts.verbose, "debayering");
            if let Some(img_res) = image.debayer() {
                img_res?
            } else {
                bail!(format!("Cannot debayer image {}", input.display()));
            }
        }
        _ => bail!(format!("Image {} already debayered", input.display())),
    };

    print_step(opts.verbose, "writing");
    let write_format = match opts.core.output_format {
        OutputFormat::Tiff => ExportFormat::Tiff(TiffOptions {
            bpp: opts.core.bpp as u32,
            compress: false,
        }),
        OutputFormat::Jpeg => ExportFormat::Jpeg(opts.core.quality),
        OutputFormat::Png => ExportFormat::Png,
        OutputFormat::Fits => ExportFormat::Fits(FitsOptions {
            bpp: opts.core.bpp as i64,
            compress: opts.core.compress,
        }),
    };
    d.export(output, write_format)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::DebayerCore;
    use libfitz::data::PixelBuffer;
    use libfitz::fits_file::load_fits;
    use libfitz::fitskit::{FitsFile, HeaderValue, ImageData, PixelData};
    use tempfile::TempDir;

    fn opts(format: OutputFormat, bpp: i32, yes: bool) -> DebayerOptions {
        DebayerOptions {
            core: DebayerCore {
                bpp,
                output_format: format,
                ..DebayerCore::default()
            },
            yes,
            ..DebayerOptions::default()
        }
    }

    /// Write a small 2D I16 mosaic, optionally tagged with a BAYERPAT, mirroring
    /// `libfitz::test_support::write_mosaic_fits` (not reusable here since it's
    /// `pub(crate)` to `libfitz`).
    fn write_mosaic_fits(path: &Path, width: usize, height: usize, pattern: Option<&str>) {
        let pixels: Vec<i16> = (0..(width * height) as i16).collect();
        let img = ImageData::new(vec![width, height], PixelData::I16(pixels));
        let mut fits = FitsFile::with_primary_image(img);
        if let Some(p) = pattern {
            fits.primary_mut()
                .header
                .set("BAYERPAT", HeaderValue::String(p.to_string()), None);
        }
        fits.primary_mut().header.set(
            "OBJECT",
            HeaderValue::String("M31".to_string()),
            None,
        );
        fits.to_file(path).unwrap();
    }

    /// Write a 3-plane (already-debayered) I16 RGB cube, for the "already
    /// debayered" no-op path.
    fn write_rgb_cube_fits(path: &Path, width: usize, height: usize) {
        let n = width * height;
        let mut pixels = Vec::with_capacity(n * 3);
        for c in 0..3 {
            for i in 0..n {
                pixels.push((c * n + i) as i16);
            }
        }
        let img = ImageData::new(vec![width, height, 3], PixelData::I16(pixels));
        FitsFile::with_primary_image(img).to_file(path).unwrap();
    }

    #[test]
    fn parse_output_format_parses_known_formats_case_insensitively() {
        assert!(matches!(parse_output_format("fits"), Ok(OutputFormat::Fits)));
        assert!(matches!(parse_output_format("FITS"), Ok(OutputFormat::Fits)));
        assert!(matches!(
            parse_output_format(" Tiff "),
            Ok(OutputFormat::Tiff)
        ));
        assert!(matches!(
            parse_output_format("jpeg"),
            Ok(OutputFormat::Jpeg)
        ));
        assert!(matches!(parse_output_format("PNG"), Ok(OutputFormat::Png)));
    }

    #[test]
    fn parse_output_format_rejects_unknown_format() {
        assert_eq!(
            parse_output_format("bmp").unwrap_err(),
            "Unsupported output format"
        );
    }

    #[test]
    fn output_format_extension_matches_format() {
        assert_eq!(OutputFormat::Fits.extension(), "fits");
        assert_eq!(OutputFormat::Tiff.extension(), "tiff");
        assert_eq!(OutputFormat::Jpeg.extension(), "jpg");
        assert_eq!(OutputFormat::Png.extension(), "png");
    }

    #[test]
    fn output_format_defaults_to_fits() {
        assert!(matches!(OutputFormat::default(), OutputFormat::Fits));
    }

    #[test]
    fn debayer_file_writes_rgb_fits_from_mosaic() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 16, 16, Some("RGGB"));
        let output = tmp.path().join("mosaic_debayer.fits");

        debayer_file(&input, &output, &opts(OutputFormat::Fits, 16, true)).unwrap();

        let debayered = load_fits(&output).unwrap();
        assert_eq!(debayered.image_type, ImageType::RGB);
        assert_eq!(debayered.width, 16);
        assert_eq!(debayered.height, 16);
        match debayered.pixels {
            PixelBuffer::U16(v) => assert_eq!(v.len(), 16 * 16 * 3),
            PixelBuffer::F32(_) => panic!("expected a u16 pixel buffer"),
        }

        // Metadata carries over; the now-stale mosaic pattern does not.
        assert_eq!(debayered.header.get_string("OBJECT"), Some("M31"));
        assert_eq!(debayered.header.get_string("BAYERPAT"), None);
    }

    #[test]
    fn debayer_file_writes_tiff() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 16, 16, Some("GRBG"));
        let output = tmp.path().join("mosaic_debayer.tiff");

        debayer_file(&input, &output, &opts(OutputFormat::Tiff, 16, true)).unwrap();

        let data = std::fs::read(&output).unwrap();
        assert!(data.starts_with(b"II") || data.starts_with(b"MM"));
    }

    #[test]
    fn debayer_file_writes_png_and_jpeg() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 16, 16, Some("BGGR"));

        let png = tmp.path().join("mosaic_debayer.png");
        debayer_file(&input, &png, &opts(OutputFormat::Png, 16, true)).unwrap();
        let png_data = std::fs::read(&png).unwrap();
        assert!(png_data.starts_with(&[0x89, b'P', b'N', b'G']));

        let jpeg = tmp.path().join("mosaic_debayer.jpg");
        debayer_file(&input, &jpeg, &opts(OutputFormat::Jpeg, 16, true)).unwrap();
        let jpeg_data = std::fs::read(&jpeg).unwrap();
        assert!(jpeg_data.starts_with(&[0xFF, 0xD8]));
    }

    #[test]
    fn debayer_file_refuses_an_already_debayered_image() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 8, 8);
        let output = tmp.path().join("rgb_debayer.fits");

        let err = debayer_file(&input, &output, &opts(OutputFormat::Fits, 16, true)).unwrap_err();
        assert!(err.to_string().contains("already debayered"));
        assert!(!output.exists());
    }

    #[test]
    fn debayer_file_refuses_to_overwrite_without_yes() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 16, 16, Some("RGGB"));
        let output = tmp.path().join("mosaic_debayer.fits");
        std::fs::write(&output, b"pre-existing").unwrap();

        // Non-interactive test process: stdin isn't a terminal, so the
        // overwrite prompt refuses outright instead of blocking on input.
        let err = debayer_file(&input, &output, &opts(OutputFormat::Fits, 16, false)).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert_eq!(std::fs::read(&output).unwrap(), b"pre-existing");
    }

    #[test]
    fn debayer_file_overwrites_with_yes() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 16, 16, Some("RGGB"));
        let output = tmp.path().join("mosaic_debayer.fits");
        std::fs::write(&output, b"pre-existing").unwrap();

        debayer_file(&input, &output, &opts(OutputFormat::Fits, 16, true)).unwrap();

        assert_ne!(std::fs::read(&output).unwrap(), b"pre-existing");
        let debayered = load_fits(&output).unwrap();
        assert_eq!(debayered.image_type, ImageType::RGB);
    }
}
