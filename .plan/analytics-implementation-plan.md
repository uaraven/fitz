# FitSmith Analytics — Time-Series Metric Charts

## Context

FitSmith (the Slint GUI in `fitsmith/`) already loads a working set of FITS files with
per-row checkboxes and computes per-image pixel statistics. The spec
([.plan/analytics.md](analytics.md)) asks for a new analytics capability: compute a
chosen metric across the **checked** files, order the samples by acquisition time, and plot
the metric over the session in a dialog. Astrophotographers use this to spot trends and bad
frames across a night (e.g. mean ADU drifting with sky brightness, spikes in saturated-pixel
counts).

Phase-1 metrics: **min, max, median, mean ADU, count of max-ADU pixels, count of min-ADU
pixels**. The chart must fit-to-screen, zoom (fit → 4×), draw a line with point marks, show an
X/Y tooltip on hover, export PNG and CSV, and adapt to light/dark themes.

### Spike result (build vs. reuse)

Slint 1.17's std-widgets ship **no** chart component (only Button/CheckBox/ComboBox/LineEdit/
ListView/TabWidget/Slider/SpinBox/…). There is no maintained third-party Slint charting crate.
Decision (matching the lean toward "my own for better control"): **build a small custom chart**
using native Slint vector primitives (`Path`, `Rectangle`, `Text`, `TouchArea`).

### Decisions

- **Rendering:** Slint-native vector drawing. PNG export via `Window::take_snapshot()` cropped
  to the chart area — one rendering path, no duplicate plotter, no font rasterizer needed
  (Slint draws all text). `take_snapshot` exists in Slint 1.17.1.
- **Missing timestamp:** files without a parseable `DATE-OBS` are **skipped**; the skipped
  count is reported in the dialog.
- **X axis:** **real elapsed time** — points sit at their true timestamps, so session gaps
  (meridian flip, clouds) show as gaps.

---

## Architecture overview

Follows the crate split already in place: pure, `Send`, testable computation in **`fitz-core`**;
GUI plumbing (Slint props, threading, dialogs) in **`fitsmith`**. The batch runs off-thread
with the established `std::thread::spawn` + `weak.upgrade_in_event_loop` + generation-guard
pattern (see `fitsmith/src/controller/export.rs::spawn_export`).

---

## fitz-core changes

### 1. Extend `PixelStats` with min/max pixel counts — [fitz-core/src/info.rs](../fitz-core/src/info.rs)

`PixelStats` (info.rs:21) currently has `min, max, mean, median, zeros, histogram`. Add:

```rust
pub min_count: usize,   // pixels equal to the minimum ADU
pub max_count: usize,   // pixels equal to the maximum ADU
```

**Single-pass, no second scan.** ADU values are 16-bit — only 65536 distinct values are
possible (analysis runs on non-debayered mono frames; see below). So build a
**full-resolution value-count array** (`Vec<u64>` of length 65536, indexed by the raw ADU
sample) in **one** parallel pass over the raw image samples — the same per-thread-array /
element-wise-`reduce` shape already used by `histogram` (info.rs:407). From that array,
everything falls out with no sort and no second pass:

- `min` = lowest occupied index, `max` = highest occupied index (map back through BSCALE/BZERO
  for the reported physical value; the affine, monotonic scaling preserves ordering and equal
  counts, so counting on raw samples is exact).
- `min_count` / `max_count` = the counts at those two indices.
- `median` = value at the cumulative-count midpoint (replaces the `select_nth_unstable`
  sort in `median_in_place`).
- `zeros` = count at the index mapping to physical 0.
- 256-bin display `histogram` = collapse the value-count array into 256 bins.

`mean` still needs the sum, which is also derivable from the value-count array
(`Σ value·count`) — so the whole of `pixel_stats` can be served by this one pass, dropping the
current min/max/zeros fold, the separate histogram pass, and the median sort. Keep the change
scoped: preserve the existing `PixelStats` public shape (plus the two new fields) and the
256-bin `histogram` output so `StatsPanel`/`StatSummary` are unaffected.

> This full-resolution histogram path assumes integer 16-bit samples. If a frame's samples
> aren't representable that way, fall back to the current fold+sort path. For analytics'
> phase-1 metrics only min/max/median/mean and the two counts are needed.

