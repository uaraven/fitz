//! Tile-compress every image HDU in a FITS file, carrying over the original
//! metadata onto the compressed container.

use std::path::Path;

use anyhow::{Context, Result};
use fitskit::{CompressOptions as FitskitCompressOptions, CompressionType, FitsFile, HduData};

use crate::fits_image::carry_over_metadata;

/// Domain options controlling how a FITS file is tile-compressed.
pub struct CompressOptions {
    pub algorithm: CompressionType,
}

impl Default for CompressOptions {
    fn default() -> Self {
        CompressOptions {
            algorithm: CompressionType::Rice1,
        }
    }
}

/// Read `input`, tile-compress every image HDU, and return the resulting
/// in-memory `FitsFile`, ready for the caller to write out. Each compressed
/// HDU's header carries over the original image's metadata (BAYERPAT, OBJECT,
/// RA/DEC, WCS, pixel scaling, …), so the output is self-describing and
/// [`crate::decompress::decompress`] round-trips it faithfully.
pub fn compress(input: &Path, opts: &CompressOptions) -> Result<FitsFile> {
    let fits =
        FitsFile::from_file(input).with_context(|| format!("cannot read {}", input.display()))?;

    let compress_opts = FitskitCompressOptions {
        algorithm: opts.algorithm,
        ..FitskitCompressOptions::default()
    };

    let mut out_fits = FitsFile::with_empty_primary();

    for hdu in &fits.hdus {
        match &hdu.data {
            HduData::Image(img) => {
                let mut compressed = img
                    .compress(&compress_opts)
                    .with_context(|| "compression failed")?;
                carry_over_metadata(&mut compressed.header, &hdu.header);
                out_fits.push_extension(compressed);
            }
            HduData::Empty => {}
            _ => {
                out_fits.push_extension(hdu.clone());
            }
        }
    }

    Ok(out_fits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompress::decompress;
    use crate::test_support::{copy_to_temp, test_data};
    use fitskit::{FitsFile, HduData};
    use tempfile::TempDir;

    #[test]
    fn compress_produces_a_compressed_hdu() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        let out_fits = compress(&input, &CompressOptions::default()).unwrap();
        assert!(out_fits.hdus.iter().any(|h| h.as_compressed_image().is_some()));
    }

    #[test]
    fn compress_carries_original_header_onto_compressed_hdu() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        let out_fits = compress(&input, &CompressOptions::default()).unwrap();

        let cimg_hdu = out_fits
            .hdus
            .iter()
            .find(|h| h.as_compressed_image().is_some())
            .expect("a compressed image HDU");
        for kw in ["BAYERPAT", "OBJECT", "GAIN", "RA", "DEC"] {
            assert!(
                cimg_hdu.header.find(kw).is_some(),
                "{kw} was dropped from the compressed HDU header"
            );
        }
        // ...including the pixel-scaling keywords needed for a faithful round-trip.
        assert_eq!(cimg_hdu.header.get_float("BZERO"), Some(32768.0));
        assert_eq!(cimg_hdu.header.get_float("BSCALE"), Some(1.0));
    }

    #[test]
    fn round_trip_preserves_metadata_and_scaling() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("uncompressed.fit", &tmp);
        let fz = tmp.path().join("out.fz");

        let compressed = compress(&input, &CompressOptions::default()).unwrap();
        compressed.to_file(&fz).unwrap();

        let restored = decompress(&fz).unwrap();
        let header = &restored.primary().header;
        assert!(header.find("BAYERPAT").is_some());
        assert!(header.find("OBJECT").is_some());
        // Scaling must survive so unsigned-16 pixels keep their physical values.
        assert_eq!(header.get_float("BZERO"), Some(32768.0));
        assert_eq!(header.get_float("BSCALE"), Some(1.0));
        // No leaked compressed-container keywords.
        for kw in ["ZIMAGE", "ZCMPTYPE", "TFORM1", "TFIELDS"] {
            assert!(header.find(kw).is_none(), "{kw} leaked into output");
        }
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
        let fz = tmp.path().join("out.fz");

        let compressed = compress(&input, &CompressOptions::default()).unwrap();
        compressed.to_file(&fz).unwrap();
        let result = decompress(&fz).unwrap();

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
