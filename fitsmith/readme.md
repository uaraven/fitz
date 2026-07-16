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
 - **Live preview** — images are displayed with debayer and auto-stretch if selected.
 - **Blink** — step or blink through the working set to compare frames; decoded images are
   kept in an LRU cache so re-selection and blinking re-render from memory.
 - **Inspect** — a Headers tab and a docked stats panel show the FITS metadata and pixel
   statistics for the selected frame.
 - **Compress / Decompress** — the Tools menu tile-compresses files to `.fz` (pick the
   algorithm) or decompresses them back. The operation runs over the checked rows, or the
   whole working set when none are checked. Choose whether to keep the originals or replace
   them in place (replaced files update to their new path in the list), or write the results
   to a different directory (which always keeps the originals).
 - **Export** — the Tools menu's Export… writes the working set (checked rows, or all when
   none are checked) into a destination folder in a chosen format, exporting each image
   exactly as the viewer shows it (the current debayer/stretch toggles are applied). A
   progress dialog tracks the batch. Per-format options:
     - **FITS** — bit depth (8-bit integer, 16-bit integer, or 32-bit float), plus optional
       tile compression with a chosen algorithm (RICE_1 / GZIP_1 / GZIP_2).
     - **TIFF** — bits per pixel (8, 16, or 32) and optional DEFLATE compression.
     - **JPEG** — encoder quality (1–100).
     - **PNG** — no options (written as 8-bit RGB).
 - **Analytics** — the Tools menu's Analytics… charts one pixel metric across the working set
   (checked rows, or all when none are checked) over the course of a session, to spot trends
   and problem frames — sky brightness creeping up as the night goes on, a jump in saturated
   pixels. Every metric is measured in a single read per file, so switching between them
   re-plots instantly with no re-read; a progress dialog tracks the batch and can cancel it.
   Drag the dialog's bottom-right corner to resize it.
     - **Metrics** — min, max, median and mean ADU, plus the number of pixels sitting exactly
       at the minimum or maximum ADU.
     - **Time axis** — frames are plotted at their real acquisition time (`DATE-OBS`), so a
       break in the session (clouds, a meridian flip) shows up as a gap in the line rather
       than being closed up. Frames with no readable `DATE-OBS`, and already-debayered RGB
       frames (whose ADU statistics aren't meaningful), are skipped and counted under the
       chart.
     - **Reading the chart** — hover a point for its time and value; the zoom slider runs from
       fit-to-width up to 4x, scrolling horizontally.


For example here is the mean ADU chart clearly showing when the wildfire smoke arrived and affected seeing and total brightness
![](../docs/analytics-mean-adu.png)

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
