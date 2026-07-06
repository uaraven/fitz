//! Decompress every tile-compressed image HDU in a FITS file, restoring the
//! original header (minus the compressed-container/`Z*` keywords).

use std::path::Path;

use anyhow::{Context, Result, bail};
use fitskit::{FitsFile, HduData, Header};

use crate::fits_image::carry_over_metadata;

/// Read `input` and decompress every tile-compressed (`ZIMAGE`) image HDU,
/// returning the resulting in-memory `FitsFile`, ready for the caller to write
/// out. The compressed HDU's header carries the original image metadata
/// (OBJECT, DATE-OBS, BAYERPAT, WCS, …) alongside the BINTABLE/tile-compression
/// container keywords; those container keywords are stripped so decompress is
/// a faithful round-trip of the original mosaic.
pub fn decompress(input: &Path) -> Result<FitsFile> {
    let fits =
        FitsFile::from_file(input).with_context(|| format!("cannot read {}", input.display()))?;

    let mut first_image: Option<(fitskit::ImageData, Header)> = None;
    let mut extra_hdus: Vec<fitskit::Hdu> = Vec::new();

    for hdu in &fits.hdus {
        if let Some(cimg) = hdu.as_compressed_image() {
            let img = cimg.decompress().with_context(|| "decompression failed")?;
            if first_image.is_none() {
                first_image = Some((img, hdu.header.clone()));
            } else {
                let mut ext = fitskit::Hdu::primary_image(img);
                carry_over_metadata(&mut ext.header, &hdu.header);
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
            carry_over_metadata(&mut fits.primary_mut().header, &src_header);
            fits
        }
        None => bail!("no compressed image found in {}", input.display()),
    };

    for hdu in extra_hdus {
        out_fits.push_extension(hdu);
    }

    Ok(out_fits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::copy_to_temp;
    use tempfile::TempDir;

    #[test]
    fn decompress_produces_a_plain_image_hdu() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let fits = decompress(&input).unwrap();
        assert!(matches!(fits.primary().data, HduData::Image(_)));
    }

    #[test]
    fn decompress_keeps_bayerpat_but_drops_container_keywords() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);
        let fits = decompress(&input).unwrap();

        let header = &fits.primary().header;
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
    fn decompress_errors_when_no_compressed_image_present() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        let err = match decompress(&input) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(err.to_string().contains("no compressed image"));
    }
}
