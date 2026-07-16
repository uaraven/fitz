# Tier 1 — robust pixel statistics (σ, MAD, mode, saturated)

Detailed plan for steps 1–3 of [analytics-metrics-plan.md](analytics-metrics-plan.md): add four
robust statistics to `PixelStats`, report them in `fitz info --pixel`, and expose them as
analytics metrics. Steps 4b/4–6 (the green-plane/RGB work) and 7–9 (star metrics) are **out of
scope here** — nothing below depends on them, and nothing below blocks them.

Tier 1 is independently shippable: three commits, each building and testing green on its own.

## What Tier 1 does not change

- `Metric::value(&PixelStats) -> f64` keeps its signature. The `Option` return is step 6, forced by
  star metrics; no Tier-1 metric can be unavailable for an analyzed frame.
- RGB cubes keep being skipped (`SkipReason::NotMono`), `info` keeps printing its "not supported
  for debayered images" notice. That is step 4b/5.
- No new command-line flags, no new files, no new module.

---

## Corrections to the parent plan

Four things in the parent plan are wrong or under-specified against the actual code. They shape the
work below.

**1. `saturation = physical(counts.len() - 1)` is wrong for `U8` frames.** `value_counts`
([info.rs:442](../fitz-core/src/info.rs#L442)) allocates `VALUE_COUNT_SLOTS` (65536) slots for
*both* sample types — a `U8` image only ever occupies slots 0..=255, but `counts.len() - 1` is
still 65535, so an 8-bit frame would report a saturation level of 65535 and a saturated count of 0.
The fix (step 1a below) is to size the array to the sample domain: 256 slots for `U8`, 65536 for
`I16`. Then `counts.len() - 1` is right by construction, and the `U8` path gets cheaper per-chunk
allocations for free.

**2. σ cannot be asserted to match exactly between the fast and general paths.** The fast path sums
`c·(v − mean)²` over ≤65536 slots; the general path sums `(v − mean)²` over every pixel. Different
rounding, different result in the last ulps — and the two paths' *means* already differ, which the
existing `fast_path_matches_general_path_on_real_data` test
([info.rs:838](../fitz-core/src/info.rs#L838)) handles with a relative tolerance. σ gets the same
treatment. MAD *is* exactly comparable (it is a selection over the same multiset, so no arithmetic
to drift) — provided both paths use the same even-count convention (correction 3).

**3. MAD needs an explicit even-count convention.** `median_in_place`
([info.rs:654](../fitz-core/src/info.rs#L654)) averages the two central values for an even count,
and `stats_from_counts` does the same via `index_at_rank`. The MAD must follow that convention in
**both** paths or they will disagree by one slot on every even-count frame — which is every frame
with an even pixel count, i.e. essentially all of them.

**4. `readme.md` has no `info` output sample.** The `info` reference lives in
[fitz-cli/readme.md](../fitz-cli/readme.md) (the `--pixel` bullet at line 184), which describes the
fields in prose rather than showing a sample. The analytics metric list lives in
[fitsmith/readme.md](../fitsmith/readme.md) line 40. Both need updating; the workspace `readme.md`
does not.

---

## Step 1 — `PixelStats` gains σ/MAD/mode/saturated

Files: [fitz-core/src/info.rs](../fitz-core/src/info.rs)

### 1a. Size the value-count array to the sample domain

`value_counts` currently hardcodes `VALUE_COUNT_SLOTS` in its inner `count` helper. Make the slot
count a parameter:

```rust
/// Number of distinct raw sample values in the 16-bit fast path.
const VALUE_COUNT_SLOTS: usize = 1 << 16;
/// … and in the 8-bit one.
const U8_COUNT_SLOTS: usize = 1 << 8;

fn value_counts(img: &ImageData) -> Option<(Vec<u64>, f64)> {
    fn count<T: Sync>(v: &[T], slots: usize, idx: impl Fn(&T) -> usize + Sync + Send) -> Vec<u64> {
        // A chunk must count enough samples to earn back its `slots`-slot
        // allocation and merge; scale the floor with the array it pays for.
        let chunk = v
            .len()
            .div_ceil(rayon::current_num_threads())
            .max(4 * slots);
        …
    }

    match &img.pixels {
        PixelData::U8(v) => Some((count(v, U8_COUNT_SLOTS, |&x| x as usize), 0.0)),
        PixelData::I16(v) => Some((count(v, VALUE_COUNT_SLOTS, |&x| (x as i32 + 32768) as usize), 32768.0)),
        _ => None,
    }
}
```

`MIN_COUNT_CHUNK` folds into `4 * slots` (same value as today for the I16 path, so no behavior
change there). `stats_from_counts` already takes `counts: &[u64]` and never assumes a length, so it
needs no change for this — but every new statistic below can now trust `counts.len() - 1` as "the
largest representable raw sample".

This is a prerequisite, not a nice-to-have: without it, `saturation`/`saturated` are wrong for 8-bit
frames.

### 1b. Add a total-sample count to `PixelStats`

`PixelStats` records `zeros`/`min_count`/`max_count` but not the number of samples measured, so no
caller can turn a count into a fraction without re-deriving `width × height` itself. The saturated
percentage needs it, and step 4's `pixel_stats_view` will need it more (a plane's count is not the
image's). Both paths already have `n` in hand — it is free.

### 1c. The new fields

```rust
pub struct PixelStats {
    // … existing: min, max, mean, median, zeros, min_count, max_count, histogram
    /// Number of samples these statistics were computed over.
    pub count: usize,
    /// Population standard deviation of the physical pixel values. Sensitive to
    /// stars and hot pixels by construction — compare against `mad`.
    pub sigma: f64,
    /// Median absolute deviation from the median, scaled by 1.4826 so it
    /// estimates σ for Gaussian noise while ignoring stars entirely.
    pub mad: f64,
    /// The most common physical pixel value — the sky background level. Ties
    /// resolve to the lowest such value. Approximated to the center of the
    /// largest histogram bucket for float samples, where no exact mode exists.
    pub mode: f64,
    /// Pixels at the sample type's saturation level. Anything above it is
    /// unrepresentable, so "at" and "at or above" are the same set.
    pub saturated: usize,
    /// The saturation level itself: the physical value of the largest
    /// representable raw sample (65535 for the unsigned-16 convention, 255 for
    /// U8), or the observed maximum for float samples — where `saturated` is
    /// therefore definitionally `max_count`.
    pub saturation: f64,
}
```

`const MAD_TO_SIGMA: f64 = 1.4826;` next to `HISTOGRAM_BUCKETS`, referenced by both paths.

Adding fields is source-compatible with every consumer: `PixelStats` is only *constructed* in
`stats_from_counts` and `pixel_stats_general`, and [fitsmith/src/doc.rs:56](../fitsmith/src/doc.rs#L56)
only reads fields into its own `StatSummary`.

### 1d. Fast path — `stats_from_counts`

Fold `mode` into the **existing** walk over occupied slots (the one already accumulating `sum`,
`zeros`, and the histogram). σ needs the mean first, so it gets its own walk; MAD needs the median
first, so it gets a third. Three O(occupied slots) walks, all free next to the counting pass that
built the array.

- **mode** — track `(best_count, best_idx)` in the existing loop with a strict `>` comparison, so
  the *first* (lowest) index wins a tie. Comment why: `max_by_key` would keep the last maximum, and
  on a bimodal amp-glow histogram the lower peak is the sky. Then `mode = physical(best_idx)`.
- **σ** — a second walk: `Σ c·(v − mean)²`, then `sqrt(that / n)`. Two-pass around the known mean,
  *not* `Σv² − (Σv)²/n` — with a sky level of 20000 ADU and noise of 10, that formula cancels
  catastrophically. Say so in a comment.
- **MAD** — a two-cursor outward walk, exact and allocation-free. Deviations `|physical(i) − median|`
  grow monotonically as `i` moves away from the median in either direction, so:
  - `lo` = the last index with `physical(lo) <= median`, `hi = lo + 1`. (Both derivable from
    `index_at_rank(counts, (n - 1) / 2)` / `index_at_rank(counts, n / 2)`, the two central slots —
    for an odd `n` they are the same slot and the median sits exactly on it.)
  - Repeatedly take whichever cursor has the smaller deviation, consume its count, move it outward
    (skipping empty slots), and accumulate. Stop when the cumulative count passes the target rank.
  - **Match `median_in_place`'s convention**: the deviation at rank `n/2` for odd `n`, the mean of
    the deviations at ranks `n/2 − 1` and `n/2` for even `n`. This is what makes the fast and
    general paths comparable exactly (correction 3).
  - `mad = MAD_TO_SIGMA * that deviation`.
- **saturation / saturated** — `saturation = physical(counts.len() - 1)`; `saturated = counts[last]`.
  Correct for both slot sizes after step 1a. A frame whose max is below saturation reads 0, which is
  the truth.

The `n == 0` early return fills the new fields with `count: 0`, `sigma: 0.0`, `mad: 0.0`,
`mode: 0.0`, `saturated: 0`, and a real `saturation: physical(counts.len() - 1)` — the saturation
level is a property of the sample type, not of the (absent) data.

### 1e. General path — `pixel_stats_general`

Order matters here: `median_in_place` reorders `values` (harmless — same multiset) but the MAD step
**overwrites** them with deviations, so σ must be computed before that.

- **σ** — after the mean is known, `values.iter().map(|v| (v - mean).powi(2)).sum::<f64>()`, then
  `sqrt(/ n)`. Kept **sequential**, matching the existing comment's rationale at
  [info.rs:585](../fitz-core/src/info.rs#L585): the mean is summed sequentially so the reported
  value doesn't drift with thread scheduling, and σ has exactly the same property to protect.
- **MAD** — reuse `median_in_place` rather than writing a second selection (CLAUDE.md: no
  duplication):
  ```rust
  values.iter_mut().for_each(|v| *v = (*v - median).abs());
  let mad = MAD_TO_SIGMA * median_in_place(&mut values);
  ```
  One extra O(n) selection, and the even-count convention matches the fast path for free because it
  is literally the same function.
- **mode** — the center of the largest histogram bucket:
  `min + (idx + 0.5) * (max - min) / HISTOGRAM_BUCKETS`, collapsing to `min` for a degenerate range.
  The `histogram` is already computed above it. This is an approximation and the field doc says so
  (1c) — for continuous float values "the most common value" is not a well-posed question.
- **saturation / saturated** — `max` and `max_count`. Documented on the field.
- **count** — `values.len()`, and the `n == 0` branch extends to the new fields as in 1d.

### 1f. Tests (in `info.rs`)

- `robust_stats_match_hand_computed_values` — a small `I16` frame with a known distribution; assert
  σ, MAD, mode, saturated, count against values computed by hand in the test comment.
- `mode_breaks_ties_to_the_lowest_value` — two values with equal counts; assert the lower one.
- `mad_averages_the_two_central_deviations_for_an_even_count` — pins convention 3 directly.
- `mad_is_robust_to_outliers_that_inflate_sigma` — a flat background plus a handful of bright
  pixels: `mad < sigma` by a wide margin. This is the entire reason MAD exists.
- `saturated_counts_pixels_at_the_sample_maximum` — N pixels at 65535 in an unsigned-16 frame →
  `saturated == N`, `saturation == 65535.0`.
- `saturation_level_follows_the_sample_type` — the same shape as a `U8` frame → `saturation == 255.0`.
  This is the regression test for correction 1; it fails on the parent plan's spec.
- Extend `fast_path_matches_general_path_on_real_data`: `mad` exactly; `sigma` with the same relative
  tolerance the `mean` assertion already uses (correction 2, with the reason in the comment);
  `count` exactly. **Not** `mode` — assert it separately in
  `mode_agrees_between_paths_only_to_histogram_bucket_width`, whose name states why.
- Extend `constant_image_degenerate_min_equals_max`: `sigma == 0.0`, `mad == 0.0`, `mode == 0.0`.
- Real data (`uncompressed.fit`, `compressed.fits.fz`): pin σ/MAD/mode as regression values, and
  assert the invariants `mad <= sigma` and `min <= mode <= max`.

---

## Step 2 — report them in `fitz info --pixel`

Files: [fitz-cli/src/info.rs](../fitz-cli/src/info.rs), [fitz-cli/readme.md](../fitz-cli/readme.md)

Four more numbers do not fit on the existing `Pixels:` line
([info.rs:74](../fitz-cli/src/info.rs#L74)) — it is already 5 fields wide. Split by meaning, using
the existing `FIELD_LABEL_WIDTH` (13) column so everything stays aligned, and keep the histogram
last:

```
  Pixels:      min=0 max=65535 mean=2447.36 median=2103 zeros=0
  Noise:       sigma=1832.44 mad=41.51
  Background:  mode=2098
  Saturated:   1204 (0.013%)
```

- Every number goes through the existing `trim_float` so formatting matches the rest of the report.
- The percentage is `saturated / count * 100` using the new `count` field (1b) — the CLI does not
  re-derive `width × height`.
- The `None` branch (RGB cube) is untouched; it becomes step 4b's problem.

Readme (correction 4): expand the `--pixel` bullet in [fitz-cli/readme.md](../fitz-cli/readme.md)
around line 184 to describe the new lines — σ vs MAD (and *why* both: MAD ignores stars, so a
divergence between them is signal, not redundancy), mode as the sky background, and saturation as
derived from the sample type rather than from `DATAMAX`. No flags change, so the `Usage:` block
below it stays as-is.

Tests: this is formatting over `fitz-core` values; the existing CLI tests cover path derivation and
histogram rendering and are unaffected. No new test earns its keep here.

---

## Step 3 — Tier-1 analytics metrics

Files: [fitz-core/src/analytics.rs](../fitz-core/src/analytics.rs),
[fitsmith/readme.md](../fitsmith/readme.md)

Four variants, appended **after** the existing six so every stored/loaded dropdown index keeps
meaning the same thing:

```rust
Metric::Sigma        => "Noise σ",
Metric::Mad          => "Noise MAD",
Metric::Mode         => "Sky background",
Metric::Saturated    => "Saturated pixels",
```

Add to `label`, to `all()` in that order, and to `value` (`stats.sigma`, `stats.mad`, `stats.mode`,
`stats.saturated as f64`). `Metric::value` keeps returning `f64` — see "What Tier 1 does not change".

GUI fallout, all mechanical:

- The ComboBox model is built from `Metric::all()`
  ([controller/analytics.rs:50](../fitsmith/src/controller/analytics.rs#L50)), so it grows 6 → 10
  with no code change. **Check the dropdown's height at 10 items on the smallest supported window** —
  that is the one thing here a test can't tell us.
- `metric_index_maps_to_the_dropdown_order`
  ([controller/analytics.rs:433](../fitsmith/src/controller/analytics.rs#L433)) covers the growth for
  free; `metric_for_index`/`DEFAULT_METRIC` are unchanged.
- `export_file_name` slugs the new labels. **"Noise σ" is the interesting one** — a non-ASCII
  character in an export filename. Extend `export_file_name_slugifies_the_metric` with `Metric::Sigma`
  and pin whatever the slugifier actually produces; if it emits a bare `analytics-noise-.png`,
  fix the label to `"Noise sigma"` rather than special-casing the slugifier.

Tests: update `assert_eq!(Metric::all().len(), 6)` in `metric_values_read_the_matching_stat`
([analytics.rs:301](../fitz-core/src/analytics.rs#L301)) to 10 and extend it with the four new
variants against the same 4x4 fixture.

Readme: extend the metric list at [fitsmith/readme.md](../fitsmith/readme.md) line 40 with the four
new entries. Worth a sentence on σ vs MAD there too — it is the same "why both" question the CLI
readme answers, and the dialog is where someone will actually ask it.

---

## Sequencing

| # | Commit | Files |
|---|--------|-------|
| 1 | Size the value-count array to the sample domain (prep, no behavior change) | `fitz-core/src/info.rs` |
| 2 | `PixelStats`: `count` + σ/MAD/mode/saturated in both paths, with tests | `fitz-core/src/info.rs` |
| 3 | `info --pixel` reports them; CLI readme | `fitz-cli/src/info.rs`, `fitz-cli/readme.md` |
| 4 | Tier-1 `Metric` variants; GUI readme | `fitz-core/src/analytics.rs`, `fitsmith/readme.md` |

Commit 1 is split out from the parent plan's step 1 because it is a pure refactor with its own
regression test (`saturation_level_follows_the_sample_type` fails without it), and because folding
it into the feature commit would hide a real bug fix inside an additive change.

`cargo test --workspace` after each. The three extra O(≤65536) walks are invisible next to the
24M-pixel counting pass; no benchmark is warranted at Tier 1 (the parent plan's step-11 benchmark
exists for star detection, which is O(pixels)).

## Follow-ups this unblocks

- Step 4b (`info --pixel` on RGB cubes) needs `count` from 1b to report a plane's fraction.
- Step 8's star detection consumes `bg.median`, `bg.mad`, and `bg.saturation` directly — the entire
  detection threshold is a Tier-1 byproduct.
