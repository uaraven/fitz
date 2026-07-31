use bayer::CFA;
use fitskit::Header;
use rayon::prelude::*;
use std::collections::HashMap;

/// A pixel buffer can be one of three types: u8, u16, or f32. Each type is represented by a vector of the corresponding type.
#[derive(Debug, PartialEq)]
pub enum PixelBuffer {
    U16(Vec<u16>),
    F32(Vec<f32>),
}

impl PixelBuffer {

    /// converts the PixelBuffer to 16-bit integer format and then flattens it to byte array
    pub(crate) fn as_u16_bytes(&self) -> Vec<u8> {
        let scale_to_u16 = |f| (f * 65535.0) as u16;
        match self {
            PixelBuffer::U16(data) => data.par_iter().flat_map_iter(|x| x.to_ne_bytes()).collect(),
            PixelBuffer::F32(data) => data.par_iter().flat_map_iter(|f| scale_to_u16(f).to_ne_bytes()).collect(),
        }
    }

    /// Converts this PixelBuffer to a vector of u8 in big-endian format
    pub(crate) fn as_u8(&self) -> Vec<u8> {
        match self {
            PixelBuffer::U16(data) => data.par_iter().flat_map(|x| x.to_be_bytes()).collect(),
            PixelBuffer::F32(data) => data.par_iter().map(|x| (x * 255.0).clamp(0.0, 255.0) as u8).collect(),
        }
    }

    /// Converts this PixelBuffer to a vector of i16
    pub(crate) fn as_i16(&self) -> (f32, f32, Vec<i16>) {
        const ZERO: u16 = 32768;
        const MIN: i16 = -32768;
        let u16_as_i16 = |x: u16| (x as i32 - ZERO as i32).clamp(MIN as i32, ZERO as i32) as i16;
        (1.0, ZERO as f32, match self {
            PixelBuffer::U16(data) => data.par_iter().map(|x| u16_as_i16(*x)).collect(),
            PixelBuffer::F32(data) => data.par_iter().map(|x| u16_as_i16((x * 65535.0) as u16)).collect(),
        })
    }

    /// Converts this PixelBuffer to a vector of i32
    pub(crate) fn as_i32(&self) -> (f32, f32, Vec<i32>) {
        const ZERO: u32 = 2147483648;
        const F32_SCALE: f32 = 4294967295.0;
        let u32_as_i32 = |x: u32| (x as i64 - ZERO as i64).clamp(-(ZERO as i64)-1, ZERO as i64) as i32;
        (1.0, ZERO as f32, match self {
            PixelBuffer::U16(data) => data.par_iter().map(|x| u32_as_i32(0x10001*(*x as u32))).collect(),
            PixelBuffer::F32(data) => data.par_iter().map(|x| u32_as_i32((x * F32_SCALE) as u32)).collect(),
        })
    }

    /// Converts this PixelBuffer to a vector of i64
    pub(crate) fn as_i64(&self) -> (f32, f32, Vec<i64>) {
        const ZERO: u64 = 9_223_372_036_854_775_808;
        const F32_SCALE: f32 = 18446744073709551615.0;
        let u64_as_i64 = |x: u64| (x as i128 - ZERO as i128).clamp(-(ZERO as i128)-1, ZERO as i128) as i64;
        let u16_as_u64 = |x: u16| 0x0001_0001_0001_0001_u64 * x as u64;
        (1.0, ZERO as f32, match self {
            PixelBuffer::U16(data) => data.par_iter().map(|x| u64_as_i64(u16_as_u64(*x))).collect(),
            PixelBuffer::F32(data) => data.par_iter().map(|x| u64_as_i64((x * F32_SCALE) as u64)).collect(),
        })
    }

    pub(crate) fn as_f32(&self) -> Vec<f32> {
        match self {
            PixelBuffer::U16(data) => data.par_iter().map(|x| (*x as f32) / 65535.0).collect(),
            PixelBuffer::F32(data) => data.par_iter().copied().collect(),
        }
    }

    pub(crate) fn as_f64(&self) -> Vec<f64> {
        match self {
            PixelBuffer::U16(data) => data.par_iter().map(|x| (*x as f64) / 65535.0).collect(),
            PixelBuffer::F32(data) => data.par_iter().map(|x| *x as f64).collect(),
        }
    }
}

/// An ImageType enum represents the type of an image, which can be either RGB or CFA (Color Filter Array) - unbayered.
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum ImageType {
    RGB,
    Grayscale,
    CFA(CFA),
}

/// An Image struct contains an image type, width, height, and a vector of pixel buffers. The pixel buffers can be one of three types: u8, u16, or f32.
#[derive(Debug)]
pub struct Image {
    pub image_type: ImageType,
    pub header: Header,
    pub width: usize,
    pub height: usize,

    pub pixels: PixelBuffer,
}

impl Image {
    /// Creates a new Image with the specified image type, width, height, and pixel buffers.
    pub fn new(image_type: ImageType,
               header: Header,
               width: usize,
               height: usize,
               pixels: PixelBuffer) -> Self {
        Self {
            image_type,
            header,
            width,
            height,
            pixels,
        }
    }

    /// Returns the image headers as a String->String hash map
    pub fn headers(&self) -> HashMap<String, String> {
        let mut result = HashMap::new();
        for keyword  in self.header.keywords.iter() {
            if let Some(value) = &keyword.value {
                result.insert(keyword.name.clone(), format!("{}", value));
            }
        }
        result
    }
}
