use std::borrow::Cow;

use crate::data::{Image, PixelBuffer, ImageType};
use crate::errors::FitsError;

use fitskit::{FitsFile, HduData, ImageData, PixelData};
use rayon::prelude::*;
use crate::fits_bayer::parse_cfa;
use crate::keywords::{BAYERPAT, BSCALE, BZERO};

/// Apply the image's BSCALE/BZERO scaling and clamp into a supported
/// `PixelBuffer`: integer sources map to `u16` over the full 0..=65535 range,
/// float/wide-integer sources to `f32` clamped to [0, 1]. Scaling and clamping
/// fuse into a single parallel pass, and the output type follows the sample
/// type directly.
fn load_pixel_plane(img: &PixelData, b_scale: f32, b_zero: f32) -> PixelBuffer {
    let scale = |x: f32| b_zero + b_scale * x;
    let to_u16 = |x: f32| (scale(x) * 65535.0).clamp(0.0, 65535.0) as u16;
    let to_f32 = |x: f32| scale(x).clamp(0.0, 1.0);

    match img {
        PixelData::U8(v) => PixelBuffer::U16(v.par_iter().map(|&x| to_u16(x as f32)).collect()),
        PixelData::I16(v) => PixelBuffer::U16(v.par_iter().map(|&x| to_u16(x as f32)).collect()),
        PixelData::I32(v) => PixelBuffer::U16(v.par_iter().map(|&x| to_u16(x as f32)).collect()),
        PixelData::I64(v) => PixelBuffer::F32(v.par_iter().map(|&x| to_f32(x as f32)).collect()),
        PixelData::F32(v) => PixelBuffer::F32(v.par_iter().map(|&x| to_f32(x)).collect()),
        PixelData::F64(v) => PixelBuffer::F32(v.par_iter().map(|&x| to_f32(x as f32)).collect()),
    }
}

pub fn load_image_from_fits(file_path: &str) -> Result<Image, FitsError> {
    let fits_file = FitsFile::from_file(file_path)?;
    let hdu = fits_file.primary();

    let axis_count = hdu.header.get_int("NAXIS").unwrap_or(2);
    let bayer_pat = hdu.header.get_string(BAYERPAT).map(parse_cfa);

    let image_type = if axis_count > 2 {
        ImageType::RGB
    } else if bayer_pat.is_none() {
        ImageType::CFA
    } else {
        ImageType::Grayscale
    };

    // Borrow a plain image HDU, but decompress a tile-compressed one into an owned buffer.
    let img: Cow<ImageData> = if let Some(compressed) = hdu.as_compressed_image() {
        Cow::Owned(compressed.decompress()?)
    } else if let HduData::Image(img) = &hdu.data {
        Cow::Borrowed(img)
    } else {
        return Err(FitsError::new_invalid_img("Invalid image type"));
    };

    let width = img.width().ok_or_else(|| FitsError::new_invalid_img("No width"))?;
    let height = img.height().ok_or_else(|| FitsError::new_invalid_img("No height"))?;

    let b_scale = hdu.header.get_float(BSCALE).unwrap_or(1.0) as f32;
    let b_zero = hdu.header.get_float(BZERO).unwrap_or(0.0) as f32;

    let pixels = load_pixel_plane(&img.pixels, b_scale, b_zero);

    Ok(Image {
        image_type,
        width,
        height,
        pixels,
    })
}