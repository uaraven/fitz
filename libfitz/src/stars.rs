//! Star detection and per-star shape measurement on a grayscale [`Image`]:
//! threshold against the image's own background, flood-fill the blobs above
//! it, reject anything that isn't a usable star, and measure what survives —
//! HFR, FWHM and eccentricity over a circular aperture wide enough to keep
//! the star's sub-threshold wings, aggregated across the frame.

use rayon::prelude::*;

use crate::data::{Image, ImageType, PixelBuffer};
use crate::stats::{Stats, full_scale};
use anyhow::Result;
use fitskit::Header;

/// Multiplier converting a Gaussian's standard deviation into its full width
/// at half maximum.
const FWHM_PER_SIGMA: f64 = 2.3548;

/// Multiplier turning a median absolute deviation into an estimate of the
/// standard deviation of normally distributed data.
const MAD_TO_SIGMA: f64 = 1.4826;

/// A frame's robust background level: its median and noise, unaffected by
/// the stars sitting on top of it.
#[derive(Clone, Copy, Debug)]
pub struct Background {
    pub median: f64,
    /// Median absolute deviation from the median, scaled to estimate noise σ.
    pub mad: f64,
}

impl Background {
    /// Builds a `Background` from an image's statistics.
    pub fn from_stats(stats: &Stats) -> Self {
        Background {
            median: stats.median as f64,
            mad: stats.mad as f64 * MAD_TO_SIGMA,
        }
    }
}

impl Image {
    /// Builds the plane star detection should run on: the raw CFA mosaic
    /// pixels treated directly as a mono frame (no debayering or channel
    /// extraction — a Bayer pixel only samples one colour, so the flux
    /// profile carries a checkerboard modulation, but that washes out of the
    /// flux-weighted centroid/moments and keeps HFR/FWHM at full sensor
    /// resolution, comparable to other tools that measure raw OSC frames the
    /// same way), a weighted luminance of an already-debayered RGB image, or
    /// the frame itself if it's grayscale.
    pub fn detection_plane(&self) -> Result<Image> {
        match self.image_type {
            ImageType::RGB => Ok(self.luminance()),
            ImageType::Grayscale | ImageType::CFA(_) => Ok(Image::new(
                ImageType::Grayscale,
                Header::new(),
                self.width,
                self.height,
                self.pixels.clone(),
            )),
        }
    }

    /// Converts the image to grayscale luminance image
    /// CFA and Grayscale images are returned as-is
    /// For RGB images the luminance is calculated from R,G,B values and returned as a single-channel image
    pub(crate) fn luminance(&self) -> Image {
        match self.image_type {
            ImageType::CFA(_) | ImageType::Grayscale => Image::new(
                self.image_type,
                self.header.clone(),
                self.width,
                self.height,
                self.pixels.clone(),
            ),
            ImageType::RGB => {
                let pixels = match &self.pixels {
                    PixelBuffer::U16(ipixels) => {
                        let pxl = ipixels
                            .par_chunks(3)
                            .map(|rgb| {
                                ((3 * rgb[0] as u32 + 10 * rgb[1] as u32 + rgb[2] as u32) / 14)
                                    .clamp(0, 65535) as u16
                            })
                            .collect();
                        PixelBuffer::U16(pxl)
                    }
                    PixelBuffer::F32(fpixels) => {
                        let pxl = fpixels
                            .par_chunks(3)
                            .map(|rgb| 0.299 * rgb[0] + 0.587 * rgb[1] + 0.114 * rgb[2])
                            .collect();
                        PixelBuffer::F32(pxl)

                    }
                };
                Image::new(
                    ImageType::Grayscale,
                    self.header.clone(),
                    self.width,
                    self.height,
                    pixels,
                )
            }
        }
    }

