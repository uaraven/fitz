use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use fitskit::{CompressOptions, FitsFile, HduData};

use crate::fits_image::{ensure_can_write, print_progress, print_step};
use crate::options::Options;

pub fn compress_file(input: &Path, output: &Path, opts: &Options) -> Result<()> {
    ensure_can_write(output, opts.force)?;
    print_progress(opts.verbose, input, output);

    print_step(opts.verbose, "reading");
    let fits = FitsFile::from_file(input)
        .with_context(|| format!("cannot read {}", input.display()))?;

    let compress_opts = CompressOptions {
        algorithm: opts.algorithm,
        ..CompressOptions::default()
    };

    let mut out_fits = FitsFile::with_empty_primary();

    print_step(opts.verbose, "compressing");
    for hdu in &fits.hdus {
        match &hdu.data {
            HduData::Image(img) => {
                let compressed = img
                    .compress(&compress_opts)
                    .with_context(|| "compression failed")?;
                out_fits.push_extension(compressed);
            }
            HduData::Empty => {}
            _ => {
                out_fits.push_extension(hdu.clone());
            }
        }
    }

    print_step(opts.verbose, "writing");
    out_fits
        .to_file(output)
        .with_context(|| format!("cannot write {}", output.display()))?;

    if !opts.keep && opts.output.is_none() {
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
    use fitskit::{FitsFile, HduData};
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn with_fz(p: &Path) -> PathBuf {
        let mut s: OsString = p.as_os_str().to_owned();
        s.push(".fz");
        PathBuf::from(s)
    }

    #[test]
    fn compress_produces_fz_file() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        let output = with_fz(&input);
        compress_file(&input, &output, &Options { keep: true, ..Options::default() }).unwrap();
        assert!(output.exists());
    }

    #[test]
    fn compress_removes_input_by_default() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        let output = with_fz(&input);
        compress_file(&input, &output, &Options::default()).unwrap();
        assert!(!input.exists());
    }

    #[test]
    fn compress_keeps_input_with_keep_flag() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        let output = with_fz(&input);
        compress_file(&input, &output, &Options { keep: true, ..Options::default() }).unwrap();
        assert!(input.exists());
    }

    #[test]
    fn compress_errors_if_output_exists_without_force() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        let output = with_fz(&input);
        std::fs::write(&output, b"dummy").unwrap();
        let err = compress_file(&input, &output, &Options { keep: true, ..Options::default() }).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn compress_force_overwrites_existing_output() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        let output = with_fz(&input);
        std::fs::write(&output, b"dummy").unwrap();
        compress_file(&input, &output, &Options { keep: true, force: true, ..Options::default() }).unwrap();
        assert!(output.metadata().unwrap().len() > 5);
    }

    #[test]
    fn compress_keeps_input_when_output_path_given() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        let out = tmp.path().join("out.fz");
        compress_file(&input, &out, &Options {
            output: Some(out.clone()),
            ..Options::default()
        }).unwrap();
        assert!(input.exists());
    }

    #[test]
    fn round_trip_preserves_pixel_data() {
        let tmp = TempDir::new().unwrap();

        let orig = FitsFile::from_file(test_data("uncompressed.fit")).unwrap();
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
        let fz = with_fz(&input);

        compress_file(&input, &fz, &Options::default()).unwrap();
        crate::decompress::decompress_file(&fz, &input, &Options::default()).unwrap();

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
