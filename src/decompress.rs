use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fitskit::{FitsFile, HduData, Header};

use crate::fits_image::{copy_metadata, ensure_can_write, print_progress, print_step};
use crate::options::Options;

pub fn decompress_file(input: &Path, output: &Path, opts: &Options) -> Result<()> {
    // Decompressing in place (output == input) is allowed and must not trip
    // the "already exists" guard.
    if output != input {
        ensure_can_write(output, opts.yes)?;
    }
    print_progress(opts.verbose, input, output);

    print_step(opts.verbose, "reading");
    let fits =
        FitsFile::from_file(input).with_context(|| format!("cannot read {}", input.display()))?;

    // The compressed HDU's header carries the original image metadata (OBJECT,
    // DATE-OBS, BAYERPAT, WCS, …) alongside the BINTABLE/tile-compression
    // container keywords; `copy_metadata` keeps the former and strips the
    // latter. BAYERPAT is preserved (empty `extra_drop`) so decompress is a
    // faithful round-trip of the original mosaic.
    let mut first_image: Option<(fitskit::ImageData, Header)> = None;
    let mut extra_hdus: Vec<fitskit::Hdu> = Vec::new();

    print_step(opts.verbose, "decompressing");
    for hdu in &fits.hdus {
        if let Some(cimg) = hdu.as_compressed_image() {
            let img = cimg.decompress().with_context(|| "decompression failed")?;
            if first_image.is_none() {
                first_image = Some((img, hdu.header.clone()));
            } else {
                let mut ext = fitskit::Hdu::primary_image(img);
                copy_metadata(&mut ext.header, &hdu.header, &[]);
                extra_hdus.push(ext);
            }
        } else {
            match &hdu.data {
                HduData::Empty => {}
                _ => extra_hdus.push(hdu.clone()),
            }
        }
    }

    let mut out_fits = match first_image {
        Some((img, src_header)) => {
            let mut fits = FitsFile::with_primary_image(img);
            copy_metadata(&mut fits.primary_mut().header, &src_header, &[]);
            fits
        }
        None => bail!("no compressed image found in {}", input.display()),
    };

    for hdu in extra_hdus {
        out_fits.push_extension(hdu);
    }

    print_step(opts.verbose, "writing");
    out_fits
        .to_file(output)
        .with_context(|| format!("cannot write {}", output.display()))?;

    if !opts.keep && opts.output.is_none() && output != input {
        fs::remove_file(input).with_context(|| format!("cannot remove {}", input.display()))?;
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
        decompress_file(
            &input,
            &output,
            &Options {
                keep: true,
                ..Options::default()
            },
        )
        .unwrap();
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
        decompress_file(
            &input,
            &output,
            &Options {
                keep: true,
                ..Options::default()
            },
        )
        .unwrap();
        assert!(input.exists());
    }

    #[test]
    fn decompress_errors_if_output_exists_without_yes() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let output = input.with_extension("");
        std::fs::write(&output, b"dummy").unwrap();
        let err = decompress_file(
            &input,
            &output,
            &Options {
                keep: true,
                ..Options::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn decompress_yes_overwrites_existing_output() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let output = input.with_extension("");
        std::fs::write(&output, b"dummy").unwrap();
        decompress_file(
            &input,
            &output,
            &Options {
                keep: true,
                yes: true,
                ..Options::default()
            },
        )
        .unwrap();
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
        decompress_file(
            &input,
            &out,
            &Options {
                output: Some(out.clone()),
                ..Options::default()
            },
        )
        .unwrap();
        assert!(input.exists());
    }

    #[test]
    fn decompress_keeps_bayerpat_but_drops_container_keywords() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let output = input.with_extension("");
        decompress_file(
            &input,
            &output,
            &Options {
                keep: true,
                ..Options::default()
            },
        )
        .unwrap();

        let header = FitsFile::from_file(&output)
            .unwrap()
            .primary()
            .header
            .clone();
        // The original mosaic metadata is round-tripped...
        assert!(header.find("BAYERPAT").is_some());
        // ...but the compressed BINTABLE container keywords are stripped.
        for kw in [
            "ZIMAGE", "ZCMPTYPE", "TFORM1", "TFIELDS", "XTENSION", "EXTNAME",
        ] {
            assert!(
                header.find(kw).is_none(),
                "{kw} leaked into decompress output"
            );
        }
    }

    #[test]
    fn decompress_with_custom_output_path() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let custom_out = tmp.path().join("custom.fits");
        decompress_file(
            &input,
            &custom_out,
            &Options {
                keep: true,
                output: Some(custom_out.clone()),
                ..Options::default()
            },
        )
        .unwrap();
        assert!(custom_out.exists());
        assert!(!tmp.path().join("compressed.fits").exists());
    }
}
