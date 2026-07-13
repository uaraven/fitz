# QuickFit — Implementation Plan

A GUI frontend (`quickfit`) for the `fitz` toolset: a desktop app built with
[Slint] (Fluent widget style) that reuses `fitz-core` for every FITS operation.
This plan follows the project's stated split: **`fitz-core` owns all FITS/image
logic; the binary owns UI/UX only.** Any new operation on FITS files goes into
`fitz-core`, not the GUI.

The good news from surveying the code: `fitz-core`'s API is already GUI-friendly.
Most of what QuickFit needs (`find_image_hdu`, `load_rgb`, `load_mono_raw`,
`auto_stretch`, `pixel_stats`/`histogram`, `resize_rgb`, `compress`/`decompress`,
`copy_missing_metadata`, `write_rgb16_fits`) is already `pub` and operates on
in-memory `(Header, ImageData)`. The core additions below are small.

**Why Slint fits this project.** Slint is pure-Rust and declarative: no C++
toolchain, no external Qt/GTK runtime dependency, `cargo build` stays the single
entry point, and `cargo test --workspace` keeps working everywhere. Its image
type takes an RGBA8 buffer directly from Rust — a perfect match for the display
buffer `fitz-core` will produce. Cross-platform (Linux/macOS/Windows) via its
winit + femtovg/skia/software renderers. The Fluent style is a built-in,
selectable widget theme.

---

## 1. Scope & non-goals

**In scope (from the spec):** file list operating on many files; image tab with
zoom (fit / 1:1 / in-out); FITS header tab; toolbar + menu toggles for *debayer
raw*, *stretch preview*, *show stats*; raw-vs-debayered preview; blink;
compress/decompress; copy/paste header; **export the displayed image as
TIFF/PNG/JPG**; about. (Save / Save As are dropped for now — see §4.3.)

**Non-goals (v1):** editing pixel data, plate solving, stacking, calibration.
Keep parity with `fitz-cli` plus the interactive niceties (blink, live toggles).

---

## 2. Workspace & crate setup

Add a third workspace member. In the root `Cargo.toml`:

```toml
members = ["fitz-core", "fitz-cli", "quickfit"]
```

New crate `quickfit/` (binary), depending on `fitz-core` via path, mirroring how
`fitz-cli` depends on it. Key deps:

- `slint` — the UI runtime. **Renderer: default GPU backend (femtovg/skia) with
  the software renderer as a documented fallback** (`SLINT_BACKEND=winit-software`
  / the `renderer-software` feature) for headless CI and GPU-less machines.
- `slint-build` (build-dependency) — compiles `.slint` files in `build.rs`.
- `rfd` — native Open / Export file dialogs (Slint ships no file-picker widget).
- `fitz-core` (path dep) — all FITS work, incl. the new image export (§4.4).
- `rayon`, `anyhow` — already used across the workspace.

**Fluent style** is selected once, via the `.slint` compile config in `build.rs`
(`slint_build::compile_with_config(..).with_style("fluent")`), so it's baked into
the build rather than relying on the `SLINT_STYLE` env var or a `Cargo.toml`
metadata key at runtime.

**Release profile:** the root `[profile.release]` uses `opt-level = 'z'`, `lto`,
`codegen-units = 1`, `strip`. Slint is pure Rust and links cleanly under these.
Add a `[profile.release.package.quickfit]` `opt-level = 2` override only if the
size-opt hurts UI responsiveness — decide empirically. Unlike the previous Qt
plan, there is no cross-language LTO fragility to worry about.

**No external runtime prerequisite.** Slint bundles its renderer; there is no
Qt6/GTK install step. A GPU-less/headless CI can use the software renderer
(`SLINT_BACKEND=winit-software` or the `renderer-software` feature). On Linux,
document the usual winit build deps (e.g. `libxkbcommon`, Wayland/X11 dev
headers) — far lighter than a Qt SDK.

---

## 3. Architecture

Three layers, matching the existing CLI's discipline (no UI code in core):

