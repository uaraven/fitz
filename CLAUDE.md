# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`fitz` is a Rust CLI utility for FITS (astronomy image) files. It compresses/decompresses
FITS files, debayers mosaic images, auto-stretches them, and splits them into per-channel
files. See `readme.md` for the full user-facing command/option reference.

Note the `readme.md` "AI Warning": this tool is intentionally low-effort, low-risk, and
largely AI-authored — favor pragmatic changes over heavy ceremony.

## Commands

```shell
cargo build                              # debug build (whole workspace)
cargo build --release                    # size-optimized release (opt-level z, LTO, strip)
cargo test --workspace                   # run all tests in both crates
cargo test -p libfitz                  # unit tests in the library
cargo test -p fitz                       # unit tests in the CLI binary
cargo test <name>                        # run tests matching a substring (e.g. cargo test resolve_cfa)
cargo run -p fitz -- <COMMAND> [args]    # e.g. cargo run -p fitz -- debayer --format tiff test-data/uncompressed.fit
```

There is no separate lint step; use `cargo clippy --workspace --all-targets` and `cargo fmt`.

The `edition = "2024"` crates require a recent stable Rust toolchain.

## Architecture

A Cargo **workspace** with two active crates, plus a third parked one:

- **`libfitz`** — the reusable library: FITS I/O (with transparent tile-decompression),
  debayering, auto-stretch, per-channel splitting, pixel statistics, star detection,
  preview rendering, header copying, and image resizing. No CLI parsing, no terminal I/O,
  no interactive prompts, no GUI types.
- **`fitz`** (in `fitz-cli/`) — the thin CLI binary: clap argument parsing, output-path
  derivation, the overwrite-confirmation prompt, `--verbose` progress printing, terminal
  rendering (`preview`/`kitty`/`terminal`), and the `info` report (header summary,
  statistics blocks, histogram). Depends on `libfitz` via a path dependency.
- **`fitsmith`** — the Slint GUI frontend. **Currently parked**: it still builds against
  the removed `fits_image`/`info` modules, so it is out of the workspace `members` list
  (see the comment in the root `Cargo.toml`) and does not compile. `libfitz`'s GUI-only
  `export.rs` and `analytics.rs` were removed with it; both need rewriting against the
  `Image` API when the GUI is ported. Until then, `--workspace` means `libfitz` + `fitz`.

Key deps: **`fitskit`** (FITS read/write/tile-compression), **`bayer`** (demosaicing) and
**`image`** (JPEG/PNG encoding) live in `libfitz`; **`clap`** (arg parsing),
**`terminal_size`**/**`supports-color`**/**`libc`** (terminal capability detection) and
**`base64`** (kitty graphics protocol) live in `fitz-cli`; **`slint`**, **`rfd`** (native
file dialogs) and **`sysinfo`** (cache sizing) live in `fitsmith`. **`tiff`**, **`rayon`**,
and **`anyhow`** are used throughout.

The release profile is split: the workspace builds at `opt-level = "z"` (size) with LTO,
but `libfitz` itself builds at `opt-level = 2` — its tight per-pixel loops dominate
runtime, and the size-optimized setting roughly doubles single-file stretch time (see the
comments in the root `Cargo.toml`).

### `libfitz` layout

Everything is built around one type: **`data::Image`** — an `ImageType` (`Grayscale`,
`CFA(pattern)` or `RGB`), the source `Header`, width/height, and a `PixelBuffer` that is
either `U16` (0..=65535) or `F32` (normalized `[0, 1]`). Commands are methods on it.

- **`data.rs`** — `Image`, `ImageType`, `PixelBuffer`, and the sample conversions
  (`as_u8`/`as_u16`/`as_i16`/…, `interleave_planes`/`deinterleave_planes`, `plane`,
  `round_to_u16`).
- **`fits_file.rs`** — the file boundary: `load_fits` (transparently decompressing a
  `ZIMAGE` HDU, applying BSCALE/BZERO, and classifying the `ImageType`), `save_fits` /
  `image_to_fits`, `export_as_tiff`, the header-only `load_header` -> `ImageMeta` fast path, and
  `find_image_hdu_index`.
- **`stats.rs`** — `Image::stats() -> Vec<Stats>`, one `Stats` per colour channel, computed
  from a single parallel value-count pass (mean/median/sigma/avg-dev/MAD, min/max and their
  counts, mode, zero and saturated counts, estimated bit depth, 256-bin histogram). Also
  hosts the shared selection-based `median_in_place`.
- **`debayer.rs`**, **`stretch.rs`**, **`split_channel.rs`** — one `Image` method each:
  `Image::debayer` (`Option<Result<Image>>`, `None` when there is nothing to demosaic),
  `Image::with_pattern` (the `--pattern`/`--force-demosaic` override), `Image::stretch`,
  `Image::split_channels` -> `[Image; 3]`.
