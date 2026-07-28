use crate::data::{Image, ImageType, PixelBuffer};
use crate::errors::FitsError;
use std::borrow::Cow;
use std::path::Path;

use crate::fits_bayer::{cfa_str, parse_cfa};
use crate::fits_image::copy_missing_metadata;
use crate::keywords::{BAYERPAT, BSCALE, BZERO};
use fitskit::{Bitpix, FitsFile, HduData, HeaderValue, ImageData, PixelData};
use rayon::prelude::*;

const MIN_U16: u32 = 0;
const MAX_U16:u32 = 65535;

const MIN_F32:f32 = 0.0;
const MAX_F32:f32 = 1.0;
fn fits_u8_to_u16(x: f32) -> u16 {
    (x as u32 * 257u32).clamp(MIN_U16, MAX_U16) as u16
}

fn fits_u16_to_u16(x: f32) -> u16 {
    (x as u32).clamp(MIN_U16, MAX_U16) as u16
}

fn fits_u32_to_f32(x: f32) -> f32 {
    (x/ 4_294_967_295.0).clamp(MIN_F32, MAX_F32)
}

fn fits_u64_to_f32(x: f32) -> f32 {
    (x/ 18_446_744_073_709_551_615.0).clamp(MIN_F32, MAX_F32)
}

/// Apply the image's BSCALE/BZERO scaling and clamp into a supported
/// `PixelBuffer`: integer sources map to `u16` over the full 0..=65535 range,
/// float/wide-integer sources to `f32` clamped to [0, 1]. Scaling and clamping
/// fuse into a single parallel pass, and the output type follows the sample
/// type directly.
fn load_pixel_plane(img: &PixelData, b_scale: f32, b_zero: f32) -> PixelBuffer {
    let scale = |x: f32| b_zero + b_scale * x;
    let to_f32 = |x: f32| scale(x).clamp(0.0, 1.0);

    match img {
        PixelData::U8(v) => PixelBuffer::U16(v.par_iter().map(|&x| fits_u8_to_u16(scale(x as f32))).collect()),
        PixelData::I16(v) => PixelBuffer::U16(v.par_iter().map(|&x| fits_u16_to_u16(scale(x as f32))).collect()),
        PixelData::I32(v) => PixelBuffer::F32(v.par_iter().map(|&x| fits_u32_to_f32(x as f32)).collect()),
        PixelData::I64(v) => PixelBuffer::F32(v.par_iter().map(|&x| fits_u64_to_f32(x as f32)).collect()),
        PixelData::F32(v) => PixelBuffer::F32(v.par_iter().map(|&x| to_f32(x)).collect()),
        PixelData::F64(v) => PixelBuffer::F32(v.par_iter().map(|&x| to_f32(x as f32)).collect()),
    }
}

pub fn load_image_from_fits(file_path: &Path) -> Result<Image, FitsError> {
    let fits_file = FitsFile::from_file(file_path)?;
    let hdu_opt = fits_file.iter().find(|hdu|
        matches!(hdu.data, HduData::Image(_)) || hdu.as_compressed_image().is_some());

    if let Some(hdu) = hdu_opt {
        // Borrow a plain image HDU, but decompress a tile-compressed one into an owned buffer.
        let img: Cow<ImageData> = if let Some(compressed) = hdu.as_compressed_image() {
            Cow::Owned(compressed.decompress()?)
        } else if let HduData::Image(img) = &hdu.data {
            Cow::Borrowed(img)
        } else {
            return Err(FitsError::new_invalid_img("Invalid image type"));
        };

        let axis_count = img.axes.len();
        let bayer_pat =  hdu.header.get_string(BAYERPAT).and_then(parse_cfa);

        let image_type = if axis_count > 2 {
            ImageType::RGB
        } else if bayer_pat.is_some() {
            ImageType::CFA(bayer_pat.unwrap())
        } else {
            ImageType::Grayscale
        };


        let width = img.width().ok_or_else(|| FitsError::new_invalid_img("No width"))?;
        let height = img.height().ok_or_else(|| FitsError::new_invalid_img("No height"))?;

        let b_scale = hdu.header.get_float(BSCALE).unwrap_or(1.0) as f32;
        let b_zero = hdu.header.get_float(BZERO).unwrap_or(0.0) as f32;

        let pixels = load_pixel_plane(&img.pixels, b_scale, b_zero);

        Ok(Image {
            image_type,
            header: hdu.header.clone(),
            width,
            height,
            pixels,
        })
    } else {
         Err(FitsError::new_invalid_img("Image HDU not found"))
    }
}


