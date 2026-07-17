# Design: RGB support for statistics and star analysis

Target: fitz v0.2.0 · Crates touched: `libfitz` (stats + detection plane), `fitsmith`
(stats panel), and `fitz-cli` / `readme` if the CLI grows a matching flag.

## 1. Problem statement

Today both pixel statistics and star analysis are **single-channel only**, and an
already-debayered 3-plane RGB cube (`NAXIS3 = 3`, no `BAYERPAT`) is refused outright:

- `pixel_stats` / `PixelStats` (`libfitz/src/info.rs:30, :491`) describe *a single-channel
  image's* physical values.
- `detection_plane` (`libfitz/src/fits_image.rs:185`) **errors** for anything that is not a
  2D image — an RGB cube is explicitly rejected (`fits_image.rs:834` test).
- `header_info_from` sets `pixel_stats`/`stars` to `None` for `is_debayered_rgb_cube`
  frames, and the GUI reflects this: `doc.rs:89` guards stats behind
  `!is_debayered_rgb_cube`, and both `view::stat_items` and `view::star_items` render an
  empty column, so the stats panel falls back to a placeholder for RGB cubes (documented in
  `fitsmith/readme.md`).

The spec asks to make statistics and star analysis meaningful for RGB images. It offers two
strategies:

1. **Luminance** — collapse `(R,G,B)` to a single luminance value per pixel, then run the
   existing single-channel stats/detection on that luminance plane.
2. **Single channel** — run the existing pipeline on one chosen channel (Red — astro frames
   are often red-heavy; or Green — twice the data of a Bayer sensor, best SNR).

## 2. Design decision: unify on a "reduce RGB → one plane" step

Both spec strategies are the *same shape*: turn a 3-plane cube into one `f64` plane and feed
the existing, well-tested machinery. This maps cleanly onto the abstraction the codebase
**already has** — `MonoPlane` (`fits_image.rs:133`) and `detection_plane`, which is the
single source of truth that already reduces a CFA mosaic to a green super-pixel plane.

So the plan is to generalise "how do we get the one plane to measure" rather than to add a
parallel RGB stats path. Concretely:

- Introduce a channel-reduction policy enum:
  ```rust
  // libfitz — new, e.g. in fits_image.rs beside detection_plane
  pub enum RgbReduction { Luminance, Red, Green, Blue }
  ```
- Extend `detection_plane` (and add a sibling for stats) so that an **RGB cube** is no longer
  an error but is reduced to a `MonoPlane` per the chosen `RgbReduction`:
  - `Luminance` → `0.2126 R + 0.7152 G + 0.0722 B` (Rec.709) per pixel. (Astronomy has no
    canonical luminance; Rec.709 is the sensible, documented default. See Open questions.)
  - `Red`/`Green`/`Blue` → select that plane directly.
- Feed the resulting `MonoPlane` into the **existing** `stats_from_values` /
  `plane_background` / `detect_stars`. Star shapes then measure on a full-resolution plane
  (not the half-res super-pixel plane a CFA mosaic uses), which is *more* accurate for an
  already-debayered frame — worth a note in the report, mirroring the existing half-res
  caveat.

### Why this over a bespoke RGB stats struct

`PixelStats` is consumed in many places (`doc.rs`, `view.rs`, `analytics.rs`, CLI `info`).
Reducing to one plane keeps `PixelStats` and every consumer unchanged; only the *source of
the plane* changes. This honours the CLAUDE.md "reuse/refactor, don't duplicate" rule and
avoids fanning a second numeric type through the whole stack.

## 3. libfitz changes

1. **`scaled_planes` / plane extraction.** An RGB cube's `ImageData` is interleaved or
   planar depending on the reader; `deinterleave_to_planes` (`fits_image.rs:494`) already
   exists. Add a helper that returns the three physical (`BSCALE/BZERO`-applied) channel
   planes as `Vec<f64>` each, reusing `scaled_pixels` semantics.
2. **`reduce_rgb(header, img, RgbReduction) -> MonoPlane`.** Builds the single plane. Full
   resolution (`width × height`), `saturation` carried from `sample_saturation` (the ceiling
   is per-sample and unchanged by a weighted sum where weights sum to 1; for a single-channel
   selection it is exactly the source ceiling).
3. **Generalise the plane entry point.** Rename/extend `detection_plane` so its RGB-cube arm
   calls `reduce_rgb` instead of `bail!`-ing. Keep the 2D mono and CFA-mosaic arms exactly as
   they are. The reduction policy needs to reach it, so thread an `RgbReduction` through
   `InfoRequest` (add a field with a `Default` of `Luminance` or `Green`).
