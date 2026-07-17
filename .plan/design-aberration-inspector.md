# Design: Aberration Inspector

Target: fitz v0.2.0 · Crates touched: `libfitz` (new crop helper), `fitsmith` (dialog,
controller, Slint UI)

## 1. Feature summary

A read-only inspection dialog, in the spirit of Siril's and ASTAP's aberration inspectors,
that lets the user judge corner-to-corner focus and optical aberration (coma, astigmatism,
tilt) at a glance. It shows a 3×3 mosaic of crops sampled from nine fixed regions of the
selected frame — four corners, four edge-midpoints, and the center — rendered with the same
debayer/stretch pipeline the main preview uses, so stars in the corners can be compared with
stars in the center without panning and zooming around a full-resolution image.

Invocation: **Tools ▸ Aberration Inspector…**. The dialog is modal, has a single **Close**
button, and operates on the *currently selected* frame only (not the checked working set —
this is a per-frame inspection, unlike Export/Analytics which are batch operations).

## 2. Behaviour specification

- **Tile size.** Each of the nine crops is `SZ × SZ` pixels, where
  `SZ = min(round(0.10 * min(width, height)), 256)`. Using `min(width, height)` (rather than
  each axis independently) keeps every tile square and keeps the 10% rule meaningful for
  non-square sensors. `SZ` is computed on the frame's *native* pixel dimensions.
- **Region placement.** Nine `SZ × SZ` source rectangles laid over the frame:
  - Corners: flush to each corner (top-left at `(0,0)`, top-right at `(W−SZ, 0)`, etc.).
  - Edge midpoints: centered along each side (e.g. top-edge at `x = (W−SZ)/2, y = 0`).
  - Center: centered on the frame (`x = (W−SZ)/2, y = (H−SZ)/2`).
  All rectangles are clamped to stay within `[0, W−SZ] × [0, H−SZ]`; for a frame smaller
  than `SZ` in some axis (pathological, only if the 256 cap and 10% both exceed the frame),
  the whole frame is used and tiles overlap — acceptable and still informative.
- **Rendering.** Each tile is cropped from the *rendered* preview so that debayering and
  auto-stretch are already applied and the tiles look exactly like the main view. Because that
  preview is rendered at the frame's native resolution (see §3), cropping it *is* cropping
  full-resolution pixels — corner detail is preserved. Tiles are shown at 1:1 (no scaling),
  separated by a thin gutter, with a subtle border so the seams between regions are visible.
  **No labels.**
- **CFA and RGB frames.** Both are handled with no special-casing: the preview buffer is
  already the debayer/stretch pipeline's output, so a raw CFA mosaic (demosaiced), an
  already-debayered RGB cube, and a mono frame all arrive here as the same interleaved RGBA8.
  The inspector only ever crops that buffer and never sees the FITS layout.
- **Static snapshot.** The tiles reflect the debayer/stretch state at the moment the dialog
  opens and **do not live-update** if those toggles change behind it. The dialog is modal, so
  the toggles can't be reached while it is open anyway.
- **Controls.** Close button only. No zoom, no toggles inside the dialog — the debayer and
  stretch state is inherited from the main window's View menu at the moment the dialog opens.
- **Empty/edge cases.** Menu item is disabled when no frame is selected. If the selected
  frame is an already-debayered RGB cube it still renders fine (the preview path handles it);
  no special-casing needed.

## 3. Where this fits in the current code

The preview pipeline already produces exactly the buffer we need to crop from:

- `libfitz::preview::PreviewImage` — interleaved RGBA8, `width * height * 4` bytes
  (`fitsmith/src/image.rs:12` confirms the invariant). This is what the main `image_view`
  displays via `preview_to_image`.
- A freshly selected frame's `PreviewImage` already lives on the cached
  `LoadedDoc.preview` (`fitsmith/src/doc.rs:60`), reachable through the controller's LRU
  cache keyed by path (`controller/mod.rs:61`). So for the currently selected frame we can
  crop tiles **without any re-read or re-render**.

The dialog therefore does not need a worker thread in the common case: the selected frame's
preview is already resident. Crucially, `render_preview` produces the buffer at the frame's
**native** pixel dimensions — `fitsmith/src/controller/viewer.rs:147` renders it and
`view.rs:15-17` hands the native `width`/`height` to Slint, which only *display*-scales it to
the window (`image-fit`). So the resident `LoadedDoc.preview` is full-resolution; cropping it
loses no corner detail and needs no re-read of the file. "Crop from the full-res image" and
"crop from the already-shown image" are the same operation on the same buffer.

## 4. Proposed design

### 4.1 Cropping helper (`libfitz`)

Add a small, pure, unit-testable helper next to `resize.rs` (or in `preview.rs`). It has no
FITS or Slint dependency — it operates on RGBA8 buffers:

```rust
// libfitz::preview (or a new libfitz::inspect module)

/// A square crop taken from a rendered RGBA8 image.
pub struct Tile {
    pub size: usize,          // SZ (side length in px)
    pub rgba8: Vec<u8>,       // size*size*4
}

/// The nine aberration-inspector regions of a `width × height` image, in
/// row-major order (TL, TC, TR, ML, C, MR, BL, BC, BR). Each origin is clamped
/// so the SZ×SZ rect stays inside the frame.
pub fn aberration_tile_size(width: usize, height: usize) -> usize;
pub fn aberration_regions(width: usize, height: usize, sz: usize) -> [(usize, usize); 9];

/// Copy one SZ×SZ tile out of an interleaved RGBA8 buffer.
pub fn crop_rgba8(src: &[u8], width: usize, height: usize,
                  x: usize, y: usize, sz: usize) -> Tile;
```