struct SaveOptions {
    compress_options: Option<fitskit::CompressOptions>,
    bitpix: Bitpix,
}
fn save_file(target: &Path, img: &Image, options: SaveOptions) -> Result<(), FitsError> {
    let (bscale, bzero, pixel_data) = match options.bitpix {
        Bitpix::U8 => (1.0, 0.0, PixelData::U8(img.pixels.as_u8())),
        Bitpix::I16 => {
            let (bscale, bzero, data) = img.pixels.as_i16();
            (bscale, bzero, PixelData::I16(data))
        },
        Bitpix::I32 => {
            let (bscale, bzero, data) = img.pixels.as_i32();
            (bscale, bzero, PixelData::I32(data))
        },
        Bitpix::I64 => {
            let (bscale, bzero, data) = img.pixels.as_i64();
            (bscale, bzero, PixelData::I64(data))
        },
        Bitpix::F32 => {
            (1.0, 0.0, PixelData::F32(img.pixels.as_f32()))
        },
        Bitpix::F64 => {
            (1.0, 0.0, PixelData::F64(img.pixels.as_f64()))
        }
    };

    let img_data = match img.image_type {
        ImageType::RGB => ImageData::new(vec![img.width, img.height, 3], pixel_data),
        ImageType::Grayscale => ImageData::new(vec![img.width, img.height], pixel_data),
        ImageType::CFA(_) => ImageData::new(vec![img.width, img.height], pixel_data),
    };


    let mut dst_file = if let Some(compress_options) = &options.compress_options {
        let mut compressed_fits = FitsFile::with_empty_primary();
        let mut co = fitskit::CompressOptions::default();
        co.algorithm = compress_options.algorithm;
        compressed_fits.push_extension(img_data.compress(&co)?);
        compressed_fits
    } else {
        let mut file = FitsFile::with_primary_image(img_data);
        file.primary_mut().header.set(BSCALE, HeaderValue::Integer(bscale as i64), None);
        file.primary_mut().header.set(BZERO, HeaderValue::Integer(bzero as i64), None);

        if let ImageType::CFA(cfa) = img.image_type {
            file.primary_mut().header.set(BAYERPAT, HeaderValue::String(cfa_str(cfa).to_string()), None);
        }
        file
    };

    copy_missing_metadata(&mut dst_file.primary_mut().header, &img.header, &[]);
    let bytes = dst_file.to_bytes()?;

    std::fs::write(target, bytes).map_err(|e| {
        FitsError::new_invalid_img(&format!("cannot write {}: {e}", target.display()))
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::data::{ImageType, PixelBuffer};
    use crate::fits_file::{SaveOptions, load_image_from_fits, save_file};
    use crate::test_support::test_data;
    use bayer::CFA;
    use fitskit::{Bitpix, CompressOptions};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    #[test]
    fn test_load_scaled_i16_image() {
        let input = test_data("cfa_orion.fits");
        let loaded = load_image_from_fits(&input).unwrap();

        assert_eq!(loaded.image_type, ImageType::CFA(CFA::RGGB));
        assert_eq!(loaded.width, 3856);
        assert_eq!(loaded.height, 2180);
        // An I16 source scales into the u16 pixel buffer.
        assert!(matches!(loaded.pixels, PixelBuffer::U16(_)));
    }

    #[test]
    fn test_load_compressed_file() {
        let input = test_data("compressed.fits.fz");
        let loaded = load_image_from_fits(&input).unwrap();

        assert_eq!(loaded.image_type, ImageType::CFA(CFA::GRBG));
        assert_eq!(loaded.width, 3008);
        assert_eq!(loaded.height, 3008);
        // An I16 source scales into the u16 pixel buffer.
        assert!(matches!(loaded.pixels, PixelBuffer::U16(_)));
    }

    #[test]
    fn test_save_uncompressed_file() {
        let input = test_data("cfa_orion.fits");
        let loaded = load_image_from_fits(&input).unwrap();

        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("raw.fits");

        save_file(&output, &loaded, SaveOptions { compress_options: None, bitpix: Bitpix::I16}).unwrap();

        let actual = format!("{:x}", Sha256::digest(std::fs::read(&output).unwrap()));
        assert_eq!(
            actual,
            "e5b5ff8800c404719862765593d39857559552120d656f41ac2ea85f85f4f7f3"
        );
    }

    #[test]
    fn test_save_compressed_file() {
        let input = test_data("cfa_orion.fits");
        let loaded = load_image_from_fits(&input).unwrap();

        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("raw.fits.fz");
        // let output = test_data("test.fits.fz");

        save_file(&output, &loaded, SaveOptions { compress_options: Some(CompressOptions::default()), bitpix: Bitpix::I16}).unwrap();

        let actual = format!("{:x}", Sha256::digest(std::fs::read(&output).unwrap()));
        assert_eq!(
            actual,
            "82c2eaa2e12b4d06de6f1424985dea2a8e924f8ff3c0bb2f630d4f5adcb90a85"
        );
    }
}