4. **`pixel_stats` for RGB.** `pixel_stats` currently takes `(header, img)` and assumes one
   channel. Route it through the same reduced plane for an RGB cube. The fast
   histogram/count path (`stats_from_counts`) assumes integer raw samples; a luminance plane
   is `f64`, so RGB stats use the general `f64` path (`stats_from_values`) — correct, just
   not the integer fast path. Acceptable: RGB cubes are the uncommon case.
5. **Remove the `is_debayered_rgb_cube` short-circuits** in `header_info_from` so `stars` and
   `pixel_stats` are populated for RGB cubes when requested.

`is_debayered_rgb_cube` / `is_rgb_cube_shape` stay as the detection predicates; they just no
longer gate *out* stats.

## 4. Choosing the policy: default + where the choice lives

Recommended default: **Green**. Rationale matches the spec's own reasoning (green carries the
most signal on a Bayer-derived frame and is the least noisy single channel), it is a plain
channel selection (no colour-weighting assumptions), and it is consistent with the existing
CFA path already detecting on the green plane — so a raw mosaic and its debayered RGB version
report comparable numbers. Luminance is the alternative default if we prefer "uses all the
data."

Exposure of the choice:

- **GUI:** a **View ▸ Statistics channel ▸ {Luminance, Red, Green, Blue}** submenu (radio
  group), stored as an app property, passed into `InfoRequest` when building a `LoadedDoc`.
  Changing it invalidates cached stats (see the caching design doc — stats become keyed on
  this policy). For a mono/CFA frame the setting is inert (there is only one plane), so it
  only affects RGB cubes.
- **CLI (optional, if desired for parity):** `fitz info --channel <lum|r|g|b>`; update
  `readme.md` and `fitz-cli/readme.md` per the CLAUDE.md rule. Can be deferred — the spec is
  framed around the GUI stats panel.

## 5. fitsmith changes

- `doc.rs`: drop the `!is_debayered_rgb_cube(...)` guard at `doc.rs:89`; always compute
  `StatSummary` (stats + star column) — the RGB cube now yields real numbers. Pass the chosen
  `RgbReduction` into the `InfoRequest`.
- `view.rs`: no structural change — `stat_items` / `star_items` already render whatever
  `StatSummary` contains. Consider a small annotation on the panel indicating which channel
  the stats reflect (e.g. label "Median ADU (G)") so an RGB frame's numbers aren't ambiguous.
- `fitsmith/readme.md`: update the stats-panel paragraph — it currently says the panel "falls
  back to a placeholder for an already-debayered RGB cube (where single-channel statistics
  aren't meaningful)." That caveat is being removed/replaced.
- Analytics/Star-metrics batches (`controller/analytics.rs`, `libfitz/src/analytics.rs`)
  currently *skip* RGB cubes. Bringing them in is a natural follow-on but is a larger change
  (the whole time-series is built around per-frame single-channel metrics); recommend scoping
  this spec item to the **stats panel + star column** first and treating analytics-batch RGB
  support as a separate task.

## 6. Testing

- `libfitz`: unit tests on synthetic 3-plane cubes (extend `test_support`) — luminance
  reduction matches the Rec.709 formula on known pixels; channel selection returns the right
  plane; `reduce_rgb` preserves dimensions and saturation; `detection_plane` no longer errors
  on a cube and detects stars planted in a synthetic RGB frame. Add an SHA-256-style
  regression only if a suitable RGB fixture exists (`test-data/uncompressed_debayer.fits`
  looks like a candidate — verify it is a 3-plane cube).
- `fitsmith`: `doc.rs` test that a `LoadedDoc` built from an RGB cube now has `Some(stats)`.

## 7. Open questions / decisions

1. **Default policy — Green vs Luminance.** Recommend Green for consistency with the CFA
   path and single-channel simplicity; confirm with the user.
2. **Luminance coefficients.** Rec.709 (0.2126/0.7152/0.0722) vs equal-weight average vs
   Rec.601. Astronomy has no standard; Rec.709 is defensible and documented. Equal-weight is
   simpler and arguably more honest for narrowband-in-RGB data. Pick one and document it.
3. **Star measurement resolution.** RGB-cube detection runs full-resolution, unlike the CFA
   half-res green plane, so HFR/FWHM numbers are ~2× a CFA frame's. Document this in the same
   spirit as the existing half-res caveat; it does not affect within-session trends.
4. **Should the channel choice also apply to a CFA mosaic?** No — a raw mosaic has no
   separated channels without demosaicing; it keeps its green super-pixel plane. The policy
   only meaningfully applies to RGB cubes (and is inert elsewhere).
