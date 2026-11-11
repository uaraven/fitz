//! Cropping: cut the same pixel rectangle out of any number of frames while
//! keeping every FITS header.
//!
//! FITS files are cropped on the raw container ([`crop_fits`], the same level
//! `raw_fits` works at), so BITPIX, BSCALE/BZERO and every header card
//! survive untouched. Camera RAW files have no FITS container to preserve, so
//! they go through [`Image::crop`] and are written as FITS.
//!
//! A crop never silently clamps: a rectangle that doesn't fit the frame is an
//! error, so a batch over mixed sizes reports the frames it could not crop.

use std::path::Path;

use anyhow::{Result, anyhow, bail};
use fitskit::{FitsFile, HduData, Header, HeaderValue, ImageData, PixelData};

use crate::data::{Image, ImageType, PixelBuffer};
use crate::fits_bayer::{BayerHeader, parse_bayer_header};
use crate::fits_file::{SaveOptions, find_image_hdu_index, save_fits};
use crate::keywords::{BAYERPAT, add_history};
use crate::loader::{is_fits_file, load_image_from_file};
use crate::raw_fits::{load_raw_with_compression, save_raw};

/// A pixel rectangle in image coordinates: `(x, y)` is the top-left corner
/// (row 0 is the first row stored in the file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CropRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl CropRect {
    /// This rectangle with its origin and size rounded down to a multiple of
    /// `grid`. A mosaic sensor repeats its colour pattern every 2 (Bayer) or
    /// 6 (X-Trans) pixels, so cutting on that grid keeps the pattern — and the
    /// header's `BAYERPAT` — valid. `grid` of 0 or 1 changes nothing.
    pub fn aligned(self, grid: usize) -> CropRect {
        let g = grid.max(1);
        CropRect {
            x: self.x / g * g,
            y: self.y / g * g,
            width: self.width / g * g,
            height: self.height / g * g,
        }
    }

    /// Errors unless the rectangle is non-empty and lies inside a
    /// `width` x `height` image.
    pub fn validate(self, width: usize, height: usize) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            bail!("crop rectangle is empty");
        }
        let fits = |origin: usize, extent: usize, limit: usize| {
            origin.checked_add(extent).is_some_and(|end| end <= limit)
        };
        if !fits(self.x, self.width, width) || !fits(self.y, self.height, height) {
            bail!(
                "crop {}x{} at ({}, {}) does not fit the {width}x{height} image",
                self.width,
                self.height,
                self.x,
                self.y
            );
        }
        Ok(())
    }
}

/// Copy `rect` out of `src`, which holds `planes` stacked images of
/// `src_width` x `src_height` pixels with `stride` samples per pixel.
/// A planar FITS cube is `planes = n, stride = 1`; interleaved RGB is
/// `planes = 1, stride = 3`.
fn crop_samples<T: Copy>(
    src: &[T],
    (src_width, src_height): (usize, usize),
    planes: usize,
    stride: usize,
    rect: CropRect,
) -> Vec<T> {
    let row_len = rect.width * stride;
    let mut out = Vec::with_capacity(planes * rect.height * row_len);
    for plane in 0..planes {
        for row in 0..rect.height {
            let start = ((plane * src_height + rect.y + row) * src_width + rect.x) * stride;
            out.extend_from_slice(&src[start..start + row_len]);
        }
    }
    out
}

/// Subtract the crop origin from `CRPIX1`/`CRPIX2` (and their alternate-WCS
/// forms `CRPIX1A`…), so the sky coordinates still land on the same stars.
/// The rest of the WCS — including SIP distortion, which is relative to
/// `CRPIX` — stays valid as it is. Keeps each card's Integer/Float type.
fn shift_wcs(header: &mut Header, rect: CropRect) {
    for kw in &mut header.keywords {
        let Some(axis) = kw.name.strip_prefix("CRPIX") else {
            continue;
        };
        let shift = match axis.as_bytes() {
            [b'1'] | [b'1', b'A'..=b'Z'] => rect.x,
            [b'2'] | [b'2', b'A'..=b'Z'] => rect.y,
            _ => continue,
        };
        kw.value = match kw.value.take() {
            Some(HeaderValue::Float(v)) => Some(HeaderValue::Float(v - shift as f64)),
            Some(HeaderValue::Integer(v)) => Some(HeaderValue::Integer(v - shift as i64)),
            other => other,
        };
    }
}