- **`compress.rs`**, **`decompress.rs`**, **`copy_header.rs`** — container-level operations
  that work on `fitskit::FitsFile` directly rather than on `Image`, since they must
  round-trip pixel data and headers untouched.
- **`stars.rs`** — `detection_plane(&Image) -> MonoPlane` (green super-pixel plane for a
  mosaic, green channel for RGB, the frame itself for mono), then detection and shape
  measurement against the plane's own `Background` (threshold, flood-fill blobs, reject
  non-stars, measure HFR/FWHM/eccentricity — HFR/FWHM as medians, eccentricity as a vector
  median of the signed ellipticity components, since a per-star eccentricity is rectified
  and noise can only inflate it).
- **`keywords.rs`** — header keyword policy: which names are structural/reserved, and the
  `copy_metadata`/`copy_missing_metadata`/`carry_over_metadata`/`add_history` helpers.
- **`preview.rs`** — `render_preview`: an `Image` to an `image::DynamicImage`.
- **`inspect.rs`** — aberration-inspector geometry: the nine fixed tile regions of a frame
  and RGBA8 cropping.
- **`resize.rs`** — generic box-filter image resizing (`resize_to_fit`), used by the CLI's
  terminal preview.
- **`errors.rs`** / **`fits_bayer.rs`** — `FitsError`, and `BAYERPAT` string ↔ `CFA`.
- **`test_support.rs`** (test-only) — fixtures: locate bundled `../test-data/`, copy into a
  temp dir, synthesize small FITS images.

### `fitz-cli` layout

- **`main.rs`** — clap `Cli`/`Command` definitions, the `*Args` structs, and `run_*`
  dispatchers that convert args into `libfitz` domain options (composed inside the CLI's own
  `*Options` structs in `options.rs`) and invoke the per-command wrapper. Also owns
  output-path derivation (`output_path` for compress/decompress, `derive_output_path` for
  debayer/stretch) and `process_files`, the batch driver.
- **`options.rs`** — CLI-side option structs (`Options`, `DebayerOptions`, `StretchOptions`,
  `SplitChannelOptions`, …) holding both the command knobs and the CLI-only fields (`yes`,
  `verbose`, `output`, `multi_file`), plus the `OutputFormat` (FITS/TIFF) enum.
- **Per-command wrapper modules** — `compress.rs`, `decompress.rs`, `debayer.rs`, `stretch.rs`,
  `split_channel.rs`, `copy_header.rs`, `info.rs`. Each resolves the output path, calls
  `io_prompt::ensure_can_write`, calls into `libfitz`, prints `--verbose` progress, and
  writes the result. `debayer.rs` also owns `apply_pattern` (the `--pattern`/
  `--force-demosaic` policy) and `source_bitpix`, both reused by `stretch`/`split`.
