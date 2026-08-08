//! Star detection and per-star shape measurement on a grayscale [`Image`]:
//! threshold against the image's own robust background (its [`Stats`]),
//! flood-fill the connected blobs above it, reject everything that isn't a
//! usable star, and measure what survives — HFR, FWHM and eccentricity,
//! aggregated across the frame. Pure and `Send` — no file I/O, no terminal
//! output — so a GUI star-metrics panel and the CLI's `info --stars` both
//! drive it.
//!
//! HFR and FWHM aggregate as plain medians; eccentricity does not, and
//! [`aggregate`] explains at length why not.

use rayon::prelude::*;

use crate::data::{Image, ImageType, PixelBuffer};
use crate::debayer::green_sites;
use crate::stats::{Stats, full_scale};
use anyhow::{Result, bail};
use bayer::CFA;
use fitskit::Header;

/// Multiplier turning a Gaussian's standard deviation into its full width at
/// half maximum: `2 * sqrt(2 * ln 2)`.
const FWHM_PER_SIGMA: f64 = 2.3548;

/// Multiplier turning a median absolute deviation into an estimate of the
/// standard deviation of normally distributed data.
const MAD_TO_SIGMA: f64 = 1.4826;

/// The robust background a detection threshold is built from: an [`Image`]'s
/// median, and its median absolute deviation scaled to a σ estimate.
///
/// Both are robust to the very stars being detected, which is why thresholding
/// on them isn't chicken-and-egg.
#[derive(Clone, Copy, Debug)]
pub struct Background {
    pub median: f64,
    /// Median absolute deviation from the median, scaled by [`MAD_TO_SIGMA`] so
    /// it estimates σ for Gaussian noise while ignoring stars entirely.
    pub mad: f64,
}

impl Background {
    /// The background implied by an image's own [`Stats`] — the single source
    /// [`Image::detect_stars`] builds its threshold from, whatever plane it is
    /// run on.
    ///
    /// [`Stats::mad`] is the raw median absolute deviation; this is where it
    /// becomes the σ estimate the threshold expects.
    pub fn from_stats(stats: &Stats) -> Self {
        Background {
            median: stats.median as f64,
            mad: stats.mad as f64 * MAD_TO_SIGMA,
        }
    }
}

impl Image {
    /// Build the plane [`Image::detect_stars`] should run on:
    ///   - a CFA mosaic → the green super-pixel plane, each pixel the mean of
    ///     one 2x2 cell's two green sites, so `(w/2 x h/2)`;
    ///   - a monochrome frame → a copy of the frame itself;
    ///   - an already-debayered RGB image → its green channel, at full
    ///     resolution ([`Image::plane`]).
    ///
    /// **A CFA frame's measurements come out in half-resolution pixels.** An
    /// HFR or FWHM measured here reads about half the number NINA reports for
    /// the same frame. That is inherent to the super-pixel plane and is not a
    /// bug: every frame in a session comes off the same sensor, so the trend —
    /// the only thing a time series shows — is unaffected. A caller that
    /// reports absolute numbers should say which plane they were measured on
    /// (compare the returned image's width/height against the source frame's).
    /// An RGB image's green plane is *full* resolution, so its numbers read
    /// about twice a raw mosaic's — the same caveat, opposite sign.
    ///
    /// Why a super-pixel plane at all: a star profile sampled through a Bayer
    /// filter is not a PSF, and the HFR measured from it is noise. And why
    /// green: a Bayer sensor has twice as many green sites as red or blue, so
    /// green carries the most signal and the least noise, and detecting on
    /// green keeps a debayered frame's numbers comparable to a raw mosaic's.
    pub fn detection_plane(&self) -> Result<Image> {
        match self.image_type {
            ImageType::RGB => Ok(self
                .plane(1)
                .expect("an RGB image always has three colour planes")),
            ImageType::Grayscale => Ok(Image::new(
                ImageType::Grayscale,
                Header::new(),
                self.width,
                self.height,
                self.pixels.clone(),
            )),
            ImageType::CFA(cfa) => {
                if self.pixels.len() != self.width * self.height {
                    bail!("mosaic pixel count does not match its declared size");
                }
                let (width, height, values) =
                    green_super_pixels(&samples(&self.pixels), self.width, self.height, cfa);
                let pixels =
                    PixelBuffer::F32(values.iter().map(|&v| (v / 65535.0) as f32).collect());
                Ok(Image::new(
                    ImageType::Grayscale,
                    Header::new(),
                    width,
                    height,
                    pixels,
                ))
            }
        }
    }