```
 fitz-core  (no UI)                pure FITS/image functions + a new `preview` module
     ▲
     │ path dep
 quickfit/src  (Rust)              app state, models (VecModel/ModelRc), async loader,
     ▲                             callback handlers wiring the .slint UI to fitz-core
     │ slint-build code-gen
 quickfit/ui/*.slint  (Slint)      declarative views: menu bar, toolbar, split panels,
                                    tab view, image w/ zoom, header table, stats panel
```

Slint is Rust-native, so there is no FFI bridge: `slint-build` generates a Rust
struct for the root component with typed getters/setters for properties and
callbacks. The Rust side owns state and calls into `fitz-core`; `.slint` markup
owns layout and styling.

---

## 4. `fitz-core` additions (small, and the only core work)

Everything else already exists. Add one module and one convenience path so both
the CLI preview and the GUI share the pixel pipeline (avoiding duplication, per
the project rules).

### 4.1 `fitz-core::preview` — display-ready RGBA from in-memory data

Today the render pipeline lives in `fitz-cli/src/preview.rs`
(`load_preview_pixels`) and reads from a `&Path` every time. The GUI must load a
file **once** and then re-render on toggle changes (debayer on/off, stretch
on/off) without touching disk. Add a pure function operating on already-loaded
data:

```rust
// fitz-core/src/preview.rs
pub struct PreviewParams {
    pub debayer: bool,          // false => raw mosaic shown as grayscale (load_mono_raw)
    pub stretch: bool,          // false => linear rgb16_to_rgb8
    pub pattern: Option<CFA>,
    pub force_demosaic: bool,
    pub brightness: f32,
    pub linked: bool,
}

pub struct PreviewImage {
    pub width: usize,
    pub height: usize,
    pub rgba8: Vec<u8>,         // interleaved RGBA, one byte per channel
    pub notice: LoadRgbNotice,  // so the GUI can badge "already debayered", etc.
}

/// Render an in-memory image to a display buffer. No I/O, no path.
pub fn render_preview(header: &Header, img: &ImageData, p: &PreviewParams)
    -> Result<PreviewImage>;
```

Internally this composes existing pieces — `load_rgb` / `load_mono_raw`,
`auto_stretch`, `rgb16_to_rgb8` — plus an RGB→RGBA widen. **RGBA8** is exactly
what Slint's `SharedPixelBuffer<Rgba8Pixel>` / `Image::from_rgba8` consumes, so
the GUI wraps it with zero conversion. Then **refactor
`fitz-cli/src/preview.rs::load_preview_pixels` to call this** so the two
frontends can't drift (the CLI keeps its own ANSI/kitty rendering, which stays
UI-specific).

### 4.2 Confirm reuse (no new code needed)

| GUI need | Existing `fitz-core` entry point |
|---|---|
| Open file, get image | `FitsFile::from_file` + `find_image_hdu` (transparent `.fz` decompress) |
| Header keyword list | `HeaderInfo.header` → `Header::iter()` (`Keyword { name, value, comment }`) |
| Image stats + histogram | `info::pixel_stats(header, img)` / `info::histogram` (in-memory) |
| Structured metadata (RA/DEC, exp, gain…) | `info::header_info` / `HeaderInfo` fields |
| Debayer to file | `debayer::debayer` + `write_rgb16_fits`/`write_rgb16_tiff`/`encode_rgb_as_source` |
| Stretch to file | `stretch::load_and_stretch` + `write_rgb16_fits` |
| Compress / decompress | `compress::compress` / `decompress::decompress` → `FitsFile::to_file` |
| Copy header → paste onto another | store a `Header`; `copy_missing_metadata(dest, &src, CFA_KEYWORDS)` then write |
| Thumbnail / zoom resize | `resize::resize_rgb` / `resize_to_fit` (only if we downscale before display) |

### 4.3 `fitz-core::export` — write the displayed image as TIFF/PNG/JPG

Replaces Save/Save As for now: export the **currently displayed processed image**
(debayer/stretch toggles applied — i.e. the `PreviewImage`) to a standard raster
format. This is a new operation on image data, so it lives in `fitz-core`, not the
GUI.

