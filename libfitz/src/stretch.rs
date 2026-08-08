//! Load a FITS image (debayering it first if needed) and apply an MTF/STF
//! auto-stretch, returning the stretched result in memory as normalized `[0,
//! 1]` `f32` samples.

use crate::data::{Image, ImageType, PixelBuffer};
use crate::stats::{Stats, single_channel_stats};
use rayon::prelude::*;

/// Working sample type for the stretch math. `f32` is more than precise enough
/// for a 16-bit result and halves the normalized-image buffer versus `f64`.
type Sample = f32;

/// Shadows clipping point, in units of (normalized) MAD below the median.
const SHADOWS_CLIP: Sample = -2.8;
/// Default `--brightness`: the target background level the stretched median is
/// pulled towards, absent user override. Must stay strictly inside `(0, 1)`.
pub const DEFAULT_BRIGHTNESS: f32 = 0.25;
/// Scale factor turning the median absolute deviation into a robust estimate of
/// the standard deviation for a normal distribution.
const MAD_NORM: Sample = 1.4826;

const OUT_MAX: Sample = u16::MAX as Sample;

impl Image {
    /// Apply an MTF/STF auto-stretch to `image`, returning a new [`Image`] with the
    /// same shape and type as the input and its pixels normalized to `[0, 1]`
    /// `f32` samples — a stretch is a tonal remap, not a bit-depth decision, so
    /// it always returns the full-precision result rather than pre-quantizing
    /// to 16 bits; callers narrow on export as needed.
    /// This is a pure, in-memory transform: it does no reading or writing to disk.
    pub fn stretch(&self, linked: bool, brightness: f32) -> Image {
        let channels = match self.image_type {
            ImageType::RGB => 3,
            ImageType::Grayscale | ImageType::CFA(_) => 1,
        };

        let mut samples = normalize_pixel_buffer(&self.pixels);
        let params = self.stretch_params(linked, channels, brightness);
        apply_params(&mut samples, channels, &params);

        Image {
            image_type: self.image_type,
            header: self.header.clone(),
            width: self.width,
            height: self.height,
            pixels: PixelBuffer::F32(samples),
        }
    }

    /// Derive one `(shadows, midtones)` pair per channel, reusing [`Image::stats`]'s
    /// already-computed median/MAD rather than re-selecting them from the raw
    /// samples. With `linked`, all channels share a single set of parameters
    /// derived from their combined statistics (all interleaved planes treated
    /// as one channel, via [`single_channel_stats`]); otherwise each channel is
    /// stretched from its own statistics, which also acts as an automatic
    /// background neutralization.
    fn stretch_params(
        &self,
        linked: bool,
        channels: usize,
        brightness: Sample,
    ) -> Vec<(Sample, Sample)> {
        if linked {
            let params = find_params(&single_channel_stats(&self.pixels).channels[0], brightness);
            vec![params; channels]
        } else {
            self.stats()
                .channels
                .iter()
                .map(|s| find_params(s, brightness))
                .collect()
        }
    }
}

/// Apply per-channel `(shadows, midtones)` params to `samples` in place:
/// `channels` interleaved planes (3 for RGB, 1 for a single-channel
/// grayscale/CFA image), `params[c]` applying to channel `c`.
fn apply_params(samples: &mut [Sample], channels: usize, params: &[(Sample, Sample)]) {
    samples.par_chunks_mut(channels).for_each(|px| {
        for (c, v) in px.iter_mut().enumerate() {
            let (shadows, midtones) = params[c];
            *v = transfer(*v, shadows, midtones);
        }
    });
}

/// Normalize a [`PixelBuffer`]'s samples to `[0, 1]`: `U16` scales by its max,
/// `F32` is assumed already in `[0, 1]`.
fn normalize_pixel_buffer(pixels: &PixelBuffer) -> Vec<Sample> {
    match pixels {
        PixelBuffer::U16(v) => v
            .par_iter()
            .map(|&x| x as Sample / u16::MAX as Sample)
            .collect(),
        PixelBuffer::F32(v) => v.par_iter().copied().collect(),
    }
}

/// Derive the `(shadows, midtones)` STF parameters from a channel's already-
/// computed [`Stats`] (see [`Image::stretch_params`]), targeting `target_bg`
/// (in `(0, 1)`) as the background brightness the median should map to (see
/// `--brightness`). `Stats::median`/`Stats::mad` are in the native
/// `0..=65535` pixel scale; both are normalized into `[0, 1]` here.
fn find_params(stats: &Stats, target_bg: Sample) -> (Sample, Sample) {
    let med = stats.median / OUT_MAX;
    let mad = (stats.mad / OUT_MAX) * MAD_NORM;

    let shadows = (med + SHADOWS_CLIP * mad).clamp(0.0, 1.0);
    // Keep the midtone strictly inside (0, 1) as `mtf` requires: degenerate
    // inputs (a near-constant image, or one with a very large spread) can push
    // `med - shadows` to 0 or >= 1, where `mtf` would otherwise return exactly
    // 0 or 1 and collapse the whole stretch to solid white or black.
    let midtones = mtf(target_bg, med - shadows).clamp(Sample::EPSILON, 1.0 - Sample::EPSILON);

    (shadows, midtones)
}

