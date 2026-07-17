# Design: Cache calculated statistics across re-renders

Target: fitz v0.2.0 · Crates touched: `fitsmith` (cache + controller), no `libfitz` change.

## 1. Problem statement

The spec asks: "Cache calculated stats, so that there is no need to recalculate them unless
the selection changes."

Statistics *are* already cached — but too coarsely, so they are thrown away and recomputed in
cases where the selection has not changed and the stats could not possibly differ.

### Current state

- A `LoadedDoc` bundles `preview + headers + info + stats` and is cached as one unit in a
  byte-budgeted LRU keyed by `PathBuf` (`fitsmith/src/doc.rs:60`, `controller/mod.rs:61`).
  So re-selecting or blinking back to a file already reuses its stats — good.
- But **toggling debayer or stretch clears the entire cache**: `controller::rerender`
  (`viewer.rs:100-110`) calls `st.cache.clear()` and re-selects, forcing a full re-decode
  *and* full stats + star-detection recomputation for the current frame.

The pixel statistics and star metrics are computed from the **raw scaled pixels and the
header** (`pixel_stats(header, img)` and `header_info_from(..., stars:true)` in
`doc.rs:80-101`). They do **not** depend on the debayer or stretch toggles at all — those
toggles only affect the rendered `preview`. So every debayer/stretch toggle needlessly
recomputes stats and re-runs star detection (the expensive part: a full pass plus flood-fill
per frame).

Star detection is the costliest thing the load path does, so recomputing it on a stretch
toggle is the wart worth removing.

## 2. Goal

Recompute pixel stats + star metrics **only when the underlying pixels change** — i.e. when
the selected *file* changes (or its bytes on disk change). Toggling debayer/stretch should
re-render the preview but **reuse** the already-computed stats. Add the RGB-channel policy
(from the RGB stats design) as an explicit cache key input, since changing that *does* change
the numbers.

## 3. Design

### 3.1 Split the cached unit: preview vs. analysis

Separate the two concerns that currently share one cache entry and one lifetime:

- **`PreviewEntry`** — depends on `(path, PreviewParams)` (debayer, stretch, …). Invalidated
  by toggle changes. This is the heavy pixel buffer that dominates the memory budget.
- **`AnalysisEntry`** (stats + star metrics + info + header cards) — depends on
  `(path, RgbReduction)` only. **Not** invalidated by toggle changes.

Two options for structuring this:

**Option A — two caches (recommended).**
Keep the existing byte-budgeted LRU for previews (rename its value to the preview + its
cost), and add a second, small cache for analysis results keyed by `(PathBuf,
RgbReduction)`. Analysis entries are tiny (a handful of numbers + a 256-bucket histogram), so
this cache can be near-unbounded (or a generous fixed count) and need not participate in the
byte budget. `rerender` then clears only the *preview* cache; the analysis cache survives a
toggle.

**Option B — one cache, decouple invalidation.**
Keep one `LoadedDoc` cache but stop calling `cache.clear()` in `rerender`; instead re-render
just the preview for the current entry and splice it back into the existing `LoadedDoc`,
preserving its stats. This is less code but muddier: `LoadedDoc` becomes partially mutable and
the "one immutable unit" invariant in `doc.rs`'s module doc is lost.

Recommend **Option A**: the two things genuinely have different keys and lifetimes, and a
dedicated analysis cache is what the spec is really asking for.

### 3.2 Load path with split caches

`viewer.rs::load_and_render` currently does read → render preview → `LoadedDoc::build`
(stats + stars) in one shot on the worker. Restructure:

1. On selecting a file, look up the analysis cache by `(path, reduction)`.
   - **Hit:** reuse it; only (re)render the preview if the preview cache misses for
     `(path, params)`.
   - **Miss:** compute analysis on the worker as today.
2. `rerender` (toggle change): clear only the preview cache, keep the analysis cache, and
   re-select — the re-select finds the analysis hit and re-renders just the preview.

This means a debayer/stretch toggle re-runs the decode + preview render (unavoidable — the
pixels are re-projected) but **skips star detection and stats entirely**, which is the
expensive win.

> Note: the decode (`FitsFile::from_file` + `find_image_hdu`) is shared by both preview and
> analysis. On a toggle we still re-decode to re-render. A future optimisation could cache the
> decoded `ImageData` too, but that is a much larger buffer than either output; leave it out
> of scope — the win here is skipping detection, not the read.

### 3.3 Invalidation rules

Invalidate an analysis entry when:

- The file is removed from the working set — reuse the existing `cache.remove(&path)` call
  sites (`remove_selected` at `mod.rs:351`) for the analysis cache too.
- The file is replaced on disk by compress/decompress-in-place (`convert.rs` updates the row
  path) — evict the old path from *both* caches.
- The `RgbReduction` policy changes — the key includes it, so old entries simply stop being
  hit; optionally prune them. (For a non-RGB frame the policy is inert, but keying on it is
  harmless and keeps the rule uniform.)

Do **not** invalidate on debayer/stretch toggles — that is the whole point.

## 4. Concurrency / correctness

- Both caches live in the thread-local `AppState` (Slint is single-threaded; all mutation is
  on the UI thread — see `mod.rs:51`). No locking needed.
- The generation counter (`AppState.generation`) still coalesces stale *loads*; it is
  orthogonal to caching and unchanged.
- A worker that computed analysis inserts into the analysis cache in `finish_load` on the UI
  thread (same place the preview is cached now), so no cross-thread cache access is added.

## 5. Memory accounting

The status-bar memory readout (`update_memory`, `mod.rs:121`) currently reports the preview
cache's resident bytes. Analysis entries are negligible; either leave the readout reporting
previews only (simplest, and previews dominate), or add the analysis cache's small footprint.
Recommend leaving the readout as-is (previews only) and noting it in a comment.

## 6. Testing

- The `LruCache` already has thorough unit tests (`cache.rs`). If a second cache type is a
  plain map (or a count-bounded LRU), add tests for its keying — notably that
  `(path, GreenReduction)` and `(path, LuminanceReduction)` are distinct keys.
- A controller-level test (using `test-data` fixtures) asserting that a simulated
  toggle-change path does not rebuild the analysis entry. This is harder to test without an
  event loop; the cleanest testable seam is a pure `fn analysis_key(path, reduction)` and a
  helper that decides "recompute analysis?" given cache state — unit-test that predicate.

## 7. Interaction with other v0.2.0 work

- **RGB stats** (separate design): supplies the `RgbReduction` cache-key dimension. Land the
  RGB work first (or together) so the analysis key is right from the start.
- **Aberration inspector**: reads the preview buffer, not stats — unaffected, but it benefits
  from the preview cache exactly as today.
- **SIMD** (separate design): speeds up the recompute; caching *avoids* the recompute. They
  are complementary — cache first (removes the biggest redundant cost, star detection on
  every toggle), SIMD second (speeds up the unavoidable first computation).

## 8. Open questions / decisions

1. **Bound the analysis cache?** Entries are tiny; a plain `HashMap<(PathBuf, RgbReduction),
   Rc<Analysis>>` cleared alongside working-set removals is probably enough. If working sets
   can be thousands of frames and every one gets visited, a count-bounded LRU (reuse the
   existing `LruCache` with a per-entry cost of 1 and a capacity of, say, 512) is safer.
2. **Cache the decoded `ImageData` too?** Out of scope (large buffers); revisit only if
   toggle latency on huge frames is still a complaint after detection is removed from the
   toggle path.
