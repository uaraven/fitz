use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use libfitz::raw_fits::CompressionSettings;

use crate::io_prompt::{ensure_can_write, print_progress, print_step};
use crate::options::Options;

pub fn decompress_file(input: &Path, output: &Path, opts: &Options) -> Result<()> {
    // Decompressing in place (output == input) is allowed and must not trip
    // the "already exists" guard.
    if output != input {
        ensure_can_write(output, opts.yes)?;
    }
    print_progress(input, output);

    print_step(opts.verbose, "reading");
    let img = libfitz::raw_fits::load_raw(input)?;
    print_step(opts.verbose, "writing");
    libfitz::raw_fits::save_raw(&img, output, CompressionSettings::NoCompression)
        .with_context(|| format!("cannot write {}", output.display()))?;

    if !opts.keep && opts.output.is_none() && output != input {
        fs::remove_file(input).with_context(|| format!("cannot remove {}", input.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfitz::fits_file::load_fits;
    use libfitz::fitskit::{FitsFile, HeaderValue, ImageData, PixelData};
    use libfitz::raw_fits::{CompressionSettings, save_raw};
    use tempfile::TempDir;

    fn opts(keep: bool, output: Option<&Path>, yes: bool) -> Options {
        Options {
            keep,
            yes,
            output: output.map(Path::to_path_buf),
            ..Options::default()
        }
    }

    /// Build a small 2D I16 mosaic, optionally tagged with a BAYERPAT, mirroring
    /// `libfitz::test_support::write_mosaic_fits` (not reusable here since it's
    /// `pub(crate)` to `libfitz`).
    fn mosaic_fits(width: usize, height: usize, pattern: Option<&str>) -> FitsFile {
        let pixels: Vec<i16> = (0..(width * height) as i16).collect();
        let img = ImageData::new(vec![width, height], PixelData::I16(pixels));
        let mut fits = FitsFile::with_primary_image(img);
        if let Some(p) = pattern {
            fits.primary_mut()
                .header
                .set("BAYERPAT", HeaderValue::String(p.to_string()), None);
        }
        fits
    }

    /// Write a Rice1 tile-compressed mosaic fixture directly (bypassing
    /// `compress_file`, which is what this module is testing).
    fn write_compressed_mosaic_fits(
        path: &Path,
        width: usize,
        height: usize,
        pattern: Option<&str>,
    ) {
        let fits = mosaic_fits(width, height, pattern);
        save_raw(&fits, path, CompressionSettings::Rice1).unwrap();
    }

    #[test]
    fn decompress_file_writes_a_plain_fits_and_round_trips_pixels() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits.fz");
        write_compressed_mosaic_fits(&input, 16, 16, Some("RGGB"));
        let output = tmp.path().join("mosaic.fits");

        decompress_file(&input, &output, &opts(true, None, true)).unwrap();

        let original = load_fits(&input).unwrap();
        let decompressed = load_fits(&output).unwrap();
        assert_eq!(decompressed.pixels, original.pixels);
        assert_eq!(decompressed.header.get_string("BAYERPAT"), Some("RGGB"));

        // The output HDU is a plain image now, not a tile-compressed binary
        // table.
        let meta = libfitz::fits_file::load_header(&output).unwrap();
        assert_eq!(meta.header.get_string("ZCMPTYPE"), None);
    }

    #[test]
    fn decompress_file_removes_input_by_default() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits.fz");
        write_compressed_mosaic_fits(&input, 8, 8, None);
        let output = tmp.path().join("mosaic.fits");

        decompress_file(&input, &output, &opts(false, None, true)).unwrap();

        assert!(!input.exists());
        assert!(output.exists());
    }

    #[test]
    fn decompress_file_keeps_input_when_keep_flag_set() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits.fz");
        write_compressed_mosaic_fits(&input, 8, 8, None);
        let output = tmp.path().join("mosaic.fits");

        decompress_file(&input, &output, &opts(true, None, true)).unwrap();

        assert!(input.exists());
    }

    #[test]
    fn decompress_file_keeps_input_when_explicit_output_given() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits.fz");
        write_compressed_mosaic_fits(&input, 8, 8, None);
        let output = tmp.path().join("custom.fits");

        // `keep` is false, but an explicit `-o` output (`opts.output.is_some()`)
        // must still preserve the original input.
        decompress_file(&input, &output, &opts(false, Some(&output), true)).unwrap();

        assert!(input.exists());
    }

    #[test]
    fn decompress_file_in_place_bypasses_the_overwrite_guard_and_does_not_delete() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("mosaic.fits.fz");
        write_compressed_mosaic_fits(&path, 8, 8, None);

        // output == input: this must not trip the "already exists" guard even
        // with `yes: false`, and the single file must survive as itself.
        decompress_file(&path, &path, &opts(false, None, false)).unwrap();

        assert!(path.exists());
        let decompressed = load_fits(&path).unwrap();
        let meta = libfitz::fits_file::load_header(&path).unwrap();
        assert_eq!(meta.header.get_string("ZCMPTYPE"), None);
        assert_eq!(decompressed.width, 8);
        assert_eq!(decompressed.height, 8);
    }

    #[test]
    fn decompress_file_refuses_to_overwrite_a_different_output_without_yes() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits.fz");
        write_compressed_mosaic_fits(&input, 8, 8, None);
        let output = tmp.path().join("mosaic.fits");
        fs::write(&output, b"pre-existing").unwrap();

        // Non-interactive test process: stdin isn't a terminal, so the
        // overwrite prompt refuses outright instead of blocking on input.
        let err = decompress_file(&input, &output, &opts(true, None, false)).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert_eq!(fs::read(&output).unwrap(), b"pre-existing");
        assert!(input.exists());
    }

    #[test]
    fn decompress_file_overwrites_a_different_output_with_yes() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits.fz");
        write_compressed_mosaic_fits(&input, 8, 8, None);
        let output = tmp.path().join("mosaic.fits");
        fs::write(&output, b"pre-existing").unwrap();

        decompress_file(&input, &output, &opts(true, None, true)).unwrap();

        assert_ne!(fs::read(&output).unwrap(), b"pre-existing");
        load_fits(&output).unwrap();
    }
}
