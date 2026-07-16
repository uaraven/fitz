//! Star detection and per-star shape measurement on a [`MonoPlane`]: threshold
//! against the plane's own robust background, flood-fill the connected blobs
//! above it, reject everything that isn't a usable star, and measure what
//! survives — HFR, FWHM and eccentricity, aggregated to medians across the
//! frame. Pure and `Send` — no file I/O, no terminal output — so the FitSmith
//! star-metrics dialog and the CLI's `info --stars` both drive it.

use rayon::prelude::*;

use crate::fits_image::MonoPlane;
use crate::info::{PixelStats, median_in_place, stats_from_values};

/// Multiplier turning a Gaussian's standard deviation into its full width at
/// half maximum: `2 * sqrt(2 * ln 2)`.
const FWHM_PER_SIGMA: f64 = 2.3548;

/// Tuning for [`detect_stars`]. Not user-configurable — [`Default`] is the only
/// constructor the frontends use. It is a struct rather than three constants so
/// the tests can drive each rejection path directly.
#[derive(Clone, Copy, Debug)]
pub struct StarDetectOptions {
    /// Detection threshold in MAD-sigmas above the background.
    pub sigma_k: f64,
    /// Smallest blob accepted as a star, in pixels — rejects hot pixels.
    pub min_pixels: usize,
    /// Largest blob accepted, in pixels — rejects nebulosity, satellite trails,
    /// and the halo of a bright star.
    pub max_pixels: usize,
}

impl Default for StarDetectOptions {
    fn default() -> Self {
        // Full-resolution numbers. A CFA frame detects on a half-resolution
        // green super-pixel plane, where every blob's area is ~4x smaller — a
        // star covering 20 px on the sensor covers ~5 px there, right at the
        // floor. The bounds are not scaled by the plane's sampling because the
        // real mosaic's detected count (pinned in this module's tests) shows
        // the floor is not eating its stars; scale them here, not lower them
        // globally, if that ever stops being true.
        Self {
            sigma_k: 5.0,
            min_pixels: 5,
            max_pixels: 2000,
        }
    }
}

/// One detected star: its centroid on the detection plane, its
/// background-subtracted flux, and its measured shape.
pub struct Star {
    pub x: f64,
    pub y: f64,
    pub flux: f64,
    /// Half-flux radius: the flux-weighted mean radius, NINA's definition.
    pub hfr: f64,
    pub fwhm: f64,
    pub eccentricity: f64,
}

/// A frame's star metrics: how many stars were accepted, and the median of each
/// shape measurement across them.
pub struct StarStats {
    pub count: usize,
    /// Median across accepted stars; `None` when none were accepted.
    pub hfr: Option<f64>,
    pub fwhm: Option<f64>,
    pub eccentricity: Option<f64>,
}

/// The detection plane's own background statistics, for [`detect_stars`]'s
/// threshold.
///
/// Kept separate from [`detect_stars`] because the threshold must reflect the
/// noise of *the plane detection runs on*, not the frame's — and on a mosaic the
/// two are not close. Averaging two green sites lowers σ by ~√2, but the bigger
/// effect is that a mosaic's MAD is not a noise estimate at all: it is dominated
/// by the level differences *between* the R, G and B sites. On the bundled
/// `uncompressed.fit` the frame's MAD is 302 ADU against the green plane's 53,
/// so a threshold built from the frame's would sit nearly 6x too high and detect
/// almost nothing on exactly the frames CFA users care about.
///
/// Separate, it also lets a mono caller pass the frame's already-computed
/// [`PixelStats`] — for a mono frame the detection plane *is* the frame, so they
/// are the same numbers.
///
/// Clones `plane.values`: [`stats_from_values`] reorders what it is given, and a
/// detection plane is addressed by index — position is the image. For a CFA
/// frame that clone is a quarter-size buffer; for a mono frame it is the size of
/// the frame's own `f64` values. Correctness beats saving the allocation.
pub fn plane_background(plane: &MonoPlane) -> PixelStats {
    stats_from_values(&mut plane.values.clone())
}

/// Detect the stars on `plane` and aggregate their shapes to per-frame medians.
///
/// `bg` is the plane's own background — see [`plane_background`]. The threshold
/// is `bg.median + sigma_k * bg.mad`: both are robust to the very stars being
/// detected, which is why it isn't chicken-and-egg.
///
/// HFR and FWHM come out in *the plane's* pixels, which for a CFA mosaic are
/// half-resolution — see [`crate::fits_image::detection_plane`].
pub fn detect_stars(plane: &MonoPlane, bg: &PixelStats, opts: &StarDetectOptions) -> StarStats {
    let threshold = bg.median + opts.sigma_k * bg.mad;
    let mut mask: Vec<bool> = plane.values.par_iter().map(|&v| v > threshold).collect();

    let blobs = blobs_above_threshold(&mut mask, plane.width, plane.height);
    let stars: Vec<Star> = blobs
        .par_iter()
        .filter(|blob| accept(blob, plane, opts))
        .filter_map(|blob| measure(blob, plane, bg.median))
        .collect();

    aggregate(&stars)
}