- **`summary.rs`** — the curated `info` header summary: which keywords are worth showing, in
  what order, and how each is formatted (sexagesimal coordinates, bit-depth and channel
  labels, the telescope's optical figure, `trim_float`).
- **`io_prompt.rs`** — the interactive overwrite-confirmation prompt (`ensure_can_write`),
  the `print_progress`/`print_step` verbose-output helpers, and `print_debayer_notice`.
- **`preview.rs`**, **`kitty.rs`**, **`terminal.rs`** — terminal-only rendering (ANSI
  half-blocks / kitty graphics protocol), capability detection, and the 16→8-bit narrowing
  (`high_byte`/`rgb16_to_rgb8`); not part of `libfitz` since a GUI frontend wouldn't use
  ANSI escape codes.
- **`test_support.rs`** (test-only) — locates bundled `../test-data/` for the CLI's own tests.

### `fitsmith` layout (parked)

A Slint GUI ("FitSmith") over the same library, currently **out of the workspace** and not
compiling — it is written against the removed `libfitz::fits_image`/`libfitz::info` API.
`ui/*.slint` holds the declarative UI (`app.slint` is the window; dialogs/panels are one
file each); `build.rs` compiles it. Rust side, split by concern:

- **`main.rs`** — window setup and callback wiring only; every callback forwards to a
  `controller` function.
- **`controller/`** — application logic bridging Slint to `libfitz`, split into `mod.rs`
  (shared `AppState` thread-local, working set, checkbox selection, batch helpers),
  `viewer.rs` (selection, off-thread load/render, blink), `convert.rs` (compress/decompress
  batches), `export.rs` (export batch), `analytics.rs` (the chart batches and their
  per-file `AnalyticsCache`), `inspect.rs` (aberration inspector). Batches run on worker
  threads, marshal results back via `upgrade_in_event_loop`, are generation-guarded against
  staleness, and are cancellable between files.
- **`doc.rs`** / **`view.rs`** — `LoadedDoc` is the display-ready document built on the
  worker (preview + header cards + stats); `view.rs` maps it onto Slint properties. Both
  are free of threading; `doc.rs` is free of Slint types.
- **`cache.rs`** — a small byte-budgeted LRU keeping rendered previews resident (budgeted
  at 80% of available memory at startup).
- **`chart.rs`** / **`chart_svg.rs`** — analytics `Series` → normalized chart geometry →
  the on-screen chart and its SVG export.
- **`files.rs`** — pure path helpers (FITS extensions, directory scan, output paths).
- **`image.rs`** — the one RGBA8-buffer → `slint::Image` conversion point.

Porting it means rebuilding `libfitz`'s `export.rs` and `analytics.rs` on the `Image` API
(both were deleted with it, and are recoverable from git history), and replacing
`header_info_from` with `load_fits`/`load_header` + `Image::stats()`.

### Conventions that span files

- **Batch processing, per-file errors:** `process_files` runs the command over every input
  path; a failure on one file prints `fitz: <path>: <err>` to stderr and is recorded, but
  does not abort the batch. The process exit code is FAILURE if any file failed.
- **Transparent decompression on read:** `load_fits` in `libfitz`'s `fits_file.rs` is the
  single entry point every read-side command uses. It borrows a plain image HDU but
  decompresses a tile-compressed (`ZIMAGE`) HDU, so every command works on `.fz` inputs with
  no separate decompress step. The compressed HDU's header carries the original keywords
  (BAYERPAT, BSCALE/BZERO, RA/DEC, …), so downstream logic is unchanged. `load_header` is
  the header-only counterpart for callers that never touch pixels (plain `info`).
- **Interleaved in memory, planar on disk:** an `ImageType::RGB` `Image` is always
  interleaved (`R,G,B,R,G,B,…`) — debayering, stretching, statistics and `plane()` all
  assume it. A FITS cube is stored *planar*. `load_fits` interleaves and `image_to_fits`
  de-interleaves; nothing else in the crate may reorder colour samples.
- **Shared "already debayered" detection:** `load_fits` is the single source of truth. A
  3-plane image (`NAXIS3=3`) is `ImageType::RGB`; a 2D image with a `BAYERPAT` header is
  `CFA(pattern)`; a 2D image without one is `Grayscale`. `Image::debayer` returns `None` for
  anything but `CFA`, so a "debayer" of an already-debayered frame is a no-op rather than an
  error. `--pattern`/`--force-demosaic` override the classification via `Image::with_pattern`
  (see `apply_pattern` in the CLI's `debayer.rs`). `libfitz` does no printing — the CLI's
  `print_debayer_notice` matches on the `ImageType` to explain a no-op run.
- **Pixel scaling:** `load_fits` applies `BSCALE`/`BZERO` and normalizes into the
  `PixelBuffer` domain: integer sources become `u16` over 0..=65535, float and wide-integer
  sources become `f32` over `[0, 1]`. FITS output uses the unsigned-16 convention
  (BITPIX 16 with BZERO 32768) so 0..=65535 round-trips.
- **CFA keywords follow the image type:** `image_to_fits` writes `BAYERPAT` for a
  `CFA` image and drops the whole `CFA_KEYWORDS` set for anything else, so a debayered cube
  or a split-out channel never carries a stale mosaic pattern.
- **Output destinations:** when `-o`/`--output` is omitted, outputs are placed beside the
  input with a suffix (`_debayer`/`_stretch`) or `.fz`. With multiple inputs, `--output` is
  treated as a directory. Compress/decompress delete the original unless `-k`/`--keep` or
  `-o` is given.
- **Verbose output:** `print_progress` (input -> output) and `print_step` (per-stage
  labels), both in `fitz-cli`'s `io_prompt.rs`, gate stdout on the global `--verbose` flag.

Tests live inline in each module under `#[cfg(test)]`. Most domain-logic tests (including the
SHA-256 regression tests against bundled fixtures) live in `libfitz`, exercising the `Image`
methods directly; `fitz-cli` keeps tests for CLI-only concerns (path derivation, the `info`
report shape, ANSI/kitty rendering, terminal capability detection).

### Rules when making changes

Avoid code duplication - reuse the existing code when applicable, refactor if needed. 
When writing code, write for performance and correctness. 
Run unit tests after every completed change, make sure no unit tests are broken.
For the new code add unit tests working on real data. 
Update readme file if the changes modify command line parameters or their behaviour.