```rust
// fitz-core/src/export.rs
pub enum ExportFormat { Tiff, Png, Jpeg { quality: u8 } }

/// Encode an interleaved RGBA8 display buffer to `output` in the given format.
pub fn export_rgba8(output: &Path, width: usize, height: usize,
                    rgba8: &[u8], format: ExportFormat) -> Result<()>;
```

- **PNG / JPEG** are 8-bit; encode straight from the `PreviewImage.rgba8` buffer
  (JPEG drops alpha). Needs one new dependency. Recommend the **`image`** crate
  (one dep covers PNG + JPEG, and TIFF too) unless we'd rather stay lean with
  `png` + `jpeg-encoder`. Decide during implementation; `image` is the pragmatic
  low-effort pick and matches the readme's stated philosophy.
- **TIFF:** the existing `fits_image::write_rgb16_tiff` already writes **16-bit**
  RGB TIFF. Prefer routing TIFF export through it (from the stretched `u16`
  buffer) so exports keep full precision, rather than an 8-bit re-encode. That
  means the `Document` should retain the stretched `u16` RGB alongside the
  display `rgba8` (cheap; already produced by `render_preview`'s internals) — or
  `render_preview` optionally returns both. Confirm bit-depth expectations in
  §11.

The GUI's Export flow: `rfd` save dialog (format inferred from the chosen
extension or a format dropdown) → `export::export_rgba8` (or `write_rgb16_tiff`
for TIFF). Batch export over the selected files is a natural follow-up but not
required for v1.

### 4.4 Possible small helper: in-memory copy-header

`copy_header::copy_header(source, target)` is file→file. The GUI's *Copy header /
Paste header* is clipboard-style (copy from selection into app state, paste onto
one or many targets). The primitive `copy_missing_metadata` already supports this;
optionally add a thin `copy_header::apply_header(target: &mut FitsFile, src:
&Header)` so the GUI doesn't reach into HDU internals. Decide during
implementation — not a blocker.

---

## 5. Rust GUI layer (`quickfit/src/`)

Slint generates a root component struct (e.g. `AppWindow`) from the `.slint`
files. The Rust `main` creates it, seeds properties/models, wires callbacks to
`fitz-core`, and runs the event loop. State lives in Rust, exposed to the UI
through properties, callbacks, and models.

- **`AppController`** — owns application state and holds a `slint::Weak<AppWindow>`
  to push updates. State: the working set of files, current selection, toggles
  (`debayer_enabled`, `stretch_enabled`, `show_stats`), zoom mode
  (`Fit`/`OneToOne`/factor), the copied `Header` stash, and an LRU cache of
  decoded documents.
- **`Document`** — a loaded file's state: owned `FitsFile` (or `Header` +
  `ImageData`), cached `PreviewImage`, cached `HeaderInfo`/`PixelStats`. Re-renders
  via `render_preview` when a toggle flips — cheap, no disk read.
- **Models (`ModelRc` / `VecModel`)**:
  - File list → `VecModel<FileRow>` (struct: name, path, status
    loaded/error/compressed) bound to a `ListView`.
  - Header table → `VecModel<HeaderRow>` (keyword / value / comment) bound to a
    `StandardTableView`.
  - Histogram → a `VecModel<f32>` (256 normalized bar heights) drawn in the stats
    panel.
- **Image feed** — build a `SharedPixelBuffer::clone_from_slice(&preview.rgba8)`,
  wrap with `slint::Image::from_rgba8`, and set the window's `image` property.
  Slint scales/pans it in the UI; no image provider or custom paint item needed.
- **Callbacks** (defined in `.slint`, implemented in Rust): `open`,
  `open_directory`, `save`, `save_as`, `compress`, `decompress`, `copy_header`,
  `paste_header`, `select_file(index)`, `set_zoom`, `toggle_*`, `start_blink` /
  `stop_blink`, `about`.

Toggles are single-source-of-truth properties on the root component, so the menu
check-items and the toolbar check-buttons bind to the **same** property (the spec
wants menu and toolbar mirrored).