/// Every 8-connected blob of set cells in `mask`, as pixel indices.
///
/// The fill is iterative with an explicit stack, never recursive: a bright
/// nebula is one blob spanning millions of pixels and would blow the stack.
/// Each visited cell is cleared, so the mask doubles as the visited set. (A
/// run-length + union-find pass is the fallback if profiling ever demands it;
/// it is not warranted up front.)
fn blobs_above_threshold(mask: &mut [bool], width: usize, height: usize) -> Vec<Vec<usize>> {
    let mut blobs = Vec::new();
    let mut stack = Vec::new();

    for start in 0..mask.len() {
        if !mask[start] {
            continue;
        }
        mask[start] = false;
        stack.push(start);
        let mut blob = Vec::new();

        while let Some(i) = stack.pop() {
            blob.push(i);
            let (x, y) = (i % width, i / width);
            for ny in y.saturating_sub(1)..(y + 2).min(height) {
                for nx in x.saturating_sub(1)..(x + 2).min(width) {
                    let n = ny * width + nx;
                    if mask[n] {
                        mask[n] = false;
                        stack.push(n);
                    }
                }
            }
        }
        blobs.push(blob);
    }
    blobs
}

/// Whether a blob is a star worth measuring: within the area bounds, clear of
/// the frame border (a truncated PSF makes garbage moments), and not
/// flat-topped.
///
/// The saturation ceiling comes from `plane.saturation` — the *source* sample
/// type's — and never from the background's `PixelStats::saturation`, which on
/// the plane's `f64` values is merely the observed maximum and would therefore
/// reject the brightest star in every frame. The rejection exists to drop stars
/// that are genuinely clipped: a flat top biases HFR low, which is exactly the
/// frame you would otherwise wrongly call well-focused.
fn accept(blob: &[usize], plane: &MonoPlane, opts: &StarDetectOptions) -> bool {
    if blob.len() < opts.min_pixels || blob.len() > opts.max_pixels {
        return false;
    }
    let (w, h) = (plane.width, plane.height);
    blob.iter().all(|&i| {
        let (x, y) = (i % w, i / w);
        x > 0 && y > 0 && x + 1 < w && y + 1 < h && plane.values[i] < plane.saturation
    })
}

/// Measure one blob's centroid and shape from its background-subtracted flux.
/// `None` for a blob with no positive flux, which has no centroid to speak of.
fn measure(blob: &[usize], plane: &MonoPlane, background: f64) -> Option<Star> {
    let w = plane.width;
    let flux_at = |i: usize| plane.values[i] - background;
    let position = |i: usize| ((i % w) as f64, (i / w) as f64);

    let (mut sum_f, mut sum_fx, mut sum_fy) = (0.0, 0.0, 0.0);
    for &i in blob {
        let (f, (x, y)) = (flux_at(i), position(i));
        sum_f += f;
        sum_fx += f * x;
        sum_fy += f * y;
    }
    if sum_f <= 0.0 {
        return None;
    }
    let (cx, cy) = (sum_fx / sum_f, sum_fy / sum_f);

    // Second pass, now that the centroid is known: the flux-weighted mean
    // radius (HFR) and the second moments around the centroid.
    let (mut sum_fr, mut mxx, mut myy, mut mxy) = (0.0, 0.0, 0.0, 0.0);
    for &i in blob {
        let (f, (x, y)) = (flux_at(i), position(i));
        let (dx, dy) = (x - cx, y - cy);
        sum_fr += f * dx.hypot(dy);
        mxx += f * dx * dx;
        myy += f * dy * dy;
        mxy += f * dx * dy;
    }
    let (mxx, myy, mxy) = (mxx / sum_f, myy / sum_f, mxy / sum_f);

    Some(Star {
        x: cx,
        y: cy,
        flux: sum_f,
        hfr: sum_fr / sum_f,
        fwhm: FWHM_PER_SIGMA * ((mxx + myy) / 2.0).max(0.0).sqrt(),
        eccentricity: eccentricity(mxx, myy, mxy),
    })
}