    /// Detect the stars on this image and aggregate their shapes to per-frame
    /// medians.
    ///
    /// The threshold is built from this image's own [`Stats`] via
    /// [`Background::from_stats`] — never from a different frame's — and the
    /// saturation ceiling from its own estimated bit depth, so a caller that
    /// wants the mosaic/green-channel caveats above must call
    /// [`Image::detection_plane`] first and detect on *that* image, not the
    /// raw source frame.
    pub fn detect_stars(&self, opts: &StarDetectOptions) -> StarStats {
        let stats = &self.stats().channels[0];
        let bg = Background::from_stats(stats);
        let saturation = full_scale(stats.estimated_bit_depth) as f64;
        let values = samples(&self.pixels);

        let threshold = bg.median + opts.sigma_k * bg.mad;
        let mut mask: Vec<bool> = values.par_iter().map(|&v| v > threshold).collect();

        let blobs = blobs_above_threshold(&mut mask, self.width, self.height);
        let stars: Vec<Star> = blobs
            .par_iter()
            .filter(|blob| accept(blob, &values, self.width, self.height, saturation, opts))
            .filter_map(|blob| measure(blob, &values, self.width, bg.median))
            .collect();

        aggregate(&stars)
    }
}

/// A pixel buffer's samples on the 0..=65535 scale, as `f64`.
///
/// A `F32` buffer holds normalized `[0, 1]` values and is mapped by a *fixed*
/// full-scale factor, the same one [`crate::stats`] bins by — deliberately not
/// the frame's own max, which would rescale every frame differently and make a
/// session's ADU trend meaningless.
fn samples(pixels: &PixelBuffer) -> Vec<f64> {
    match pixels {
        PixelBuffer::U16(v) => v.par_iter().map(|&x| x as f64).collect(),
        PixelBuffer::F32(v) => v
            .par_iter()
            .map(|&x| (x as f64 * 65535.0).clamp(0.0, 65535.0).trunc())
            .collect(),
    }
}

/// Reduce a raw mosaic to its green super-pixel plane: one output pixel per
/// 2x2 CFA cell, the mean of that cell's two green sites. A 2x2 cell is the
/// quantum here, so an odd width or height drops its last column/row.
pub(crate) fn green_super_pixels(
    values: &[f64],
    width: usize,
    height: usize,
    cfa: CFA,
) -> (usize, usize, Vec<f64>) {
    let (pw, ph) = (width / 2, height / 2);
    let green_offsets = green_sites(cfa);
    let (ax, ay) = green_offsets[0];
    let (bx, by) = green_offsets[1];
    let plane = (0..ph)
        .into_par_iter()
        .flat_map_iter(|cy| {
            let site =
                move |gx: usize, gy: usize, cx: usize| values[(2 * cy + gy) * width + 2 * cx + gx];
            (0..pw).map(move |cx| (site(ax, ay, cx) + site(bx, by, cx)) / 2.0)
        })
        .collect();
    (pw, ph, plane)
}

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
    /// Ellipticity along the x/y axes: `(mxx - myy) / (mxx + myy)`.
    pub e1: f64,
    /// Ellipticity along the diagonals: `2 mxy / (mxx + myy)`.
    pub e2: f64,
}