    /// Detects the stars on this image, returning each one's centroid and
    /// measured shape. The basis for both [`Image::detect_stars`]'s
    /// per-frame aggregate and a GUI overlay that needs each star's position.
    pub fn detect_star_list(&self, opts: &StarDetectOptions) -> Vec<Star> {
        let stats = &self.stats().channels[0];
        let bg = Background::from_stats(stats);
        let saturation = full_scale(stats.estimated_bit_depth) as f64;
        let values = samples(&self.pixels);

        let threshold = bg.median + opts.sigma_k * bg.mad;
        let mut mask: Vec<bool> = values.par_iter().map(|&v| v > threshold).collect();

        let blobs = blobs_above_threshold(&mut mask, self.width, self.height);
        blobs
            .par_iter()
            .filter(|blob| accept(blob, &values, self.width, self.height, saturation, opts))
            .filter_map(|blob| measure(blob, &values, self.width, self.height, bg.median))
            .collect()
    }

    /// Detects the stars on this image and aggregates their shapes into
    /// per-frame HFR, FWHM and eccentricity.
    pub fn detect_stars(&self, opts: &StarDetectOptions) -> StarStats {
        aggregate(&self.detect_star_list(opts))
    }

    /// Detects stars on this image's detection plane and returns their
    /// per-frame HFR, FWHM and eccentricity.
    pub fn star_stats(&self, opts: &StarDetectOptions) -> Result<StarStats> {
        Ok(self.detection_plane()?.detect_stars(opts))
    }

    /// Detects stars on this image's detection plane and returns each one's
    /// centroid and measured shape, for an overlay that marks every detected
    /// star rather than just reporting the frame's aggregate shape.
    pub fn star_list(&self, opts: &StarDetectOptions) -> Result<Vec<Star>> {
        Ok(self.detection_plane()?.detect_star_list(opts))
    }
}

/// Converts a pixel buffer's samples to `f64` values on the 0..=65535 scale.
fn samples(pixels: &PixelBuffer) -> Vec<f64> {
    match pixels {
        PixelBuffer::U16(v) => v.par_iter().map(|&x| x as f64).collect(),
        PixelBuffer::F32(v) => v
            .par_iter()
            .map(|&x| (x as f64 * 65535.0).clamp(0.0, 65535.0).trunc())
            .collect(),
    }
}

/// Tuning parameters for star detection.
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
        // Sized for full-resolution frames; every detection plane (mono,
        // raw CFA, or an RGB cube's green channel) is now full resolution.
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
    /// Half-flux radius: the flux-weighted mean radius from the star's centroid.
    pub hfr: f64,
    pub fwhm: f64,
    /// Ellipticity along the star's x/y axes.
    pub e1: f64,
    /// Ellipticity along the star's diagonal axes.
    pub e2: f64,
}

impl Star {
    /// This star's own eccentricity — noisy and biased high for a faint star.
    pub fn eccentricity(&self) -> f64 {
        eccentricity_from_ellipticity(self.e1.hypot(self.e2))
    }
}

/// A frame's star metrics: how many stars were accepted, and the median of each
/// shape measurement across them.
#[derive(Clone)]
pub struct StarStats {
    pub count: usize,
    /// Median across accepted stars; `None` when none were accepted.
    pub hfr: Option<f64>,
    pub fwhm: Option<f64>,
    pub eccentricity: Option<f64>,
}

/// Finds every 8-connected blob of set cells in `mask`, as pixel indices.
/// Iterative rather than recursive, so a blob spanning millions of pixels
/// can't overflow the stack.
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

/// Whether a blob is a star worth measuring: the right size, clear of the
/// frame border, and not saturated.
fn accept(
    blob: &[usize],
    values: &[f64],
    width: usize,
    height: usize,
    saturation: f64,
    opts: &StarDetectOptions,
) -> bool {
    if blob.len() < opts.min_pixels || blob.len() > opts.max_pixels {
        return false;
    }
    blob.iter().all(|&i| {
        let (x, y) = (i % width, i / width);
        x > 0 && y > 0 && x + 1 < width && y + 1 < height && values[i] < saturation
    })
}