/// Eccentricity from the second moments: `sqrt(1 - λ₂/λ₁)` over the eigenvalues
/// of the moment matrix — 0 for a round star, approaching 1 for a streak.
fn eccentricity(mxx: f64, myy: f64, mxy: f64) -> f64 {
    let mean = (mxx + myy) / 2.0;
    let spread = (((mxx - myy) / 2.0).powi(2) + mxy * mxy).sqrt();
    let (major, minor) = (mean + spread, mean - spread);
    if major <= 0.0 {
        return 0.0;
    }
    (1.0 - (minor / major).clamp(0.0, 1.0)).sqrt()
}

/// Reduce per-star measurements to per-frame medians — medians, not means, so
/// one satellite streak that survives the rejections cannot move the number.
fn aggregate(stars: &[Star]) -> StarStats {
    let median_of = |f: fn(&Star) -> f64| {
        (!stars.is_empty()).then(|| median_in_place(&mut stars.iter().map(f).collect::<Vec<_>>()))
    };
    StarStats {
        count: stars.len(),
        hfr: median_of(|s| s.hfr),
        fwhm: median_of(|s| s.fwhm),
        eccentricity: median_of(|s| s.eccentricity),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fits_image::{detection_plane, find_image_hdu};
    use crate::test_support::{test_data, write_star_field_fits};
    use fitskit::FitsFile;
    use tempfile::TempDir;

    /// The detection plane of a synthetic star field, reached the way the
    /// frontends reach it: through a real unsigned-16 FITS frame, so the plane's
    /// saturation is the source sample type's rather than a test's assertion.
    fn star_field_plane(
        width: usize,
        height: usize,
        background: f64,
        stars: &[(f64, f64, f64, f64, f64)],
    ) -> MonoPlane {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("field.fits");
        write_star_field_fits(&path, width, height, background, stars);

        let fits = FitsFile::from_file(&path).unwrap();
        let (header, img) = find_image_hdu(&fits, &path).unwrap();
        detection_plane(header, img.as_ref()).unwrap()
    }

    /// Detect on a synthetic field with the shipping defaults.
    fn detect(plane: &MonoPlane) -> StarStats {
        detect_stars(
            plane,
            &plane_background(plane),
            &StarDetectOptions::default(),
        )
    }

    /// Every star's centroid, in detection order.
    fn stars_of(plane: &MonoPlane) -> Vec<Star> {
        let bg = plane_background(plane);
        let opts = StarDetectOptions::default();
        let threshold = bg.median + opts.sigma_k * bg.mad;
        let mut mask: Vec<bool> = plane.values.iter().map(|&v| v > threshold).collect();
        blobs_above_threshold(&mut mask, plane.width, plane.height)
            .iter()
            .filter(|b| accept(b, plane, &opts))
            .filter_map(|b| measure(b, plane, bg.median))
            .collect()
    }

    #[test]
    fn detects_every_star_in_a_synthetic_field() {
        // Nine round stars on a 3x3 grid, well clear of each other and of the
        // border.
        let truth: Vec<(f64, f64, f64, f64, f64)> = (0..3)
            .flat_map(|r| {
                (0..3).map(move |c| {
                    (
                        20.0 + 30.0 * c as f64,
                        20.0 + 30.0 * r as f64,
                        2.0,
                        2.0,
                        5000.0,
                    )
                })
            })
            .collect();
        let plane = star_field_plane(100, 100, 1000.0, &truth);

        assert_eq!(detect(&plane).count, 9);

        // Every truth position is matched by a centroid within 0.1 px.
        let found = stars_of(&plane);
        for &(x, y, ..) in &truth {
            assert!(
                found
                    .iter()
                    .any(|s| (s.x - x).abs() < 0.1 && (s.y - y).abs() < 0.1),
                "no centroid within 0.1 px of ({x}, {y})"
            );
        }
    }

    #[test]
    fn fwhm_and_hfr_match_the_gaussian_they_were_measured_from() {
        const SIGMA: f64 = 2.0;
        let plane = star_field_plane(60, 60, 1000.0, &[(30.0, 30.0, SIGMA, SIGMA, 5000.0)]);
        let stats = detect(&plane);
        assert_eq!(stats.count, 1);

        // A 2D Gaussian's FWHM is 2.3548σ and its flux-weighted mean radius is
        // sqrt(π/2)σ ≈ 1.2533σ.
        let (fwhm, hfr) = (stats.fwhm.unwrap(), stats.hfr.unwrap());
        let (true_fwhm, true_hfr) = (FWHM_PER_SIGMA * SIGMA, 1.2533 * SIGMA);
        assert!((fwhm - true_fwhm).abs() < 0.15 * true_fwhm, "fwhm {fwhm}");
        assert!((hfr - true_hfr).abs() < 0.15 * true_hfr, "hfr {hfr}");

        // Both are biased *low*, and the direction is a property of the method,
        // not slop: thresholding truncates the wings, and the flux this drops
        // is all at large radius. A bound on |error| alone would hide the bias
        // flipping sign.
        assert!(fwhm < true_fwhm, "fwhm {fwhm} should be biased low");
        assert!(hfr < true_hfr, "hfr {hfr} should be biased low");
    }

    #[test]
    fn eccentricity_measures_elongation() {
        let round = star_field_plane(60, 60, 1000.0, &[(30.0, 30.0, 2.0, 2.0, 5000.0)]);
        assert!(detect(&round).eccentricity.unwrap() < 0.05);

        // σx = 2σy ⇒ sqrt(1 − λ₂/λ₁) = sqrt(1 − ¼) ≈ 0.866.
        let elongated = star_field_plane(60, 60, 1000.0, &[(30.0, 30.0, 4.0, 2.0, 5000.0)]);
        let ecc = detect(&elongated).eccentricity.unwrap();
        assert!((ecc - 0.866).abs() < 0.05, "eccentricity {ecc}");
    }

    #[test]
    fn rejects_hot_pixels_below_the_area_floor() {
        // A single bright pixel is a cosmic ray or a hot pixel, not a star.
        let mut plane = star_field_plane(60, 60, 1000.0, &[]);
        plane.values[30 * 60 + 30] = 60000.0;
        assert_eq!(detect(&plane).count, 0);
    }

    #[test]
    fn rejects_stars_touching_the_border() {
        // A truncated PSF makes garbage moments, so a star on the edge is
        // dropped rather than measured.
        let plane = star_field_plane(60, 60, 1000.0, &[(0.0, 30.0, 2.0, 2.0, 5000.0)]);
        assert_eq!(detect(&plane).count, 0);

        // The same star, moved clear of the edge, is kept — so it is the border
        // that rejected it and not its shape.
        let inside = star_field_plane(60, 60, 1000.0, &[(30.0, 30.0, 2.0, 2.0, 5000.0)]);
        assert_eq!(detect(&inside).count, 1);
    }

    #[test]
    fn rejects_flat_topped_saturated_stars() {
        // Clipped at the plane's saturation: its HFR would read low, which is
        // exactly the frame you'd wrongly call well-focused.
        let plane = star_field_plane(60, 60, 1000.0, &[(30.0, 30.0, 2.0, 2.0, 200_000.0)]);
        assert!(plane.values.iter().any(|&v| v >= plane.saturation));
        assert_eq!(detect(&plane).count, 0);

        // The ceiling must come from the source sample type, never from the
        // background's PixelStats::saturation — on an f64 plane that is just the
        // observed maximum, and reading it there would reject the brightest star
        // in *every* frame, saturated or not.
        let unsaturated = star_field_plane(60, 60, 1000.0, &[(30.0, 30.0, 2.0, 2.0, 5000.0)]);
        let bg = plane_background(&unsaturated);
        assert!(unsaturated.values.iter().any(|&v| v >= bg.saturation));
        assert_eq!(detect(&unsaturated).count, 1);
    }

    #[test]
    fn empty_frame_detects_nothing() {
        let plane = star_field_plane(60, 60, 1000.0, &[]);
        let stats = detect(&plane);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.hfr, None);
        assert_eq!(stats.fwhm, None);
        assert_eq!(stats.eccentricity, None);
    }

    #[test]
    fn real_mosaic_detects_plausible_stars() {
        let path = test_data("uncompressed.fit");
        let fits = FitsFile::from_file(&path).unwrap();
        let (header, img) = find_image_hdu(&fits, &path).unwrap();
        let plane = detection_plane(header, img.as_ref()).unwrap();
        let stats = detect(&plane);

        // Pinned as a regression value: it is also the evidence that the
        // full-resolution area floor is not eating stars on a half-resolution
        // green plane.
        assert_eq!(stats.count, REAL_MOSAIC_STAR_COUNT);
        let hfr = stats.hfr.unwrap();
        assert!((0.5..10.0).contains(&hfr), "implausible HFR {hfr}");
        // A tracked sub is not made of streaks.
        assert!(stats.eccentricity.unwrap() < 0.8);
    }

    /// Stars detected on `uncompressed.fit`'s green super-pixel plane with the
    /// default options. Shared with `info`'s test of the same frame reached
    /// through `header_info_with`.
    ///
    /// The evidence that the full-resolution area floor is safe on a half-
    /// resolution plane: of the 6513 blobs over the threshold, 6116 are smaller
    /// than 5 px and half are a *single* pixel — the floor is rejecting a noise
    /// population, not stars. Dropping it to 1 admits all of them and drives the
    /// median HFR to 0, which is what a one-pixel "star" measures.
    pub(crate) const REAL_MOSAIC_STAR_COUNT: usize = 395;
}