Keeping `aberration_tile_size` / `aberration_regions` as separate pure functions makes the
geometry (the part most likely to have off-by-one bugs) trivially testable with tiny
synthetic dimensions, independent of any pixel data.

Rationale for putting this in `libfitz`: the CLAUDE.md contract is that `libfitz` owns image
computation and `fitsmith` owns presentation. Cropping and region math is computation; a
future CLI `fitz inspect` subcommand (out of scope now) could reuse it.

### 4.2 Controller (`fitsmith`)

Add `controller::open_aberration_dialog(app)` (new file `controller/inspect.rs`, or fold
into `viewer.rs` since it is about the selected frame):

1. Read the selected index from `STATE`; if none, no-op (menu already gates this).
2. Get the selected frame's `Rc<LoadedDoc>` from the cache. It is essentially always present
   (the selected frame is what is on screen). If for some reason it is not cached, fall back
   to spawning the same off-thread load `viewer.rs` uses, then open the dialog on completion.
3. Compute `SZ` and the nine regions from `doc.preview.{width,height}`.
4. Crop nine `Tile`s and convert each to a Slint `Image` via a helper mirroring
   `image::preview_to_image` (extend `image.rs` with `tile_to_image(&Tile) -> Image`).
5. Push the nine images into a `VecModel<Image>` bound to the dialog, set
   `aberration_visible = true`.

Wire a `close-aberration` callback that clears the model and hides the dialog, following the
existing `close-analytics` pattern in `main.rs`.

### 4.3 UI (`fitsmith/ui`)

- New `ui/aberration.slint` exporting `AberrationDialog`, built on the existing `DialogCard`
  chrome (`ui/dialog.slint`) — the same scrim + centered card used by every other dialog,
  giving click-outside / Close dismissal for free. Card sized to the 3×3 grid plus padding
  and the Close button.
- The grid: a `GridLayout` (or nested `VerticalLayout`/`HorizontalLayout`) of 9 `Image`
  elements with `image-fit: fill` at `SZ × SZ`, thin gutters, and a 1px border per tile.
  The model is a `[image]` property (`in property <[image]> aberration-tiles;`) indexed 0..8.
- In `app.slint`: a new **Tools ▸ Aberration Inspector…** `MenuItem`
  (`enabled: root.selected-index >= 0;`) invoking a new `open-aberration-dialog()` callback,
  plus an `if root.aberration-visible: AberrationDialog { … }` instance alongside the other
  dialog instances, and a `close-aberration()` callback.
- `main.rs`: `forward!(on_open_aberration_dialog, …)` and `forward!(on_close_aberration, …)`
  following the existing macro pattern.

## 5. Data flow

```
Tools ▸ Aberration Inspector…
  └─ open_aberration_dialog(app)
       ├─ selected LoadedDoc (from LRU cache — already resident)
       ├─ SZ = aberration_tile_size(preview.w, preview.h)
       ├─ regions = aberration_regions(preview.w, preview.h, SZ)
       ├─ for each region: crop_rgba8(preview.rgba8, …) → Tile → tile_to_image
       └─ set AppWindow.aberration-tiles + aberration-visible = true
Close → close_aberration(app): clear model, aberration-visible = false
```

## 6. Testing

- `libfitz`: unit-test `aberration_tile_size` (10% rule, 256 cap, `min` of axes, tiny
  frames) and `aberration_regions` (all nine origins clamped inside the frame, center/edge
  math, odd dimensions) with synthetic sizes. Unit-test `crop_rgba8` on a small hand-built
  RGBA8 buffer (e.g. a 4×4 with known bytes) to confirm the correct sub-rectangle and stride
  handling.
- `fitsmith`: the controller step is thin and Slint-bound; keep logic in the tested
  `libfitz` helpers. A `#[cfg(test)]` check that nine tiles are produced for a real
  `test-data/uncompressed.fit` preview is reasonable.

## 7. Resolved decisions

1. **Crop from the preview or from full-res pixels?** **Crop from the full-resolution image —
   which is the already-shown preview buffer.** The concern that a preview might be downscaled
   does not apply here: `render_preview` renders at native resolution and only Slint's
   `image-fit` scales the *display* (verified at `controller/viewer.rs:147` and
   `view.rs:15-17`). So the resident `LoadedDoc.preview` already carries full-resolution,
   debayered, stretched pixels, and the inspector simply crops it — no re-read, no worker
   thread, no separate native-pixel path.
2. **Tile labels?** **None.** The 3×3 arrangement is self-evident.
3. **Live update while dialog is open?** **No — a static snapshot.** Tiles reflect the
   debayer/stretch state at open time. The dialog is modal, so the toggles can't be changed
   behind it; no staleness in practice.
4. **CFA vs. RGB frames.** Handled uniformly: the crop source is the pipeline's RGBA8 output,
   which already resolved CFA demosaicing / RGB-cube reinterleaving / mono replication. The
   inspector needs no branch on frame type.