/// Record the crop in the header: move the WCS reference pixel and add a
/// HISTORY card.
fn stamp_crop(header: &mut Header, rect: CropRect) {
    shift_wcs(header, rect);
    add_history(
        header,
        &format!(
            "cropped to {}x{} at ({}, {})",
            rect.width, rect.height, rect.x, rect.y
        ),
    );
}

/// The mosaic repeat period a FITS header implies: 2 for a Bayer `BAYERPAT`,
/// 6 for an X-Trans one, 1 otherwise.
fn header_grid(header: &Header) -> usize {
    match header.get_string(BAYERPAT).and_then(parse_bayer_header) {
        Some(BayerHeader::Standard(_)) => 2,
        Some(BayerHeader::XTrans(_)) => 6,
        None => 1,
    }
}

/// The mosaic repeat period of an image type (see [`CropRect::aligned`]).
fn image_grid(image_type: ImageType) -> usize {
    match image_type {
        ImageType::CFA(_) => 2,
        ImageType::XTrans(_) => 6,
        ImageType::RGB | ImageType::Grayscale => 1,
    }
}

/// Crop the image HDU of `fits` in place and return the rectangle actually
/// used (after aligning to the mosaic grid). The HDU must hold plain pixels —
/// load a `.fz` through [`crate::raw_fits::load_raw`] first.
///
/// Supports every BITPIX and 2D images or 3-axis cubes (planar on disk). All
/// header cards are kept; `CRPIX*` is shifted by the crop origin, the
/// now-stale `CHECKSUM`/`DATASUM` are dropped and a HISTORY card records the
/// crop. NAXIS* is regenerated by the writer from the new geometry.
pub fn crop_fits(fits: &mut FitsFile, rect: CropRect) -> Result<CropRect> {
    let idx = find_image_hdu_index(fits).ok_or_else(|| anyhow!("no image data"))?;
    let hdu = &mut fits.hdus[idx];
    let HduData::Image(img) = &mut hdu.data else {
        bail!("image is still tile-compressed; load it with raw_fits::load_raw first");
    };
    let (width, height, planes) = match img.axes.as_slice() {
        [w, h] => (*w, *h, 1),
        [w, h, p] => (*w, *h, *p),
        axes => bail!("cannot crop a {}-axis image", axes.len()),
    };
    // The geometry comes from the header, which is untrusted: the pixel buffer
    // must really hold that many samples before anything is sliced out of it.
    let samples = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(planes));
    if samples != Some(img.pixels.len()) {
        bail!(
            "header says {width}x{height}x{planes} but the file holds {} pixel samples",
            img.pixels.len()
        );
    }
    let grid = if img.axes.len() == 2 {
        header_grid(&hdu.header)
    } else {
        1
    };
    let rect = rect.aligned(grid);
    rect.validate(width, height)?;

    let dims = (width, height);
    let pixels = match &img.pixels {
        PixelData::U8(v) => PixelData::U8(crop_samples(v, dims, planes, 1, rect)),
        PixelData::I16(v) => PixelData::I16(crop_samples(v, dims, planes, 1, rect)),
        PixelData::I32(v) => PixelData::I32(crop_samples(v, dims, planes, 1, rect)),
        PixelData::I64(v) => PixelData::I64(crop_samples(v, dims, planes, 1, rect)),
        PixelData::F32(v) => PixelData::F32(crop_samples(v, dims, planes, 1, rect)),
        PixelData::F64(v) => PixelData::F64(crop_samples(v, dims, planes, 1, rect)),
    };
    let mut axes = vec![rect.width, rect.height];
    if img.axes.len() == 3 {
        axes.push(planes);
    }
    *img = ImageData::new(axes, pixels);

    hdu.header
        .keywords
        .retain(|k| k.name != "CHECKSUM" && k.name != "DATASUM");
    stamp_crop(&mut hdu.header, rect);
    Ok(rect)
}