Update the existing `PixelStats` construction and any struct-literal test fixtures. Surfacing
the two new counts in the GUI `StatsPanel` is optional (not required by the spec).

### 2. Timestamp parsing — new helper in [fitz-core/src/info.rs](../fitz-core/src/info.rs) (or a small `time.rs`)

`DATE-OBS` is read today only as a raw `Option<String>` (info.rs:114); there is no date crate
and no parsing. Add a dependency-free parser for the FITS ISO-8601 form
(`YYYY-MM-DDTHH:MM:SS[.sss]`, UTC by convention) that returns seconds since the Unix epoch as
`f64` (fractional seconds preserved), suitable both as a sortable key and as the numeric X
value:

```rust
pub fn parse_date_obs(s: &str) -> Option<f64>   // epoch seconds, None if unparseable
```

Manual field parse + a civil-date-to-days computation (`days_from_civil`, the standard
Howard-Hinnant algorithm) — no external crate, consistent with the project's low-dependency
ethos. Unit-test round-trips and ordering. Also keep the raw string for tooltip/CSV display.

### 3. Analytics series module — new [fitz-core/src/analytics.rs](../fitz-core/src/analytics.rs)

Pure, Slint-free, `Send`. This is the reusable core (a future CLI subcommand could call it too).

```rust
#[derive(Clone, Copy)]
pub enum Metric { Min, Max, Median, Mean, MaxPixelCount, MinPixelCount }
impl Metric { pub fn label(self) -> &'static str; pub fn all() -> &'static [Metric]; }

pub struct SamplePoint { pub time: f64, pub time_str: String, pub value: f64, pub path: PathBuf }
pub struct Series { pub metric: Metric, pub points: Vec<SamplePoint>, pub skipped: Vec<PathBuf> }

// Compute ALL phase-1 metrics for one file in a single pixel read.
pub struct FileMetrics { pub time: f64, pub time_str: String, pub stats: PixelStats }
pub fn analyze_file(path: &Path) -> Result<Option<FileMetrics>>;  // None = skip (no DATE-OBS / not mono)

pub fn build_series(files: &[FileMetrics], metric: Metric) -> Series;  // extract + sort by time
pub fn write_csv(series: &Series, w: impl std::io::Write) -> io::Result<()>;
```

`analyze_file` reuses `find_image_hdu` + `header_info_from`/`pixel_stats` (so `.fz` inputs work
transparently) and `parse_date_obs`. Computing every metric once per file means switching the
dropdown re-plots instantly with **no** re-read — collect `Vec<FileMetrics>` once, then
`build_series` per metric selection.

**Mono-only.** ADU metrics are only meaningful on raw, non-debayered frames, so `analyze_file`
returns `None` (skip) for a debayered/RGB cube — exactly the case where `pixel_stats` already
declines (`header_info_with_pixels` leaves `pixel_stats` `None` for RGB, info.rs). `Series`
should distinguish files skipped for **no DATE-OBS** from those skipped as **not-mono** so the
dialog can report both counts (or fold both into one "skipped" line with a reason).

`write_csv` emits `time_iso,epoch_seconds,value` rows (header line + one row per point).

