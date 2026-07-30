//! Load a FITS image (debayering it first if needed) and apply an MTF/STF
//! auto-stretch, returning the stretched 16-bit result in memory.

use crate::data::{Image, ImageType, PixelBuffer};
use crate::fits_image::round_to_u16;
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


/// Apply an MTF/STF auto-stretch to `image`, returning a new [`Image`] with the
/// same shape and type as the input. 
/// This is a pure, in-memory transform: it does no reading or writing to disk.
pub fn stretch(image: &Image, linked: bool, brightness: f32) -> Image {
    let channels = match image.image_type {
        ImageType::RGB => 3,
        ImageType::Grayscale | ImageType::CFA(_) => 1,
    };

    let mut samples = normalize_pixel_buffer(&image.pixels);
    stretch_samples(&mut samples, channels, linked, brightness);

    Image {
        image_type: image.image_type.clone(),
        header: image.header.clone(),
        width: image.width,
        height: image.height,
        pixels: PixelBuffer::U16(samples_to_u16(&samples)),
    }
}

/// Stretch `samples` in place: `channels` interleaved planes (3 for RGB, 1 for
/// a single-channel grayscale/CFA image). With `linked`, one set of stretch
/// parameters (derived from all channels together) is applied to every
/// channel; otherwise each channel is stretched from its own statistics, which
/// also acts as an automatic background neutralization.
fn stretch_samples(samples: &mut [Sample], channels: usize, linked: bool, brightness: Sample) {
    if linked {
        let (shadows, midtones) = find_params(&mut samples.to_vec(), brightness);
        samples
            .par_iter_mut()
            .for_each(|v| *v = transfer(*v, shadows, midtones));
    } else {
        let params: Vec<(Sample, Sample)> = (0..channels)
            .into_par_iter()
            .map(|start| {
                let mut chan: Vec<Sample> =
                    samples.iter().skip(start).step_by(channels).copied().collect();
                find_params(&mut chan, brightness)
            })
            .collect();
        samples.par_chunks_mut(channels).for_each(|px| {
            for (c, v) in px.iter_mut().enumerate() {
                let (shadows, midtones) = params[c];
                *v = transfer(*v, shadows, midtones);
            }
        });
    }
}

/// Round normalized `[0, 1]` samples to interleaved 16-bit samples in `[0, 65535]`.
fn samples_to_u16(samples: &[Sample]) -> Vec<u16> {
    samples
        .par_iter()
        .map(|&v| round_to_u16((v * OUT_MAX) as f64))
        .collect()
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

/// Derive the `(shadows, midtones)` STF parameters from a set of normalized
/// samples, targeting `target_bg` (in `(0, 1)`) as the background brightness the
/// median should map to (see `--brightness`). `samples` is consumed as scratch:
/// it's reordered by the median selection and then overwritten in place with
/// absolute deviations.
fn find_params(samples: &mut [Sample], target_bg: Sample) -> (Sample, Sample) {
    let med = median(samples);

    for v in samples.iter_mut() {
        *v = (*v - med).abs();
    }
    let mad = median(samples) * MAD_NORM;

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

/// The median of `values`, selecting in place. For an even count this averages
/// the two central elements. Returns 0.0 for an empty slice.
fn median(values: &mut [Sample]) -> Sample {
    let n = values.len();
    if n == 0 {
        return 0.0;
    }

    let mid = n / 2;
    let hi = *select_nth(values, mid);
    if n % 2 == 1 {
        hi
    } else {
        let lo = *select_nth(values, mid - 1);
        (lo + hi) / 2.0
    }
}

/// Partition `values` so the element at `k` is the one that belongs there in
/// sorted order, returning a reference to it (a total order is fine: samples are
/// always finite).
fn select_nth(values: &mut [Sample], k: usize) -> &Sample {
    let (_, nth, _) = values.select_nth_unstable_by(k, |a, b| a.total_cmp(b));
    nth
}

#[cfg(test)]
mod tests {
    use fitskit::Header;
    use super::*;
    use crate::test_support::test_data;

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
    fn median_of_even_and_odd_counts() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    #[test]
    fn stretch_image_preserves_shape_and_type_on_real_cfa_data() {
        // `stretch` never demosaics: a CFA input stays CFA, same width/height,
        // just restretched into a u16 buffer.
        let loaded = crate::fits_file::load_fits(&test_data("cfa_orion.fits")).unwrap();

        let stretched = stretch(&loaded, false, DEFAULT_BRIGHTNESS);

        assert_eq!(stretched.image_type, loaded.image_type);
        assert_eq!(stretched.width, loaded.width);
        assert_eq!(stretched.height, loaded.height);
        match stretched.pixels {
            PixelBuffer::U16(v) => {
                assert_eq!(v.len(), loaded.width * loaded.height);
                assert!(v.iter().any(|&x| x > 0));
                assert!(v.iter().any(|&x| x < u16::MAX));
            }
            PixelBuffer::F32(_) => panic!("expected a u16 pixel buffer"),
        }
    }

    #[test]
    fn stretch_image_preserves_shape_and_type_on_real_debayered_rgb_data() {
        // A debayered RGB image stays RGB and keeps its shape, with all three
        // interleaved planes present.
        let loaded = crate::fits_file::load_fits(&test_data("cfa_orion.fits")).unwrap();
        let rgb = loaded.debayer().unwrap().unwrap();

        let stretched = stretch(&rgb, false, DEFAULT_BRIGHTNESS);

        assert_eq!(stretched.image_type, ImageType::RGB);
        assert_eq!(stretched.width, rgb.width);
        assert_eq!(stretched.height, rgb.height);
        match stretched.pixels {
            PixelBuffer::U16(v) => assert_eq!(v.len(), rgb.width * rgb.height * 3),
            PixelBuffer::F32(_) => panic!("expected a u16 pixel buffer"),
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

        let per_channel = stretch(&image, false, DEFAULT_BRIGHTNESS);
        let linked = stretch(&image, true, DEFAULT_BRIGHTNESS);
        assert_ne!(per_channel.pixels, linked.pixels);
    }
}
