use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use fitskit::{CompressOptions, CompressionType, FitsFile, HduData};

pub fn compress_file(input: &Path, keep: bool, force: bool, verbose: bool) -> Result<()> {
    if input.extension().map(|e| e == "fz").unwrap_or(false) {
        bail!("already has .fz extension — skipping (use -d to decompress)");
    }

    let mut out_os: OsString = input.as_os_str().to_owned();
    out_os.push(".fz");
    let output = PathBuf::from(out_os);

    if output.exists() && !force {
        bail!(
            "{} already exists — use -f to overwrite",
            output.display()
        );
    }

    if verbose {
        println!("{} -> {}", input.display(), output.display());
    }

    let fits = FitsFile::from_file(input)
        .with_context(|| format!("cannot read {}", input.display()))?;

    let opts = CompressOptions {
        algorithm: CompressionType::Rice1,
        ..CompressOptions::default()
    };

    let mut out_fits = FitsFile::with_empty_primary();

    for hdu in &fits.hdus {
        match &hdu.data {
            HduData::Image(img) => {
                let compressed = img
                    .compress(&opts)
                    .with_context(|| "RICE_1 compression failed")?;
                out_fits.push_extension(compressed);
            }
            HduData::Empty => {} // primary empty HDU is already covered by with_empty_primary()
            _ => {
                // Preserve non-image HDUs (ASCII/BINTABLE) unchanged
                out_fits.push_extension(hdu.clone());
            }
        }
    }

    out_fits
        .to_file(&output)
        .with_context(|| format!("cannot write {}", output.display()))?;

    if !keep {
        fs::remove_file(input)
            .with_context(|| format!("cannot remove {}", input.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fitskit::{FitsFile, HduData};
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
    fn compress_produces_fz_file() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        compress_file(&input, true, false, false).unwrap();
        assert!(tmp.path().join("uncompressed.fit.fz").exists());
    }

    #[test]
    fn compress_removes_input_by_default() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        compress_file(&input, false, false, false).unwrap();
        assert!(!input.exists());
    }

    #[test]
    fn compress_keeps_input_with_keep_flag() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        compress_file(&input, true, false, false).unwrap();
        assert!(input.exists());
    }

    #[test]
    fn compress_errors_if_output_exists_without_force() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        std::fs::write(tmp.path().join("uncompressed.fit.fz"), b"dummy").unwrap();
        let err = compress_file(&input, true, false, false).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn compress_force_overwrites_existing_output() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        let output = tmp.path().join("uncompressed.fit.fz");
        std::fs::write(&output, b"dummy").unwrap();
        compress_file(&input, true, true, false).unwrap();
        assert!(output.metadata().unwrap().len() > 5);
    }

    #[test]
    fn compress_rejects_fz_input() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let err = compress_file(&input, true, false, false).unwrap_err();
        assert!(err.to_string().contains(".fz extension"));
    }

    #[test]
    fn round_trip_preserves_pixel_data() {
        let tmp = TempDir::new().unwrap();

        // Capture original axes and pixel bytes before any transformation.
        let orig = FitsFile::from_file(&test_data("uncompressed.fit")).unwrap();
        let orig_images: Vec<_> = orig
            .hdus
            .iter()
            .filter_map(|h| {
                if let HduData::Image(img) = &h.data {
                    Some((img.axes.clone(), img.pixels.to_bytes()))
                } else {
                    None
                }
            })
            .collect();

        let input = copy_to_temp("uncompressed.fit", &tmp);
        let fz_path = PathBuf::from({
            let mut s = input.as_os_str().to_owned();
            s.push(".fz");
            s
        });

        // Compress (removes input), then decompress (removes .fz, recreates input).
        compress_file(&input, false, false, false).unwrap();
        crate::decompress::decompress_file(&fz_path, false, false, false).unwrap();

        let result = FitsFile::from_file(&input).unwrap();
        let result_images: Vec<_> = result
            .hdus
            .iter()
            .filter_map(|h| {
                if let HduData::Image(img) = &h.data {
                    Some((img.axes.clone(), img.pixels.to_bytes()))
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(result_images.len(), orig_images.len());
        for (result_img, orig_img) in result_images.iter().zip(orig_images.iter()) {
            assert_eq!(result_img.0, orig_img.0, "axes mismatch");
            assert_eq!(result_img.1, orig_img.1, "pixel data mismatch");
        }
    }
}
