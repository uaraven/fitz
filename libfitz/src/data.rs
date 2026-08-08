use crate::convert::{float_to_u16, u16_to_float};
use bayer::CFA;
use fitskit::Header;
use rayon::prelude::*;
use std::borrow::Cow;
use std::collections::HashMap;

/// A pixel buffer can be one of three types: u8, u16, or f32. Each type is represented by a vector of the corresponding type.
#[derive(Debug, Clone, PartialEq)]
pub enum PixelBuffer {
    U16(Vec<u16>),
    F32(Vec<f32>),
}

/// Round a physical pixel value to the nearest u16, clamping to its range.
pub fn round_to_u16(v: f64) -> u16 {
    v.round().clamp(0.0, 65535.0) as u16
}

/// Reorder concatenated R, G, B planes into interleaved `R,G,B,R,G,B,…` order.
/// Output sample `i` is pixel `i / 3` of channel `i % 3`.
fn interleave<T: Copy + Send + Sync>(planes: &[T]) -> Vec<T> {
    let n = planes.len() / 3;
    (0..3 * n)
        .into_par_iter()
        .map(|i| planes[(i % 3) * n + i / 3])
        .collect()
}

/// The inverse of [`interleave`]: split an interleaved RGB buffer back into
/// concatenated R, G, B planes. Output sample `i` is pixel `i % n` of channel
/// `i / n`.
fn deinterleave<T: Copy + Send + Sync>(samples: &[T]) -> Vec<T> {
    let n = samples.len() / 3;
    (0..3 * n)
        .into_par_iter()
        .map(|i| samples[(i % n) * 3 + i / n])
        .collect()
}

impl PixelBuffer {
    /// This buffer read as a 3-plane FITS cube (R plane, then G, then B),
    /// reordered into the interleaved layout every in-memory
    /// [`ImageType::RGB`] buffer uses. FITS stores a cube planar; debayering,
    /// stretching and statistics all step by 3, so the conversion has to happen
    /// at the file boundary and nowhere else.
    pub(crate) fn interleave_planes(&self) -> PixelBuffer {
        match self {
            PixelBuffer::U16(v) => PixelBuffer::U16(interleave(v)),
            PixelBuffer::F32(v) => PixelBuffer::F32(interleave(v)),
        }
    }

    /// The inverse of [`PixelBuffer::interleave_planes`], applied on the way
    /// back out to a FITS cube.
    pub(crate) fn deinterleave_planes(&self) -> PixelBuffer {
        match self {
            PixelBuffer::U16(v) => PixelBuffer::U16(deinterleave(v)),
            PixelBuffer::F32(v) => PixelBuffer::F32(deinterleave(v)),
        }
    }

    /// The number of samples in this buffer.
    pub(crate) fn len(&self) -> usize {
        match self {
            PixelBuffer::U16(data) => data.len(),
            PixelBuffer::F32(data) => data.len(),
        }
    }