---

## 6. Slint UI (`quickfit/ui/`)

Compose the spec's layout from `std-widgets` under the Fluent style:

- **`Window` with a `MenuBar`** — Slint's native menu bar (`MenuBar`/`Menu`/
  `MenuItem`) for File / Edit / View / Help, with checkable items for the three
  View toggles. File is **Open… / Open directory… / Export… / Exit** (Save / Save
  As omitted for now).
- **Toolbar** — a `HorizontalBox` of `CheckBox`es (*Debayer raw*, *Stretch
  preview*, *Show image stats*), bound to the same properties as the View menu.
- **Two-panel split** — Slint has **no built-in SplitView/splitter widget**, so
  build a resizable split: a `HorizontalLayout` with left `ListView`, a draggable
  separator (a thin `Rectangle` with a `TouchArea` updating a `panel_width`
  property), and the right content. (v1 fallback: a fixed-width left panel if the
  draggable handle is fiddly.)
- **Right side: `TabWidget`** with two tabs — **Image** and **Headers**.
  - **Image tab:** a `Flickable` (pan) wrapping an `Image`. Zoom = scale the
    image; **Fit** derives scale from viewport vs. image size, **1:1** sets 1.0,
    **+/-** and pointer-wheel multiply. When *Show stats* is on, a stats panel
    docks below and the image area shrinks to fit (spec requirement) via the
    layout.
  - **Headers tab:** a `StandardTableView` over the header-rows model (columns:
    keyword / value / comment).
- **Stats panel** — min/max/mean/median/zeros text + a histogram drawn with a
  `Repeater` of `Rectangle` bars (or a `Path`).
- **About** — a small dialog/popup for Help ▸ About.

Blink = a Slint `Timer` advancing the selection index across the working set at a
set interval; because re-render is in-memory and cached per document, blink stays
responsive.

---

## 7. Threading / async model (important)

FITS load + debayer + stretch is CPU-heavy (multi-MB frames, rayon-parallel) and
**must not run on the Slint UI/event-loop thread**. Pattern:

1. On open/selection, spawn the work on a `std::thread` (or the rayon pool):
   `from_file` + `find_image_hdu` + `render_preview`.
2. Marshal the finished `PreviewImage`/`HeaderInfo` back to the UI thread with
   `slint::Weak::upgrade_in_event_loop(move |app| { … set properties … })` (or
   `slint::invoke_from_event_loop`). This is Slint's equivalent of a thread queue.
3. Show a busy indicator (`Spinner`/`ProgressIndicator`) while in flight; coalesce
   rapid selection changes (blink, arrow-key scrubbing) so only the latest render
   wins (track a generation counter; drop stale results).

Cache decoded documents (LRU by path) so re-selecting and blinking don't re-read
disk. Toggle changes re-run only `render_preview` on the cached `(header, img)` —
no reload.

---

## 8. Feature → implementation map

| Menu / control | Rust action | `fitz-core` call |
|---|---|---|
| File ▸ Open… / Open directory… | `rfd` picker → add to file model, async-load | `FitsFile::from_file`, `find_image_hdu` |
| File ▸ Export… (TIFF/PNG/JPG) | `rfd` save dialog → encode displayed image | `export::export_rgba8` / `write_rgb16_tiff` |
| Edit ▸ Compress | `compress` current → write `.fz` | `compress::compress` |
| Edit ▸ Decompress | `decompress` current → write | `decompress::decompress` |
| Edit ▸ Copy header | stash current `Header` in `AppController` | `header.clone()` |
| Edit ▸ Paste header | apply stash onto selected file(s), write | `copy_missing_metadata` / new `apply_header` |
| View ▸ Debayer raw ☑ | flip property, re-render | `render_preview` (debayer flag) |
| View ▸ Stretch preview ☑ | flip property, re-render | `render_preview` (stretch flag) |
| View ▸ Show image stats ☑ | flip property, compute if needed | `info::pixel_stats` |
| Image zoom Fit / 1:1 / +/- | Slint view transform only | — |
| Blink | timer advances selection | cached `render_preview` |
| Headers tab | populate table model | `Header::iter()` |
| Help ▸ About | static dialog | — |

