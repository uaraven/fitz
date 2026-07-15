# FitSmith

FitSmith is a desktop GUI frontend for the [`fitz`](../fitz-cli/readme.md) FITS toolset. It
gives you a quick, interactive way to look through FITS (astronomy image) files without
dropping to the command line.

All FITS and image operations — reading, debayering, auto-stretching, header parsing and
pixel statistics — are performed by the shared [`fitz-core`](../fitz-core) library, so the GUI
and the CLI behave identically.

## What it does

 - **Working set** — open a single file or a whole directory and browse the files as a list.
   A file that fails to load is highlighted with a red background; hover it to see the error.
 - **Select** — each row has a checkbox for building a multi-file selection. Click the box, or
   press Space on the highlighted row, to check or uncheck it.
 - **Live preview** — images are decoded off the UI thread and displayed with debayer and
   auto-stretch toggles that re-render the current frame live.
 - **Blink** — step or blink through the working set to compare frames; decoded images are
   kept in an LRU cache so re-selection and blinking re-render from memory.
 - **Inspect** — a Headers tab and a docked stats panel show the FITS metadata and pixel
   statistics for the selected frame.

## Building and running

FitSmith is part of the `fitz` Cargo workspace:

```shell
cargo run -p fitsmith                 # run the GUI
cargo build -p fitsmith --release     # build a release binary
```

You can also pass files or folders on the command line to seed the working set:

```shell
cargo run -p fitsmith -- path/to/images/
```

## Slint and licensing

FitSmith's user interface is built with [Slint](https://slint.dev/). Slint is available under
several licenses; FitSmith uses it under the **GNU General Public License, version 3 (GPLv3)**.
Because of this, distributing FitSmith binaries is subject to the terms of the GPLv3. The rest
of the `fitz` project (the `fitz-core` library and the `fitz` CLI) remains under the MIT
license — see [LICENSE](../LICENSE).
