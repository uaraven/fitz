use rayon::prelude::*;
use crate::data::{Image, PixelBuffer};

impl Image {
    pub fn invert(&self) -> anyhow::Result<Image> {
        let new_buf = match &self.pixels {
            PixelBuffer::U16(pixels) => PixelBuffer::U16( pixels.par_iter().map(|px| Self::invert_u16(*px)).collect()),
            PixelBuffer::F32(pixels) => PixelBuffer::F32( pixels.par_iter().map(|px| Self::invert_f32(*px)).collect()),
        };
        self.with_pixels(new_buf)
    }

    fn invert_u16(pixel: u16) -> u16 {
        u16::MAX - pixel
    }

    fn invert_f32(pixel: f32) -> f32 {
        1.0 - pixel
    }
}
