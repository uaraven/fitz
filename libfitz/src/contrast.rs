use crate::convert::u16_to_float;
use crate::data::{Image, PixelBuffer};
use rayon::prelude::*;
use crate::resize::fit_dimensions;
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

    pub fn resize(&self, dst_w: usize, dst_h: usize) -> Pixels {
        if dst_w == 0 || dst_h == 0 {
            return Pixels::new(vec![], 0, 0);
        }
        let mut out = vec![0f32; dst_w * dst_h];
        // Each destination row reads a disjoint span of source rows and writes its
        // own output row, so rows are independent and processed in parallel.
        out.par_chunks_mut(dst_w)
            .enumerate()
            .for_each(|(dy, out_row)| {
                let sy0 = dy * self.height / dst_h;
                let sy1 = (((dy + 1) * self.height) / dst_h).max(sy0 + 1);
                for dx in 0..dst_w {
                    let sx0 = dx * self.width / dst_w;
                    let sx1 = (((dx + 1) * self.width) / dst_w).max(sx0 + 1);

                    let mut px = 0f32;
                    let mut count = 0usize;
                    for sy in sy0..sy1 {
                        let row = sy * self.width;
                        for sx in sx0..sx1 {
                            px += self.values[row + sx];
                            count += 1;
                        }
                    }

                    out_row[dx] = px / count as f32;
                }
            });
        Pixels::new(out, dst_w, dst_h)
    }


}

/// Calculate percentiles of the pixel values
/// ```rust
/// # use libfitz::contrast::percentile;
/// let data = [0.1, 0.2, 0.3, 0.4, 0.5];
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

const MEDIAN_KERNEL_SIZE: usize = 3;
const GAUSS_SIGMA: f32 = 1.0;
const PERCENTILE_IMG_SIZE: usize = 512;