/// Crop the FITS file `input` into `output`, losslessly. A tile-compressed
/// input is written back with the same compression algorithm. Returns the
/// rectangle actually used (see [`crop_fits`]).
pub fn crop_fits_file(input: &Path, output: &Path, rect: CropRect) -> Result<CropRect> {
    let (mut fits, compression) = load_raw_with_compression(input)?;
    let used = crop_fits(&mut fits, rect)?;
    save_raw(&fits, output, compression)?;
    Ok(used)
}

/// Crop any openable image — FITS (losslessly, see [`crop_fits_file`]) or
/// camera RAW — into the file `output`. A RAW file has no FITS container to
/// preserve, so it is decoded, cropped and written as an uncompressed 16-bit
/// FITS. Returns the rectangle actually used.
pub fn crop_file(input: &Path, output: &Path, rect: CropRect) -> Result<CropRect> {
    if is_fits_file(input) {
        return crop_fits_file(input, output, rect);
    }
    let image = load_image_from_file(input)?;
    let used = rect.aligned(image_grid(image.image_type));
    let cropped = image.crop(rect)?;
    save_fits(output, &cropped, SaveOptions::default())?;
    Ok(used)
}

impl Image {
    /// A copy of the `rect` region, aligned to the mosaic grid of a CFA or
    /// X-Trans image so the pattern stays valid. The header is kept, with
    /// `CRPIX*` shifted and a HISTORY card added. Errors if the (aligned)
    /// rectangle is empty or not inside the image.
    pub fn crop(&self, rect: CropRect) -> Result<Image> {
        let rect = rect.aligned(image_grid(self.image_type));
        rect.validate(self.width, self.height)?;
        let dims = (self.width, self.height);
        let stride = self.channels();
        let pixels = match &self.pixels {
            PixelBuffer::U16(v) => PixelBuffer::U16(crop_samples(v, dims, 1, stride, rect)),
            PixelBuffer::F32(v) => PixelBuffer::F32(crop_samples(v, dims, 1, stride, rect)),
        };
        let mut header = self.header.clone();
        stamp_crop(&mut header, rect);
        Ok(Image::new(
            self.image_type,
            header,
            rect.width,
            rect.height,
            pixels,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fits_file::load_fits;
    use crate::raw_fits::load_raw;
    use crate::test_support::test_data;
    use tempfile::TempDir;

    fn rect(x: usize, y: usize, width: usize, height: usize) -> CropRect {
        CropRect {
            x,
            y,
            width,
            height,
        }
    }

    /// The (header, pixels, axes) of a file's image HDU, decompressed.
    fn read(path: &Path) -> (Header, ImageData) {
        let fits = load_raw(path).unwrap();
        let idx = find_image_hdu_index(&fits).unwrap();
        let hdu = &fits.hdus[idx];
        let HduData::Image(img) = &hdu.data else {
            panic!("not a plain image")
        };
        (hdu.header.clone(), img.clone())
    }

    /// Row `row` of plane `plane`, columns `x..x + w`, of a planar f32/i16 image.
    fn slice_of<T: Copy>(
        src: &[T],
        axes: &[usize],
        plane: usize,
        row: usize,
        x: usize,
        w: usize,
    ) -> Vec<T> {
        let start = (plane * axes[1] + row) * axes[0] + x;
        src[start..start + w].to_vec()
    }

    #[test]
    fn aligned_rounds_down_to_the_grid() {
        assert_eq!(rect(3, 5, 11, 9).aligned(2), rect(2, 4, 10, 8));
        assert_eq!(rect(7, 13, 20, 20).aligned(6), rect(6, 12, 18, 18));
        assert_eq!(rect(3, 5, 11, 9).aligned(1), rect(3, 5, 11, 9));
        assert_eq!(rect(3, 5, 11, 9).aligned(0), rect(3, 5, 11, 9));
    }

    #[test]
    fn validate_rejects_empty_and_out_of_bounds() {
        assert!(rect(0, 0, 0, 5).validate(10, 10).is_err());
        assert!(rect(0, 0, 5, 0).validate(10, 10).is_err());
        assert!(rect(6, 0, 5, 5).validate(10, 10).is_err());
        assert!(rect(0, 6, 5, 5).validate(10, 10).is_err());
        assert!(rect(5, 5, 5, 5).validate(10, 10).is_ok());
    }

    #[test]
    fn crops_a_float_wcs_cube_keeping_headers() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("rgb_cropped.fits");
        let src = test_data("rgb.fits");
        let used = crop_fits_file(&src, &out, rect(101, 203, 300, 250)).unwrap();
        assert_eq!(used, rect(101, 203, 300, 250));

        let (src_header, src_img) = read(&src);
        let (out_header, out_img) = read(&out);
        assert_eq!(out_img.axes, vec![300, 250, 3]);
        assert_eq!(out_header.get_int("NAXIS1"), Some(300));
        assert_eq!(out_header.get_int("NAXIS2"), Some(250));
        assert_eq!(out_header.get_int("NAXIS3"), Some(3));
        let (PixelData::F32(s), PixelData::F32(o)) = (&src_img.pixels, &out_img.pixels) else {
            panic!("rgb.fits should be BITPIX -32")
        };
        for plane in 0..3 {
            for row in [0, 1, 124, 249] {
                assert_eq!(
                    slice_of(o, &out_img.axes, plane, row, 0, 300),
                    slice_of(s, &src_img.axes, plane, 203 + row, 101, 300),
                    "plane {plane} row {row}"
                );
            }
        }

        // The WCS reference pixel moved with the crop; the rest is untouched.
        assert_eq!(
            out_header.get_float("CRPIX1"),
            Some(src_header.get_float("CRPIX1").unwrap() - 101.0)
        );
        assert_eq!(
            out_header.get_float("CRPIX2"),
            Some(src_header.get_float("CRPIX2").unwrap() - 203.0)
        );
        for name in ["CRVAL1", "CRVAL2", "CTYPE1", "A_ORDER", "PC1_1"] {
            assert_eq!(
                out_header.find(name).map(|k| &k.value),
                src_header.find(name).map(|k| &k.value),
                "{name}"
            );
        }
        // Every non-structural source card survives, plus the HISTORY record.
        for kw in &src_header.keywords {
            if matches!(kw.name.as_str(), "CHECKSUM" | "DATASUM")
                || kw.name.starts_with("NAXIS")
                || kw.name.starts_with("CRPIX")
            {
                continue;
            }
            assert!(out_header.find(&kw.name).is_some(), "lost {}", kw.name);
        }
        assert!(out_header.keywords.iter().any(|k| k.name == "HISTORY"
            && k.comment.as_deref() == Some("cropped to 300x250 at (101, 203)")));
    }

    #[test]
    fn bayer_crop_snaps_to_the_pattern_grid_and_keeps_encoding() {
        let tmp = TempDir::new().unwrap();
        for (name, pattern) in [("cfa_orion.fits", "RGGB"), ("uncompressed.fit", "GRBG")] {
            let out = tmp.path().join(format!("cropped_{name}"));
            let src = test_data(name);
            let used = crop_fits_file(&src, &out, rect(11, 7, 401, 303)).unwrap();
            assert_eq!(used, rect(10, 6, 400, 302), "{name}");

            let (src_header, src_img) = read(&src);
            let (out_header, out_img) = read(&out);
            assert_eq!(out_img.axes, vec![400, 302], "{name}");
            assert_eq!(out_header.get_string("BAYERPAT"), Some(pattern));
            // The on-disk encoding is untouched: still 16-bit with the same BZERO.
            assert_eq!(out_img.bitpix(), src_img.bitpix());
            assert_eq!(out_header.get_float("BZERO"), src_header.get_float("BZERO"));
            let (PixelData::I16(s), PixelData::I16(o)) = (&src_img.pixels, &out_img.pixels) else {
                panic!("{name} should be BITPIX 16")
            };
            for row in [0, 150, 301] {
                assert_eq!(
                    slice_of(o, &out_img.axes, 0, row, 0, 400),
                    slice_of(s, &src_img.axes, 0, 6 + row, 10, 400),
                    "{name} row {row}"
                );
            }
        }
    }

    #[test]
    fn compressed_input_stays_compressed() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("cropped.fits.fz");
        crop_fits_file(&test_data("compressed.fits.fz"), &out, rect(8, 8, 256, 128)).unwrap();

        let fits = FitsFile::from_file(&out).unwrap();
        let idx = find_image_hdu_index(&fits).unwrap();
        assert!(fits.hdus[idx].as_compressed_image().is_some());
        let (_, img) = read(&out);
        assert_eq!(&img.axes[..2], &[256, 128]);
    }

    #[test]
    fn validate_survives_rectangles_that_would_overflow() {
        assert!(rect(usize::MAX, 0, 2, 2).validate(10, 10).is_err());
        assert!(rect(0, usize::MAX - 1, 2, 2).validate(10, 10).is_err());
        assert!(rect(1, 1, usize::MAX, 1).validate(10, 10).is_err());
    }

    #[test]
    fn a_pixel_buffer_that_disagrees_with_the_header_is_an_error_not_a_panic() {
        // 100x100 declared, but only half the samples present.
        let lying = ImageData::new(vec![100, 100], PixelData::I16(vec![7; 5_000]));
        let mut fits = FitsFile::with_primary_image(lying);
        let err = crop_fits(&mut fits, rect(10, 10, 50, 50)).unwrap_err();
        assert!(err.to_string().contains("5000 pixel samples"), "{err}");

        // And a declared size whose product overflows.
        let huge = ImageData::new(vec![usize::MAX, 4], PixelData::I16(vec![0; 16]));
        let mut fits = FitsFile::with_primary_image(huge);
        assert!(crop_fits(&mut fits, rect(0, 0, 2, 2)).is_err());
    }

    #[test]
    fn crop_outside_the_frame_is_an_error_and_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("never.fits");
        let err = crop_fits_file(&test_data("cfa_orion.fits"), &out, rect(3000, 0, 2000, 100))
            .unwrap_err();
        assert!(err.to_string().contains("does not fit"), "{err}");
        assert!(!out.exists());
    }

