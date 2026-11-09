# Issue #23 — Group images by session, frame type and filter

## Context
Issue #23: given a directory tree, group images by **session**, **image type** and **filter**.
- **Revised scanning logic (supersedes the issue's "session = directory with a lights folder" rule):** walk the tree recursively and look at every image. Each image's **date and filter** come from its headers/EXIF. Its **frame type** comes from the folder names: check the parent folder's name against the known types (case-insensitive: `LIGHTS`/`light`/`Lights`/`LiGhTs` are all lights), and if it is not one, go one level higher, and so on until a type is found or the root folder is reached. An image with no match gets the default type **Light**.
- A *session* is one observation night, derived from the frame's date (see Decisions).

**DSLR / mirrorless (ILMC) RAW frames must be supported alongside FITS files.** The code already reads RAW metadata and translates it to FITS-compatible headers (`metadata_to_headers` in `libfitz/src/raw_file.rs:158`, which sets `DATE-OBS`, `EXPTIME`, `ISOSPEED`, `INSTRUME`, `TELESCOP`, …). Session grouping reuses that translation, so the grouping code only ever sees a FITS-style `Header`, whichever format the file is. RAW frames carry no FILTER, so they land in the "no filter" group.

Delivered in three phases: **Phase 0** the model and scanner in `libfitz` (shared by both frontends), **Phase 1** the `fitz summary` command (only a command plus a printer, no logic), **Phase 2** the same in `fitsmith`.

## Decisions (confirmed)
- `Session.integration` and `Summary.integration` are the **lights-only** total. Group totals (`FilterGroup`, `TypeGroup`) are plain sums over their own frames.
- Observation time is `DATE-LOC`, falling back to `DATE-OBS`.
- **Session = observation night**, independent of folder layout. The night is computed from the frame's observation time with a noon cutoff: `night = floor((t - 12h) / 24h)` in days since the epoch, so a night that crosses midnight stays in one session. Frames without a parsable date go into one "unknown date" session (sorted last). Calibration frames taken on a different night than the lights form their own session.
- **Frame type from folders:** starting at the image's parent folder, check each folder name against the known types and walk up one level at a time. The scan root's own name is checked too (so scanning a folder called `Darks` works) and nothing above the root is looked at. No match means `Light`. The nearest match wins, so `lights/darks/x.fit` is a dark.
- Folder names recognised: `light(s)`, `dark(s)`, `bias`/`biases`, `flat(s)`, `darkflat(s)`/`dark_flat(s)`, case-insensitive. No further aliases for now.
- "DLS/ILMC" means RAW files from DSLRs and mirrorless cameras.

## Reusable pieces found
- `libfitz::fits_file::load_header(&Path) -> ImageMeta` (header-only) at `libfitz/src/fits_file.rs:376`.
- `libfitz::fits_file::parse_date_obs` at `fits_file.rs:339` (header timestamp → epoch f64); `DATE-LOC`-then-`DATE-OBS` precedent in `fitsmith/src/controller/metrics.rs:163`.
- `libfitz::loader` (`loader.rs`): `is_fits_file` (magic-bytes sniff) and `load_image_from_file`, which dispatches FITS vs RAW. The header-only counterpart is added in 0b.
- `libfitz::raw_file::metadata_to_headers` (private) and `load_raw_image`.
- `libfitz::non_blank` at `libfitz/src/lib.rs:38`.
- Path helpers in `fitsmith/src/files.rs`: `FITS_EXTENSIONS`, `RAW_EXTENSIONS`, `has_extension`, `is_fits_path`, `is_raw_image`, `is_openable_image`, `macos_special_file`. They live in fitsmith today and must move to libfitz.
- `libfitz/src/keywords.rs` for keyword constants.
- Test helpers in `libfitz/src/test_support.rs`, with `TempDir` fixtures; `test-data/comet.ARW` is a real RAW fixture.

Naming: `libfitz::summary` (curated `info` header fields) and `fitz-cli/src/summary.rs` already exist. The new code uses `session` (libfitz) and `session_summary.rs` (CLI) to avoid clashing with them.

## Phase 0 — `libfitz`

### 0a. Move path helpers into libfitz
- New `libfitz/src/files.rs` (`pub mod files`) takes `FITS_EXTENSIONS`, `RAW_EXTENSIONS`, `has_extension`, `is_fits_path`, `is_raw_image`, `is_openable_image` and `macos_special_file` from `fitsmith/src/files.rs`, with their tests.
- `fitsmith/src/files.rs` re-exports them (`pub use libfitz::files::{…}`), so the callers in `controller/mod.rs`, `analytics.rs` and `main.rs` are untouched.
- Caveat: `RAW_EXTENSIONS` contains `tif`, `data` and `raw`. Because the scan now takes every image in every folder, with no lights-folder gate, stray exported TIFFs would show up in `errors` (and as noise). Decide during implementation whether to filter `tif` out of the scan.

### 0b. Header-only reading for both formats
- `raw_file.rs`: add `pub fn load_raw_header(source: &Path) -> Result<Header>`. It does `RawImage::open` (no `unpack()`, so no pixel decode) and builds the header from `full_info()` via the existing `metadata_to_headers`, which is reused rather than duplicated. `metadata_to_headers` currently needs `image_type`, `w` and `h`. Either derive the CFA pattern cheaply from `idata` without unpacking, or make those inputs optional and skip `BAYERPAT` when unknown. Check the signature first. The grouping code does not need `BAYERPAT`.
- `loader.rs`: add `pub fn load_header_from_file(source: &Path) -> Result<Header>` that mirrors `load_image_from_file`: `is_fits_file` → `fits_file::load_header(..).header`, else `load_raw_header`. This is the single header entry point session scanning uses.
- Cost note: RAW needs the whole file read into memory for libraw (`std::fs::read`), so RAW directories are slower to scan than FITS. Headers are read in parallel with rayon to compensate.

### 0c. Keyword constants
Add `FILTER`, `EXPTIME`, `DATE_LOC` and `DATE_OBS` to `libfitz/src/keywords.rs`.

### 0d. `libfitz/src/session.rs` (`pub mod session`)

**Types** (all `Debug`, `Clone`):
- `FrameType { Light, Dark, Bias, Flat, DarkFlat }`.
  - `FrameType::from_folder_name(&str) -> Option<Self>`: trim, ASCII-lowercase, match singular or plural.
  - `label()` for display.
  - The derived `Ord` gives the display order Light, Dark, Bias, Flat, DarkFlat.
- `Frame { path: PathBuf, exposure: f64 /* s, 0 if EXPTIME missing */, observed: Option<f64> /* epoch */ }`.
- `FilterGroup { filter: Option<String>, frames: Vec<Frame>, count: usize, integration: f64 }`. Filter is trimmed, and a blank or missing filter is `None`. Group key is exact-match on the trimmed string.
- `TypeGroup { frame_type, filters: Vec<FilterGroup>, count, integration }`, summing its `FilterGroup`s.
- `Session { night: Option<i64> /* days since epoch of the night's start; None = unknown date */, types: Vec<TypeGroup>, count, integration, start: Option<f64>, end: Option<f64> }`. `start`/`end` are the min/max frame `observed`. `Session::label()` is `"2026-06-22"` (the night's start date) or `"unknown date"`. `integration` is lights-only; `count` covers every frame. A session has no path, since it may span several folders.
- `Summary { root: PathBuf, sessions: Vec<Session>, count, integration, errors: Vec<(PathBuf, String)> }`. `errors` collects unreadable files, so one bad file never aborts the scan (the project's per-file error convention). `integration` is lights-only.

Each aggregate is built bottom-up by a `finish()`/constructor that computes `count` and `integration` from its children, so the numbers cannot drift from the frames.

**Observation time:** `DATE-LOC` if present, otherwise `DATE-OBS` (which is what RAW EXIF time is translated to), parsed with the existing `parse_date_obs`. Local time keeps a night that crosses midnight on one calendar date. A bare-date `DATE-OBS` does not parse and gives `observed = None`, which is tolerated.

**Public API:**
- `pub fn summarize_dir(root: &Path) -> Result<Summary>`: discover, read headers in parallel, aggregate.
- `pub fn scan_images(root: &Path) -> Vec<(PathBuf, FrameType)>`: the cheap discovery step, with no header reads. It walks every folder recursively and pairs each image with its folder-derived frame type. Useful for the GUI to show progress, and easy to unit test.
- `pub fn frame_type_for(path: &Path, root: &Path) -> FrameType`: the ancestor walk (also exported, so it is tested on its own).
- `pub fn night_of(observed: f64) -> i64`: the noon-cutoff night number.
- `pub fn format_duration(secs: f64) -> String` (`"5h 12m 30s"`) and `pub fn format_timestamp(epoch: f64) -> String` (`"2026-06-22 21:30"`). The latter needs the inverse of `days_from_civil` (`civil_from_days`). Both frontends share them.

**Discovery rules:**
- Walk `root` recursively through all folders (sorted for determinism). Symlinked directories are not followed, to avoid loops.
- Take every `is_openable_image` file (FITS **and** RAW) anywhere in the tree. Nothing is gated on folder names.
- For each file, `frame_type_for(path, root)` checks the parent folder name with `FrameType::from_folder_name`, then each higher ancestor, up to and including `root`'s own name. The first match wins. No match → `FrameType::Light`.
- Read each file's header with `load_header_from_file` (0b), in parallel with rayon. Map to `Frame` plus its trimmed `FILTER`, and bucket by `night_of(observed)` (or `None`), then type, then filter.
- A tree with no images yields an empty `Summary`.

**Ordering** (deterministic): sessions by `night` (None last). Types by `FrameType`. Filters alphabetically with `None` last. Frames by `observed`, then path.

### 0e. Tests (inline, per project convention)
- `test_support.rs`: add `write_frame_fits(path, filter: Option<&str>, exptime: f64, date_loc: Option<&str>, date_obs: Option<&str>)`. It writes a tiny image using the `FitsFile::with_primary_image` + `header.set` pattern already used in `copy_header.rs:60`.
- Synthetic `TempDir` trees cover:
  - folder-name case-insensitivity (`LiGhTs`, `Dark`, `BIASES`);
  - singular and plural names;
  - `frame_type_for`: nearest ancestor wins (`lights/darks/x` is a dark), unknown folders are walked past (`darks/batch1/x` is a dark), the scan root's own name counts, no match → Light, names above the root are ignored;
  - images in a tree with no type folders at all all become Light;
  - sessions split by night: frames at 22:00 and 02:00 the next day share a session, a frame at 13:00 the next day starts a new one; a frame with no date goes to the unknown-date session (last);
  - nested `lights/Ha/`;
  - missing FILTER → `None` group, and missing EXPTIME → 0;
  - count and integration sums at every level, and the lights-only session total;
  - a corrupt file lands in `errors` while the rest is still summarised;
  - `DATE-LOC` preferred over `DATE-OBS`, session `start`/`end`;
  - `format_duration` and `format_timestamp` (round-trip against `parse_date_obs`).
- Real-data tests (per CLAUDE.md):
  - Copy `test-data/uncompressed.fit` (FILTER `'Narrowband 7nm'`) and `test-data/bad-image/good1.fits` (FILTER `'HaOIII 7nm'`) into a temp `Session/Lights/`, and assert the two filter groups and their exposure/time from the real headers.
  - **RAW:** copy `test-data/comet.ARW` into a temp `Session/Lights/` next to a FITS file. Assert it is picked up with `filter == None`, with `exposure` and `observed` taken from its EXIF-derived `EXPTIME`/`DATE-OBS`. Also test `load_raw_header` directly on `comet.ARW` against the header from `load_raw_image` (same `DATE-OBS`, `EXPTIME`, `INSTRUME`).
  - `compressed.fits.fz` is handled, if its header carries the keywords.
- `cargo test -p libfitz` and `cargo clippy --workspace --all-targets` clean.

## Phase 1 — `fitz summary`
`fitz-cli` gets only the command and the printer. All logic comes from `libfitz::session`.

- `fitz-cli/src/main.rs`:
  - Add the `Summary(SummaryArgs)` variant to `Command`, with doc comment "Summarise sessions, frame types and filters in a directory tree (FITS and camera RAW)".
  - `SummaryArgs { dirs: Vec<PathBuf> }` (one or more directories).
  - `run_summary`, modelled on `run_preview`/`run_copy_header`. It is directory-oriented, so it does not use `process_files`.
  - It calls `summarize_dir` per dir, prints the report, prints `fitz: <path>: <err>` to stderr for each `errors` entry, and returns FAILURE if any errors occurred or a dir is not a directory.
  - If no images are found, print `fitz: no images found in <dir>`.
- `fitz-cli/src/session_summary.rs` (new, a pure `fn format_summary(&Summary) -> String` plus a thin print wrapper, so it is testable). The report is built in a `String` and emitted with one `print!`, like `info.rs`. It reuses `info.rs`'s aligned-row helpers if they generalise; otherwise a small local table.
  ```
  Session 2026-06-22   (2026-06-22 21:30 – 2026-06-23 03:12)
    Lights   120 frames   10h 00m 00s
      Ha 7nm        60   5h 00m 00s
      OIII 7nm      60   5h 00m 00s
    Darks     30 frames    0h 15m 00s
      (no filter)   30   ...
  Total: 1 session, 150 frames, light integration 10h 00m 00s
  ```
- Docs: `fitz-cli/readme.md` (add to the intro list and the COMMAND list, plus a `### summary` section with the usage block, noting FITS and camera RAW support), and `CLAUDE.md` (layout: `libfitz/session.rs`, `libfitz/files.rs`, `loader::load_header_from_file`, `fitz-cli/session_summary.rs`).
- Tests:
  - `format_summary` over a hand-built `Summary` (golden string: alignment, `(no filter)`, missing times).
  - An end-to-end test over a temp tree containing both FITS and the real `comet.ARW`.
  - Clap parse test for `summary`.

## Phase 2 — `fitsmith` (outline, to be detailed after Phase 1)
- A "Add Sessions…" (or "Open Session Folder…") menu item next to `Add Folder…` (`ui/app.slint:335-350`, wired via `main.rs:74-75`).
  - New `controller/sessions.rs` runs `summarize_dir` on a worker thread (the `spawn_exposure_load` pattern in `controller/mod.rs:373`).
  - It marshals the `Summary` back with `upgrade_in_event_loop`, is generation-guarded and cancellable like the other batches, and reuses `libfitz::session::Summary` as-is (no parallel struct, per the repo's reuse guidance).
- UI: a session panel, a tree of session → type → filter with counts and integration times (shared `format_duration`/`format_timestamp`). Group checkboxes call `set_checked_paths` (`controller/mod.rs:644`), so existing `operation_targets` batches (export, convert, analytics) work unchanged. An "Add to working set" action goes through `add_paths`, which already handles RAW files.
- The flat `FileList` keeps its index-based model, so no changes to keyboard navigation.
- Update `fitsmith/readme.md` Features and the CLAUDE.md layout.

## Verification
1. `cargo test --workspace` after each phase. `cargo clippy --workspace --all-targets` and `cargo fmt`.
2. Phase 1 manual run: build a throwaway tree in the scratchpad (`S1/{LIGHTS,darks,Biases,flat}`, `S2/lights`, a plain `misc/` folder with loose images that should default to Light), populate it by copying files from `test-data/` (FITS plus `comet.ARW`), then run `cargo run -p fitz -- summary <root>`. Check the split by night, case-insensitive types, the walk-up for unknown subfolders, filter groups, the RAW frame in the "(no filter)" group, and counts and sums against `fitz info` on individual files.
3. Phase 2: run fitsmith, open the tree, tick a group, and run an export on it.