/// The dimensions to resize `width` x `height` down to before the median
/// filter, Gaussian blur and percentile pass: fit within
/// `PERCENTILE_IMG_SIZE` x `PERCENTILE_IMG_SIZE` preserving aspect ratio, or
/// left unchanged if it already fits within that box (never upscales).
fn contrast_target_dimensions(width: usize, height: usize) -> (usize, usize) {
    if width > PERCENTILE_IMG_SIZE || height > PERCENTILE_IMG_SIZE {
        fit_dimensions(width, height, PERCENTILE_IMG_SIZE, PERCENTILE_IMG_SIZE)
    } else {
        (width, height)
    }
}

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

        let (target_w, target_h) = contrast_target_dimensions(pixels.width, pixels.height);
        let pixels = if (target_w, target_h) == (pixels.width, pixels.height) {
            pixels
        } else {
            pixels.resize(target_w, target_h)
        };
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
    use crate::resize::resize_rgb;
    use crate::test_support::write_fits_with_float_keywords;
    use fitskit::{FitsFile, HeaderValue, ImageData, PixelData};
    use tempfile::TempDir;

    const SIZE: usize = 40;

    fn load_image(name: &str, width: usize, height: usize, pixels: Vec<i16>) -> Image {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(name);
        write_fits_with_float_keywords(&path, vec![width, height], PixelData::I16(pixels), &[]);
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
        let image = load_image("flat.fits", SIZE, SIZE, pixels);
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
        let image = load_image("split.fits", SIZE, SIZE, pixels);
        let flat = load_image("flat.fits", SIZE, SIZE, vec![10_000i16; SIZE * SIZE]);

        let split_contrast = image.contrast();
        let flat_contrast = flat.contrast();
        assert!(
            split_contrast > flat_contrast,
            "expected the split frame's contrast ({split_contrast}) to exceed the flat frame's ({flat_contrast})"
        );
    }

    #[test]
    fn hot_pixel_stars_do_not_inflate_contrast_of_a_flat_background() {
        // A handful of single-pixel "stars" scattered on an otherwise flat
        // background: rejecting these before the percentile step is the
        // whole reason for the median filter, so they must not register as
        // large-scale contrast, no matter how MEDIAN_KERNEL_SIZE is tuned.
        let mut pixels = vec![10_000i16; SIZE * SIZE];
        for &(x, y) in &[(5, 5), (12, 30), (25, 10), (33, 33), (20, 20)] {
            pixels[y * SIZE + x] = 30_000;
        }
        let image = load_image("hot_pixels.fits", SIZE, SIZE, pixels);
        let contrast = image.contrast();
        assert!(
            contrast.abs() < 1e-4,
            "expected scattered hot pixels to be rejected by the median filter, got {contrast}"
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

    #[test]
    fn contrast_target_dimensions_keeps_small_images_unchanged() {
        assert_eq!(contrast_target_dimensions(300, 200), (300, 200));
    }

    #[test]
    fn contrast_target_dimensions_at_the_threshold_is_unchanged() {
        // Strictly-greater-than: exactly PERCENTILE_IMG_SIZE on both sides
        // must not trigger a resize.
        assert_eq!(
            contrast_target_dimensions(PERCENTILE_IMG_SIZE, 300),
            (PERCENTILE_IMG_SIZE, 300)
        );
    }

    #[test]
    fn contrast_target_dimensions_downscales_when_only_width_exceeds() {
        // A previous `&&`-based guard skipped resizing whenever only one
        // dimension was oversized; an elongated frame like this must still
        // trigger a downscale.
        assert_eq!(contrast_target_dimensions(1024, 300), (512, 150));
    }

    #[test]
    fn contrast_target_dimensions_downscales_when_only_height_exceeds() {
        assert_eq!(contrast_target_dimensions(300, 1024), (150, 512));
    }

    #[test]
    fn contrast_target_dimensions_downscales_preserving_aspect_ratio() {
        // 2:1 aspect ratio in, 2:1 aspect ratio out, scaled down to fit.
        assert_eq!(contrast_target_dimensions(4096, 2048), (512, 256));
    }

    #[test]
    fn resize_averages_block_to_single_pixel() {
        // Same source values as resize_rgb_averages_block_to_single_pixel's R
        // channel in resize.rs, so the expected average is a proven case.
        let pixels = Pixels::new(vec![0.0, 10.0, 20.0, 30.0], 2, 2);
        let resized = pixels.resize(1, 1);

        assert_eq!((resized.width, resized.height), (1, 1));
        assert_eq!(resized.values, vec![15.0]);
    }

    #[test]
    fn resize_preserves_solid_color() {
        let pixels = Pixels::new(vec![7.0; 16], 4, 4);
        let resized = pixels.resize(2, 3);

        assert_eq!(resized.values.len(), 6);
        assert!(resized.values.iter().all(|&v| v == 7.0));
    }

    #[test]
    fn resize_upscales_without_panicking() {
        let pixels = Pixels::new(vec![5.0], 1, 1);
        let resized = pixels.resize(3, 2);

        assert_eq!(resized.values.len(), 6);
        assert!(resized.values.iter().all(|&v| v == 5.0));
    }

    #[test]
    fn resize_zero_dst_dimension_returns_empty() {
        let pixels = Pixels::new(vec![1.0, 2.0, 3.0, 4.0], 2, 2);

        let resized = pixels.resize(0, 5);
        assert_eq!((resized.width, resized.height), (0, 0));
        assert!(resized.values.is_empty());

        let resized = pixels.resize(5, 0);
        assert_eq!((resized.width, resized.height), (0, 0));
        assert!(resized.values.is_empty());
    }

    #[test]
    fn resize_averages_each_quadrant_independently() {
        // 4x4 -> 2x2 is an exact factor-of-2 downscale: each destination pixel
        // is the average of one disjoint 2x2 source quadrant. Distinct values
        // per pixel mean a gap or overlap in the block partitioning would
        // shift the result away from these hand-computed quadrant averages.
        let values: Vec<f32> = (0..16).map(|i| i as f32).collect();
        let resized = Pixels::new(values, 4, 4).resize(2, 2);

        assert_eq!((resized.width, resized.height), (2, 2));
        assert_eq!(resized.values, vec![2.5, 4.5, 10.5, 12.5]);
    }

    #[test]
    fn contrast_downscale_path_handles_an_elongated_oversized_frame() {
        // 600 wide x 300 tall: only the width exceeds PERCENTILE_IMG_SIZE, the
        // exact shape a previous `&&`-based guard would have skipped
        // entirely. Exercises the full contrast() pipeline (stretch,
        // luminance, resize, median filter, Gaussian blur, percentile) at a
        // size that actually engages the downscale path, not just the small
        // fixtures used elsewhere in this module.
        const W: usize = 600;
        const H: usize = 300;
        let mut pixels = vec![10_000i16; W * H];
        for y in (H / 2)..H {
            for x in 0..W {
                pixels[y * W + x] = 30_000;
            }
        }
        let image = load_image("elongated_split.fits", W, H, pixels);
        let flat = load_image("elongated_flat.fits", W, H, vec![10_000i16; W * H]);

        let split_contrast = image.contrast();
        let flat_contrast = flat.contrast();
        assert!(split_contrast.is_finite());
        assert!(
            split_contrast > flat_contrast,
            "expected the split frame's contrast ({split_contrast}) to exceed the flat frame's ({flat_contrast})"
        );
    }

    #[test]
    fn resize_matches_resize_rgb_on_equivalent_data() {
        // 4x4 -> 2x3 is not an even factor, so it exercises the same
        // ragged block-boundary math (sy0/sy1/sx0/sx1) that resize_rgb
        // uses. Values are multiples of 4 so every block's sum divides
        // evenly, keeping resize_rgb's truncating u16 division exactly
        // equal to Pixels::resize's f32 average rather than off by rounding.
        let mono: Vec<f32> = (0..16).map(|i| (i * 4) as f32).collect();
        let rgb: Vec<u16> = mono.iter().flat_map(|&v| [v as u16; 3]).collect();

        let resized_mono = Pixels::new(mono, 4, 4).resize(2, 3);
        let resized_rgb = resize_rgb(&rgb, 4, 4, 2, 3);

        assert_eq!((resized_mono.width, resized_mono.height), (2, 3));
        for (i, &value) in resized_mono.values.iter().enumerate() {
            assert_eq!(resized_rgb[i * 3], resized_rgb[i * 3 + 1], "cell {i}");
            assert_eq!(resized_rgb[i * 3], resized_rgb[i * 3 + 2], "cell {i}");
            assert_eq!(value, resized_rgb[i * 3] as f32, "cell {i}");
        }
    }
}