    #[test]
    fn image_crop_handles_interleaved_rgb_and_mosaics() {
        // Interleaved RGB keeps its three samples per pixel together.
        let rgb = load_fits(&test_data("rgb.fits")).unwrap();
        let cropped = rgb.crop(rect(5, 9, 40, 30)).unwrap();
        assert_eq!((cropped.width, cropped.height), (40, 30));
        let (PixelBuffer::F32(s), PixelBuffer::F32(o)) = (&rgb.pixels, &cropped.pixels) else {
            panic!("rgb.fits should load as f32")
        };
        let row = 7;
        let src_start = ((9 + row) * rgb.width + 5) * 3;
        assert_eq!(
            &o[row * 40 * 3..(row + 1) * 40 * 3],
            &s[src_start..src_start + 120]
        );

        // A CFA image snaps to even, and keeps its pattern.
        let cfa = load_fits(&test_data("cfa_orion.fits")).unwrap();
        let cropped = cfa.crop(rect(1, 1, 101, 51)).unwrap();
        assert_eq!((cropped.width, cropped.height), (100, 50));
        assert_eq!(cropped.image_type, cfa.image_type);
        let (PixelBuffer::U16(s), PixelBuffer::U16(o)) = (&cfa.pixels, &cropped.pixels) else {
            panic!("cfa_orion.fits should load as u16")
        };
        assert_eq!(&o[..100], &s[..100]);
        assert_eq!(&o[100..200], &s[cfa.width..cfa.width + 100]);

        assert!(cfa.crop(rect(0, 0, cfa.width + 2, 10)).is_err());
    }

    #[test]
    fn crops_a_camera_raw_file_into_fits() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("comet_cropped.fits");
        let used = crop_file(&test_data("comet.ARW"), &out, rect(101, 51, 600, 400)).unwrap();
        assert_eq!(used, rect(100, 50, 600, 400));

        let cropped = load_fits(&out).unwrap();
        assert_eq!((cropped.width, cropped.height), (600, 400));
        assert!(matches!(
            cropped.image_type,
            ImageType::CFA(_) | ImageType::XTrans(_)
        ));
    }
}
