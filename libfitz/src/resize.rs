//! Generic box-filter resizing of interleaved 16-bit RGB image buffers, used
//! to fit an image into a bounded canvas (a terminal preview, a GUI thumbnail
//! or blink view, …) while preserving its aspect ratio.

use rayon::prelude::*;

/// Scale an interleaved 16-bit RGB image so it fits within `max_w` x `max_h`
/// while preserving its aspect ratio, returning the new `(width, height,
/// samples)`. Uses box (area-average) sampling, the right filter for large
/// down-scales.
pub fn resize_to_fit(
    src: &[u16],
    src_w: usize,
    src_h: usize,
    max_w: usize,
    max_h: usize,
) -> (usize, usize, Vec<u16>) {
    let (dst_w, dst_h) = fit_dimensions(src_w, src_h, max_w, max_h);
    let scaled = resize_rgb(src, src_w, src_h, dst_w, dst_h);
    (dst_w, dst_h, scaled)
}

/// The largest `(width, height)` that fits inside `max_w` x `max_h` with the
/// same aspect ratio as `src_w` x `src_h`. Both dimensions are kept at least 1
/// for any non-empty source; an empty source or box maps to `(0, 0)`.
pub fn fit_dimensions(src_w: usize, src_h: usize, max_w: usize, max_h: usize) -> (usize, usize) {
    if src_w == 0 || src_h == 0 || max_w == 0 || max_h == 0 {
        return (0, 0);
    }
    let scale = (max_w as f64 / src_w as f64).min(max_h as f64 / src_h as f64);
    let w = ((src_w as f64 * scale).round() as usize).max(1);
    let h = ((src_h as f64 * scale).round() as usize).max(1);
    (w, h)
}

/// Resample an interleaved RGB image from `src_w` x `src_h` to `dst_w` x
/// `dst_h`. Each destination pixel is the average of the block of source pixels
/// it maps to (a box filter); the integer source spans partition the image
/// exactly when down-scaling, with no gaps or overlap. Returns an empty buffer
/// for a zero-sized target.
pub fn resize_rgb(src: &[u16], src_w: usize, src_h: usize, dst_w: usize, dst_h: usize) -> Vec<u16> {
    if dst_w == 0 || dst_h == 0 {
        return Vec::new();
    }
    let mut out = vec![0u16; dst_w * dst_h * 3];
    // Each destination row reads a disjoint span of source rows and writes its
    // own output row, so rows are independent and processed in parallel.
    out.par_chunks_mut(dst_w * 3)
        .enumerate()
        .for_each(|(dy, out_row)| {
            let sy0 = dy * src_h / dst_h;
            let sy1 = (((dy + 1) * src_h) / dst_h).max(sy0 + 1);
            for dx in 0..dst_w {
                let sx0 = dx * src_w / dst_w;
                let sx1 = (((dx + 1) * src_w) / dst_w).max(sx0 + 1);

                let (mut r, mut g, mut b, mut count) = (0u64, 0u64, 0u64, 0u64);
                for sy in sy0..sy1 {
                    let row = sy * src_w;
                    for sx in sx0..sx1 {
                        let i = (row + sx) * 3;
                        r += src[i] as u64;
                        g += src[i + 1] as u64;
                        b += src[i + 2] as u64;
                        count += 1;
                    }
                }

                let o = dx * 3;
                out_row[o] = (r / count) as u16;
                out_row[o + 1] = (g / count) as u16;
                out_row[o + 2] = (b / count) as u16;
            }
        });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_dimensions_is_width_limited_for_landscape() {
        // 3:2 image into a square box: width fills, height scales to match.
        assert_eq!(fit_dimensions(300, 200, 60, 60), (60, 40));
    }

    #[test]
    fn fit_dimensions_is_height_limited_for_portrait() {
        // tall image into a wide box: height fills, width scales to match.
        assert_eq!(fit_dimensions(200, 400, 100, 50), (25, 50));
    }

    #[test]
    fn fit_dimensions_keeps_at_least_one_pixel() {
        // an extreme aspect ratio must not round a dimension down to zero.
        let (w, h) = fit_dimensions(3008, 4, 80, 48);
        assert!(w >= 1 && h >= 1);
    }

    #[test]
    fn fit_dimensions_handles_empty_source() {
        assert_eq!(fit_dimensions(0, 0, 80, 24), (0, 0));
    }

    #[test]
    fn resize_rgb_averages_block_to_single_pixel() {
        // 2x2 distinct pixels averaged into one: per-channel arithmetic mean.
        let src: Vec<u16> = vec![
            0, 1, 2, 10, 11, 12, //
            20, 21, 22, 30, 31, 32,
        ];
        assert_eq!(resize_rgb(&src, 2, 2, 1, 1), vec![15, 16, 17]);
    }

    #[test]
    fn resize_rgb_preserves_solid_color() {
        let src: Vec<u16> = std::iter::repeat_n([7u16, 8, 9], 16).flatten().collect();
        let out = resize_rgb(&src, 4, 4, 2, 3);
        assert_eq!(out.len(), 2 * 3 * 3);
        assert!(out.chunks_exact(3).all(|c| c == [7, 8, 9]));
    }

    #[test]
    fn resize_rgb_upscales_without_panicking() {
        // 1x1 source replicated across a larger target.
        let out = resize_rgb(&[1, 2, 3], 1, 1, 3, 2);
        assert_eq!(out.len(), 3 * 2 * 3);
        assert!(out.chunks_exact(3).all(|c| c == [1, 2, 3]));
    }
}
