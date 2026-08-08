//! Demosaic a raw CFA mosaic [`Image`] into an interleaved RGB one.

use std::io::Cursor;

use crate::data::{Image, ImageType, PixelBuffer};
use anyhow::{Result, anyhow};
use bayer::{BayerDepth, CFA, RasterDepth, RasterMut, run_demosaic};
use rayon::prelude::*;

#[cfg(target_endian = "little")]
const NATIVE_BAYER_DEPTH16: BayerDepth = BayerDepth::Depth16LE;
#[cfg(target_endian = "big")]
const NATIVE_BAYER_DEPTH16: BayerDepth = BayerDepth::Depth16BE;

/// The two green sites within a 2x2 CFA cell, as `(x, y)` offsets.
pub(crate) fn green_sites(cfa: CFA) -> Vec<(usize, usize)> {
    match cfa {
        CFA::RGGB | CFA::BGGR => vec![(1, 0), (0, 1)],
        CFA::GBRG | CFA::GRBG => vec![(0, 0), (1, 1)],
    }
}

pub(crate) fn blue_sites(cfa: CFA) -> Vec<(usize, usize)> {
    match cfa {
        CFA::RGGB => vec![(1, 1)],
        CFA::BGGR => vec![(0, 0)],
        CFA::GBRG => vec![(1, 0)],
        CFA::GRBG => vec![(0, 1)],
    }
}
pub(crate) fn red_sites(cfa: CFA) -> Vec<(usize, usize)> {
    match cfa {
        CFA::RGGB => vec![(0, 0)],
        CFA::BGGR => vec![(1, 1)],
        CFA::GBRG => vec![(0, 1)],
        CFA::GRBG => vec![(1, 0)],
    }
}

pub(crate) fn cfa_pixels(image: &Image, pattern: &Vec<(usize, usize)>) -> (usize, usize, Vec<u16>) {
    let (pw, ph) = (image.width / 2, image.height / 2);
    let values = &image.pixels.as_u16ref();
    let plane = (0..ph)
        .into_par_iter()
        .flat_map_iter(|cy| {
            let site = move |gx: usize, gy: usize, cx: usize| {
                values[(2 * cy + gy) * image.width + 2 * cx + gx] as u32
            };
            (0..pw).map(move |cx| {
                (pattern.iter().map(|p| site(p.0, p.1, cx)).sum::<u32>() / pattern.len() as u32)
                    as u16
            })
        })
        .collect();
    (pw, ph, plane)
}

fn demosaic_to_rgb(
    img: &PixelBuffer,
    width: usize,
    height: usize,
    cfa: CFA,
) -> Result<PixelBuffer> {
    let raw = img.as_u16_bytes();

    // we always work with 16-bit images internally, meaning 6 bytes per RGB pixel
    let mut out_buf = vec![0u8; 6 * width * height];
    {
        let mut raster = RasterMut::new(width, height, RasterDepth::Depth16, &mut out_buf);
        run_demosaic(
            &mut Cursor::new(&raw[..]),
            NATIVE_BAYER_DEPTH16,
            cfa,
            bayer::Demosaic::Linear,
            &mut raster,
        )
        .map_err(|e| anyhow!("{e}"))?;
    }

    let rgb = PixelBuffer::U16(
        out_buf
            .par_chunks_exact(2)
            .map(|c| u16::from_ne_bytes([c[0], c[1]]))
            .collect(),
    );

    Ok(rgb)
}

impl Image {
    /// Debayers the image into the RGB image
    pub fn debayer(&self) -> Option<Result<Image>> {
        if let ImageType::CFA(cfa) = self.image_type {
            let rgb_pixels = demosaic_to_rgb(&self.pixels, self.width, self.height, cfa);
            Some(rgb_pixels.map(|rgb_pixels| {
                Image::new(
                    ImageType::RGB,
                    self.header.clone(),
                    self.width,
                    self.height,
                    rgb_pixels,
                )
            }))
        } else {
            None
        }
    }

    /// This image reinterpreted as a raw mosaic with Bayer pattern `cfa`,
    /// overriding whatever [`load_fits`](crate::fits_file::load_fits) inferred
    /// from the header. Backs the CLI's `--pattern` / `--force-demosaic`, which
    /// exist for frames whose `BAYERPAT` is missing or wrong.
    ///
    /// Only a single-plane image can be a mosaic, so an already-debayered RGB
    /// cube is rejected rather than silently reinterpreted.
    pub fn with_pattern(self, cfa: CFA) -> Result<Image> {
        if self.image_type == ImageType::RGB {
            return Err(anyhow!(
                "expected a 2D mosaic image, found an already-debayered RGB cube"
            ));
        }
        Ok(Image {
            image_type: ImageType::CFA(cfa),
            ..self
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fits_file::load_fits;
    use crate::test_support::{test_data, write_mosaic_fits, write_rgb_cube_fits};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    #[test]
    fn debayer_cfa_image() {
        let test_img = load_fits(&test_data("cfa_orion.fits")).unwrap();
        let debayered = test_img.debayer().unwrap().unwrap();

        assert_eq!(debayered.image_type, ImageType::RGB);
        assert_eq!(debayered.width, test_img.width);
        assert_eq!(debayered.height, test_img.height);

        let sha = format!("{:x}", Sha256::digest(debayered.pixels.as_u16_bytes()));
        assert_eq!(
            sha,
            "d4166430a8d94b586bee199d500fe26a25af0dbe65f79f932e786f2f7c8a0beb"
        );
    }

    #[test]
    fn debayer_returns_none_for_a_non_mosaic() {
        // A 2D frame with no BAYERPAT is an already-debayered mono image, and a
        // 3-plane cube is an already-debayered colour one: neither is a mosaic.
        let tmp = TempDir::new().unwrap();
        let mono = tmp.path().join("mono.fits");
        write_mosaic_fits(&mono, 8, 6, None);
        assert!(load_fits(&mono).unwrap().debayer().is_none());

        let cube = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&cube, 8, 6);
        assert!(load_fits(&cube).unwrap().debayer().is_none());
    }

    #[test]
    fn with_pattern_makes_an_untagged_mono_frame_demosaicable() {
        // No BAYERPAT in the header, so `load_fits` calls it grayscale; the
        // override is what `--pattern` gives a frame whose camera didn't write
        // the keyword.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("mono.fits");
        write_mosaic_fits(&path, 8, 6, None);

        let img = load_fits(&path).unwrap();
        assert_eq!(img.image_type, ImageType::Grayscale);

        let forced = img.with_pattern(CFA::BGGR).unwrap();
        assert_eq!(forced.image_type, ImageType::CFA(CFA::BGGR));
        let rgb = forced.debayer().unwrap().unwrap();
        assert_eq!(rgb.image_type, ImageType::RGB);
        assert_eq!(rgb.pixels.len(), 8 * 6 * 3);
    }

    #[test]
    fn with_pattern_rejects_an_already_debayered_cube() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&path, 4, 3);

        let err = load_fits(&path)
            .unwrap()
            .with_pattern(CFA::RGGB)
            .unwrap_err();
        assert!(err.to_string().contains("2D mosaic image"));
    }
}