Register the module in `fitz-core/src/lib.rs`. Unit-test `analyze_file`/`build_series`/
`write_csv` against bundled `test-data/` fixtures (some fixtures may lack `DATE-OBS` — assert
they're skipped; synthesize one with a known `DATE-OBS` via `test_support` if needed).

---

## fitsmith changes

### 4. Shared data types — [fitsmith/ui/types.slint](../fitsmith/ui/types.slint)

Add a Slint struct for chart points and re-export from `app.slint` (line 16):

```
export struct ChartPoint { x: float, y: float, time-label: string, value-label: string }
```

The controller maps a `Series` into a `Vec<ChartPoint>` in **screen-normalized 0..1** X/Y (so
the Slint side stays layout-only), plus axis-tick label lists.

### 5. Chart component — new [fitsmith/ui/chart.slint](../fitsmith/ui/chart.slint)

Self-contained `AnalyticsChart` component (a plot area, not a dialog):

- **Inputs:** `points: [ChartPoint]`, `x-ticks`/`y-ticks` label+position lists, `zoom: float`
  (1.0 = fit, up to 4.0), `metric-label: string`, and theme colors.
- **Drawing:** axes + gridlines via `Rectangle`/`Path`; the series line via a Slint `Path`
  built from the points; point marks via small `Rectangle`s (or `Path` circles) at each point.
- **Fit-to-screen vs zoom:** X positions are `point.x * plot-width * zoom` inside a horizontal
  `Flickable` (Slint's scrollable viewport) so zoom > fit scrolls horizontally; Y always fits.
  A slider/`+`/`−` control drives `zoom` in [1.0, 4.0].
- **Hover tooltip:** one `TouchArea` per mark (`has-hover`) toggles a small `Rectangle`+`Text`
  bubble showing `time-label` / `value-label`, positioned near the mark (mirrors the existing
  hover-tooltip pattern in `fitsmith/ui/file_list.slint:33-39`).
- **Theme-aware:** derive all colors from `Palette.color-scheme == ColorScheme.dark` (the
  established pattern in `file_list.slint`) — axis, grid, line, mark, text, tooltip for both
  light and dark. This is the one place the spec explicitly requires theme support.
- **Snapshot geometry:** expose `out property <length> plot-x/plot-y/plot-w/plot-h` bound from
  the plot area's `absolute-position`/size so Rust can crop the window snapshot to the chart.

### 6. Analytics dialog — new [fitsmith/ui/analytics.slint](../fitsmith/ui/analytics.slint)

`AnalyticsDialog inherits DialogCard` (from `fitsmith/ui/dialog.slint`), following the
`ExportDialog` recipe (`fitsmith/ui/export.slint`). Contents:

- A **metric `ComboBox`** (Min / Max / Median / Mean / Max-pixel count / Min-pixel count) →
  `metric-changed(int)` callback.
- The `AnalyticsChart` filling the body; zoom control.
- A footer: **Export PNG**, **Export CSV**, **Close** buttons → callbacks.
- A small line showing `N plotted, M skipped (no DATE-OBS / not mono)`.

### 7. Wire dialog state into the window — [fitsmith/ui/app.slint](../fitsmith/ui/app.slint)

Mirror the export dialog wiring (app.slint:420-515 mount pattern, and the `export-*` props):

- Properties: `show-analytics: bool`, `analytics-points: [ChartPoint]`, `analytics-metric:
  int`, `analytics-zoom: float`, x/y tick models, `analytics-plotted-count`,
  `analytics-skipped-count`, plot-geometry out-props.
- Callbacks: `open-analytics-dialog()`, `analytics-metric-changed(int)`,
  `analytics-export-png()`, `analytics-export-csv()`, `analytics-set-zoom(float)`,
  `close-analytics()`.
- Mount `if root.show-analytics: AnalyticsDialog { ... }` at the end of the window with `<=>`
  bindings, exactly like the other dialogs.
- Add a **Tools ▸ Analytics…** menu item (menu block around app.slint:225-240) and/or a
  toolbar button to invoke `open-analytics-dialog`.

### 8. Controller — new [fitsmith/src/controller/analytics.rs](../fitsmith/src/controller/analytics.rs)

`pub use`-d from `fitsmith/src/controller/mod.rs`. Holds analytics state in `AppState` (the
collected `Vec<FileMetrics>` + current `Metric` + `zoom`). Functions:

- `open_analytics_dialog(app)` — gather targets via the existing
  `operation_targets(is_fits_path)` (mod.rs:424 — checked rows, else all), then **spawn a
  worker thread** that runs `analytics::analyze_file` over each (per-file progress marshaled
  back like `spawn_export`), collects `Vec<FileMetrics>` + skipped count, stores it, plots the
  default metric, and shows the dialog. Reuse the generation guard so a stale batch is dropped.
- `set_metric(app, idx)` — `build_series` from the cached metrics (no re-read) → recompute
  normalized `ChartPoint`s + axis ticks → set Slint props. Pure, instant.
- `set_zoom(app, z)` — clamp to [1.0, 4.0], update prop.
- `export_png(app)` — `rfd::FileDialog::new().add_filter("PNG",&["png"]).set_file_name(...)
  .save_file()` (new save-file pattern; rfd 0.15 supports it), then `app.window()
  .take_snapshot()` → **crop** to the chart's `plot-x/y/w/h` (accounting for
  `window().scale_factor()` for HiDPI) → encode with `image::codecs::png::PngEncoder`
  (the same encoder `fitz-core/src/export.rs::write_png` uses; call it or a small local copy
  over the cropped `Rgba8`→`Rgb8` buffer).
- `export_csv(app)` — `save_file()` with a `.csv` filter, then `analytics::write_csv(series,
  BufWriter::new(File::create(path)?))`.
- `close_analytics(app)` — hide dialog, clear cached metrics.

A pure **normalization helper** (`Series` → `Vec<ChartPoint>` in 0..1 + tick lists) lives here
(or in a small `fitsmith/src/chart.rs` presentation module, mirroring `src/view.rs`
"data in → Slint props out"). Compute nice axis ticks (min/max of times & values, a handful of
evenly-spaced labels; format time as `HH:MM` and values with the existing `format_stat` style
from `view.rs:96`).

### 9. Bridge wiring — [fitsmith/src/main.rs](../fitsmith/src/main.rs)

Add the new callbacks to the `forward!` block (main.rs:49-105), each forwarding to a
`controller::*` function, matching how `on_open_export_dialog` / `on_run_export` are wired
(main.rs:95-101).

---

## Files touched (summary)

New: `fitz-core/src/analytics.rs`, `fitsmith/ui/chart.slint`, `fitsmith/ui/analytics.slint`,
`fitsmith/src/controller/analytics.rs`.
Modified: `fitz-core/src/info.rs` (+min/max counts, `parse_date_obs`), `fitz-core/src/lib.rs`,
`fitsmith/ui/types.slint`, `fitsmith/ui/app.slint`, `fitsmith/src/controller/mod.rs`,
`fitsmith/src/main.rs`. Docs: extend `readme.md` if any CLI surface is added (none planned in
phase 1 — GUI only).

---

## Verification

1. **Unit tests (`cargo test --workspace`)** — new tests for:
   - `parse_date_obs`: valid/invalid strings, fractional seconds, ordering of two parsed times.
   - `PixelStats` min/max counts on a synthesized known image (assert `min_count`, `max_count`,
     and the min==max degenerate case; assert median/mean still match the previous fold+sort
     path on the bundled fixtures so the single-pass rewrite is behavior-preserving).
   - `analytics::analyze_file` on bundled `test-data/` (a file lacking `DATE-OBS` → skipped; a
     debayered/RGB frame → skipped as not-mono; a synthesized mono file with a known
     `DATE-OBS` → correct time + metrics).
   - `build_series` sorts by time and reports skipped; `write_csv` output shape (header + rows).
   Confirm no existing tests break (the `PixelStats` field addition touches fixtures).
2. **Clippy/fmt** — `cargo clippy --workspace --all-targets` and `cargo fmt`.
3. **End-to-end GUI run** — `cargo run -p fitsmith`; open several FITS frames from `test-data/`,
   check a few, open **Tools ▸ Analytics…**, and verify: line + marks render; the metric
   dropdown switches instantly; hovering a mark shows the X (time) / Y (value) tooltip; zoom
   slider goes fit→4× and scrolls horizontally; **Export PNG** writes a cropped chart image that
   opens correctly; **Export CSV** writes rows matching the plotted points; the skipped-count
   line is correct for a frame with no `DATE-OBS`. Repeat with the OS in **light** and **dark**
   mode to confirm both themes render legibly.

---

## Phasing

1. fitz-core: `PixelStats` counts + `parse_date_obs` + `analytics.rs` + tests (no UI).
2. Slint `chart.slint` + `analytics.slint` + `types.slint`/`app.slint` wiring (static/mock data).
3. `controller/analytics.rs`: gather + off-thread compute + plot + metric switch + zoom.
4. PNG (snapshot+crop) and CSV export.
5. Theme pass (light/dark) + polish (ticks, tooltip placement) + end-to-end verification.
