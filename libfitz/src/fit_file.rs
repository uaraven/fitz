use crate::data::{Image, PixelBuffer, ImageType};
use crate::errors::FitsError;

use fitskit::{Bitpix, FitsFile, HduData, PixelData};
use rayon::prelude::*;


fn normalize(x: f32, min: f32, max: f32) -> f32 {
    clamp((x - min) / (max - min))
}

fn clamp(x: f32) -> f32 {
    if x < 0.0 {
        0.0
    } else if x > 1.0 {
        1.0
    } else {
        x
    }
}

fn load_pixel_plane(img: &PixelData, b_scale: f32, b_zero: f32) -> PixelBuffer {

    // scales the pixel value according to bscale and bzero parameters
    let scaler = |x: f32| b_zero + b_scale * x;

    let scaled_pixels: Vec<f32> = match img {
        PixelData::U8(v) => v.par_iter().map(|&x| scaler(x as f32)).collect(),
        PixelData::I16(v) => v.par_iter().map(|&x| scaler(x as f32)).collect(),
        PixelData::I32(v) => v.par_iter().map(|&x| scaler(x as f32)).collect(),
        PixelData::I64(v) => v.par_iter().map(|&x| scaler(x as f32)).collect(),
        PixelData::F32(v) => v.par_iter().map(|&x| scaler(x)).collect(),
        PixelData::F64(v) => v.par_iter().map(|&x| scaler(x as f32)).collect(),
    };

    // calculate min and max values for normalization via a parallel reduction
    let (min, max) = scaled_pixels
        .par_iter()
        .fold(
            || (f32::INFINITY, f32::NEG_INFINITY),
            |(min, max), &x| (min.min(x), max.max(x)),
        )
        .reduce(
            || (f32::INFINITY, f32::NEG_INFINITY),
            |(min1, max1), (min2, max2)| (min1.min(min2), max1.max(max2)),
        );

    // normalize pixel values and convert them to supported pixel formats
    match img.bitpix() {
        Bitpix::U8 | Bitpix::I32 | Bitpix::I16 =>
            PixelBuffer::U16(scaled_pixels.par_iter().map(|&x| (normalize(x, min, max) * 65535.0) as u16).collect()),
        Bitpix::I64 | Bitpix::F32 | Bitpix::F64 =>
            PixelBuffer::F32(scaled_pixels.par_iter().map(|&x| normalize(x, min, max)).collect()),
    }
}

fn load_fits_data_from_file(file_path: &str) -> Result<Image, FitsError> {
    // Open the FITS file
    let fits_file = FitsFile::from_file(file_path)?;

    // Read the primary HDU (Header Data Unit)
    let hdu = fits_file.primary();


    if let HduData::Image(img) = &hdu.data {
        // Get the image dimensions
        let width = img.width().ok_or_else(|| FitsError::new_invalid_img("No width"))?;
        let height = img.width().ok_or_else(|| FitsError::new_invalid_img("No height"))?;

        let b_scale =hdu.header.get_float("BSCALE").unwrap_or(1.0) as f32;
        let b_zero =hdu.header.get_float("BZERO").unwrap_or(0.0) as f32;

        let pixels = load_pixel_plane(&img.pixels, b_scale, b_zero);

        // Create an Image struct with the loaded data
        let image = Image {
            image_type: ImageType::RGB, // Assuming RGB for this example; adjust as needed
            width,
            height,
            pixels,
        };

        Ok(image)
    } else {
        Err( FitsError::new_invalid_img("Invalid image type"))
    }
}