/// How many HFR estimates the aperture is widened to. 3 × HFR is ≈ 3.8σ for
/// a Gaussian and contains >99.9% of its flux, so the aperture measurement
/// converges to the untruncated shape.
const APERTURE_PER_HFR: f64 = 3.0;

/// Measure one blob's centroid and shape from its background-subtracted flux.
/// `None` for a blob with no positive flux, which has no centroid to speak of.
///
/// The thresholded blob only supplies the centroid and a seed size: measuring
/// on it alone truncates the star at the detection threshold and reads HFR and
/// FWHM up to ~2× low for a star barely above it. The shape is instead
/// measured over a circular aperture around the centroid, widened iteratively
/// to [`APERTURE_PER_HFR`] × the HFR it measures, with negative flux clamped
/// to zero so background noise can't cancel the wings — the same aperture
/// approach (and the same flux-weighted mean-radius HFR) as NINA.
fn measure(
    blob: &[usize],
    values: &[f64],
    width: usize,
    height: usize,
    background: f64,
) -> Option<Star> {
    let position = |i: usize| ((i % width) as f64, (i / width) as f64);

    // Flux-weighted centroid of the thresholded pixels: they are the star's
    // bright core, so the centroid is stable against background noise.
    let (mut sum_f, mut sum_fx, mut sum_fy) = (0.0, 0.0, 0.0);
    for &i in blob {
        let (x, y) = position(i);
        let f = values[i] - background;
        sum_f += f;
        sum_fx += f * x;
        sum_fy += f * y;
    }
    if sum_f <= 0.0 {
        return None;
    }
    let (cx, cy) = (sum_fx / sum_f, sum_fy / sum_f);

    // Seed the aperture from the blob itself: its own flux-weighted mean
    // radius, and its extent as a floor so a tiny blob still gets a sane
    // aperture.
    let (mut sum_fr, mut extent) = (0.0, 0.0f64);
    for &i in blob {
        let (x, y) = position(i);
        let r = (x - cx).hypot(y - cy);
        sum_fr += (values[i] - background) * r;
        extent = extent.max(r);
    }
    let floor = extent + 2.0;
    let mut radius = (APERTURE_PER_HFR * sum_fr / sum_f).max(floor);

    // Widen until the radius stabilizes: each pass measures over the current
    // circle, the next circle follows the HFR it found.
    let mut star = None;
    for _ in 0..3 {
        let measured = measure_aperture(values, width, height, cx, cy, radius, background)?;
        let next = (APERTURE_PER_HFR * measured.hfr).max(floor);
        let converged = (next - radius).abs() <= 0.05 * radius;
        star = Some(measured);
        radius = next;
        if converged {
            break;
        }
    }
    star
}

