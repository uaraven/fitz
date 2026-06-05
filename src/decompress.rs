use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use fitskit::{FitsFile, HduData};

use crate::options::Options;

pub fn decompress_file(input: &Path, opts: &Options) -> Result<()> {
    if let Some(ext) = input.extension() && ext != "fz" {
        bail!("not a .fz file");
    }

    // Remove .fz suffix: "image.fits.fz" -> "image.fits"
    let output: PathBuf = match opts.output.as_deref() {
        Some(p) => p.to_path_buf(),
        None => input.with_extension(""),
    };

    if output.exists() && !opts.force {
        bail!(
            "{} already exists — use -f to overwrite",
            output.display()
        );
    }

    if opts.verbose {
        println!("{} -> {}", input.display(), output.display());
    }

    let fits = FitsFile::from_file(input)
        .with_context(|| format!("cannot read {}", input.display()))?;

    // The first decompressed image becomes the primary HDU (matches funpack behaviour).
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
                HduData::Empty => {} // skip the shell empty primary
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
        .to_file(&output)
        .with_context(|| format!("cannot write {}", output.display()))?;

    if !opts.keep {
        fs::remove_file(input)
            .with_context(|| format!("cannot remove {}", input.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::Options;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn test_data(filename: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test-data")
            .join(filename)
    }

    fn copy_to_temp(filename: &str, tmp: &TempDir) -> PathBuf {
        let dst = tmp.path().join(filename);
        std::fs::copy(test_data(filename), &dst).unwrap();
        dst
    }

    #[test]
    fn decompress_produces_fits_file() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        decompress_file(&input, &Options { keep: true, ..Options::default() }).unwrap();
        assert!(tmp.path().join("compressed.fits").exists());
    }

    #[test]
    fn decompress_removes_input_by_default() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        decompress_file(&input, &Options::default()).unwrap();
        assert!(!input.exists());
    }

    #[test]
    fn decompress_keeps_input_with_keep_flag() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        decompress_file(&input, &Options { keep: true, ..Options::default() }).unwrap();
        assert!(input.exists());
    }

    #[test]
    fn decompress_errors_if_output_exists_without_force() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        std::fs::write(tmp.path().join("compressed.fits"), b"dummy").unwrap();
        let err = decompress_file(&input, &Options { keep: true, ..Options::default() }).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn decompress_force_overwrites_existing_output() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let output = tmp.path().join("compressed.fits");
        std::fs::write(&output, b"dummy").unwrap();
        decompress_file(&input, &Options { keep: true, force: true, ..Options::default() }).unwrap();
        assert!(output.metadata().unwrap().len() > 5);
    }

    #[test]
    fn decompress_rejects_non_fz_input() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        let err = decompress_file(&input, &Options { keep: true, ..Options::default() }).unwrap_err();
        assert!(err.to_string().contains("not a .fz file"));
    }

    #[test]
    fn decompress_with_custom_output_path() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let custom_out = tmp.path().join("custom.fits");
        decompress_file(&input, &Options {
            keep: true,
            output: Some(custom_out.clone()),
            ..Options::default()
        }).unwrap();
        assert!(custom_out.exists());
        assert!(!tmp.path().join("compressed.fits").exists());
    }
}
