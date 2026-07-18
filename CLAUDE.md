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

A Cargo **workspace** with three crates:

- **`libfitz`** — the reusable library: FITS I/O (with transparent tile-decompression),
  debayering, auto-stretch, per-channel splitting, header/pixel-stat inspection, star
  detection, session analytics, export encoding, preview rendering, header copying, and
  image resizing. No CLI parsing, no terminal I/O, no interactive prompts, no GUI types —
  both frontends depend on it the same way.
- **`fitz`** (in `fitz-cli/`) — the thin CLI binary: clap argument parsing, output-path
  derivation, the overwrite-confirmation prompt, `--verbose` progress printing, terminal
  rendering (`preview`/`kitty`/`terminal`), and text-report formatting for `info`. Depends on
  `libfitz` via a path dependency.
- **`fitsmith`** — the Slint GUI frontend (see its own section below). Also depends on
  `libfitz` only; all pixel/statistics work lives in the library.

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

- **`fits_image.rs`** — shared image plumbing: locate the image HDU (`find_image_hdu`,
  transparently decompressing `ZIMAGE` HDUs), resolve the Bayer pattern (`resolve_cfa`),
  demosaic into an interleaved RGB buffer (`demosaic_to_rgb`, `load_rgb`), write results back
  as FITS or TIFF, and copy/filter header metadata.
- **`debayer.rs`**, **`stretch.rs`**, **`split_channel.rs`**, **`compress.rs`**,
  **`decompress.rs`**, **`copy_header.rs`** — one pure "compute" function per command (e.g.
  `debayer::debayer`, `stretch::load_and_stretch`, `split_channel::split_channels`,
  `compress::compress`), each taking a plain `*Options` domain struct and returning an
  in-memory result. No path derivation, prompting, or printing — that's the CLI's job.
