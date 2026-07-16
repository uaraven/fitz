# Analytics — Tier 1 (robust pixel statistics) and Tier 2 (star metrics)

## Context

`fitz-core/src/analytics.rs` computes six per-frame metrics (min/max/median/mean ADU and the
min/max pixel counts), keys each frame by `DATE-OBS`, and hands a time-ordered `Series` to the
FitSmith analytics dialog. Every metric comes out of one `pixel_stats` call per file, so
switching the dropdown re-plots from cache.

This plan adds two groups of metrics and extends the whole feature to RGB frames:

- **Tier 1 — robust statistics** (σ, MAD, mode/sky background, saturated-pixel count). These
  fall out of the value-count array `pixel_stats` already builds: no extra file read, no extra
  pass over the pixels.
- **Tier 2 — star metrics** (star count, HFR, FWHM, eccentricity). One shared star-detection
  pass per frame yields all four. This is what astrophotographers actually cull subs on:
  transparency, focus, seeing, and tracking.

## Decisions

- **Saturation is derived from the sample type, not from `DATAMAX`.** The saturation level is the
  physical value of the largest representable raw sample (`BSCALE`/`BZERO` applied): 65535 for the
  unsigned-16 convention, 255 for `U8`, and the observed max for float samples. No optional keyword.
- **A metric may be unavailable for a frame** (HFR of a frame with no detected stars). This is
  distinct from a *skip* — the frame analyzed fine, this one metric has no value. `Metric::value`
  returns `Option<f64>` and `build_series` drops the point, reporting the count in the dialog.
- **Star metrics are always computed**, not opt-in. They cost one extra pass over data already in
  memory, against a file read that dominates. Revisit only if the benchmark in step 8 says otherwise.
- **CFA mosaics get star-detected on a green super-pixel plane**, not on the raw mosaic — a star
  profile sampled through a Bayer filter is not a PSF, and its measured HFR is noise.
- **Tier 1 statistics on mono/mosaic frames keep measuring the full frame**, exactly as today.
  RGB cubes (previously skipped) measure their green plane. This preserves every existing value
  and the SHA-256/stat regression tests.

---

## Part 1 — Tier 1: robust statistics