**Export** writes the currently displayed processed image (debayer/stretch
applied). TIFF keeps 16-bit precision via `write_rgb16_tiff`; PNG/JPG are 8-bit
from the display buffer. See §4.3 and the bit-depth question in §11.

---

## 9. Testing

- **All FITS logic tested in `fitz-core`** on real bundled fixtures
  (`test-data/`), consistent with the current suite. `render_preview` gets unit
  tests: debayer on/off, stretch on/off, already-debayered cube, mono raw, RGBA
  output length = `w*h*4`, and that the CLI-refactor path produces identical bytes
  to the old `load_preview_pixels` (regression guard).
- **`cargo test --workspace` works everywhere** — Slint is pure Rust with no
  external SDK, so no special CI setup is needed beyond winit's Linux dev headers.
  Pure-Rust GUI logic (model row mapping, zoom math, LRU cache, blink index
  stepping, stale-result coalescing) is unit-tested without running the event
  loop.
- **GUI smoke test** (manual/CI-optional, software renderer): launch, open
  `test-data/`, toggle each control, blink, view headers/stats, compress +
  decompress round-trip.
- Run `cargo clippy --workspace --all-targets` and `cargo fmt`; update `readme.md`
  only if CLI behavior changes (the shared preview refactor must keep CLI output
  identical).

---

## 10. Milestones (incremental, each independently verifiable)

1. **Core preview module.** Add `fitz-core::preview::render_preview`, refactor
   `fitz-cli` onto it, tests green. *No UI yet — de-risks the reusable half.*
2. **Walking skeleton.** New `quickfit` crate, `slint-build` with the Fluent
   style, a `Window` that shows a hardcoded RGBA image via `Image::from_rgba8`.
   Trivial with Slint (no external toolchain), so this milestone is mostly
   scaffolding.
3. **Open + display.** File open via `rfd`, async load, Image tab with Flickable;
   Fit / 1:1 / zoom. Debayer & stretch toggles (menu + toolbar bound to one
   property).
4. **File list + selection + blink.** File-rows model, Open directory, selection
   drives the image, blink timer, LRU cache, stale-result coalescing.
5. **Headers + stats.** `StandardTableView` header tab; stats panel with
   histogram; Show-stats toggle resizes the image view.
6. **Edit + Export ops.** Compress / Decompress / Copy header / Paste header;
   Export… to TIFF/PNG/JPG (add `fitz-core::export`).
7. **Polish & packaging.** About, per-file error surfacing (mirror the CLI's
   per-file non-fatal error model), busy indicators, per-OS bundling (much simpler
   than Qt — a single self-contained binary; `cargo-bundle`/`cargo-packager` for
   `.app`/`.msi`/AppImage if desired).

---

## 11. Open questions (confirm before/early in build)

1. **Slint widget gaps.** No native SplitView (custom draggable separator) and no
   native file dialog (use `rfd`). Confirm the custom splitter is worth it for v1
   vs. a fixed-width left panel. Also confirm `StandardTableView` meets the header
   tab's needs (sorting/copy) or whether a `ListView` of rows suffices.
2. **Export details:** TIFF at 16-bit (via `write_rgb16_tiff`) vs uniform 8-bit
   across all three formats; JPEG quality default; and which image-encoding
   dependency (`image` crate vs `png` + `jpeg-encoder`). Also: export only the
   displayed image (assumed) vs. batch-export the selected files.
3. **Renderer:** decided — default GPU (femtovg/skia) with the software renderer
   as a documented fallback for headless CI / GPU-less machines.
4. **Workspace membership:** `quickfit` in default `members` — safe here since
   Slint needs no external SDK, so `cargo build`/`cargo test --workspace` still
   run on a bare machine (plus winit's Linux headers).
5. **Crate name:** spec calls it *QuickFit*; suggest crate `quickfit`
   (binary `quickfit`). Confirm vs. `fitz-gui`.

[Slint]: https://slint.dev/