/// Rescale a sample against the shadows clip, then apply the midtones transfer.
fn transfer(v: Sample, shadows: Sample, midtones: Sample) -> Sample {
    let denom = 1.0 - shadows;
    let rescaled = if denom > 0.0 {
        ((v - shadows) / denom).clamp(0.0, 1.0)
    } else {
        0.0
    };
    mtf(midtones, rescaled)
}

/// The midtones transfer function: a monotonic curve on `[0, 1]` with
/// `mtf(m, 0) = 0`, `mtf(m, 1) = 1`, and `mtf(m, m) = 0.5`. The midtone `m` is
/// expected to lie in `(0, 1)`, which is guaranteed for the values
/// [`find_params`] derives.
fn mtf(m: Sample, x: Sample) -> Sample {
    if x <= 0.0 {
        0.0
    } else if x >= 1.0 {
        1.0
    } else if (x - m).abs() < Sample::EPSILON {
        0.5
    } else {
        ((m - 1.0) * x) / ((2.0 * m - 1.0) * x - m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_data;
    use fitskit::Header;

    #[test]
    fn mtf_hits_its_anchor_points() {
        let m = 0.25;
        assert_eq!(mtf(m, 0.0), 0.0);
        assert_eq!(mtf(m, 1.0), 1.0);
        assert!((mtf(m, m) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn mtf_is_monotonic() {
        let m = 0.2;
        let mut prev = mtf(m, 0.0);
        for i in 1..=100 {
            let cur = mtf(m, i as Sample / 100.0);
            assert!(cur >= prev, "mtf decreased at {i}: {cur} < {prev}");
            prev = cur;
        }
    }

    #[test]
    fn stretch_image_preserves_shape_and_type_on_real_cfa_data() {
        // `stretch` never demosaics: a CFA input stays CFA, same width/height,
        // just restretched into normalized `[0, 1]` f32 samples — a stretch is
        // a tonal remap, not a bit-depth decision.
        let loaded = crate::fits_file::load_fits(&test_data("cfa_orion.fits")).unwrap();

        let stretched = loaded.stretch(false, DEFAULT_BRIGHTNESS);

        assert_eq!(stretched.image_type, loaded.image_type);
        assert_eq!(stretched.width, loaded.width);
        assert_eq!(stretched.height, loaded.height);
        match stretched.pixels {
            PixelBuffer::F32(v) => {
                assert_eq!(v.len(), loaded.width * loaded.height);
                assert!(v.iter().all(|&x| (0.0..=1.0).contains(&x)));
                assert!(v.iter().any(|&x| x > 0.0));
                assert!(v.iter().any(|&x| x < 1.0));
            }
            PixelBuffer::U16(_) => panic!("expected a normalized f32 pixel buffer"),
        }
    }

    #[test]
    fn stretch_image_preserves_shape_and_type_on_real_debayered_rgb_data() {
        // A debayered RGB image stays RGB and keeps its shape, with all three
        // interleaved planes present.
        let loaded = crate::fits_file::load_fits(&test_data("cfa_orion.fits")).unwrap();
        let rgb = loaded.debayer().unwrap().unwrap();

        let stretched = rgb.stretch(false, DEFAULT_BRIGHTNESS);

        assert_eq!(stretched.image_type, ImageType::RGB);
        assert_eq!(stretched.width, rgb.width);
        assert_eq!(stretched.height, rgb.height);
        match stretched.pixels {
            PixelBuffer::F32(v) => assert_eq!(v.len(), rgb.width * rgb.height * 3),
            PixelBuffer::U16(_) => panic!("expected a normalized f32 pixel buffer"),
        }
    }

    #[test]
    fn stretch_image_linked_and_per_channel_differ_on_imbalanced_color() {
        // Same imbalanced-color scenario as `linked_and_per_channel_differ_on_imbalanced_color`,
        // exercised through the pure `Image -> Image` entry point.
        let n = 32usize;
        let samples: Vec<u16> = (0..n)
            .flat_map(|i| {
                let r = 40000 + (i % 100) as u16;
                let g = 8000 + (i % 100) as u16;
                let b = 1000 + (i % 100) as u16;
                [r, g, b]
            })
            .collect();
        let image = Image {
            image_type: ImageType::RGB,
            header: Header::default(),
            width: n,
            height: 1,
            pixels: PixelBuffer::U16(samples),
        };

        let per_channel = image.stretch(false, DEFAULT_BRIGHTNESS);
        let linked = image.stretch(true, DEFAULT_BRIGHTNESS);
        assert_ne!(per_channel.pixels, linked.pixels);
    }
}