All four new statistics derive from the existing full-resolution value-count array in
[`stats_from_counts`](fitz-core/src/info.rs#L502). That array has at most 65536 occupied slots
regardless of frame size, so each addition is an O(65536) walk — free next to the 24M-pixel
counting pass that produced it.

### 1. Extend `PixelStats` — [fitz-core/src/info.rs](fitz-core/src/info.rs#L23)

```rust
pub struct PixelStats {
    // … existing: min, max, mean, median, zeros, min_count, max_count, histogram
    /// Standard deviation of the physical pixel values.
    pub sigma: f64,
    /// Median absolute deviation from the median, scaled by 1.4826 so it
    /// estimates σ for Gaussian noise while ignoring stars entirely.
    pub mad: f64,
    /// The most common physical pixel value — the sky background level.
    pub mode: f64,
    /// Pixels at or above the sample type's saturation level.
    pub saturated: usize,
    /// The saturation level itself, so callers can report the fraction and
    /// star detection can discard flat-topped stars.
    pub saturation: f64,
}
```

### 2. Compute them in `stats_from_counts`

- **σ**: a second walk over the occupied slots accumulating `Σ c·(v − mean)²`, then
  `sqrt(that / n)`. Two-pass around the known mean rather than `Σv² − (Σv)²/n`, which cancels
  catastrophically when the sky level is 20000 ADU and the noise is 10.
- **MAD**: exact, with no sort. Deviations `|v − median|` grow monotonically as you move away
  from the median slot in either direction, so walk two cursors outward from `index_at_rank(counts,
  n/2)`, always advancing the cursor whose deviation is smaller, accumulating counts until the
  cumulative count passes `n/2`. That deviation is the MAD. O(occupied slots), exact, allocation-free.
- **mode**: `counts.iter().enumerate().max_by_key(|(_, c)| **c)` mapped through `physical`. Ties
  break to the lowest value (`max_by_key` keeps the last maximum — use `position_max`-style manual
  loop with `>` so the first/lowest wins, and say so in a comment; it matters for a frame with a
  bimodal amp-glow histogram).
- **saturated**: `saturation` is `physical(counts.len() - 1)`; `saturated` is `counts[last]`.
  (Any value above it is unrepresentable by construction, so "at or above" is just the last slot.)

Fold σ and the mode into the **existing** single walk that already computes the sum, the zero
count, and the display histogram, except σ, which needs the mean first — so: one walk for
sum/zeros/histogram/mode, one walk for σ, one two-cursor walk for MAD. Three O(65536) walks total.

### 3. Mirror them in `pixel_stats_general` — [fitz-core/src/info.rs](fitz-core/src/info.rs#L580)

The float/wide-sample fallback already materializes `Vec<f64>` and already sorts in place for the
median:

- **σ**: parallel fold for `Σ(v − mean)²` after the mean is known.
- **MAD**: map the values to `|v − median|` in place and `select_nth_unstable` again — the same
  trick the median already uses. One extra O(n) selection.
- **mode**: no exact mode exists for continuous float values. Use the center of the largest
  `HISTOGRAM_BUCKETS` bucket and **document the approximation on the field**. This path only
  triggers for float frames, where "the most common value" is not a well-posed question anyway.
- **saturation/saturated**: for float samples, saturation is the observed `max`, so `saturated`
  is `max_count`. Document that a float frame's saturated count is definitionally its max count.

### 4. Surface them in `info` — [fitz-cli/src/info.rs](fitz-cli/src/info.rs)

`fitz info --pixel` prints `PixelStats` ([info.rs:74](fitz-cli/src/info.rs#L74)); add σ, MAD,
mode, and saturated (as a count plus a percentage of the frame). **Update `readme.md`'s `info`
output sample.** No new flags, so no command-line surface change.

**Free win to fold in at step 4:** `fitz info --pixel test-data/uncompressed_debayer.fits`
currently prints *no* `Pixels:` line at all, because `header_info_with_pixels` declines RGB cubes
with the same rule analytics used. Once `pixel_stats_view` exists, `info` can report the green
plane's statistics for an RGB cube and label the line `Pixels (green):`. Same one-line fix, same
justification, and it removes the last place that silently drops RGB frames.

### 5. New `Metric` variants — [fitz-core/src/analytics.rs](fitz-core/src/analytics.rs#L19)

`Sigma` ("Noise σ"), `Mad` ("Noise MAD"), `Mode` ("Sky background"), `Saturated` ("Saturated
pixels"). Add to `Metric::all()` in that order, after the existing six.

---

## Part 2 — RGB frames via the green channel

Today `analyze_file` skips any 3-plane RGB cube with `SkipReason::NotMono`, mirroring
`header_info_with_pixels`. That is a real gap: a session of debayered subs charts nothing at all.

**Green is the right channel to measure.** It carries half the CFA sites (so the best SNR), it is
what the luminance of an OSC image is dominated by, and it is what focus and star metrics are
conventionally measured on.

### 6. An analysis view over the raw samples — [fitz-core/src/fits_image.rs](fitz-core/src/fits_image.rs)

The key constraint: **do not lose the 16-bit fast path**. FITS stores an RGB cube as three
*contiguous* planes, so the green plane is the raw sample sub-slice `[plane_len .. 2*plane_len]`.
Counting a sub-slice is the same code as counting the whole buffer.

```rust
/// Which samples of an image a statistic is computed over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlaneView {
    /// Every sample: a mono frame or a raw CFA mosaic (all Bayer sites).
    Full,
    /// One plane of a 3-plane cube, by index (green is 1).
    Plane(usize),
}

/// The sample range `view` selects, or `None` if the image has no such plane.
pub fn plane_range(img: &ImageData, view: PlaneView) -> Option<Range<usize>>;

/// The view analytics measures: the green plane of a debayered RGB cube, the
/// whole frame otherwise.
pub fn analysis_view(header: &Header, img: &ImageData) -> PlaneView;
```

Refactor `value_counts(img)` and `pixel_stats_general` to take the sample range, and add
`pub fn pixel_stats_view(header, img, view) -> PixelStats`. Keep `pixel_stats(header, img)` as
`pixel_stats_view(header, img, PlaneView::Full)` so `info` and every existing caller and test are
untouched — this is a pure addition.

### 7. Wire it into analytics — [fitz-core/src/analytics.rs](fitz-core/src/analytics.rs#L116)

- `analyze_file` calls `pixel_stats_view(header, img, analysis_view(header, img))`.
- **`SkipReason::NotMono` → `SkipReason::UnsupportedShape`**, and it now only fires for images
  that are neither 2D nor a 3-plane cube (a 4-plane cube, a 1D spectrum). Update the variant doc.
- `FileMetrics` gains `plane: PlaneView` so the CSV/tooltip can note that a value is green-only.
- **UI text**: `fitsmith/ui/analytics.slint` lines 24–25 and 111–114 —
  `skipped-not-mono` → `skipped-unsupported`, message "skipped (unsupported image shape)".
  Rename the matching `set_analytics_skipped_not_mono` / `Batch::not_mono` in
  [fitsmith/src/controller/analytics.rs](fitsmith/src/controller/analytics.rs#L118).

A mixed batch (some mono, some RGB) charts both, with the green plane standing in for the RGB
frames. That is a defensible comparison for a trend line and a bad one for absolute ADU; the
dialog says which plane was measured in the subtitle when the batch is mixed.

---

## Part 3 — Tier 2: star metrics

One detection pass per frame produces star count, HFR, FWHM, and eccentricity together. None of
the four justifies the detection code alone; all four together clearly do.

### 8. The detection plane — [fitz-core/src/fits_image.rs](fitz-core/src/fits_image.rs)

Star detection needs a real, physically-sampled mono image — unlike Tier 1, it cannot run on a
raw mosaic:

```rust
/// A mono f64 image to detect stars on.
pub struct MonoPlane { pub width: usize, pub height: usize, pub values: Vec<f64> }

/// Build the plane star detection runs on:
///   - CFA mosaic (BAYERPAT present) → green super-pixel plane: each 2x2 cell
///     contributes the mean of its two green sites, giving a (w/2 x h/2) image.
///   - RGB cube  → the green plane at full resolution.
///   - mono      → the frame itself.
pub fn detection_plane(header: &Header, img: &ImageData) -> Result<MonoPlane>;
```

The super-pixel plane halves the resolution, so a CFA frame's HFR is in half-res pixels — roughly
half the number NINA reports. **This is fine and must be documented**: every frame in a session
comes off the same sensor, so the *trend* — the only thing a time series shows — is unaffected.
Reuse `resolve_cfa` to locate the two green sites for the pattern; reuse `scaled_pixels`-style
`BSCALE`/`BZERO` mapping (extract the per-sample scaling closure so it is not re-implemented).

### 9. New module `fitz-core/src/stars.rs`

Pure, `Send`, no I/O — same contract as the rest of `fitz-core`. Add `pub mod stars;` to
[lib.rs](fitz-core/src/lib.rs).

```rust
pub struct StarDetectOptions {
    /// Detection threshold in MAD-sigmas above the background. Default 5.0.
    pub sigma_k: f64,
    /// Smallest blob accepted as a star, in pixels. Default 5 (rejects hot pixels).
    pub min_pixels: usize,
    /// Largest blob accepted, in pixels. Default 2000 (rejects nebulosity,
    /// satellite trails, and the halo of a bright star).
    pub max_pixels: usize,
}

pub struct Star {
    pub x: f64, pub y: f64,
    pub flux: f64,
    pub hfr: f64,
    pub fwhm: f64,
    pub eccentricity: f64,
}

pub struct StarStats {
    pub count: usize,
    /// Median across accepted stars; `None` when none were accepted.
    pub hfr: Option<f64>,
    pub fwhm: Option<f64>,
    pub eccentricity: Option<f64>,
}

pub fn detect_stars(plane: &MonoPlane, bg: &PixelStats, opts: &StarDetectOptions) -> StarStats;
```

**Algorithm.**

1. **Background and threshold — free.** `bg.median` is the sky level and `bg.mad` is the noise σ,
   both already computed in Part 1, both robust to the stars themselves. Threshold =
   `median + sigma_k * mad`. *(Caveat: for a CFA frame the `PixelStats` describe the full mosaic
   while detection runs on the green super-pixel plane, whose noise is lower by ~√2 from averaging
   two sites. Compute a second, cheap `PixelStats` over the detection plane rather than reusing
   the mosaic's — correctness beats saving one pass. Since the plane is `f64`, this is the general
   path; it is O(n) plus one selection.)*
2. **Mask** — parallel `values[i] > threshold` into a `Vec<bool>` (rayon, trivially parallel).
3. **Connected components**, 8-connected, **iterative flood fill with an explicit stack** — never
   recursion, a bright nebula would blow the stack. Sequential over the mask; a run-length +
   union-find pass is the fallback if profiling demands it. Collect each blob's pixel indices.
4. **Reject** blobs with `area < min_pixels`, `area > max_pixels`, any pixel touching the frame
   border (truncated PSF ⇒ garbage moments), or a peak at/above `bg.saturation` (flat-topped ⇒
   HFR biased low, which is exactly the frame you'd wrongly call well-focused).
5. **Measure**, per accepted blob, on background-subtracted flux `f_i = v_i − bg.median`:
   - centroid `x̄ = Σ f_i·x_i / Σ f_i`, likewise `ȳ`;
   - **HFR** = `Σ f_i·r_i / Σ f_i` where `r_i = hypot(x_i − x̄, y_i − ȳ)` — the flux-weighted mean
     radius, i.e. NINA's definition;
   - second moments `Mxx = Σ f_i·(x_i − x̄)² / Σ f_i`, `Myy`, `Mxy`;
   - **FWHM** = `2.3548 * sqrt((Mxx + Myy) / 2)` — the Gaussian-equivalent σ;
   - **eccentricity** = `sqrt(1 − λ₂/λ₁)` from the eigenvalues of `[[Mxx, Mxy], [Mxy, Myy]]`
     (`λ = (Mxx+Myy)/2 ± sqrt(((Mxx−Myy)/2)² + Mxy²)`); `0.0` when `λ₁ ≤ 0`.
6. **Aggregate** to medians across stars — medians, not means, because one satellite streak that
   survives step 4 should not move the number.

**Blobs are measured in parallel** (`par_iter` over the collected blobs) once labeling is done.

### 10. New `Metric` variants and `Option` values

```rust
StarCount ("Star count"), Hfr ("HFR"), Fwhm ("FWHM"), Eccentricity ("Eccentricity")
```

`FileMetrics` gains `pub stars: StarStats`. This forces the signature change:

```rust
// Metric::value(&PixelStats) -> f64   becomes:
pub fn value(self, m: &FileMetrics) -> Option<f64>
```

`Min`/`Max`/`Mean`/… return `Some`; `StarCount` returns `Some(count as f64)`; `Hfr`/`Fwhm`/
`Eccentricity` return the `Option` as-is. `build_series` uses `filter_map`, and `Series` gains
`pub unavailable: usize` — the count of frames that analyzed but had no value for *this* metric.
The dialog reports it next to the plotted count ("14 plotted, 2 with no stars detected"), which
doubles as a cloud indicator in its own right.

### 11. Cost

Threshold + labeling + moments over data already in memory: roughly one extra pass plus the
sequential label walk, against a file read (and, for `.fz`, a tile decompression) that dominates.
**Benchmark before deciding anything**: time `analyze_file` on `test-data/uncompressed.fit` and
`compressed.fits.fz` before and after. If star detection exceeds ~30% of per-file wall time, add
a "Detect stars" checkbox to the dialog that re-runs the batch — but do not add it speculatively.

---

## Part 4 — GUI

[fitsmith/src/controller/analytics.rs](fitsmith/src/controller/analytics.rs) and
[fitsmith/ui/analytics.slint](fitsmith/ui/analytics.slint):

- The dropdown grows from 6 to 14 entries — it is still built from `Metric::all()`, so it needs no
  code change, but check the ComboBox's height at 14 items on the smallest supported window.
- `analytics_unavailable` property + subtitle text (step 10); `skipped_not_mono` →
  `skipped_unsupported` (step 7).
- `metric_for_index` / `DEFAULT_METRIC` are unchanged; the `metric_index_maps_to_the_dropdown_order`
  test covers the growth for free.
- `export_file_name` slugs the new labels — `analytics-hfr.png`, `analytics-star-count.csv`.
  Extend `export_file_name_slugifies_the_metric`.
- `write_csv` unchanged in shape; the plane note (step 7) rides in a comment line? **No** — keep
  the CSV a clean `time_iso,epoch_seconds,value`. The plane belongs in the dialog, not in a data
  file people import into a spreadsheet.

---

## Part 5 — Tests

Per CLAUDE.md, new code gets unit tests on real data.

**Tier 1** (in `info.rs`):
- Synthetic frame with a known distribution: assert σ, MAD, mode, saturated exactly against
  hand-computed values.
- **Fast path vs general path agree**: build the same values as `I16` and as `F32`, assert every
  `PixelStats` field matches (σ/MAD exactly; mode only to bucket width — assert that separately,
  and say why in the test name).
- A frame with N pixels at 65535 → `saturated == N`, `saturation == 65535.0`.
- Real data: `uncompressed.fit` and `compressed.fits.fz` — pin σ/MAD/mode as regression values,
  and assert `mad <= sigma` (stars inflate σ, not MAD) and `min <= mode <= max`.

**Green plane** (in `fits_image.rs` / `analytics.rs`):
- `plane_range` on a 3-plane cube returns the middle third; `analysis_view` returns `Plane(1)` for
  an RGB cube, `Full` for a mosaic and for mono.
- An RGB cube with distinct constant planes (R=10, G=20, B=30) → `mean == 20.0`. This is the whole
  feature in one assertion.
- `test-data/uncompressed_debayer.fits` (a real debayered cube) now analyzes instead of skipping;
  the existing `analyze_file_skips_rgb_cube_as_not_mono` test **inverts** — rewrite it as
  `analyze_file_measures_the_green_plane_of_an_rgb_cube`, and add a genuine unsupported shape
  (`NAXIS3=4`) for `UnsupportedShape`.

**Tier 2** (in `stars.rs`), on synthesized star fields — add `write_star_field_fits(path, w, h,
&[(x, y, sigma_x, sigma_y, peak)])` to [test_support.rs](fitz-core/src/test_support.rs):
- 9 round Gaussians on a known background+noise → `count == 9`, each centroid within 0.1 px of
  truth, `fwhm ≈ 2.3548·σ` and `hfr ≈ 1.2533·σ` (the flux-weighted mean radius of a 2D Gaussian).
  Tolerance ~15%: thresholding truncates the wings, which biases both **low** — assert the
  direction of that bias too, since it is a property of the method, not slop.
- σx = 2σy → `eccentricity ≈ 0.866` (`sqrt(1 − ¼)`), within 0.05.
- Rejection: a single hot pixel → `count == 0`; a star touching the border → excluded; a
  flat-topped star at 65535 → excluded.
- Empty frame (pure noise) → `count == 0`, `hfr == None`, and `build_series` on it yields zero
  points with `unavailable == 1`.
- Real data: `uncompressed.fit` → `count > 0`, `hfr` in a plausible range; pin the count as a
  regression value so a refactor that changes detection is a visible test failure.

---

## Sequencing

Each step builds and tests green on its own; each is a commit.

| # | Step | Files |
|---|------|-------|
| 1 | `PixelStats` + σ/MAD/mode/saturated in both stat paths, with tests | `info.rs` |
| 2 | `info` report + readme sample | `fitz-cli/src/info.rs`, `readme.md` |
| 4b | `info --pixel` reports the green plane for RGB cubes | `info.rs`, `fitz-cli/src/info.rs` |
| 3 | Tier-1 `Metric` variants | `analytics.rs` |
| 4 | `PlaneView`/`plane_range`/`analysis_view`, `pixel_stats_view` refactor | `fits_image.rs`, `info.rs` |
| 5 | Analytics on RGB green plane; `NotMono` → `UnsupportedShape` | `analytics.rs`, controller, `analytics.slint` |
| 6 | `Metric::value` → `Option`, `Series::unavailable`, dialog text | `analytics.rs`, controller, `analytics.slint` |
| 7 | `detection_plane` (green super-pixel + cube green + mono) | `fits_image.rs` |
| 8 | `stars.rs`: detect + measure + aggregate, with synthetic-field tests | `stars.rs`, `test_support.rs`, `lib.rs` |
| 9 | Star metrics into `FileMetrics`/`Metric`; benchmark step 11 | `analytics.rs` |
| 10 | Readme: analytics metric list, the CFA half-res HFR caveat | `readme.md` |

Steps 1–3 are independently shippable and land the whole Tier-1 win. Steps 4–6 are the RGB
extension. Steps 7–9 are Tier 2, and 7 depends on 4.

## Deliberately deferred

- **Skewness / kurtosis** — cheap from the same counts and decent cloud detectors, but star count
  measures transparency directly and better. Add only if star detection proves too slow.
- **Percentiles (p1/p99)** — `index_at_rank` generalizes to `value_at_quantile` in three lines
  whenever a use case shows up.
- **SNR weight** (PixInsight's combined score) — needs star count and noise, so it becomes
  arithmetic over `FileMetrics` once Tier 2 lands. A follow-up, not a blocker.
- **Frame-to-frame registration offset / drift** — needs star *matching* between frames, not just
  detection. Materially bigger, and a different data structure (`Series` is per-frame scalars).
