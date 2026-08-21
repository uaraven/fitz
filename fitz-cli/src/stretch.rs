//! The `stretch` command: load a FITS image (debayering it first if needed),
//! apply an MTF/STF auto-stretch, and save the result as FITS (normalized
//! `[0, 1]` float samples), TIFF, PNG or JPEG.

use std::path::Path;

use crate::debayer::OutputFormat;
use crate::io_prompt::{ensure_can_write, print_progress, print_step};
use crate::options::StretchOptions;
use anyhow::Result;
use libfitz::export::{ExportFormat, FitsOptions, TiffOptions};

pub fn stretch_file(input: &Path, output: &Path, opts: &StretchOptions) -> Result<()> {
    ensure_can_write(output, opts.yes)?;
    print_progress(input, output);

    print_step(opts.verbose, "reading");
    let image = libfitz::fits_file::load_fits(input)?;

    print_step(opts.verbose, "stretching");
    let image = image.stretch(opts.core.linked, opts.core.brightness);

    print_step(opts.verbose, "writing");
    let format = match opts.format {
        OutputFormat::Tiff => ExportFormat::Tiff(TiffOptions{ bpp: 16, compress: false }),
        OutputFormat::Fits => ExportFormat::Fits(FitsOptions{ bpp: -32, compress: false }),
        OutputFormat::Png => ExportFormat::Png,
        OutputFormat::Jpeg => ExportFormat::Jpeg(90),
    };

    image.export(output, format)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfitz::data::{ImageType, PixelBuffer};
    use libfitz::fits_file::load_fits;
    use libfitz::fitskit::{FitsFile, HeaderValue, ImageData, PixelData};
    use tempfile::TempDir;

    fn opts(format: OutputFormat, yes: bool) -> StretchOptions {
        StretchOptions {
            format,
            yes,
            ..StretchOptions::default()
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
        fits.to_file(path).unwrap();
    }

    #[test]
    fn stretch_file_writes_a_stretched_fits_and_preserves_bayer_pattern() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 16, 16, Some("RGGB"));
        let output = tmp.path().join("mosaic-stretch.fits");

        stretch_file(&input, &output, &opts(OutputFormat::Fits, true)).unwrap();

        let original = load_fits(&input).unwrap();
        let stretched = load_fits(&output).unwrap();
        assert_eq!(stretched.image_type, ImageType::CFA(bayer::CFA::RGGB));
        assert_eq!(stretched.width, original.width);
        assert_eq!(stretched.height, original.height);
        match stretched.pixels {
            PixelBuffer::F32(v) => {
                assert_eq!(v.len(), original.width * original.height);
                // A non-degenerate ramp input stretches to span most of the
                // normalized [0, 1] range rather than collapsing to a single value.
                assert!(v.iter().all(|&x| (0.0..=1.0).contains(&x)));
                assert!(v.iter().any(|&x| x > 0.0));
                assert!(v.iter().any(|&x| x < 1.0));
            }
            PixelBuffer::U16(_) => panic!("expected a normalized f32 pixel buffer"),
        }
    }

    #[test]
    fn stretch_file_writes_tiff() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 16, 16, None);
        let output = tmp.path().join("mosaic-stretch.tiff");

        stretch_file(&input, &output, &opts(OutputFormat::Tiff, true)).unwrap();

        let data = std::fs::read(&output).unwrap();
        assert!(data.starts_with(b"II") || data.starts_with(b"MM"));
    }

    #[test]
    fn stretch_file_writes_png_and_jpeg() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 16, 16, None);

        let png = tmp.path().join("mosaic-stretch.png");
        stretch_file(&input, &png, &opts(OutputFormat::Png, true)).unwrap();
        let png_data = std::fs::read(&png).unwrap();
        assert!(png_data.starts_with(&[0x89, b'P', b'N', b'G']));

        let jpeg = tmp.path().join("mosaic-stretch.jpg");
        stretch_file(&input, &jpeg, &opts(OutputFormat::Jpeg, true)).unwrap();
        let jpeg_data = std::fs::read(&jpeg).unwrap();
        assert!(jpeg_data.starts_with(&[0xFF, 0xD8]));
    }

    #[test]
    fn stretch_file_brightness_shifts_the_stretched_median() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 32, 32, None);

        let median_of = |brightness: f32| {
            let output = tmp
                .path()
                .join(format!("mosaic-{}.fits", (brightness * 1000.0) as i32));
            let mut o = opts(OutputFormat::Fits, true);
            o.core.brightness = brightness;
            stretch_file(&input, &output, &o).unwrap();
            let PixelBuffer::F32(pixels) = load_fits(&output).unwrap().pixels else {
                panic!("expected a normalized f32 pixel buffer");
            };
            let mut sorted = pixels;
            sorted.sort_unstable_by(|a, b| a.total_cmp(b));
            sorted[sorted.len() / 2]
        };

        // A brighter target background should pull the stretched median up.
        assert!(median_of(0.6) > median_of(0.1));
    }

    #[test]
    fn stretch_file_refuses_to_overwrite_without_yes() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 16, 16, None);
        let output = tmp.path().join("mosaic-stretch.fits");
        std::fs::write(&output, b"pre-existing").unwrap();

        // Non-interactive test process: stdin isn't a terminal, so the
        // overwrite prompt refuses outright instead of blocking on input.
        let err = stretch_file(&input, &output, &opts(OutputFormat::Fits, false)).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert_eq!(std::fs::read(&output).unwrap(), b"pre-existing");
    }

    #[test]
    fn stretch_file_overwrites_with_yes() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 16, 16, None);
        let output = tmp.path().join("mosaic-stretch.fits");
        std::fs::write(&output, b"pre-existing").unwrap();

        stretch_file(&input, &output, &opts(OutputFormat::Fits, true)).unwrap();

        assert_ne!(std::fs::read(&output).unwrap(), b"pre-existing");
        load_fits(&output).unwrap();
    }
}
