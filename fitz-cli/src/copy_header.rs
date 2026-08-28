//! The `copy-header` command: copy FITS header keywords from a source image
//! onto a target image, filling in only the keywords the target doesn't
//! already carry (its own resolution, bit depth, channel count, pixel
//! scaling, and any other keyword it already has are left untouched). A
//! `BAYERPAT` (and related CFA keywords) from the source is also skipped when
//! the target is already a debayered 3-plane cube, so it doesn't start looking
//! like undebayered raw sensor data again.

use std::path::Path;

use anyhow::{Context, Result};
use libfitz::fitskit::FitsFile;
use crate::io_prompt::{ensure_can_write, print_progress, print_step};
use crate::options::CopyHeaderOptions;

pub fn copy_header_file(source: &Path, target: &Path, opts: &CopyHeaderOptions) -> Result<()> {
    print_step(opts.verbose, "reading");
    let source_fits = FitsFile::from_file(source).with_context(|| format!("cannot open {}", source.display()))?;
    let target_fits = &mut FitsFile::from_file(target).with_context(|| format!("cannot open {}", target.display()))?;
    print_step(opts.verbose, "copying header");
    let copied = libfitz::raw_fits::copy_headers_raw(&source_fits, target_fits)?;

    let output = opts.output.clone().unwrap_or_else(|| target.to_path_buf());
    // Overwriting the target in place is the whole point of this command and
    // must not trip the "already exists" guard, the same way decompress
    // handles its default in-place output.
    if output != target {
        ensure_can_write(&output, opts.yes)?;
    }
    print_progress(source, &output);

    print_step(opts.verbose, "writing");
    target_fits
        .to_file(&output)
        .with_context(|| format!("cannot write {}", output.display()))?;

    if opts.verbose {
        println!("copied {copied} header keyword(s)");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfitz::fitskit::{FitsFile, Header, HeaderValue, ImageData, PixelData};
    use tempfile::TempDir;

    fn opts(output: Option<&Path>, yes: bool) -> CopyHeaderOptions {
        CopyHeaderOptions {
            yes,
            output: output.map(Path::to_path_buf),
            ..CopyHeaderOptions::default()
        }
    }

    /// Build a small 2D I16 image, writing it to `path`. When `with_metadata`
    /// is set it carries `OBJECT`/`DATE-OBS` keywords a `copy-header` run can
    /// fill into a target that lacks them.
    fn write_fits(path: &Path, width: usize, height: usize, with_metadata: bool) {
        let pixels: Vec<i16> = (0..(width * height) as i16).collect();
        let img = ImageData::new(vec![width, height], PixelData::I16(pixels));
        let mut fits = FitsFile::with_primary_image(img);
        if with_metadata {
            let header = &mut fits.primary_mut().header;
            header.set("OBJECT", HeaderValue::String("M31".to_string()), None);
            header.set(
                "DATE-OBS",
                HeaderValue::String("2026-06-22T00:00:00".to_string()),
                None,
            );
        }
        fits.to_file(path).unwrap();
    }

    fn read_header(path: &Path) -> Header {
        FitsFile::from_file(path).unwrap().primary().header.clone()
    }

    #[test]
    fn copy_header_file_fills_in_missing_metadata_in_place() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source.fits");
        write_fits(&source, 8, 8, true);
        let target = tmp.path().join("target.fits");
        write_fits(&target, 8, 8, false);

        copy_header_file(&source, &target, &opts(None, true)).unwrap();

        let header = read_header(&target);
        assert_eq!(header.get_string("OBJECT"), Some("M31"));
        assert_eq!(header.get_string("DATE-OBS"), Some("2026-06-22T00:00:00"));
    }

    #[test]
    fn copy_header_file_does_not_overwrite_existing_target_metadata() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source.fits");
        write_fits(&source, 8, 8, true);
        let target = tmp.path().join("target.fits");
        write_fits(&target, 8, 8, false);
        {
            let mut target_fits = FitsFile::from_file(&target).unwrap();
            target_fits.primary_mut().header.set(
                "OBJECT",
                HeaderValue::String("Target".to_string()),
                None,
            );
            target_fits.to_file(&target).unwrap();
        }

        copy_header_file(&source, &target, &opts(None, true)).unwrap();

        assert_eq!(
            read_header(&target).get_string("OBJECT"),
            Some("Target"),
            "a keyword the target already carries must not be overwritten"
        );
    }

    #[test]
    fn copy_header_file_writes_to_explicit_output_leaving_target_untouched() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source.fits");
        write_fits(&source, 8, 8, true);
        let target = tmp.path().join("target.fits");
        write_fits(&target, 8, 8, false);
        let output = tmp.path().join("output.fits");

        copy_header_file(&source, &target, &opts(Some(&output), true)).unwrap();

        assert_eq!(read_header(&output).get_string("OBJECT"), Some("M31"));
        assert_eq!(
            read_header(&target).get_string("OBJECT"),
            None,
            "the target file itself must be untouched when -o is given"
        );
    }

    #[test]
    fn copy_header_file_in_place_bypasses_the_overwrite_guard() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source.fits");
        write_fits(&source, 8, 8, true);
        let target = tmp.path().join("target.fits");
        write_fits(&target, 8, 8, false);

        // output == target (the default, no -o given) must not trip the
        // "already exists" guard even with `yes: false`.
        copy_header_file(&source, &target, &opts(None, false)).unwrap();

        assert_eq!(read_header(&target).get_string("OBJECT"), Some("M31"));
    }

    #[test]
    fn copy_header_file_refuses_to_overwrite_a_different_output_without_yes() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source.fits");
        write_fits(&source, 8, 8, true);
        let target = tmp.path().join("target.fits");
        write_fits(&target, 8, 8, false);
        let output = tmp.path().join("output.fits");
        std::fs::write(&output, b"pre-existing").unwrap();

        let err = copy_header_file(&source, &target, &opts(Some(&output), false)).unwrap_err();

        assert!(err.to_string().contains("already exists"));
        assert_eq!(std::fs::read(&output).unwrap(), b"pre-existing");
    }

    #[test]
    fn copy_header_file_overwrites_a_different_output_with_yes() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source.fits");
        write_fits(&source, 8, 8, true);
        let target = tmp.path().join("target.fits");
        write_fits(&target, 8, 8, false);
        let output = tmp.path().join("output.fits");
        std::fs::write(&output, b"pre-existing").unwrap();

        copy_header_file(&source, &target, &opts(Some(&output), true)).unwrap();

        assert_ne!(std::fs::read(&output).unwrap(), b"pre-existing");
        assert_eq!(read_header(&output).get_string("OBJECT"), Some("M31"));
    }

    #[test]
    fn copy_header_file_errors_when_source_has_no_image() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source.fits");
        FitsFile::with_empty_primary().to_file(&source).unwrap();
        let target = tmp.path().join("target.fits");
        write_fits(&target, 8, 8, false);

        assert!(copy_header_file(&source, &target, &opts(None, true)).is_err());
    }
}