/// Measure HFR and second moments over every frame pixel within `radius` of
/// the centroid, with negative background-subtracted flux clamped to zero.
fn measure_aperture(
    values: &[f64],
    width: usize,
    height: usize,
    cx: f64,
    cy: f64,
    radius: f64,
    background: f64,
) -> Option<Star> {
    let x_lo = ((cx - radius).floor().max(0.0)) as usize;
    let y_lo = ((cy - radius).floor().max(0.0)) as usize;
    let x_hi = ((cx + radius).ceil() as usize + 1).min(width);
    let y_hi = ((cy + radius).ceil() as usize + 1).min(height);

    let (mut sum_f, mut sum_fr, mut mxx, mut myy, mut mxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for y in y_lo..y_hi {
        for x in x_lo..x_hi {
            let (dx, dy) = (x as f64 - cx, y as f64 - cy);
            let r = dx.hypot(dy);
            if r > radius {
                continue;
            }
            let f = (values[y * width + x] - background).max(0.0);
            sum_f += f;
            sum_fr += f * r;
            mxx += f * dx * dx;
            myy += f * dy * dy;
            mxy += f * dx * dy;
        }
    }
    if sum_f <= 0.0 {
        return None;
    }
    let (mxx, myy, mxy) = (mxx / sum_f, myy / sum_f, mxy / sum_f);
    let trace = mxx + myy;
    if trace <= 0.0 {
        return None;
    }

    Some(Star {
        x: cx,
        y: cy,
        flux: sum_f,
        hfr: sum_fr / sum_f,
        fwhm: FWHM_PER_SIGMA * (trace / 2.0).sqrt(),
        e1: (mxx - myy) / trace,
        e2: 2.0 * mxy / trace,
    })
}

/// Converts an ellipticity magnitude into an eccentricity that scales
/// linearly with the star's axis ratio: 0 for a round star, approaching 1
/// for a streak.
fn eccentricity_from_ellipticity(e: f64) -> f64 {
    (1.0 - ((1.0 - e) / (1.0 + e)).sqrt()).clamp(0.0, 1.0)
}

/// Reduces per-star measurements to per-frame values: medians for HFR and
/// FWHM, and a noise-resistant vector average for eccentricity so a handful
/// of faint, noisy stars can't fabricate elongation that isn't there. Public
/// so a caller that already has a [`Star`] list (e.g. from
/// [`Image::detect_star_list`], to draw an overlay) can derive the same
/// [`StarStats`] without detecting twice.
pub fn aggregate(stars: &[Star]) -> StarStats {
    let median_of = |f: fn(&Star) -> f64| {
        (!stars.is_empty()).then(|| median_in_place(&mut stars.iter().map(f).collect::<Vec<_>>()))
    };
    StarStats {
        count: stars.len(),
        hfr: median_of(|s| s.hfr),
        fwhm: median_of(|s| s.fwhm),
        eccentricity: median_of(|s| s.e1)
            .zip(median_of(|s| s.e2))
            .map(|(e1, e2)| eccentricity_from_ellipticity(e1.hypot(e2))),
    }
}

/// The median of `values`, computed in place. Panics on an empty slice.
pub fn median_in_place(values: &mut [f64]) -> f64 {
    let mid = values.len() / 2;
    values.select_nth_unstable_by(mid, |a, b| a.partial_cmp(b).unwrap());
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        let hi = values[mid];
        let lo = values[..mid]
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        (lo + hi) / 2.0
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fits_file::load_fits;
    use crate::test_support::{
        test_data, write_mosaic_fits, write_noisy_star_field_fits, write_rgb_cube_fits,
        write_star_field_fits,
    };
    use tempfile::TempDir;

    /// The detection plane of a FITS frame loaded from disk.
    fn plane_of(path: &std::path::Path) -> Image {
        load_fits(path).unwrap().detection_plane().unwrap()
    }

    /// The detection plane of a synthetic star field written to a temporary
    /// FITS file.
    fn star_field_plane(
        width: usize,
        height: usize,
        background: f64,
        stars: &[(f64, f64, f64, f64, f64)],
    ) -> Image {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("field.fits");
        write_star_field_fits(&path, width, height, background, stars);
        plane_of(&path)
    }

    /// Detect on a synthetic field with the shipping defaults.
    fn detect(plane: &Image) -> StarStats {
        plane.detect_stars(&StarDetectOptions::default())
    }

    /// Every star's centroid, in detection order.
    fn stars_of(plane: &Image) -> Vec<Star> {
        let bg = Background::from_stats(&plane.stats().channels[0]);
        let opts = StarDetectOptions::default();
        let values = samples(&plane.pixels);
        let threshold = bg.median + opts.sigma_k * bg.mad;
        let mut mask: Vec<bool> = values.iter().map(|&v| v > threshold).collect();
        let saturation = full_scale(plane.stats().channels[0].estimated_bit_depth) as f64;
        blobs_above_threshold(&mut mask, plane.width, plane.height)
            .iter()
            .filter(|b| accept(b, &values, plane.width, plane.height, saturation, &opts))
            .filter_map(|b| measure(b, &values, plane.width, plane.height, bg.median))
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
        // sqrt(π/2)σ ≈ 1.2533σ. The aperture keeps the sub-threshold wings, so
        // the tolerance is tight — the old blob-only measurement truncated the
        // star at the detection threshold and read ~15% low even on a star
        // this bright.
        let (fwhm, hfr) = (stats.fwhm.unwrap(), stats.hfr.unwrap());
        let (true_fwhm, true_hfr) = (FWHM_PER_SIGMA * SIGMA, 1.2533 * SIGMA);
        assert!((fwhm - true_fwhm).abs() < 0.05 * true_fwhm, "fwhm {fwhm}");
        assert!((hfr - true_hfr).abs() < 0.05 * true_hfr, "hfr {hfr}");
    }

    /// A faint star must measure the same HFR as a bright one of the same σ.
    ///
    /// This is the regression test for HFR/FWHM reading 2–2.5× smaller than
    /// NINA: measuring only the pixels above the detection threshold truncates
    /// a star barely above it at ~1σ, so a frame whose median star is faint
    /// reported roughly half the true value. The aperture measurement must be
    /// brightness-invariant.
    #[test]
    fn faint_and_bright_stars_measure_the_same_hfr() {
        const SIGMA: f64 = 2.0;
        // Background 1000 with no noise floor still yields a small MAD from
        // the stars' own wings; the faint star's peak is a few hundred ADU —
        // far below the bright star's 5000, near the detection limit.
        let plane = star_field_plane(
            120,
            60,
            1000.0,
            &[
                (30.0, 30.0, SIGMA, SIGMA, 5000.0),
                (90.0, 30.0, SIGMA, SIGMA, 300.0),
            ],
        );
        let stars = stars_of(&plane);
        assert_eq!(stars.len(), 2, "both stars must be detected");

        let true_hfr = 1.2533 * SIGMA;
        for s in &stars {
            assert!(
                (s.hfr - true_hfr).abs() < 0.15 * true_hfr,
                "hfr {} at ({}, {}) is not within 15% of {true_hfr}",
                s.hfr,
                s.x,
                s.y
            );
        }
        let ratio = stars[0].hfr / stars[1].hfr;
        assert!(
            (0.87..1.15).contains(&ratio),
            "bright/faint HFR ratio {ratio} should be ~1"
        );
    }

    #[test]
    fn eccentricity_measures_elongation() {
        let round = star_field_plane(60, 60, 1000.0, &[(30.0, 30.0, 2.0, 2.0, 5000.0)]);
        assert!(detect(&round).eccentricity.unwrap() < 0.05);

        // σx = 2σy ⇒ 1 − λ₂/λ₁ = 1 − sqrt(¼) = 1 − ½ = 0.5.
        let elongated = star_field_plane(60, 60, 1000.0, &[(30.0, 30.0, 4.0, 2.0, 5000.0)]);
        let ecc = detect(&elongated).eccentricity.unwrap();
        assert!((ecc - 0.5).abs() < 0.05, "eccentricity {ecc}");

        // A 10% axis-ratio difference: 1 − σx/σy = 1 − 2.0/2.2 ≈ 0.091. Pinned
        // against the geometric eccentricity (0.417 for this same star) as the
        // evidence this formula is linear in the axis ratio rather than steep
        // near 0.
        let mild = star_field_plane(60, 60, 1000.0, &[(30.0, 30.0, 2.0, 2.2, 5000.0)]);
        let ecc = detect(&mild).eccentricity.unwrap();
        assert!((ecc - 0.091).abs() < 0.03, "eccentricity {ecc}");
    }

    /// Round stars must stay round as they get fainter.
    ///
    /// Per-star eccentricity is rectified — noise can only push it up — so
    /// taking the frame's number as the median of the per-star values reports a
    /// fake elongation that grows without bound as SNR drops. Aggregating the
    /// signed `e1`/`e2` first is what keeps it honest, and this pins the
    /// difference: the per-star median is asserted to be *badly* wrong on the
    /// same data, so the test fails if the aggregation ever reverts.
    #[test]
    fn noise_does_not_fabricate_elongation() {
        // A grid of round stars at the sampling and read noise of the bundled
        // mosaics: σ = 0.78 px on the detection plane, 13 ADU noise.
        let truth: Vec<(f64, f64, f64, f64, f64)> = (0..8)
            .flat_map(|r| {
                (0..8).map(move |c| {
                    // The half-pixel offsets keep the stars off a common
                    // sub-pixel phase, which would correlate their moments.
                    let (x, y) = (20.0 + 30.0 * c as f64, 20.0 + 30.0 * r as f64);
                    (x + 0.37 * r as f64, y + 0.51 * c as f64, 0.78, 0.78, 400.0)
                })
            })
            .collect();

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("noisy.fits");
        write_noisy_star_field_fits(&path, 260, 260, 1000.0, 13.0, &truth);
        let plane = plane_of(&path);

        let stats = detect(&plane);
        assert!(stats.count > 30, "only {} stars detected", stats.count);

        let ecc = stats.eccentricity.unwrap();
        assert!(ecc < 0.25, "round stars reported as eccentricity {ecc}");

        // The same stars, aggregated the biased way: far off zero, which is
        // what a real frame full of faint stars used to report.
        let stars = stars_of(&plane);
        let per_star =
            median_in_place(&mut stars.iter().map(Star::eccentricity).collect::<Vec<_>>());
        assert!(
            per_star > 2.0 * ecc,
            "median per-star eccentricity {per_star} should be far above the \
             vector-aggregated {ecc}; if it isn't, this test proves nothing"
        );
    }

    /// Overwrite one pixel with an ADU value on the 0..=65535 scale, whatever
    /// the plane's underlying `PixelBuffer` variant.
    fn set_pixel(plane: &mut Image, index: usize, adu: f64) {
        match &mut plane.pixels {
            PixelBuffer::U16(v) => v[index] = adu as u16,
            PixelBuffer::F32(v) => v[index] = (adu / 65535.0) as f32,
        }
    }

    #[test]
    fn rejects_hot_pixels_below_the_area_floor() {
        // A single bright pixel is a cosmic ray or a hot pixel, not a star.
        let mut plane = star_field_plane(60, 60, 1000.0, &[]);
        set_pixel(&mut plane, 30 * 60 + 30, 60000.0);
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

    /// The saturation ceiling [`Image::detect_stars`] derives internally: the
    /// full scale of the plane's own [`Stats::estimated_bit_depth`].
    fn saturation_of(plane: &Image) -> f64 {
        full_scale(plane.stats().channels[0].estimated_bit_depth) as f64
    }

    #[test]
    fn rejects_flat_topped_saturated_stars() {
        // Clipped at the plane's saturation: its HFR would read low, which is
        // exactly the frame you'd wrongly call well-focused.
        let plane = star_field_plane(60, 60, 1000.0, &[(30.0, 30.0, 2.0, 2.0, 200_000.0)]);
        let saturation = saturation_of(&plane);
        assert_eq!(saturation, 65535.0);
        assert!(samples(&plane.pixels).iter().any(|&v| v >= saturation));
        assert_eq!(detect(&plane).count, 0);

        // The ceiling must never be the plane's observed maximum: reading it
        // there would reject the brightest star in *every* frame, saturated or
        // not. An unclipped field's maximum sits strictly below full scale, so
        // its brightest star survives.
        let unsaturated = star_field_plane(60, 60, 1000.0, &[(30.0, 30.0, 2.0, 2.0, 5000.0)]);
        let unsaturated_ceiling = saturation_of(&unsaturated);
        let observed_max = samples(&unsaturated.pixels)
            .iter()
            .copied()
            .fold(0.0, f64::max);
        assert!(observed_max < unsaturated_ceiling);
        assert_eq!(detect(&unsaturated).count, 1);
    }

    #[test]
    fn detection_plane_shape_follows_the_image_type() {
        let tmp = TempDir::new().unwrap();

        // A CFA mosaic detects on its own raw pixels, at full resolution —
        // no debayering or channel extraction.
        let raw = load_fits(&test_data("uncompressed.fit")).unwrap();
        let mosaic = plane_of(&test_data("uncompressed.fit"));
        assert_eq!((mosaic.width, mosaic.height), (raw.width, raw.height));
        assert_eq!(samples(&mosaic.pixels), samples(&raw.pixels));

        // A mono frame detects on itself, at full resolution.
        let mono_path = tmp.path().join("mono.fits");
        write_mosaic_fits(&mono_path, 8, 6, None);
        let mono = plane_of(&mono_path);
        assert_eq!((mono.width, mono.height), (8, 6));

        // An RGB cube detects on a weighted luminance of its channels, also
        // at full resolution.
        let cube_path = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&cube_path, 8, 6);
        let cube = plane_of(&cube_path);
        assert_eq!((cube.width, cube.height), (8, 6));
        // `write_rgb_cube_fits` fills plane `c` with `c*n + i`, so the
        // per-pixel R/G/B triple is `(i, n+i, 2n+i)` — the check that
        // `luminance()`'s (3R + 10G + B) / 14 weighting was applied, not a
        // raw channel extraction.
        let n = 8 * 6;
        let expected: Vec<f64> = (0..n)
            .map(|i| {
                let (r, g, b) = (i as u32, (n + i) as u32, (2 * n + i) as u32);
                ((3 * r + 10 * g + b) / 14) as f64
            })
            .collect();
        assert_eq!(samples(&cube.pixels), expected);
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
        let plane = plane_of(&test_data("uncompressed.fit"));
        let stats = detect(&plane);

        // Pinned as a regression value.
        assert_eq!(stats.count, REAL_MOSAIC_STAR_COUNT);
        let hfr = stats.hfr.unwrap();
        assert!((0.5..10.0).contains(&hfr), "implausible HFR {hfr}");
        // A tracked sub is not made of streaks. Pinned rather than bounded.
        let ecc = stats.eccentricity.unwrap();
        assert!((ecc - 0.080).abs() < 0.02, "eccentricity {ecc}");
    }

    /// `star_stats` is just the `detection_plane` + `detect_stars` pipeline
    /// in one call — check it agrees with calling the two steps directly, on
    /// both a raw CFA mosaic and a synthetic mono field.
    #[test]
    fn star_stats_matches_the_detection_plane_pipeline() {
        let assert_matches_plane = |image: &Image| {
            let via_plane = image
                .detection_plane()
                .unwrap()
                .detect_stars(&StarDetectOptions::default());
            let via_star_stats = image.star_stats(&StarDetectOptions::default()).unwrap();
            assert_eq!(via_star_stats.count, via_plane.count);
            assert_eq!(via_star_stats.hfr, via_plane.hfr);
            assert_eq!(via_star_stats.fwhm, via_plane.fwhm);
            assert_eq!(via_star_stats.eccentricity, via_plane.eccentricity);
        };

        assert_matches_plane(&load_fits(&test_data("uncompressed.fit")).unwrap());

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
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("mono_field.fits");
        write_star_field_fits(&path, 100, 100, 1000.0, &truth);
        assert_matches_plane(&load_fits(&path).unwrap());
    }

    /// Stars detected directly on `uncompressed.fit`'s raw CFA pixels (no
    /// green-plane extraction) with the default options. Shared with `info`'s
    /// test of the same frame reached through `header_info_with`.
    pub(crate) const REAL_MOSAIC_STAR_COUNT: usize = 352;
}
