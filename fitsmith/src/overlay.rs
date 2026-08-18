//! Draws the star-detection overlay for View ▸ Show stars: a thin green ring
//! around each detected star, on top of the already-rendered preview. Lives
//! here rather than in `libfitz` since no `fitz` CLI command needs it — it's
//! purely a GUI display concern, like `chart.rs`/`chart_svg.rs`.

use image::{Rgb, RgbImage};
use libfitz::stars::Star;

/// Bright green, easy to spot against a stretched astronomical frame.
const RING_COLOR: Rgb<u8> = Rgb([0, 255, 0]);
/// A ring never shrinks below this radius, so a tightly focused (small HFR)
/// star's marker stays visible rather than collapsing to a dot.
const MIN_RADIUS: f64 = 3.0;
/// The ring is drawn a few times a star's half-flux radius out, so it frames
/// the star's visible disc rather than cutting through it.
const RADIUS_SCALE: f64 = 3.0;

/// A copy of `preview` with a green ring drawn around every star in `stars`.
/// Star coordinates are on the detection plane, which is always the same
/// width/height as the rendered preview (debayering and stretching don't
/// resample), so no coordinate mapping is needed.
pub fn draw_star_rings(preview: &RgbImage, stars: &[Star]) -> RgbImage {
    let mut out = preview.clone();
    for star in stars {
        let radius = (star.hfr * RADIUS_SCALE).max(MIN_RADIUS);
        draw_ring(&mut out, star.x, star.y, radius);
    }
    out
}

/// Plot a circle of the given radius centered at `(cx, cy)` as a sequence of
/// points close enough together to leave no gaps, clipping anything outside
/// the image bounds.
fn draw_ring(img: &mut RgbImage, cx: f64, cy: f64, radius: f64) {
    let steps = ((2.0 * std::f64::consts::PI * radius).ceil() as u32).max(16);
    for i in 0..steps {
        let theta = 2.0 * std::f64::consts::PI * (i as f64) / (steps as f64);
        let (x, y) = (cx + radius * theta.cos(), cy + radius * theta.sin());
        if x >= 0.0 && y >= 0.0 {
            let (xi, yi) = (x.round() as u32, y.round() as u32);
            if xi < img.width() && yi < img.height() {
                img.put_pixel(xi, yi, RING_COLOR);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn star(x: f64, y: f64, hfr: f64) -> Star {
        Star {
            x,
            y,
            flux: 1.0,
            hfr,
            fwhm: 1.0,
            e1: 0.0,
            e2: 0.0,
        }
    }

    #[test]
    fn draws_a_ring_around_a_centered_star() {
        let preview = RgbImage::from_pixel(40, 40, Rgb([10, 10, 10]));
        let out = draw_star_rings(&preview, &[star(20.0, 20.0, 3.0)]);

        let green_pixels: Vec<(u32, u32)> = out
            .enumerate_pixels()
            .filter(|(_, _, p)| **p == RING_COLOR)
            .map(|(x, y, _)| (x, y))
            .collect();
        assert!(!green_pixels.is_empty());

        // Every drawn point sits at (radius ± rounding) from the center.
        let radius = 3.0 * RADIUS_SCALE;
        for (x, y) in &green_pixels {
            let d = ((*x as f64 - 20.0).powi(2) + (*y as f64 - 20.0).powi(2)).sqrt();
            assert!((d - radius).abs() < 1.0, "point ({x},{y}) at distance {d}");
        }

        // The original buffer is untouched.
        assert_eq!(*preview.get_pixel(20, 20), Rgb([10, 10, 10]));
    }

    #[test]
    fn faint_stars_get_a_floor_sized_ring() {
        let preview = RgbImage::from_pixel(40, 40, Rgb([0, 0, 0]));
        let out = draw_star_rings(&preview, &[star(20.0, 20.0, 0.1)]);

        let has_far_pixel = out
            .enumerate_pixels()
            .any(|(x, y, p)| *p == RING_COLOR && (x as f64 - 20.0).hypot(y as f64 - 20.0) > 3.0);
        assert!(has_far_pixel, "ring should not collapse for a tiny HFR");
    }

    #[test]
    fn a_star_off_the_edge_only_draws_the_part_on_screen() {
        let preview = RgbImage::from_pixel(20, 20, Rgb([0, 0, 0]));
        // No panic despite the ring extending past every edge.
        let out = draw_star_rings(&preview, &[star(0.0, 0.0, 4.0)]);
        assert!(out.pixels().any(|p| *p == RING_COLOR));
    }

    #[test]
    fn no_stars_leaves_the_image_unchanged() {
        let preview = RgbImage::from_pixel(10, 10, Rgb([5, 6, 7]));
        let out = draw_star_rings(&preview, &[]);
        assert_eq!(out, preview);
    }
}
