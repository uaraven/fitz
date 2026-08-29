use crate::convert::u16_to_float;
use crate::data::{Image, PixelBuffer};
use rayon::prelude::*;
use crate::stretch::DEFAULT_BRIGHTNESS;

/// Build a normalized 1D Gaussian kernel for a given sigma.
fn gaussian_kernel_1d(sigma: f32) -> Vec<f32> {
    let radius = (sigma * 3.0).ceil().max(1.0) as isize;
    let mut kernel: Vec<f32> = (-radius..=radius)
        .map(|i| {
            let x = i as f32;
            (-0.5 * (x * x) / (sigma * sigma)).exp()
        })
        .collect();
    let sum: f32 = kernel.iter().sum();
    for v in kernel.iter_mut() {
        *v /= sum;
    }
    kernel
}

struct Pixels {
    width: usize,
    height: usize,
    values: Vec<f32>
}

impl Pixels {
    fn new(values: Vec<f32>, width: usize, height: usize) -> Self {
        Pixels {
            width,
            height,
            values,
        }
    }

    #[inline]
    fn get_pixel(&self, x: isize, y: isize) -> f32 {
        let cx = x.clamp(0, self.width as isize - 1) as usize;
        let cy = y.clamp(0, self.height as isize - 1) as usize;
        self.values[cy * self.width + cx]
    }

    // calculates a median of the square `kernel_size` x `kernel_size`
    // this removes stars smaller than `kernel_size`/2 and hot pixels
    // but keeps large structures, such as nebulae
    // `kernel_size` must be odd
    pub fn median_filter(&self, kernel_size: usize) -> Vec<f32> {
        assert_eq!(kernel_size % 2, 1, "kernel_size must be odd");
        let radius = (kernel_size / 2) as isize;
        let mut out = vec![0.0f32; self.values.len()];

        out.par_chunks_mut(self.width)
            .enumerate()
            .for_each(|(y, row)| {
                let y = y as isize;
                let mut window: Vec<f32> = Vec::with_capacity(kernel_size * kernel_size);
                for (x, out_px) in row.iter_mut().enumerate() {
                    let x = x as isize;
                    window.clear();
                    for dy in -radius..=radius {
                        for dx in -radius..=radius {
                            window.push(self.get_pixel(x + dx, y + dy));
                        }
                    }
                    let mid = window.len() / 2;
                    window.select_nth_unstable_by(mid, |a, b| a.partial_cmp(b).unwrap());
                    *out_px = window[mid];
                }
            });
        out
    }


    /// Separable Gaussian blur: horizontal pass then vertical pass.
    /// This is the standard O(N * kernel_radius) approach rather than a
    /// full O(N * kernel_radius^2) 2D convolution.
    pub fn gaussian_filter(&self, sigma: f32) -> Vec<f32> {
        if sigma <= 0.0 {
            return self.values.clone();
        }
        let kernel = gaussian_kernel_1d(sigma);
        let radius = (kernel.len() / 2) as isize;

        // Horizontal pass
        let mut temp = vec![0.0f32; self.values.len()];
        for y in 0..self.height as isize {
            for x in 0..self.width as isize {
                let mut acc = 0.0f32;
                for (k, &w) in kernel.iter().enumerate() {
                    let dx = k as isize - radius;
                    acc += self.get_pixel(x + dx, y) * w;
                }
                temp[(y as usize) * self.width + (x as usize)] = acc;
            }
        }

        let tmp_px = Pixels::new(temp, self.width, self.height);

        // Vertical pass
        let mut out = vec![0.0f32; self.values.len()];
        for y in 0..self.height as isize {
            for x in 0..self.width as isize {
                let mut acc = 0.0f32;
                for (k, &w) in kernel.iter().enumerate() {
                    let dy = k as isize - radius;
                    acc += tmp_px.get_pixel(x, y + dy) * w;
                }
                out[(y as usize) * self.width + (x as usize)] = acc;
            }
        }
        out
    }

}


/// Calculate percentiles of the pixel values
/// ```rust
/// let (percentiles, _median) = percentile(&data, &[2, 98]);
/// println!("2% percentile: {}, 98% percentile: {}", percentiles[0], percentiles[1]);
/// ```
pub fn percentile(data: &[f32], percentiles: &[usize]) -> (Vec<f32>, f32) {
    let mut sorted: Vec<f32> = data.to_vec();
    sorted.sort_by(|a,b| a.partial_cmp(b).unwrap());
    let median = if sorted.len() % 2 == 0 {
        let idx = sorted.len()/2;
        (sorted[idx] + sorted[idx-1]) / 2.0
    } else {
        sorted[sorted.len() / 2]
    };
    
    (percentiles.iter().map(|&p| {
        let index = (sorted.len()-1) as f32 * (p as f32 / 100.0);
        let lower = index.floor();
        let upper = index.ceil();
        if lower == upper {
            return sorted[lower as usize]
        }
        // Linear interpolation between the two surrounding values
        let weight = index - lower;
        sorted[lower as usize] * (1.0 - weight) + sorted[upper as usize] * weight
    }).collect(), median)
}

const MEDIAN_KERNEL_SIZE: usize = 5;
const GAUSS_SIGMA: f32 = 1.5;
impl Image {