impl Star {
    /// This star's own eccentricity. Meaningful for a bright, well-sampled
    /// star; on a faint one it is mostly noise, and strongly biased high — see
    /// [`aggregate`] for why the frame's number is not the median of these.
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
/// `saturation` is the full scale of the plane's estimated bit depth — never
/// the plane's observed maximum, which would reject the brightest star in
/// every frame. The rejection exists to drop stars that are genuinely
/// clipped: a flat top biases HFR low, which is exactly the frame you would
/// otherwise wrongly call well-focused.
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

/// Measure one blob's centroid and shape from its background-subtracted flux.
/// `None` for a blob with no positive flux, which has no centroid to speak of.
fn measure(blob: &[usize], values: &[f64], width: usize, background: f64) -> Option<Star> {
    let w = width;
    let flux_at = |i: usize| values[i] - background;
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

/// Eccentricity from the ellipticity magnitude `|e| = (λ₁ - λ₂) / (λ₁ + λ₂)`.
///
/// `sqrt(1 - λ₂/λ₁)` rewritten as `sqrt(2|e| / (1 + |e|))` — the same number,
/// reached from a quantity that survives averaging (see [`aggregate`]). 0 for a
/// round star, approaching 1 for a streak.
fn eccentricity_from_ellipticity(e: f64) -> f64 {
    (2.0 * e / (1.0 + e)).clamp(0.0, 1.0).sqrt()
}

/// Reduce per-star measurements to per-frame values — medians, not means, so
/// one satellite streak that survives the rejections cannot move the number.
///
/// **Eccentricity is aggregated as a vector, not as a median of the per-star
/// eccentricities**, and the distinction is not cosmetic. Eccentricity is a
/// rectified statistic: `sqrt(1 - λ₂/λ₁)` is ≥ 0 whichever way the measurement
/// noise pushed the moments, and the square root is vertical at 0, so noise
/// alone lifts it far off zero and never cancels. On round synthetic stars at
/// this frame's sampling the per-star median reads 0.70 at the detection limit
/// and still 0.08 at SNR 2000 — a fake elongation that tracks brightness rather
/// than shape, and it dominates any frame whose star list is mostly faint.
///
/// The ellipticity components `e1`/`e2` are *signed*, so their noise is
/// symmetric and does cancel: taking each one's median first and forming the
/// magnitude afterwards recovers 0.03–0.19 on those same round stars while
/// still reporting 0.866 for a 2:1 elongation and 0.417 for a 10% one.
///
/// The trade-off is that this measures the field's *common* elongation. Star
/// shapes that fan out symmetrically — pure field rotation about the frame
/// centre, or radial coma — partially cancel and read low. That is the right
/// bias for what this number is for: trailing and tracking drift, which push
/// every star the same way.
fn aggregate(stars: &[Star]) -> StarStats {
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

/// The median of `values`, sorting them in place. `values` is a handful of
/// per-star scalar measurements (not pixel intensities, so [`Image::stats`]'s
/// histogram-counting median doesn't apply here). Panics on an empty slice —
/// every caller in this module only reaches it behind `!stars.is_empty()`.
fn median_in_place(values: &mut [f64]) -> f64 {
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

    /// The detection plane of a FITS frame on disk, reached the way the
    /// frontends reach it.
    fn plane_of(path: &std::path::Path) -> Image {
        load_fits(path).unwrap().detection_plane().unwrap()
    }

    /// The detection plane of a synthetic star field, written out as a real
    /// unsigned-16 FITS frame first so the plane comes from the same path a
    /// captured sub would take.
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
            .filter_map(|b| measure(b, &values, plane.width, bg.median))
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

        // A 10% axis-ratio difference: eccentricity is savagely non-linear near
        // 0, so this reads 0.417, not "about 0.1". Pinned because that surprise
        // is the reason a small measurement bias looks like a broken metric.
        let mild = star_field_plane(60, 60, 1000.0, &[(30.0, 30.0, 2.0, 2.2, 5000.0)]);
        let ecc = detect(&mild).eccentricity.unwrap();
        assert!((ecc - 0.417).abs() < 0.03, "eccentricity {ecc}");
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

        // A CFA mosaic halves in both directions: one super-pixel per 2x2 cell.
        let mosaic = plane_of(&test_data("uncompressed.fit"));
        assert_eq!((mosaic.width, mosaic.height), (1504, 1504));

        // A mono frame detects on itself, at full resolution.
        let mono_path = tmp.path().join("mono.fits");
        write_mosaic_fits(&mono_path, 8, 6, None);
        let mono = plane_of(&mono_path);
        assert_eq!((mono.width, mono.height), (8, 6));

        // An RGB cube detects on its green channel, also at full resolution.
        let cube_path = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&cube_path, 8, 6);
        let cube = plane_of(&cube_path);
        assert_eq!((cube.width, cube.height), (8, 6));
        // `write_rgb_cube_fits` fills plane `c` with `c*n + i`, so the green
        // plane is the middle run — the check that it took the right channel.
        let n = 8 * 6;
        let expected: Vec<f64> = (0..n).map(|i| (n + i) as f64).collect();
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

        // Pinned as a regression value: it is also the evidence that the
        // full-resolution area floor is not eating stars on a half-resolution
        // green plane.
        assert_eq!(stats.count, REAL_MOSAIC_STAR_COUNT);
        let hfr = stats.hfr.unwrap();
        assert!((0.5..10.0).contains(&hfr), "implausible HFR {hfr}");
        // A tracked sub is not made of streaks. Pinned rather than bounded:
        // this frame's elongation is *coherent* — every star's major axis
        // points the same way — so the vector aggregation barely moves it,
        // which is the evidence it suppresses only noise and not real shape.
        let ecc = stats.eccentricity.unwrap();
        assert!((ecc - 0.69).abs() < 0.02, "eccentricity {ecc}");
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