    /// converts the PixelBuffer to 16-bit integer format and then flattens it to byte array
    pub(crate) fn as_u16_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![0u8; 2 * self.len()];
        match self {
            PixelBuffer::U16(data) => bytes
                .par_chunks_exact_mut(2)
                .zip(data.par_iter())
                .for_each(|(out, x)| out.copy_from_slice(&x.to_ne_bytes())),
            PixelBuffer::F32(data) => bytes
                .par_chunks_exact_mut(2)
                .zip(data.par_iter())
                .for_each(|(out, f)| out.copy_from_slice(&(float_to_u16(*f) as u16).to_ne_bytes())),
        }
        bytes
    }

    /// Narrows this PixelBuffer to one 8-bit sample per pixel.
    ///
    /// A `U16` sample keeps its high byte — the same narrowing `export`'s
    /// `high_byte` applies. Emitting both of a `u16`'s bytes here would produce
    /// twice as many samples as the image has pixels, which the FITS writer
    /// then lays down past the end of the declared data block.
    pub(crate) fn as_u8(&self) -> Vec<u8> {
        match self {
            PixelBuffer::U16(data) => data.par_iter().map(|x| (x >> 8) as u8).collect(),
            PixelBuffer::F32(data) => data
                .par_iter()
                .map(|x| (x * 255.0).clamp(0.0, 255.0) as u8)
                .collect(),
        }
    }

    /// Converts this PixelBuffer to a vector of i16
    pub(crate) fn as_i16(&self) -> (f32, f32, Vec<i16>) {
        const ZERO: u16 = 32768;
        const MIN: i16 = -32768;
        let u16_as_i16 = |x: u16| (x as i32 - ZERO as i32).clamp(MIN as i32, ZERO as i32) as i16;
        (
            1.0,
            ZERO as f32,
            match self {
                PixelBuffer::U16(data) => data.par_iter().map(|x| u16_as_i16(*x)).collect(),
                PixelBuffer::F32(data) => data
                    .par_iter()
                    .map(|x| u16_as_i16((x * 65535.0) as u16))
                    .collect(),
            },
        )
    }

    /// Converts this PixelBuffer to a vector of i32
    pub(crate) fn as_i32(&self) -> (f32, f32, Vec<i32>) {
        const ZERO: u32 = 2147483648;
        const F32_SCALE: f32 = 4294967295.0;
        let u32_as_i32 =
            |x: u32| (x as i64 - ZERO as i64).clamp(-(ZERO as i64) - 1, ZERO as i64) as i32;
        (
            1.0,
            ZERO as f32,
            match self {
                PixelBuffer::U16(data) => data
                    .par_iter()
                    .map(|x| u32_as_i32(0x10001 * (*x as u32)))
                    .collect(),
                PixelBuffer::F32(data) => data
                    .par_iter()
                    .map(|x| u32_as_i32((x * F32_SCALE) as u32))
                    .collect(),
            },
        )
    }

    /// Converts this PixelBuffer to a vector of i64
    pub(crate) fn as_i64(&self) -> (f32, f32, Vec<i64>) {
        const ZERO: u64 = 9_223_372_036_854_775_808;
        const F32_SCALE: f32 = 18446744073709551615.0;
        let u64_as_i64 =
            |x: u64| (x as i128 - ZERO as i128).clamp(-(ZERO as i128) - 1, ZERO as i128) as i64;
        let u16_as_u64 = |x: u16| 0x0001_0001_0001_0001_u64 * x as u64;
        (
            1.0,
            ZERO as f32,
            match self {
                PixelBuffer::U16(data) => data
                    .par_iter()
                    .map(|x| u64_as_i64(u16_as_u64(*x)))
                    .collect(),
                PixelBuffer::F32(data) => data
                    .par_iter()
                    .map(|x| u64_as_i64((x * F32_SCALE) as u64))
                    .collect(),
            },
        )
    }

    /// This buffer as 16-bit samples — the identity for `U16`, and the
    /// full-scale `[0, 1] -> [0, 65535]` map for `F32`.
    pub fn as_u16(&self) -> Vec<u16> {
        match self {
            // Nothing to convert: cloning beats driving the whole buffer
            // through rayon's collect machinery to copy it element by element.
            PixelBuffer::U16(data) => data.clone(),
            PixelBuffer::F32(data) => data.par_iter().map(|&v| float_to_u16(v) as u16).collect(),
        }
    }

    pub fn as_u16ref(&self) -> Cow<'_, Vec<u16>> {
        match self {
            // Nothing to convert: cloning beats driving the whole buffer
            // through rayon's collect machinery to copy it element by element.
            PixelBuffer::U16(data) => Cow::Borrowed(data),
            PixelBuffer::F32(data) => {
                Cow::Owned(data.par_iter().map(|&v| float_to_u16(v) as u16).collect())
            }
        }
    }

    /// This buffer as 32-bit samples. A `U16` sample is promoted by bit
    /// replication (`x * 65537`), so 0 and 65535 map exactly onto 0 and
    /// `u32::MAX`.
    pub fn as_u32(&self) -> Vec<u32> {
        match self {
            PixelBuffer::U16(data) => data.par_iter().map(|x| *x as u32 * 65537).collect(),
            PixelBuffer::F32(data) => data
                .par_iter()
                .map(|x| (*x as f64 * u32::MAX as f64).clamp(0.0, u32::MAX as f64) as u32)
                .collect(),
        }
    }

    pub(crate) fn as_f32(&self) -> Vec<f32> {
        match self {
            PixelBuffer::U16(data) => data.par_iter().map(|&v| u16_to_float(v)).collect(),
            // Nothing to convert — a plain clone beats driving the whole buffer
            // through rayon's collect machinery to copy it element by element.
            PixelBuffer::F32(data) => data.clone(),
        }
    }

    pub(crate) fn as_f64(&self) -> Vec<f64> {
        match self {
            PixelBuffer::U16(data) => data.par_iter().map(|x| (*x as f64) / 65535.0).collect(),
            PixelBuffer::F32(data) => data.par_iter().map(|x| *x as f64).collect(),
        }
    }
}