    /// Calculates percentile contrast of the image
    /// C = (P95 - P5) / median
    /// The image is processed with median filter and gaussian filter before calculating
    /// percentile values and median to minimize the influence of stars and dark patches
    /// Contrast value can be used to estimate whether the target features are
    /// in the image.
    ///
    pub fn contrast(&self) -> f32 {
        let source = self.stretch(true, DEFAULT_BRIGHTNESS);

        let luminance = source.luminance();
        let pixels = Pixels::new(match luminance.pixels {
            PixelBuffer::U16(v) => v.par_iter().map(|x| u16_to_float(*x)).collect(),
            PixelBuffer::F32(v) => v,
        }, source.width, source.height);

        let median = Pixels::new( pixels.median_filter(MEDIAN_KERNEL_SIZE), pixels.width, pixels.height);
        let blurred = median.gaussian_filter( GAUSS_SIGMA);
        let (percentiles, median) = percentile(&blurred, &[5, 95]);

        // A perfectly flat (zero-noise) background stretches to solid black:
        // the auto-stretch has nothing to key the background level off of, so
        // both the spread and the median collapse to exactly 0. That's still
        // "no contrast", not undefined, so guard the division rather than
        // propagate the resulting 0/0 as NaN.
        if median == 0.0 {
            0.0
        } else {
            (percentiles[1] - percentiles[0]) / median
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fits_file::load_fits;
    use crate::keywords::BAYERPAT;
    use crate::test_support::write_fits_with_float_keywords;
    use fitskit::{FitsFile, HeaderValue, ImageData, PixelData};
    use tempfile::TempDir;

    const SIZE: usize = 40;

    fn load_image(name: &str, pixels: Vec<i16>) -> Image {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(name);
        write_fits_with_float_keywords(&path, vec![SIZE, SIZE], PixelData::I16(pixels), &[]);
        load_fits(&path).unwrap()
    }

    /// Writes an RGGB raw mosaic: every `(even x, even y)` Bayer site (the red
    /// sites) gets `base[i] + r_site_offset`, every other site gets `base[i]`
    /// unchanged. Simulates a scene that's identical across color channels
    /// (`base`) plus a fixed per-channel calibration/filter imbalance
    /// (`r_site_offset`), which is what a real narrowband dual-band filter
    /// produces on an OSC sensor.
    fn load_cfa_image(name: &str, size: usize, base: &[i16], r_site_offset: i16) -> Image {
        let mut pixels = base.to_vec();
        for y in (0..size).step_by(2) {
            for x in (0..size).step_by(2) {
                pixels[y * size + x] += r_site_offset;
            }
        }
        let img = ImageData::new(vec![size, size], PixelData::I16(pixels));
        let mut fits = FitsFile::with_primary_image(img);
        fits.primary_mut()
            .header
            .set(BAYERPAT, HeaderValue::String("RGGB".to_string()), None);
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(name);
        fits.to_file(&path).unwrap();
        load_fits(&path).unwrap()
    }

    #[test]
    fn flat_background_has_near_zero_contrast() {
        let pixels = vec![10_000i16; SIZE * SIZE];
        let image = load_image("flat.fits", pixels);
        let contrast = image.contrast();
        assert!(
            contrast.abs() < 1e-4,
            "expected a uniform frame to have ~0 contrast, got {contrast}"
        );
    }

    #[test]
    fn large_scale_split_has_higher_contrast_than_flat_background() {
        let mut pixels = vec![10_000i16; SIZE * SIZE];
        // Fill the bottom half with a much brighter value: a region large
        // enough to survive the median filter, standing in for a real large-
        // scale feature (e.g. a nebula) rather than noise or a star.
        for y in (SIZE / 2)..SIZE {
            for x in 0..SIZE {
                pixels[y * SIZE + x] = 30_000;
            }
        }
        let image = load_image("split.fits", pixels);
        let flat = load_image("flat.fits", vec![10_000i16; SIZE * SIZE]);

        let split_contrast = image.contrast();
        let flat_contrast = flat.contrast();
        assert!(
            split_contrast > flat_contrast,
            "expected the split frame's contrast ({split_contrast}) to exceed the flat frame's ({flat_contrast})"
        );
    }

    #[test]
    fn cfa_channel_imbalance_does_not_outrank_a_real_large_scale_feature() {
        const N: usize = 60;

        // A flat scene through a narrowband-style filter where the red Bayer
        // sites see far more signal than green/blue: the raw mosaic
        // alternates between very different values even though the scene
        // itself has no large-scale structure at all.
        let imbalanced_flat = load_cfa_image("imbalanced_flat.fits", N, &[1_000i16; N * N], 8_000);

        // A real two-level large-scale feature with balanced Bayer sites (no
        // per-channel offset): a much smaller amplitude than the raw
        // checkerboard above, but genuine image content.
        let mut base = vec![1_000i16; N * N];
        for y in (N / 2)..N {
            for x in 0..N {
                base[y * N + x] = 4_000;
            }
        }
        let real_gradient = load_cfa_image("real_gradient.fits", N, &base, 0);

        let imbalanced_contrast = imbalanced_flat.contrast();
        let gradient_contrast = real_gradient.contrast();
        assert!(
            gradient_contrast > imbalanced_contrast,
            "a real large-scale feature ({gradient_contrast}) should score higher than pure \
             per-channel Bayer imbalance on an otherwise flat scene ({imbalanced_contrast})"
        );
        assert!(
            imbalanced_contrast.abs() < 0.05,
            "a flat scene should have ~0 contrast regardless of Bayer channel imbalance, got {imbalanced_contrast}"
        );
    }
}
