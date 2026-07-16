# Tier 2 — star metrics in a Tools ▸ Star metrics… dialog

Detailed plan for steps 7–9 of [analytics-metrics-plan.md](analytics-metrics-plan.md): one star
detection pass per frame yielding star count, HFR, FWHM and eccentricity, reached from a **new
Tools ▸ Star metrics… menu item** rather than from the existing Analytics dropdown, and reported
per-frame by a **new `fitz info --stars` flag**.

Tier 1 (σ/MAD/mode/saturated) has shipped. Part 2 (the green-plane/RGB work, steps 4–6 of the
parent) is **out of scope here**, and — see correction 1 — nothing below depends on it.

Three phases, each independently shippable:

| Phase | What | Steps |
|-------|------|-------|
| 1 | The detection library — nothing calls it yet | 1–3 |
| 2 | The GUI: metric families and the Star metrics dialog | 4–5 |
| 3 | The CLI: `fitz info --stars` | 6–7 |

Phase 1 is the part worth getting wrong slowly, and it is fully testable before a line of GUI or
CLI exists. Phases 2 and 3 are independent of each other — either can land first.

## What Tier 2 does not change

- **The Analytics dialog keeps exactly its ten pixel metrics.** No star metric appears in its
  dropdown; no pixel metric appears in the star one. The Tier-1 append-only ordering rule now
  applies per family list.
- **RGB cubes stay skipped** — by both dialogs (`SkipReason::NotMono`) and by `info --stars`.
  Star detection could measure a cube's green plane without any of Part 2's machinery, but doing
  it here would mean Star metrics charts a debayered session while Analytics skips it. Part 2
  lifts the restriction everywhere at once.
- **`PixelStats` gains nothing**, and `--pixel`'s output is untouched. `--stars` is a separate
  flag that neither implies nor is implied by it.
- The CSV export keeps its `time_iso,epoch_seconds,value` shape.

---

## Corrections to the parent plan

Five things in the parent plan are wrong, moot, or under-specified against the code as it now
stands. They shape the work below.

**1. Tier 2 does not depend on step 4.** The parent's sequencing asserts "7 depends on 4"
(`PlaneView`/`plane_range`/`pixel_stats_view`). That was true only because `detection_plane` was
specified to handle RGB cubes. With cubes out of scope, `detection_plane` needs the mono and CFA
branches only, and nothing in Tier 2 reads `pixel_stats_view`. Tier 2 is independently shippable
today, on top of Tier 1 alone.

**2. The separate dialog *is* the opt-in, so step 11's checkbox question is already answered.**
The parent frets that star detection is "always computed, not opt-in", and reserves the right to
add a "Detect stars" checkbox if the benchmark shows detection exceeding ~30% of per-file wall
time. A separate menu item makes that moot: Analytics never detects stars and stays exactly as
fast as it is today, and Star metrics detects them because that is the entire point of opening
it. **Do not add a checkbox.** The benchmark still runs (step 6), but it records a number rather
than gating a decision.

