use crate::convert::u16_to_float;
use crate::data::{Image, PixelBuffer};
use rayon::prelude::*;


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
        let mut window: Vec<f32> = Vec::with_capacity(kernel_size * kernel_size);

        for y in 0..self.height as isize {
            for x in 0..self.width as isize {
                window.clear();
                for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        window.push(self.get_pixel( x + dx, y + dy));
                    }
                }
                window.sort_by(|a, b| a.partial_cmp(b).unwrap());
                out[(y as usize) * self.width + (x as usize)] = window[window.len() / 2];
            }
        }
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
/// let percentiles = pixels.percentile(&[2, 98]);
/// println!("2% percentile: {}, 98% percentile: {}", percentiles[0], percentiles[1]);
/// ```
pub fn percentile(data: &[f32], percentiles: &[usize]) -> (Vec<f32>, f32) {
    let mut sorted: Vec<f32> = data.clone().into();
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

const MEDIAN_KERNEL_SIZE: usize = 10;
const GAUSS_SIGMA: f32 = 1.5;
impl Image {

    /// Calculates percentile contrast of the image
    /// C = (P95 - P5) / median
    /// The image is processed with median filter and gaussian filter before calculating
    /// percentile values and median to minimize the influence of stars and dark patches
    /// Contrast value can be used to estimate whether the target features are 
    /// in the image.
    pub fn contrast(&self) -> f32 {
        let pixels = self.luminance();
        let pixels = Pixels::new(match pixels.pixels {
            PixelBuffer::U16(v) => v.par_iter().map(|x| u16_to_float(*x)).collect(),
            PixelBuffer::F32(v) => v,
        }, self.width, self.height);

        let median = Pixels::new( pixels.median_filter(MEDIAN_KERNEL_SIZE), pixels.width, pixels.height);
        let blurred = median.gaussian_filter( GAUSS_SIGMA);
        let (percentiles, median) = percentile(&blurred, &[5, 95]);

        (percentiles[1] - percentiles[0])/ median
    }

}