- **`info.rs`** — `header_info`/`header_info_with` build a `HeaderInfo` struct (resolution,
  bit depth, sky coordinates, and — per `InfoRequest` — `PixelStats`/histogram and a
  `StarReport`); formatting it into text is left to the caller. `measure_frame` is the
  shared engine that computes pixel statistics and star metrics together, reusing the
  expensive intermediates (a mono frame's stats double as the detection background; an RGB
  cube's green plane is materialized once for both).
- **`stars.rs`** — star detection and shape measurement on a `MonoPlane` (threshold against
  the plane's robust background, flood-fill blobs, reject non-stars, measure HFR/FWHM/
  eccentricity, aggregate to medians). Driven by `info --stars`, the GUI stats panel, and
  the Star-metrics batch.
- **`analytics.rs`** — per-frame session metrics (`analyze_file`) keyed by acquisition time,
  assembled into a plottable time `Series` per `Metric` (`build_series`), plus CSV output.
  Backs the GUI's Analytics / Star metrics chart dialogs.
- **`preview.rs`** — the display pipeline shared by CLI preview and GUI viewer: resolve the
  debayer/stretch toggles into an RGB buffer (`preview_rgb`, `render_export_rgb`) or a
  ready-to-paint RGBA8 buffer (`render_preview`).
- **`export.rs`** — encode a rendered RGB image to FITS/TIFF/JPEG/PNG with per-format
  options (`export_rgb`), and the one-call `export_file` that renders through the preview
  pipeline first.
- **`inspect.rs`** — aberration-inspector geometry: the nine fixed tile regions of a frame
  and RGBA8 cropping.
- **`resize.rs`** — generic box-filter image resizing (`resize_to_fit`), used by the CLI's
  terminal preview and reusable by a GUI's thumbnail/blink view.
- **`test_support.rs`** (test-only) — fixtures: locate bundled `../test-data/`, copy into a
  temp dir, synthesize small FITS images.

### `fitz-cli` layout

- **`main.rs`** — clap `Cli`/`Command` definitions, the `*Args` structs, and `run_*`
  dispatchers that convert args into `libfitz` domain options (composed inside the CLI's own
  `*Options` structs in `options.rs`) and invoke the per-command wrapper. Also owns
  output-path derivation (`output_path` for compress/decompress, `derive_output_path` for
  debayer/stretch) and `process_files`, the batch driver.
- **`options.rs`** — CLI-side option structs (`Options`, `DebayerOptions`, `StretchOptions`,
  `SplitChannelOptions`, …), each composing the matching `libfitz::*::*Options` plus
  CLI-only fields (`yes`, `verbose`, `output`, `multi_file`).
- **Per-command wrapper modules** — `compress.rs`, `decompress.rs`, `debayer.rs`, `stretch.rs`,
  `split_channel.rs`, `copy_header.rs`, `info.rs`. Each resolves the output path, calls
  `io_prompt::ensure_can_write`, calls into `libfitz`, prints `--verbose` progress, and
  writes the result.
- **`io_prompt.rs`** — the interactive overwrite-confirmation prompt (`ensure_can_write`) and
  `print_progress`/`print_step` verbose-output helpers.
- **`preview.rs`**, **`kitty.rs`**, **`terminal.rs`** — terminal-only rendering (ANSI
  half-blocks / kitty graphics protocol) and capability detection; not part of `libfitz`
  since a GUI frontend wouldn't use ANSI escape codes.
- **`test_support.rs`** (test-only) — locates bundled `../test-data/` for the CLI's own tests.

### `fitsmith` layout

A Slint GUI ("FitSmith") over the same library. `ui/*.slint` holds the declarative UI
(`app.slint` is the window; dialogs/panels are one file each); `build.rs` compiles it.
Rust side, split by concern:

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
  worker (preview + header cards + stats, one `header_info_from` call); `view.rs` maps it
  onto Slint properties. Both are free of threading; `doc.rs` is free of Slint types.
- **`cache.rs`** — a small byte-budgeted LRU keeping rendered previews resident (budgeted
  at 80% of available memory at startup).
- **`chart.rs`** / **`chart_svg.rs`** — analytics `Series` → normalized chart geometry →
  the on-screen chart and its SVG export.
- **`files.rs`** — pure path helpers (FITS extensions, directory scan, output paths).
- **`image.rs`** — the one RGBA8-buffer → `slint::Image` conversion point.

### Conventions that span files

- **Batch processing, per-file errors:** `process_files` runs the command over every input
  path; a failure on one file prints `fitz: <path>: <err>` to stderr and is recorded, but
  does not abort the batch. The process exit code is FAILURE if any file failed.
- **Transparent decompression on read:** `find_image_hdu` in `libfitz`'s `fits_image.rs` is
  the single entry point the `debayer`/`stretch`/`split`/`info` commands use to get an image.
  It borrows a plain image HDU but decompresses a tile-compressed (`ZIMAGE`) HDU into an owned
  `ImageData`, returning a `Cow<ImageData>`, so every read-side command works on `.fz` inputs
  with no separate decompress step. The compressed HDU's header carries the original
  keywords (BAYERPAT, BSCALE/BZERO, RA/DEC, …), so downstream logic is unchanged.
- **Shared "already debayered" detection:** `load_rgb` in `libfitz`'s `fits_image.rs` is the
  single source of truth for debayer/stretch/split. A 2D image is demosaiced; a 3-plane image
  (`NAXIS3=3`) with **no** `BAYERPAT` header is treated as an already-debayered RGB image and
  skips demosaicing. `--force-demosaic` overrides this (and then needs a Bayer pattern from
  `--pattern` or the header). The Bayer pattern resolves from `--pattern` first, else the
  FITS `BAYERPAT` keyword (`resolve_cfa`). `load_rgb` itself does no printing — it returns a
  `LoadRgbNotice` (`Demosaiced` / `AlreadyDebayeredRgbCube` / `AlreadyDebayeredMono`) that each
  CLI wrapper matches on to print the right message, so `libfitz` stays free of terminal I/O.
- **Pixel scaling:** physical pixel values are recovered via `BSCALE`/`BZERO`
  (`scaled_pixels` / `bscale_bzero`). FITS RGB output uses the unsigned-16 convention
  (BITPIX 16 with BZERO 32768) so 0..=65535 round-trips (`write_rgb16_fits`).
- **Output destinations:** when `-o`/`--output` is omitted, outputs are placed beside the
  input with a suffix (`_debayer`/`_stretch`) or `.fz`. With multiple inputs, `--output` is
  treated as a directory. Compress/decompress delete the original unless `-k`/`--keep` or
  `-o` is given.
- **Verbose output:** `print_progress` (input -> output) and `print_step` (per-stage
  labels), both in `fitz-cli`'s `io_prompt.rs`, gate stdout on the global `--verbose` flag.

Tests live inline in each module under `#[cfg(test)]`. Most domain-logic tests (including the
SHA-256 regression tests against bundled fixtures) live in `libfitz`, exercising the pure
`*_file`-equivalent functions directly; `fitz-cli` keeps tests for CLI-only concerns (path
derivation, ANSI/kitty rendering, terminal capability detection).

### Rules when making changes

Avoid code duplication - reuse the existing code when applicable, refactor if needed. 
When writing code, write for performance and correctness. 
Run unit tests after every completed change, make sure no unit tests are broken.
For the new code add unit tests working on real data. 
Update readme file if the changes modify command line parameters or their behaviour.
