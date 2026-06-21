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
cargo build                       # debug build
cargo build --release             # size-optimized release (opt-level z, LTO, strip)
cargo test                        # run all tests
cargo test --bin=fitz             # unit tests in the binary
cargo test <name>                 # run tests matching a substring (e.g. cargo test resolve_cfa)
cargo run -- <COMMAND> [args]     # e.g. cargo run -- debayer --format tiff test-data/uncompressed.fit
```

There is no separate lint step; use `cargo clippy` and `cargo fmt`.

The `edition = "2024"` crate requires a recent stable Rust toolchain.

## Architecture

Thin CLI over the **`fitskit`** crate (FITS read/write/tile-compression). Other key deps:
**`bayer`** (demosaicing), **`tiff`** (TIFF output), **`clap`** (derive-based arg parsing),
**`anyhow`** (errors).

- **`main.rs`** — clap `Cli`/`Command` definitions, the `*Args` structs, and `run_*`
  dispatchers that convert args into the option structs and invoke per-command logic. Also
  owns output-path derivation (`output_path` for compress/decompress, `derive_output_path`
  for debayer/stretch) and `process_files`, the batch driver.
- **`options.rs`** — plain option structs (`Options`, `DebayerOptions`, `StretchOptions`,
  `SplitChannelOptions`) passed down to each command. The CLI `*Args` structs in `main.rs`
  are translated into these.
- **`fits_image.rs`** — shared image plumbing used by debayer/stretch/split: locate the
  image HDU, resolve the Bayer pattern, demosaic into an interleaved RGB buffer, and write
  results back as FITS or TIFF.
- **Per-command modules** — `compress.rs`, `decompress.rs`, `debayer.rs`, `stretch.rs`,
  `split_channel.rs`, `info.rs`. Each exposes a `*_file(input, …, opts)` function (`info`
  only reads and prints, so it takes no output path).
- **`test_support.rs`** (test-only) — fixtures: locate bundled `test-data/`, copy into a
  temp dir, synthesize small FITS images.

### Conventions that span files

- **Batch processing, per-file errors:** `process_files` runs the command over every input
  path; a failure on one file prints `fitz: <path>: <err>` to stderr and is recorded, but
  does not abort the batch. The process exit code is FAILURE if any file failed.
- **Transparent decompression on read:** `find_image_hdu` in `fits_image.rs` is the single
  entry point the `debayer`/`stretch`/`split`/`info` commands use to get an image. It
  borrows a plain image HDU but decompresses a tile-compressed (`ZIMAGE`) HDU into an owned
  `ImageData`, returning a `Cow<ImageData>`, so every read-side command works on `.fz` inputs
  with no separate decompress step. The compressed HDU's header carries the original
  keywords (BAYERPAT, BSCALE/BZERO, RA/DEC, …), so downstream logic is unchanged.
- **Shared "already debayered" detection:** `load_rgb` in `fits_image.rs` is the single
  source of truth for debayer/stretch/split. A 2D image is demosaiced; a 3-plane image
  (`NAXIS3=3`) with **no** `BAYERPAT` header is treated as an already-debayered RGB cube and
  skips demosaicing. `--force-demosaic` overrides this (and then needs a Bayer pattern from
  `--pattern` or the header). The Bayer pattern resolves from `--pattern` first, else the
  FITS `BAYERPAT` keyword (`resolve_cfa`).
- **Pixel scaling:** physical pixel values are recovered via `BSCALE`/`BZERO`
  (`scaled_pixels` / `bscale_bzero`). FITS RGB output uses the unsigned-16 convention
  (BITPIX 16 with BZERO 32768) so 0..=65535 round-trips (`write_rgb16_fits`).
- **Output destinations:** when `-o`/`--output` is omitted, outputs are placed beside the
  input with a suffix (`_debayer`/`_stretch`) or `.fz`. With multiple inputs, `--output` is
  treated as a directory. Compress/decompress delete the original unless `-k`/`--keep` or
  `-o` is given.
- **Verbose output:** `print_progress` (input -> output) and `print_step` (per-stage
  labels) gate stdout on the global `--verbose` flag.

Tests live inline in each module under `#[cfg(test)]`, using `test_support` helpers and
bundled fixtures in `test-data/`.

### Rules when making changes

Avoid code duplication - reuse the existing code when applicable, refactor if needed. 
When writing code, write for performance and correctness. 
Run unit tests after every completed change, make sure no unit tests are broken.
For the new code add unit tests working on real data. 
Update readme file if the changes modify command line parameters or their behaviour.