/// Every `channel`th sample of an interleaved 3-channel buffer.
fn extract_plane<T: Copy + Send + Sync>(samples: &[T], channel: usize) -> Vec<T> {
    let n = samples.len() / 3;
    (0..n)
        .into_par_iter()
        .map(|i| samples[i * 3 + channel])
        .collect()
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
    pub fn new(
        image_type: ImageType,
        header: Header,
        width: usize,
        height: usize,
        pixels: PixelBuffer,
    ) -> Self {
        Self {
            image_type,
            header,
            width,
            height,
            pixels,
        }
    }

    /// Number of interleaved colour planes in [`Image::pixels`].
    pub fn channels(&self) -> usize {
        match self.image_type {
            ImageType::RGB => 3,
            ImageType::Grayscale | ImageType::CFA(_) => 1,
        }
    }

    /// One colour plane of an interleaved RGB image, as a standalone grayscale
    /// [`Image`] sharing this image's header and geometry. `None` for anything
    /// that isn't a 3-channel image, or a `channel` outside `0..3`.
    pub fn plane(&self, channel: usize) -> Option<Image> {
        if self.image_type != ImageType::RGB || channel >= 3 {
            return None;
        }
        let pixels = match &self.pixels {
            PixelBuffer::U16(v) => PixelBuffer::U16(extract_plane(v, channel)),
            PixelBuffer::F32(v) => PixelBuffer::F32(extract_plane(v, channel)),
        };
        Some(Image::new(
            ImageType::Grayscale,
            self.header.clone(),
            self.width,
            self.height,
            pixels,
        ))
    }

    /// Returns the image headers as a String->String hash map
    pub fn headers(&self) -> HashMap<String, String> {
        let mut result = HashMap::new();
        for keyword in self.header.keywords.iter() {
            if let Some(value) = &keyword.value {
                result.insert(keyword.name.clone(), format!("{}", value));
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_u16_bytes_matches_native_u16_bytes_for_both_buffer_kinds() {
        // `float_to_u16` returns a `usize`; `as_u16_bytes` must narrow it to a
        // `u16` before taking its native-endian bytes, or the 2-byte-per-sample
        // output buffer it writes into panics on a length mismatch.
        let floats = [0.0f32, 0.25, 0.5, 1.0];
        let expected: Vec<u8> = floats
            .iter()
            .flat_map(|&v| (crate::convert::float_to_u16(v) as u16).to_ne_bytes())
            .collect();

        let f32_buf = PixelBuffer::F32(floats.to_vec());
        assert_eq!(f32_buf.as_u16_bytes(), expected);

        let u16_values = [0u16, 100, 32768, 65535];
        let u16_expected: Vec<u8> = u16_values.iter().flat_map(|v| v.to_ne_bytes()).collect();
        let u16_buf = PixelBuffer::U16(u16_values.to_vec());
        assert_eq!(u16_buf.as_u16_bytes(), u16_expected);
    }
}
