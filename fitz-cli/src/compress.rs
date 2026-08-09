use std::fs;
use std::path::Path;

use crate::io_prompt::{ensure_can_write, print_progress, print_step};
use crate::options::Options;
use anyhow::{Context, Result};
use libfitz::raw_fits::compression_settings_for;

pub fn compress_file(input: &Path, output: &Path, opts: &Options) -> Result<()> {
    ensure_can_write(output, opts.yes)?;
    print_progress(opts.verbose, input, output);

    print_step(opts.verbose, "reading");
    let img = libfitz::raw_fits::load_raw(input)?;
    print_step(opts.verbose, "compressing");
    let compression = compression_settings_for(opts.algorithm);
    print_step(opts.verbose, "writing");
    libfitz::raw_fits::save_raw(&img, output, compression)?;

    if !opts.keep && opts.output.is_none() {
        fs::remove_file(input).with_context(|| format!("cannot remove {}", input.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfitz::fits_file::load_fits;
    use libfitz::fitskit::{CompressionType, FitsFile, HeaderValue, ImageData, PixelData};
    use tempfile::TempDir;

    fn opts(algorithm: CompressionType, keep: bool, output: Option<&Path>, yes: bool) -> Options {
        Options {
            keep,
            yes,
            output: output.map(Path::to_path_buf),
            algorithm,
            ..Options::default()
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
    fn compress_file_writes_a_tile_compressed_fits_and_round_trips_pixels() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 16, 16, Some("RGGB"));
        let output = tmp.path().join("mosaic.fits.fz");

        compress_file(
            &input,
            &output,
            &opts(CompressionType::Rice1, true, None, true),
        )
        .unwrap();

        // Rice1 is lossless for integer data, so the pixels and Bayer pattern
        // must survive the round trip exactly.
        let original = load_fits(&input).unwrap();
        let compressed = load_fits(&output).unwrap();
        assert_eq!(compressed.pixels, original.pixels);
        assert_eq!(compressed.header.get_string("BAYERPAT"), Some("RGGB"));

        // The output HDU is actually a tile-compressed binary table, not a
        // plain image copy.
        let meta = libfitz::fits_file::load_header(&output).unwrap();
        assert_eq!(meta.header.get_string("ZCMPTYPE"), Some("RICE_1"));
    }

    #[test]
    fn compress_file_selects_the_requested_algorithm() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 16, 16, None);

        for (algorithm, expected) in [
            (CompressionType::Rice1, "RICE_1"),
            (CompressionType::Gzip1, "GZIP_1"),
            (CompressionType::Gzip2, "GZIP_2"),
        ] {
            let output = tmp.path().join(format!("{expected}.fits.fz"));
            compress_file(&input, &output, &opts(algorithm, true, None, true)).unwrap();
            let meta = libfitz::fits_file::load_header(&output).unwrap();
            assert_eq!(meta.header.get_string("ZCMPTYPE"), Some(expected));
        }
    }

    #[test]
    fn compress_file_falls_back_to_no_compression_for_unsupported_algorithms() {
        // `compress_file`'s match on `opts.algorithm` only handles Rice1/Gzip1/
        // Gzip2 explicitly; anything else (e.g. Hcompress1) silently falls back
        // to `CompressionSettings::NoCompression` rather than erroring.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 16, 16, None);
        let output = tmp.path().join("mosaic.fits.fz");

        compress_file(
            &input,
            &output,
            &opts(CompressionType::Hcompress1, true, None, true),
        )
        .unwrap();

        let meta = libfitz::fits_file::load_header(&output).unwrap();
        assert_eq!(meta.header.get_string("ZCMPTYPE"), None);
        let original = load_fits(&input).unwrap();
        let saved = load_fits(&output).unwrap();
        assert_eq!(saved.pixels, original.pixels);
    }

    #[test]
    fn compress_file_removes_input_by_default() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 8, 8, None);
        let output = tmp.path().join("mosaic.fits.fz");

        compress_file(
            &input,
            &output,
            &opts(CompressionType::Rice1, false, None, true),
        )
        .unwrap();

        assert!(!input.exists());
        assert!(output.exists());
    }

    #[test]
    fn compress_file_keeps_input_when_keep_flag_set() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 8, 8, None);
        let output = tmp.path().join("mosaic.fits.fz");

        compress_file(
            &input,
            &output,
            &opts(CompressionType::Rice1, true, None, true),
        )
        .unwrap();

        assert!(input.exists());
    }

    #[test]
    fn compress_file_keeps_input_when_explicit_output_given() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 8, 8, None);
        let output = tmp.path().join("custom.fits.fz");

        // `keep` is false, but an explicit `-o` output (`opts.output.is_some()`)
        // must still preserve the original input.
        compress_file(
            &input,
            &output,
            &opts(CompressionType::Rice1, false, Some(&output), true),
        )
        .unwrap();

        assert!(input.exists());
    }

    #[test]
    fn compress_file_refuses_to_overwrite_without_yes() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 8, 8, None);
        let output = tmp.path().join("mosaic.fits.fz");
        fs::write(&output, b"pre-existing").unwrap();

        // Non-interactive test process: stdin isn't a terminal, so the
        // overwrite prompt refuses outright instead of blocking on input.
        let err = compress_file(
            &input,
            &output,
            &opts(CompressionType::Rice1, true, None, false),
        )
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert_eq!(fs::read(&output).unwrap(), b"pre-existing");
        assert!(input.exists());
    }

    #[test]
    fn compress_file_overwrites_with_yes() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 8, 8, None);
        let output = tmp.path().join("mosaic.fits.fz");
        fs::write(&output, b"pre-existing").unwrap();

        compress_file(
            &input,
            &output,
            &opts(CompressionType::Rice1, true, None, true),
        )
        .unwrap();

        assert_ne!(fs::read(&output).unwrap(), b"pre-existing");
        load_fits(&output).unwrap();
    }
}