**3. `bg.saturation` cannot drive the flat-topped-star rejection.** Step 9.4 rejects blobs whose
peak sits at or above `bg.saturation`, where `bg` is the detection plane's `PixelStats`. But the
detection plane is `f64`, so it takes the general path — and Tier 1 defined that path's
saturation as *the observed maximum*, with `saturated == max_count` (documented on the field at
[info.rs:53](../fitz-core/src/info.rs#L53)). The brightest blob in every frame would peak at
exactly `bg.saturation` and be rejected, every time. That is precisely backwards: the rejection
exists to drop stars that are genuinely clipped, not the best star in the frame.

The saturation ceiling has to come from the **source sample type**, which the plane no longer
remembers once it is `f64`. So `MonoPlane` carries its own `saturation` (step 2), derived from
the source `PixelData` variant, and `detect_stars` reads `plane.saturation` — never
`bg.saturation`. For a float source there is no ceiling, so the field is `f64::INFINITY` and the
rejection correctly never fires.

**4. `Metric::all()` cannot drive two dropdowns.** `metric_for_index`
([controller/analytics.rs:33](../fitsmith/src/controller/analytics.rs#L33)) maps a ComboBox index
through `Metric::all()`, and `DEFAULT_METRIC` ([:41](../fitsmith/src/controller/analytics.rs#L41))
is a constant. With two dropdowns listing two disjoint families, both become family-scoped:
`Metric::of_family(family)` and `default_metric(family)`. `Metric::all()` stays for tests and
exhaustiveness. `metric_index_maps_to_the_dropdown_order`
([:432](../fitsmith/src/controller/analytics.rs#L432)) has to grow a family parameter — it is the
test that keeps a stored index meaning what it meant.

**5. The CFA super-pixel plane changes what `min_pixels`/`max_pixels` mean.** The defaults (5 and
2000) are full-resolution numbers, but a CFA frame detects on a half-resolution plane where every
blob's area is ~4x smaller. A star that covers 20 px full-res covers ~5 px there — right at the
`min_pixels` floor meant to reject hot pixels. Do not tune these blind: step 3 pins the detected
count on the real `uncompressed.fit` mosaic (a GRBG frame, so a 1504x1504 detection plane), and
that number is the evidence. If the floor proves too aggressive, scale the bounds by the plane's
sampling rather than lowering them globally, and say so in a comment.

---

# Phase 1 — the detection library

## Step 1 — prep: `PixelStats` from a plain value slice

Files: [fitz-core/src/info.rs](../fitz-core/src/info.rs)

`stars.rs` needs the background (median + MAD) of a detection plane it holds as `Vec<f64>`, but
the only route to `PixelStats` over `f64` values today is `pixel_stats_general(header, img)`
([info.rs:712](../fitz-core/src/info.rs#L712)), which starts from a `header`/`ImageData` pair and
calls `scaled_pixels` itself. Split the back half out:

```rust
/// Every `PixelStats` field from already-scaled physical values. Reorders
/// `values` (selection for the median, then overwritten with deviations for
/// the MAD) — the caller keeps the multiset, not the order.
pub(crate) fn stats_from_values(values: &mut Vec<f64>) -> PixelStats
```

`pixel_stats_general` becomes `stats_from_values(&mut scaled_pixels(header, img))`. Pure
extraction, no behavior change, every existing test unchanged — hence its own commit, on the
Tier-1 precedent.

Also make `median_in_place` ([info.rs:830](../fitz-core/src/info.rs#L830)) `pub(crate)`: step 3
aggregates per-star measurements to medians and must not grow a second selection (CLAUDE.md: no
duplication).

**Note the ordering hazard for the caller.** `stats_from_values` destroys the order of what it is
given, and a detection plane is addressed *by index* — position is the image. So step 3 hands it
a **clone**, never the plane's own buffer. That clone is the price of a correct threshold; see
step 3.

---

## Step 2 — the detection plane

Files: [fitz-core/src/fits_image.rs](../fitz-core/src/fits_image.rs)

```rust
/// A mono f64 image to detect stars on.
pub struct MonoPlane {
    pub width: usize,
    pub height: usize,
    /// Physical (BSCALE/BZERO-applied) values, row-major.
    pub values: Vec<f64>,
    /// The physical saturation level of the *source* samples — see
    /// `sample_saturation`. Not derivable from `values`, which are f64 and
    /// have no ceiling of their own. `f64::INFINITY` for a float source.
    pub saturation: f64,
}

/// The physical value of the largest representable raw sample: 65535 for the
/// unsigned-16 convention, 255 for `U8`, `f64::INFINITY` for float samples,
/// which have no ceiling to clip against.
pub fn sample_saturation(header: &Header, img: &ImageData) -> f64;

/// Build the plane star detection runs on:
///   - CFA mosaic (BAYERPAT present) → green super-pixel plane, (w/2 x h/2)
///   - mono (2D, no BAYERPAT)        → the frame's scaled values
/// Errors for any other shape; RGB cubes are Part 2's job.
pub fn detection_plane(header: &Header, img: &ImageData) -> Result<MonoPlane>;
```

**Why a super-pixel plane at all:** a star profile sampled through a Bayer filter is not a PSF,
and its measured HFR is noise. Each 2x2 cell contributes the mean of its two green sites. Locate
those two sites from `resolve_cfa` ([fits_image.rs:77](../fitz-core/src/fits_image.rs#L77)) —
`RGGB`/`BGGR` have green at `(1,0)` and `(0,1)`, `GBRG`/`GRBG` at `(0,0)` and `(1,1)`. An odd
width or height drops the last column/row; a 2x2 cell is the quantum here.

Averaging two sites preserves the saturation level (the mean of two clipped samples is still the
clip level), so `MonoPlane::saturation` is the source's `sample_saturation` unchanged — worth a
comment, since it is the reason correction 3's fix is this cheap.

**The half-resolution consequence must be documented on `detection_plane`, not just noted here:**
a CFA frame's HFR and FWHM come out in half-res pixels, roughly half the number NINA reports for
the same frame. That is fine and is not a bug to fix later — every frame in a session comes off
the same sensor, so the *trend*, which is the only thing a time series shows, is unaffected. The
same sentence goes in the readme (step 6).

Reuse `bscale_bzero` ([:117](../fitz-core/src/fits_image.rs#L117)) for the scaling; do not
re-implement the per-sample map.

**Tests** (in `fits_image.rs`):
- `detection_plane_averages_the_green_sites_of_a_mosaic` — a synthetic 4x4 `RGGB` frame with
  known values → a 2x2 plane whose each pixel is the mean of that cell's two greens.
- `detection_plane_locates_green_for_every_bayer_pattern` — the same cell values under all four
  patterns; the green mean differs per pattern and is asserted per pattern.
- `detection_plane_of_a_mono_frame_is_the_frame` — dimensions and values match `scaled_pixels`.
- `detection_plane_rejects_an_rgb_cube` — the documented error, so the Part 2 boundary is a test,
  not a comment.
- `sample_saturation_follows_the_sample_type` — 65535 for unsigned-16, 255 for `U8`, infinite for
  `F32`.
- `sample_saturation_agrees_with_pixel_stats` — the ceiling `sample_saturation` derives from the
  `PixelData` variant must equal the one `stats_from_counts` derives from its array length
  ([info.rs:532](../fitz-core/src/info.rs#L532)). Two mechanisms, one truth; this is the guard
  against them drifting.
- Real data: `uncompressed.fit` (a 3008x3008 GRBG mosaic) → a 1504x1504 plane, and
  `sample_saturation == 65535.0`.

---

## Step 3 — `fitz-core/src/stars.rs`

Files: new `fitz-core/src/stars.rs`, [fitz-core/src/lib.rs](../fitz-core/src/lib.rs),
[fitz-core/src/test_support.rs](../fitz-core/src/test_support.rs)

Pure, `Send`, no I/O — the same contract as the rest of `fitz-core`. Add `pub mod stars;` to
`lib.rs`.

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

pub struct Star { pub x: f64, pub y: f64, pub flux: f64, pub hfr: f64, pub fwhm: f64, pub eccentricity: f64 }

pub struct StarStats {
    pub count: usize,
    /// Median across accepted stars; `None` when none were accepted.
    pub hfr: Option<f64>,
    pub fwhm: Option<f64>,
    pub eccentricity: Option<f64>,
}

/// The plane's own background, for `detect_stars`'s threshold.
pub fn plane_background(plane: &MonoPlane) -> PixelStats;

pub fn detect_stars(plane: &MonoPlane, bg: &PixelStats, opts: &StarDetectOptions) -> StarStats;
```

`StarDetectOptions` is not user-configurable in Tier 2 — `Default` is the only constructor the
GUI uses. It is a struct rather than three constants so the tests can drive rejection paths
directly.

**Why `plane_background` is separate from `detect_stars`.** The threshold must reflect the noise
of *the plane detection runs on*, not the mosaic's: a green super-pixel averages two sites, so its
σ is lower by ~√2, and a threshold built from the mosaic's MAD would sit ~1.4σ too high on
exactly the frames CFA users care about. Keeping it a separate call leaves `detect_stars` pure
and testable against a synthetic background, and leaves the door open for a mono caller to pass
the frame's already-computed `PixelStats` (for a mono frame the detection plane *is* the frame,
so they are the same numbers) — an optimization the step-6 benchmark can justify, not one to take
on spec.

`plane_background` clones `plane.values` and calls `stats_from_values` (step 1): the plane is
addressed by index and must survive. For the CFA case that is a quarter-size buffer; for a full
mono frame it is a transient copy the size of the frame's own `f64` values. Correctness beats
saving the allocation — say so in the comment, and let the benchmark reopen it.

### Algorithm

1. **Threshold** = `bg.median + opts.sigma_k * bg.mad`. Both are Tier-1 statistics, both robust
   to the very stars being detected — which is why the threshold is not chicken-and-egg.
2. **Mask** — `values[i] > threshold` into a `Vec<bool>`, parallel (`rayon`).
3. **Connected components**, 8-connected, **iterative flood fill with an explicit stack — never
   recursion**: a bright nebula is one blob spanning millions of pixels and would blow the stack.
   Sequential over the mask, clearing each visited cell so the mask doubles as the visited set.
   Collect each blob's pixel indices. (A run-length + union-find pass is the fallback if
   profiling ever demands it; it is not warranted up front.)
4. **Reject** blobs with `area < min_pixels`, `area > max_pixels`, any pixel touching the frame
   border (a truncated PSF makes garbage moments), or a peak `>= plane.saturation` (flat-topped ⇒
   HFR biased low, which is exactly the frame you would wrongly call well-focused). Correction 3:
   `plane.saturation`, **not** `bg.saturation`.
5. **Measure** each accepted blob **in parallel** (`par_iter` over the collected blobs), on
   background-subtracted flux `f_i = v_i − bg.median`:
   - centroid `x̄ = Σ f_i·x_i / Σ f_i`, likewise `ȳ`;
   - **HFR** = `Σ f_i·r_i / Σ f_i`, `r_i = hypot(x_i − x̄, y_i − ȳ)` — the flux-weighted mean
     radius, i.e. NINA's definition;
   - second moments `Mxx = Σ f_i·(x_i − x̄)² / Σ f_i`, `Myy`, `Mxy`;
   - **FWHM** = `2.3548 * sqrt((Mxx + Myy) / 2)`;
   - **eccentricity** = `sqrt(1 − λ₂/λ₁)` from `λ = (Mxx+Myy)/2 ± sqrt(((Mxx−Myy)/2)² + Mxy²)`;
     `0.0` when `λ₁ <= 0`.
6. **Aggregate** to medians across accepted stars via `median_in_place` (step 1) — medians, not
   means, so one satellite streak that survives step 4 cannot move the number.

### Test fixtures

Add to `test_support.rs`:

```rust
pub(crate) fn write_star_field_fits(
    path: &Path, w: usize, h: usize, background: f64, stars: &[(f64, f64, f64, f64, f64)],
) // (x, y, sigma_x, sigma_y, peak)
```

Deterministic, no RNG — a fixed low-amplitude ripple stands in for noise so a test can never flake
on a seed.

### Tests (in `stars.rs`)

- `detects_every_star_in_a_synthetic_field` — 9 round Gaussians → `count == 9`, each centroid
  within 0.1 px of truth.
- `fwhm_and_hfr_match_the_gaussian_they_were_measured_from` — `fwhm ≈ 2.3548σ`, `hfr ≈ 1.2533σ`
  (the flux-weighted mean radius of a 2D Gaussian) to ~15%, **and assert both are biased low** —
  thresholding truncates the wings, so the bias has a direction. It is a property of the method,
  not slop, and a test that only bounds `|error|` would hide it flipping sign.
- `eccentricity_measures_elongation` — σx = 2σy → `≈ 0.866` (`sqrt(1 − ¼)`) within 0.05; and a
  round star → `≈ 0`.
- `rejects_hot_pixels_below_the_area_floor` — a single hot pixel → `count == 0`.
- `rejects_stars_touching_the_border` — a Gaussian centered on the edge → excluded.
- `rejects_flat_topped_saturated_stars` — a star clipped at the plane's saturation → excluded.
  This is correction 3's regression test: it passes trivially if `saturation` is read from the
  right place and fails loudly if `bg.saturation` creeps back in.
- `empty_frame_detects_nothing` — pure background → `count == 0`, `hfr == None`.
- `real_mosaic_detects_plausible_stars` — `uncompressed.fit` through `detection_plane` → `count`
  pinned as a regression value (correction 5's evidence), `hfr` in a plausible range, and
  `eccentricity < 0.8` (a tracked sub is not made of streaks).

---

# Phase 2 — the GUI

## Step 4 — metric families and `Option` values

Files: [fitz-core/src/analytics.rs](../fitz-core/src/analytics.rs)

```rust
/// Which dialog lists a metric — and, therefore, whether a batch has to detect
/// stars to answer it.
pub enum MetricFamily { Pixel, Star }

Metric::StarCount    => "Star count",
Metric::Hfr          => "HFR",
Metric::Fwhm         => "FWHM",
Metric::Eccentricity => "Eccentricity",
```

All four labels are ASCII, so the `export_file_name` slugifier needs no help this time (Tier 1's
`"Noise sigma"` compromise does not repeat).

- `Metric::family(self) -> MetricFamily`, and `Metric::of_family(f) -> &'static [Metric]` — the
  dropdown source (correction 4). Appended after the existing ten in `all()`.
- `AnalyzeOptions { pub detect_stars: bool }`; `analyze_file(path, &opts)`
  ([analytics.rs:134](../fitz-core/src/analytics.rs#L134)). The only callers are the FitSmith
  controller and this module's tests — the CLI does not use `analytics`.
- `FileMetrics` gains `pub stars: Option<StarStats>` — `None` meaning *this batch did not ask*,
  which is distinct from `Some(StarStats { count: 0, hfr: None, .. })` meaning *asked, found
  none*. The dialog's "no stars detected" note depends on telling those apart.
- **`Metric::value(self, m: &FileMetrics) -> Option<f64>`**, replacing
  `value(self, stats: &PixelStats) -> f64` ([:68](../fitz-core/src/analytics.rs#L68)). This is
  the signature change Tier 1 deliberately deferred. Pixel metrics return `Some`; `StarCount`
  returns `Some(count as f64)`; `Hfr`/`Fwhm`/`Eccentricity` pass their `Option` through; every
  star metric returns `None` when `stars` is `None`.
- `Series` gains `pub unavailable: usize` — frames that analyzed fine but have no value for *this*
  metric. `build_series` ([:159](../fitz-core/src/analytics.rs#L159)) `filter_map`s and counts.

This commit must include the controller's compile fixes (`build_series`/`value` callers) to stay
green; the dialog itself is step 5.

**Tests**: extend `metric_values_read_the_matching_stat` to 14 and to the new signature; a frame
analyzed without stars yields `None` for every star metric and `Some` for every pixel metric;
`build_series` over a mix of star-bearing and starless frames drops the latter and counts them in
`unavailable`; `of_family` partitions `all()` with no overlap and no omission.

---

## Step 5 — the Tools ▸ Star metrics… dialog

Files: [fitsmith/ui/app.slint](../fitsmith/ui/app.slint),
[fitsmith/ui/analytics.slint](../fitsmith/ui/analytics.slint),
[fitsmith/src/controller/analytics.rs](../fitsmith/src/controller/analytics.rs)

**A separate menu item, opening one dialog that has two modes.** Tools ▸ Star metrics… is its own
entry next to Tools ▸ Analytics…, with its own callback, its own default metric and its own batch
— from the menu down, the two are distinct features and a user never switches "mode" by hand.
What they share is the widget tree behind them, because the star dialog differs from the
analytics one in exactly four things: its title, which metrics its dropdown lists, whether its
batch detects stars, and its export file-name prefix. Everything else — the chart, the zoom
slider, the progress overlay, the cancel path, the PNG snapshot/crop, the CSV writer, the
resizable card — is identical. Copying `AnalyticsDialog` (147 lines of Slint) and the controller
(540 lines of Rust) to change four things would be the largest duplication in the codebase, and
CLAUDE.md forbids it. The family travels as state; the menu entries and the widgets are two
different questions.

- **`app.slint`**: a `MenuItem { title: "Star metrics…"; }` after `Analytics…`
  ([app.slint:311](../fitsmith/ui/app.slint#L311)) firing a new `open-star-metrics-dialog()`
  callback ([:158](../fitsmith/ui/app.slint#L158)). The `analytics-*` property set is reused
  as-is, plus `analytics-title` and `analytics-unavailable-note`.
- **`analytics.slint`**: the hardcoded `"Analytics"` title ([:56](../fitsmith/ui/analytics.slint#L56))
  becomes `in property <string> title-text`. The count line
  ([:109–114](../fitsmith/ui/analytics.slint#L109)) appends an `in property <string>
  unavailable-note`. It is a controller-built string, not an int like its `skipped-*` neighbours,
  because the wording is family-specific ("2 with no stars detected" means nothing in the pixel
  dialog) and a presentational component should not branch on a family it cannot see.
- **`controller/analytics.rs`**: `AppState` gains `analytics_family`. `open_analytics_dialog`
  ([:47](../fitsmith/src/controller/analytics.rs#L47)) becomes
  `open_chart_dialog(app, MetricFamily)`, with two thin public entry points. `metric_for_index`
  and `DEFAULT_METRIC` become family-scoped (correction 4); the star default is `Hfr` — the metric
  people actually cull subs on. `analyze_batch` ([:103](../fitsmith/src/controller/analytics.rs#L103))
  passes `AnalyzeOptions { detect_stars: family == Star }`. `replot`
  ([:226](../fitsmith/src/controller/analytics.rs#L226)) fills the unavailable note.
  `export_file_name` ([:245](../fitsmith/src/controller/analytics.rs#L245)) takes the prefix from
  the family: `analytics-mean-adu.png`, `star-hfr.png`.

**Known consequence, accepted:** both dialogs share `analytics-card-width`/`-height`, so resizing
one resizes the other. They are the same dialog; a second remembered size is not worth a second
property pair.

**Tests**: `export_file_name_slugifies_the_metric` extends to `Metric::Hfr` and the star prefix;
`metric_index_maps_to_the_dropdown_order` grows its family parameter and covers both lists;
`analyze_batch` gets a star-family case asserting `stars.is_some()` on a real mosaic and
`is_none()` for the pixel family — the latter being the test that Analytics did not silently start
paying for detection.

**The one thing tests can't cover:** check the star dropdown's height at 4 items and the analytics
one at 10 on the smallest supported window. (Tier 1 left the 10-item check open; it lands here.)

---

# Phase 3 — the CLI

## Step 6 — `HeaderInfo` can carry star metrics

Files: [fitz-core/src/info.rs](../fitz-core/src/info.rs), [fitsmith/src/doc.rs](../fitsmith/src/doc.rs)

**The problem this step exists to avoid: a second file read.** `info_file`
([fitz-cli/src/info.rs:23](../fitz-cli/src/info.rs#L23)) gets everything from
`header_info_with_pixels(input)`, which reads the file, finds the image HDU (decompressing a `.fz`
on the way) and returns a `HeaderInfo` — but not the image. A CLI that then called into `stars`
itself would have to open and decompress the frame a second time. `--pixel --stars` on a batch of
`.fz` subs would double the most expensive thing the command does.

So the request travels *in*, and the results come back on `HeaderInfo`:

```rust
/// What to compute beyond the header-derived metadata. Each field costs a
/// pass over the pixels, so the caller asks for what it will print.
#[derive(Clone, Copy, Default)]
pub struct InfoRequest {
    pub pixel_stats: bool,
    pub stars: bool,
}

/// Star metrics, plus the plane they were measured on — a green super-pixel
/// plane is half the frame's size, and its HFR/FWHM are in *its* pixels, so a
/// report that omits this is actively misleading.
pub struct StarReport {
    pub stats: StarStats,
    pub plane_width: usize,
    pub plane_height: usize,
}

// HeaderInfo gains:
pub stars: Option<StarReport>,
```

- `header_info_with_pixels(input)` **is replaced** by `header_info_with(input, InfoRequest)` —
  a third `header_info_with_stars` and a fourth for the combination is exactly the API sprawl
  a request struct exists to prevent. The CLI is its only caller; two tests in `info.rs` name it
  and move over mechanically.
- `header_info_from(header, img, with_pixels: bool)` → `(header, img, req: InfoRequest)`.
  [doc.rs:54](../fitsmith/src/doc.rs#L54) passes `false` today and becomes
  `InfoRequest::default()`. `header_info(input)` is unchanged — it reads no pixels at all.
- The `stars` field is filled by `detection_plane` + `plane_background` + `detect_stars`
  (steps 2–3), skipped for an RGB cube exactly as `pixel_stats` already is, via the same
  `is_debayered_rgb_cube` guard at [info.rs:133](../fitz-core/src/info.rs#L133).

`StarReport` carries the plane dimensions rather than letting the CLI halve the frame's own
resolution: the "is it a super-pixel plane, and did an odd width drop a column" rule lives in
`detection_plane` and must not be re-derived by a caller that would then drift from it. The CLI
prints the note when `plane_width != info.width` — a comparison, not a copy of the rule.

**Tests** (in `info.rs`): `header_info_with_stars_reads_star_metrics_on_real_data` —
`uncompressed.fit` yields `Some`, with plane dimensions of 1504x1504 (the mosaic halved) and a
count matching step 3's pinned regression value, reached through a different entry point;
`header_info_with_stars_on_an_rgb_cube_has_no_stars` — the cube guard; and
`InfoRequest::default()` computes neither, so the cheap path stays cheap.

---

## Step 7 — `fitz info --stars`

Files: [fitz-cli/src/main.rs](../fitz-cli/src/main.rs),
[fitz-cli/src/options.rs](../fitz-cli/src/options.rs),
[fitz-cli/src/info.rs](../fitz-cli/src/info.rs), [fitz-cli/readme.md](../fitz-cli/readme.md)

A new `--stars` flag on `info`, mirroring `--pixel`'s shape exactly: `InfoArgs.stars`
([main.rs:312](../fitz-cli/src/main.rs#L312)) → `InfoOptions.stars`
([options.rs:59](../fitz-cli/src/options.rs#L59)) → `run_info`
([main.rs:719](../fitz-cli/src/main.rs#L719)) composes the `InfoRequest`. No new command, no new
module: `info` is already the "tell me about this frame" verb, and star metrics are a fact about
a frame.

**The two flags are independent in both directions.** `--stars` does not imply `--pixel`: star
detection builds its threshold from the *detection plane's* own background (step 3), never from
the frame's `PixelStats`, so `--stars` alone prints no `Pixels:` line and skips the value-count
pass entirely. And `--pixel` does not imply `--stars`, which is what keeps `--pixel` as fast as
it is today.

```
  Pixels:      min=228 max=65532 mean=778.952603 median=808 zeros=0
  …
  Stars:       count=1243 hfr=2.41 fwhm=3.62 eccentricity=0.31
               measured on the green super-pixel plane, 1504 x 1504
```

- Every number through `trim_float`, and the label into the existing `FIELD_LABEL_WIDTH` column,
  as in Tier 1.
- **The second line appears only when the detection plane is not the frame** (`plane_width !=
  info.width`), and it is the whole reason `StarReport` carries the dimensions. This is where the
  half-resolution caveat meets the person who would otherwise file "fitz reports half of NINA's
  HFR" as a bug. A readme note is easy to miss; the line above the number is not.
- No stars detected: `Stars:       none detected` — an outcome, not an error. It is also a cloud
  indicator, so it must be reportable rather than silent.
- RGB cube: `Stars:       star metrics are not supported for debayered images`, mirroring the
  existing `--pixel` notice at [fitz-cli/src/info.rs:68](../fitz-cli/src/info.rs#L68). It must
  **not** become a per-file error: `process_files` would print `fitz: <path>: <err>` and fail the
  exit code for a batch, and a debayered cube is an unsupported shape, not a broken file.
- Star detection uses rayon inside a `process_files` batch that is itself parallel across files.
  That nesting already exists (`pixel_stats` does it under `--pixel`), so it needs no new
  handling — worth a sentence in the review, not new code.

**Tests**: `fitz-cli` keeps tests for CLI-only concerns, and this step's logic is formatting over
`fitz-core` values — the same call Tier 1's step 2 made, for the same reason. The one test that
earns its keep is the plane-note rule, which is a real branch rather than a format string: extract
it as a small pure fn (`star_plane_note(info) -> Option<String>`) and test that a mosaic gets the
note, a mono frame gets none.

**Readme** (this changes the command-line surface, so CLAUDE.md requires it): a `--stars` bullet
in the `info` section of [fitz-cli/readme.md](../fitz-cli/readme.md) — the four metrics and what
each one is for, that it is independent of `--pixel`, and the half-res CFA caveat — plus the
`Usage:` block below it, which enumerates the flags and now omits one. The workspace
[readme.md](../readme.md) does not enumerate `info`'s flags and stays as-is.

---

# Closing

## Step 8 — benchmark and readme

Files: [fitsmith/readme.md](../fitsmith/readme.md)

**Benchmark** (correction 2 — informational, not a gate): time `analyze_file` on
`test-data/uncompressed.fit` and `compressed.fits.fz` with `detect_stars` off and on. Record both
numbers in the commit message. They are the evidence for two open questions the plan deliberately
left to data: whether `plane_background`'s clone is worth removing for mono frames (step 3), and
whether the CFA area bounds need scaling (correction 5). No checkbox either way.

**Readme**: a Star metrics section next to the Analytics one covering the four metrics, what each
one tells you (HFR/FWHM = focus and seeing, eccentricity = tracking, star count = transparency),
why it is a separate menu item (it reads every pixel *and* detects stars; Analytics stays cheap),
the "N with no stars detected" note as a cloud indicator in its own right, and — the one caveat a
user will otherwise file as a bug — **that a CFA frame's HFR and FWHM are in half-resolution
pixels and read about half of NINA's number, while the trend is unaffected.**

---

## Sequencing

| # | Phase | Commit | Files |
|---|-------|--------|-------|
| 1 | 1 | Extract `stats_from_values`; `median_in_place` `pub(crate)` (prep, no behavior change) | `fitz-core/src/info.rs` |
| 2 | 1 | `MonoPlane`, `sample_saturation`, `detection_plane`, with tests | `fitz-core/src/fits_image.rs` |
| 3 | 1 | `stars.rs`: detect, measure, aggregate, with synthetic-field tests | `fitz-core/src/stars.rs`, `test_support.rs`, `lib.rs` |
| 4 | 2 | Metric families, `value` → `Option`, `Series::unavailable`, `FileMetrics::stars` | `fitz-core/src/analytics.rs`, controller (compile fixes) |
| 5 | 2 | Tools ▸ Star metrics… dialog | `app.slint`, `analytics.slint`, `controller/analytics.rs` |
| 6 | 3 | `InfoRequest`/`StarReport`; `header_info_with` | `fitz-core/src/info.rs`, `fitsmith/src/doc.rs`, `fitz-cli/src/info.rs` |
| 7 | 3 | `fitz info --stars`; CLI readme | `fitz-cli/src/{main,options,info}.rs`, `fitz-cli/readme.md` |
| 8 | — | Benchmark; GUI readme | `fitsmith/readme.md` |

`cargo test --workspace` after each; each builds and tests green on its own. Commits 1–3 add a
library nothing calls yet — deliberate, and the reason each carries its own tests: `stars.rs` is
the part worth getting wrong slowly, and it is fully testable before a single line of frontend
exists. Commits 4–5 and 6–7 are independent of each other and only depend on 1–3, so the GUI and
the CLI can land in either order.

## Follow-ups this unblocks

- **Part 2 (RGB cubes)**: `detection_plane` grows a cube branch returning the green plane at full
  resolution, and the skip rule relaxes for both dialogs at once.
- **SNR weight** (PixInsight's combined score): arithmetic over `FileMetrics` once star count and
  noise are both on it — but it needs the two families in one place, so it either forces a metric
  that reads both or lives in a third dialog. Worth deciding before it is built.
- **Tunable `StarDetectOptions`** — `sigma_k` especially, for a sparse field. The struct already
  exists for it; the dialog needs a control and the CLI needs flags (`--star-sigma` and the area
  bounds). Deliberately not in Tier 2: defaults that no one has yet had a reason to change do not
  need a knob, and correction 5's pinned count is the evidence that would justify one.
- **A CLI session view** — `info --stars` reports one frame at a time; charting a session is
  GUI-only. `analytics::build_series` and `write_csv` are pure and frontend-agnostic, so a
  `fitz analytics --metric hfr --csv` subcommand is arithmetic plus argument parsing. The parent
  plan anticipated this ("a future CLI subcommand") when it put `analytics` in `fitz-core`.
