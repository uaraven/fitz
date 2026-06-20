use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use fitskit::{FitsFile, HduData};

use crate::fits_image::{ensure_can_write, print_progress};
use crate::options::Options;

pub fn decompress_file(input: &Path, output: &Path, opts: &Options) -> Result<()> {
    // Decompressing in place (output == input) is allowed and must not trip
    // the "already exists" guard.
    if output != input {
        ensure_can_write(output, opts.force)?;
    }
    print_progress(opts.verbose, input, output);

    let fits = FitsFile::from_file(input)
        .with_context(|| format!("cannot read {}", input.display()))?;

    let mut first_image: Option<fitskit::ImageData> = None;
    let mut extra_hdus: Vec<fitskit::Hdu> = Vec::new();

    for hdu in &fits.hdus {
        if let Some(cimg) = hdu.as_compressed_image() {
            let img = cimg
                .decompress()
                .with_context(|| "decompression failed")?;
            if first_image.is_none() {
                first_image = Some(img);
            } else {
                extra_hdus.push(fitskit::Hdu::primary_image(img));
            }
        } else {
            match &hdu.data {
                HduData::Empty => {}
                _ => extra_hdus.push(hdu.clone()),
            }
        }
    }

    let mut out_fits = match first_image {
        Some(img) => FitsFile::with_primary_image(img),
        None => bail!("no compressed image found in {}", input.display()),
    };

    for hdu in extra_hdus {
        out_fits.push_extension(hdu);
    }

    out_fits
        .to_file(output)
        .with_context(|| format!("cannot write {}", output.display()))?;

    if !opts.keep && opts.output.is_none() && output != input {
        fs::remove_file(input)
            .with_context(|| format!("cannot remove {}", input.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::Options;
    use crate::test_support::{copy_to_temp, test_data};
    use tempfile::TempDir;

    #[test]
    fn decompress_produces_fits_file() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let output = input.with_extension("");
        decompress_file(&input, &output, &Options { keep: true, ..Options::default() }).unwrap();
        assert!(output.exists());
    }

    #[test]
    fn decompress_removes_input_by_default() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let output = input.with_extension("");
        decompress_file(&input, &output, &Options::default()).unwrap();
        assert!(!input.exists());
    }

    #[test]
    fn decompress_keeps_input_with_keep_flag() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let output = input.with_extension("");
        decompress_file(&input, &output, &Options { keep: true, ..Options::default() }).unwrap();
        assert!(input.exists());
    }

    #[test]
    fn decompress_errors_if_output_exists_without_force() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let output = input.with_extension("");
        std::fs::write(&output, b"dummy").unwrap();
        let err = decompress_file(&input, &output, &Options { keep: true, ..Options::default() }).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn decompress_force_overwrites_existing_output() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let output = input.with_extension("");
        std::fs::write(&output, b"dummy").unwrap();
        decompress_file(&input, &output, &Options { keep: true, force: true, ..Options::default() }).unwrap();
        assert!(output.metadata().unwrap().len() > 5);
    }

    #[test]
    fn decompress_without_fz_extension_replaces_file_in_place() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("compressed.fits");
        std::fs::copy(test_data("compressed.fits.fz"), &input).unwrap();
        decompress_file(&input, &input, &Options::default()).unwrap();
        assert!(input.exists());
    }

    #[test]
    fn decompress_keeps_input_when_output_path_given() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let out = tmp.path().join("out.fits");
        decompress_file(&input, &out, &Options {
            output: Some(out.clone()),
            ..Options::default()
        }).unwrap();
        assert!(input.exists());
    }

    #[test]
    fn decompress_with_custom_output_path() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let custom_out = tmp.path().join("custom.fits");
        decompress_file(&input, &custom_out, &Options {
            keep: true,
            output: Some(custom_out.clone()),
            ..Options::default()
        }).unwrap();
        assert!(custom_out.exists());
        assert!(!tmp.path().join("compressed.fits").exists());
    }
}
