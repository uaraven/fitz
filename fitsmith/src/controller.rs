//! Application logic bridging the Slint UI to `fitz-core`. Files are decoded and
//! rendered off the UI thread into display-ready [`LoadedDoc`]s (preview +
//! headers + stats), kept in a byte-budgeted LRU cache so re-selecting or
//! blinking back to a file re-displays instantly, with no re-decode. A
//! generation counter drops stale results when the user scrubs faster than
//! frames can render. Turning a document into UI properties is [`crate::view`]'s
//! job; this module owns state, threading and blink.
//!
//! Blink is load-aware: it never advances before the current frame is on
//! screen. Manually selecting a file while blinking simply continues the loop
//! from that file. Because the cache holds *rendered* documents, it is cleared
//! whenever the debayer/stretch settings change (which invalidate them).

use std::cell::RefCell;
use std::cmp::max;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use fitz_core::fits_image::find_image_hdu;
use fitz_core::fitskit::{CompressionType, FitsFile};
use fitz_core::preview::{PreviewParams, PreviewStage, render_preview_with_progress};
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel, Weak};

use crate::doc::LoadedDoc;
use crate::files::{
    compressed_output_path, decompressed_output_path, display_name, expand_inputs, is_compressed,
    next_index, scan_directory,
};
use crate::{AppWindow, FileRow, view};

/// Maximum total size of rendered previews to keep cached. Sized so a handful
/// of full-frame images stay resident for instant blink / re-selection.
/// TODO: make this user-configurable.
const CACHE_CAPACITY_BYTES: usize = 1024 * 1024 * 1024;

/// How long a blink frame stays on screen *after it has finished loading*
/// before advancing to the next. The load is always awaited first, so blink
/// never runs ahead of the images it is showing.
const BLINK_DWELL: Duration = Duration::from_millis(400);

/// All UI-thread application state. Lives in a thread-local because Slint is
/// single-threaded: every mutation happens either from a callback or from a
/// worker's `upgrade_in_event_loop` closure, both of which run here.
struct AppState {
    /// The working set, in list order.
    paths: Vec<PathBuf>,
    /// The `[FileRow]` model backing the list view (mirrors `paths`).
    files_model: Rc<VecModel<FileRow>>,
    /// Loaded documents (preview + headers + stats), keyed by path. Cleared
    /// when a setting invalidates the rendered preview.
    cache: crate::cache::LruCache<PathBuf, Rc<LoadedDoc>>,
    /// Currently selected index into `paths`, if any.
    selected: Option<usize>,
    /// Bumped on every selection/re-render request; a worker result is applied
    /// only if its captured generation still matches (stale-result coalescing).
    generation: u64,
    /// One-shot timer that advances blink after the current frame's dwell.
    blink_timer: Timer,
}

impl AppState {
    fn new() -> Self {

        let mut sys = sysinfo::System::new_all();
        sys.refresh_memory();
        let max_mem = sys.total_memory();
        let cache_capacity = max( CACHE_CAPACITY_BYTES, (max_mem / 8) as usize);

        Self {
            paths: Vec::new(),
            files_model: Rc::new(VecModel::default()),
            cache: crate::cache::LruCache::new(cache_capacity),
            selected: None,
            generation: 0,
            blink_timer: Timer::default(),
        }
    }
}

thread_local! {
    static STATE: RefCell<AppState> = RefCell::new(AppState::new());
}

/// Bind the file-list model to the window once, at startup.
pub fn init(app: &AppWindow) {
    STATE.with(|s| {
        app.set_files(ModelRc::from(s.borrow().files_model.clone()));
    });
    update_memory(app);
}

/// Refresh the status bar's memory readout from the cache's resident bytes.
/// Called after every cache mutation (load, clear, settings change).
fn update_memory(app: &AppWindow) {
    let (used, capacity) = STATE.with(|s| {
        let st = s.borrow();
        (st.cache.total_bytes(), st.cache.capacity())
    });
    app.set_memory_text(
        format!("Memory: {} / {}", format_bytes(used), format_bytes(capacity)).into(),
    );
}

/// Human-readable byte size (B/KB/MB/GB) for the memory readout.
fn format_bytes(n: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * KB;
    const GB: f64 = 1024.0 * MB;
    let n = n as f64;
    if n >= GB {
        format!("{:.1} GB", n / GB)
    } else if n >= MB {
        format!("{:.0} MB", n / MB)
    } else if n >= KB {
        format!("{:.0} KB", n / KB)
    } else {
        format!("{n:.0} B")
    }
}

/// Snapshot the toggle state from the UI into preview parameters.
fn params(app: &AppWindow) -> PreviewParams {
    PreviewParams {
        debayer: app.get_debayer_enabled(),
        stretch: app.get_stretch_enabled(),
        ..PreviewParams::default()
    }
}

/// Build the list row for a path: base name plus a "compressed" badge for `.fz`.
fn make_row(path: &Path) -> FileRow {
    FileRow {
        name: display_name(path).into(),
        status: if is_compressed(path) {
            "compressed"
        } else {
            ""
        }
        .into(),
        path: path.to_string_lossy().into_owned().into(),
        error: "".into(),
        checked: false,
    }
}

// --- opening files -------------------------------------------------------

/// Prompt for a single FITS file, add it to the working set, and select it.
pub fn open_file(app: &AppWindow) {
    if let Some(path) = rfd::FileDialog::new()
        .add_filter("FITS images", &["fit", "fits", "fts", "fz"])
        .add_filter("All files", &["*"])
        .pick_file()
    {
        add_and_select(app, vec![path]);
    }
}

/// Prompt for a directory, add every FITS file it contains, and select the
/// first newly added one.
pub fn open_directory(app: &AppWindow) {
    let Some(dir) = rfd::FileDialog::new().pick_folder() else {
        return;
    };
    let paths = scan_directory(&dir);
    if paths.is_empty() {
        app.set_status_text(format!("No FITS files in {}", dir.display()).into());
        return;
    }
    add_and_select(app, paths);
}

/// Add the files and directories passed on the command line to the working set
/// and select the first (see [`expand_inputs`]). Called once at startup.
pub fn open_args(app: &AppWindow, args: impl IntoIterator<Item = PathBuf>) {
    add_and_select(app, expand_inputs(args));
}

/// Add `paths` to the working set and select the first of them (whether newly
/// added or already present). A no-op for an empty list.
fn add_and_select(app: &AppWindow, paths: Vec<PathBuf>) {
    let Some(first) = paths.first().cloned() else {
        return;
    };
    let target = add_paths(paths).or_else(|| index_of(&first));
    if let Some(index) = target {
        select_file(app, index as i32);
    }
}

/// Remove every file from the working set and reset the view. Bumping the
/// generation makes any in-flight load land as stale and be dropped.
pub fn clear_files(app: &AppWindow) {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.blink_timer.stop();
        st.paths.clear();
        st.files_model.set_vec(Vec::new());
        st.cache.clear();
        st.selected = None;
        st.generation += 1;
    });
    app.set_blinking(false);
    app.set_selected_index(-1);
    app.set_busy(false);
    app.set_status_text("No image — add files to view".into());
    app.set_stage_text("".into());
    view::clear(app);
    update_memory(app);
}

/// The rows a remove-action drops: every checked row, or — when none are
/// checked — just the highlighted row (if any). Sorted and de-duplicated.
fn removal_targets(checked: impl Iterator<Item = usize>, selected: Option<usize>) -> Vec<usize> {
    let mut targets: Vec<usize> = checked.collect();
    if targets.is_empty() {
        targets.extend(selected);
    }
    targets.sort_unstable();
    targets.dedup();
    targets
}

/// Which row to highlight after a removal: the previously highlighted file if
/// it survived (at its new index `survived`), else the nearest surviving row to
/// the old highlight, or `None` when the set is now empty.
fn next_selection(new_len: usize, survived: Option<usize>, old_index: Option<usize>) -> Option<usize> {
    if new_len == 0 {
        None
    } else if let Some(i) = survived {
        Some(i)
    } else {
        Some(old_index.unwrap_or(0).min(new_len - 1))
    }
}

/// Remove the checked rows from the working set — or, when nothing is checked,
/// the highlighted row. Dropped rows shift the indices below them, so this
/// evicts each removed file's cached preview, rebuilds the model, and re-homes
/// the highlight to a surviving row (or clears the view when the set empties).
pub fn remove_selected(app: &AppWindow) {
    let reselect = STATE.with(|s| {
        let mut st = s.borrow_mut();
        let checked = (0..st.files_model.row_count())
            .filter(|&i| st.files_model.row_data(i).is_some_and(|r| r.checked));
        let targets = removal_targets(checked, st.selected);
        if targets.is_empty() {
            return None;
        }

        let old_index = st.selected;
        let selected_path = st.selected.and_then(|i| st.paths.get(i).cloned());

        // Drop rows high-index-first so earlier indices stay valid, evicting
        // each removed file's cached preview. Any in-flight load is orphaned by
        // the generation bump below.
        st.blink_timer.stop();
        for &i in targets.iter().rev() {
            let path = st.paths.remove(i);
            st.files_model.remove(i);
            st.cache.remove(&path);
        }
        st.generation += 1;
        st.selected = None;

        let len = st.paths.len();
        let survived = selected_path.and_then(|p| st.paths.iter().position(|q| q == &p));
        Some(next_selection(len, survived, old_index))
    });

    let Some(target) = reselect else {
        return; // nothing was checked or highlighted
    };
    update_memory(app);
    match target {
        // `select_file` re-displays a surviving file straight from the cache.
        Some(index) => select_file(app, index as i32),
        None => {
            app.set_blinking(false);
            app.set_selected_index(-1);
            app.set_busy(false);
            app.set_status_text("No image — add files to view".into());
            app.set_stage_text("".into());
            view::clear(app);
        }
    }
}

/// Append any paths not already in the working set to both `paths` and the list
/// model. Returns the index of the first newly added path (for auto-select), or
/// `None` if every path was already present.
fn add_paths(new_paths: Vec<PathBuf>) -> Option<usize> {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        let mut first_added = None;
        for path in new_paths {
            if st.paths.iter().any(|p| p == &path) {
                continue;
            }
            st.files_model.push(make_row(&path));
            st.paths.push(path);
            first_added.get_or_insert(st.paths.len() - 1);
        }
        first_added
    })
}

fn index_of(path: &Path) -> Option<usize> {
    STATE.with(|s| s.borrow().paths.iter().position(|p| p == path))
}

// --- selection & rendering ----------------------------------------------

/// Select the file at `index`: mark it current, bump the generation, and either
/// display it straight from the preview cache or load it from disk off-thread.
/// Any pending blink advance is cancelled here; the newly shown frame schedules
/// the next one, so blink continues from whatever the user just selected.
pub fn select_file(app: &AppWindow, index: i32) {
    let action = STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.blink_timer.stop();
        let index = usize::try_from(index).ok()?;
        let path = st.paths.get(index)?.clone();
        st.selected = Some(index);
        st.generation += 1;
        let req = st.generation;
        let cached = st.cache.get(&path).cloned();
        Some((index, path, cached, req))
    });
    let Some((index, path, cached, req)) = action else {
        return;
    };

    app.set_selected_index(index as i32);
    if let Some(doc) = cached {
        display_doc(app, &path, &doc);
        return;
    }
    app.set_busy(true);
    // Left half: the high-level activity; the worker fills the right half with
    // the fine-grained step (reading / debayering / stretching).
    let action = if app.get_blinking() {
        "Blinking"
    } else {
        "Loading"
    };
    app.set_status_text(format!("{action}: {}", display_name(&path)).into());
    spawn_load(app.as_weak(), path, params(app), req);
}

/// Move the selection by `delta` rows (arrow / page / space keys), clamped to
/// the list. With nothing selected yet, any key lands on the first row.
pub fn navigate(app: &AppWindow, delta: i32) {
    let target = STATE.with(|s| {
        let st = s.borrow();
        let len = st.paths.len() as i64;
        if len == 0 {
            return None;
        }
        let index = match st.selected {
            None => 0,
            Some(cur) => (cur as i64 + delta as i64).clamp(0, len - 1),
        };
        Some(index as i32)
    });
    if let Some(index) = target {
        select_file(app, index);
    }
}

/// Jump the selection to the first (`last = false`) or last row (Home / End).
pub fn navigate_edge(app: &AppWindow, last: bool) {
    let target = STATE.with(|s| {
        let len = s.borrow().paths.len();
        (len > 0).then(|| if last { len - 1 } else { 0 } as i32)
    });
    if let Some(index) = target {
        select_file(app, index);
    }
}

/// Handle a debayer/stretch toggle change: the cached previews were rendered
/// with the old settings, so drop them all and re-render the current selection.
pub fn rerender(app: &AppWindow) {
    let selected = STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.cache.clear();
        st.selected
    });
    update_memory(app);
    if let Some(index) = selected {
        select_file(app, index as i32);
    }
}

/// Read a file, decode it, and render its preview on a worker thread, reporting
/// each pipeline stage to the right-hand status field and marshaling the final
/// result back to the UI thread.
fn spawn_load(weak: Weak<AppWindow>, path: PathBuf, p: PreviewParams, req: u64) {
    std::thread::spawn(move || {
        // Push the current pipeline step to the UI thread, but only while this
        // request is still the latest one (a superseded load stays quiet).
        let report = {
            let weak = weak.clone();
            move |label: &'static str| {
                let weak = weak.clone();
                let _ = weak.upgrade_in_event_loop(move |app| {
                    if is_current(req) {
                        app.set_stage_text(label.into());
                    }
                });
            }
        };
        let outcome = load_and_render(&path, &p, &report);
        let _ = weak.upgrade_in_event_loop(move |app| finish_load(&app, path, outcome, req));
    });
}

/// Read a FITS file, render its preview, and derive its header cards and pixel
/// stats — the whole display-ready document — calling `report` with the name of
/// each stage as it starts. Runs on a worker.
fn load_and_render(
    path: &Path,
    p: &PreviewParams,
    report: &dyn Fn(&'static str),
) -> Result<LoadedDoc> {
    report("Reading");
    let fits = FitsFile::from_file(path)?;
    let (header, img) = find_image_hdu(&fits, path)?;
    let preview =
        render_preview_with_progress(header, &img, p, |stage| report(stage_label(stage)))?;
    Ok(LoadedDoc::build(header, &img, preview))
}

/// The right-hand status label for a render stage.
fn stage_label(stage: PreviewStage) -> &'static str {
    match stage {
        PreviewStage::Debayering => "Debayering",
        PreviewStage::Stretching => "Stretching",
    }
}

/// Whether `req` is still the latest request; stale results are dropped so the
/// newest selection always wins.
fn is_current(req: u64) -> bool {
    STATE.with(|s| s.borrow().generation == req)
}

/// Cache a freshly loaded document (always, so the work isn't wasted) and
/// display it only if the selection hasn't moved on.
fn finish_load(app: &AppWindow, path: PathBuf, outcome: Result<LoadedDoc>, req: u64) {
    match outcome {
        Ok(doc) => {
            let doc = Rc::new(doc);
            let cost = doc.preview.rgba8.len();
            STATE.with(|s| s.borrow_mut().cache.put(path.clone(), doc.clone(), cost));
            update_memory(app);
            if is_current(req) {
                display_doc(app, &path, &doc);
            }
        }
        Err(e) => {
            set_row_status(&path, "error", &e.to_string());
            if is_current(req) {
                app.set_busy(false);
                app.set_status_text(format!("Failed to open {}: {e}", display_name(&path)).into());
                app.set_stage_text("".into());
                // Don't let a broken file stall an in-progress blink loop.
                schedule_next_blink(app);
            }
        }
    }
}

/// Show a loaded document on screen — image, header table and stats panel — and,
/// if blink is running, arm the next advance.
fn display_doc(app: &AppWindow, path: &Path, doc: &LoadedDoc) {
    view::show_doc(app, doc);
    app.set_busy(false);
    app.set_status_text(
        format!(
            "{}   {}×{}",
            display_name(path),
            doc.preview.width,
            doc.preview.height
        )
        .into(),
    );
    app.set_stage_text("".into()); // pipeline finished for this frame
    schedule_next_blink(app);
}

/// Update a file row's status badge (e.g. mark a failed load "error") and its
/// error message (shown as a tooltip; pass "" for none).
fn set_row_status(path: &Path, status: &str, error: &str) {
    let target = path.to_string_lossy();
    STATE.with(|s| {
        let model = &s.borrow().files_model;
        for i in 0..model.row_count() {
            if let Some(mut row) = model.row_data(i)
                && row.path.as_str() == target.as_ref()
            {
                row.status = status.into();
                row.error = error.into();
                model.set_row_data(i, row);
                break;
            }
        }
    });
}

/// Flip a file row's `checked` (selection) state. Driven by the row's checkbox
/// click and by pressing Space on the highlighted row; the toggled state feeds
/// straight back into the list via the model binding.
pub fn toggle_check(_app: &AppWindow, index: i32) {
    if index < 0 {
        return;
    }
    STATE.with(|s| toggle_check_row(&s.borrow().files_model, index as usize));
}

/// Flip the `checked` flag on one row of the file model. A no-op for an
/// out-of-range index. Split out from [`toggle_check`] so it needs no window.
fn toggle_check_row(model: &VecModel<FileRow>, index: usize) {
    if let Some(mut row) = model.row_data(index) {
        row.checked = !row.checked;
        model.set_row_data(index, row);
    }
}

/// Check every row in the working set — Edit ▸ Select All (Ctrl/Cmd+A).
pub fn select_all(_app: &AppWindow) {
    STATE.with(|s| set_all_checked(&s.borrow().files_model, true));
}

/// Uncheck every row in the working set — Edit ▸ Deselect All (Ctrl/Cmd+D).
pub fn deselect_all(_app: &AppWindow) {
    STATE.with(|s| set_all_checked(&s.borrow().files_model, false));
}

/// Set every file row's `checked` flag to `checked`, only rewriting rows that
/// actually change (avoiding needless model updates). Split out from
/// [`select_all`]/[`deselect_all`] so it needs no window and is unit-testable.
fn set_all_checked(model: &VecModel<FileRow>, checked: bool) {
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i)
            && row.checked != checked
        {
            row.checked = checked;
            model.set_row_data(i, row);
        }
    }
}

// --- blink ---------------------------------------------------------------

/// Start or stop blink. Starting advances immediately; from then on each frame
/// arms the next advance once it is displayed (see [`schedule_next_blink`]).
pub fn set_blinking(app: &AppWindow, on: bool) {
    if on {
        advance_blink(app);
    } else {
        STATE.with(|s| s.borrow().blink_timer.stop());
    }
}

/// If blink is on, arm a one-shot timer to advance after the dwell. Called after
/// every displayed frame, so the loop only ever runs one load at a time.
fn schedule_next_blink(app: &AppWindow) {
    if !app.get_blinking() {
        return;
    }
    let weak = app.as_weak();
    STATE.with(|s| {
        s.borrow()
            .blink_timer
            .start(TimerMode::SingleShot, BLINK_DWELL, move || {
                if let Some(app) = weak.upgrade() {
                    advance_blink(&app);
                }
            });
    });
}

/// Advance the selection to the next file, wrapping around.
fn advance_blink(app: &AppWindow) {
    let next = STATE.with(|s| {
        let st = s.borrow();
        let len = st.paths.len();
        (len > 0).then(|| next_index(st.selected.unwrap_or(0), len))
    });
    if let Some(index) = next {
        select_file(app, index as i32);
    }
}

// --- compress / decompress ----------------------------------------------

/// The two file operations the Edit menu drives. `Compress` carries the
/// chosen tile-compression algorithm; `Decompress` needs no parameters.
#[derive(Clone, Copy)]
enum Operation {
    Compress(CompressionType),
    Decompress,
}

impl Operation {
    /// Present-progressive verb for the status bar ("Compressing" / …).
    fn progressive(self) -> &'static str {
        match self {
            Operation::Compress(_) => "Compressing",
            Operation::Decompress => "Decompressing",
        }
    }

    /// Past-tense noun for the completion summary ("Compressed" / …).
    fn past(self) -> &'static str {
        match self {
            Operation::Compress(_) => "Compressed",
            Operation::Decompress => "Decompressed",
        }
    }
}

/// Map a compress-dialog algorithm index (the ComboBox order) to a fitskit
/// compression type. Falls back to Rice for any out-of-range index.
fn algorithm_for_index(index: i32) -> CompressionType {
    match index {
        1 => CompressionType::Gzip1,
        2 => CompressionType::Gzip2,
        _ => CompressionType::Rice1,
    }
}

/// The working-set paths a bulk operation applies to: the checked rows, or the
/// whole set when nothing is checked, kept to those matching `predicate` (e.g.
/// only already-compressed files for decompress).
fn operation_targets(predicate: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    STATE.with(|s| {
        let st = s.borrow();
        let model = &st.files_model;
        let any_checked =
            (0..model.row_count()).any(|i| model.row_data(i).is_some_and(|r| r.checked));
        st.paths
            .iter()
            .enumerate()
            .filter(|(i, _)| !any_checked || model.row_data(*i).is_some_and(|r| r.checked))
            .map(|(_, p)| p.clone())
            .filter(|p| predicate(p.as_path()))
            .collect()
    })
}

/// Open the Compress dialog: count the files it would compress (every target
/// that isn't already `.fz`), reset the shared settings, and show it.
pub fn open_compress_dialog(app: &AppWindow) {
    let count = operation_targets(|p| !is_compressed(p)).len();
    app.set_compress_count(count as i32);
    reset_op_fields(app);
    app.set_show_compress(true);
}

/// Open the Decompress dialog: count the files it would decompress (every `.fz`
/// target), reset the shared settings, and show it.
pub fn open_decompress_dialog(app: &AppWindow) {
    let count = operation_targets(is_compressed).len();
    app.set_decompress_count(count as i32);
    reset_op_fields(app);
    app.set_show_decompress(true);
}

/// Restore the shared dialog settings to their defaults before showing a dialog.
fn reset_op_fields(app: &AppWindow) {
    app.set_op_algorithm(0);
    app.set_op_keep_source(true);
    app.set_op_use_custom_dir(false);
    app.set_op_output_dir("".into());
}

/// Open the native folder picker for the "different directory" field, writing
/// the chosen path back into the dialog's text field.
pub fn browse_output_dir(app: &AppWindow) {
    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
        app.set_op_output_dir(dir.to_string_lossy().into_owned().into());
    }
}

/// The output directory the dialog selected, or `None` for "beside the source".
/// Returns an error message when a custom directory is requested but the field
/// is empty or doesn't name an existing directory.
fn dialog_output_dir(app: &AppWindow) -> Result<Option<PathBuf>, String> {
    if !app.get_op_use_custom_dir() {
        return Ok(None);
    }
    let text = app.get_op_output_dir();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("Choose an output directory first".into());
    }
    let dir = PathBuf::from(trimmed);
    if !dir.is_dir() {
        return Err(format!("Not a directory: {trimmed}"));
    }
    Ok(Some(dir))
}

/// Confirm the Compress dialog: gather its settings and start the batch.
pub fn run_compress(app: &AppWindow) {
    let op = Operation::Compress(algorithm_for_index(app.get_op_algorithm()));
    start_operation(app, op, |p| !is_compressed(p), |app| app.set_show_compress(false));
}

/// Confirm the Decompress dialog: gather its settings and start the batch.
pub fn run_decompress(app: &AppWindow) {
    start_operation(app, Operation::Decompress, is_compressed, |app| {
        app.set_show_decompress(false)
    });
}

/// Shared confirm handler: validate the destination, hide the dialog, and spawn
/// the batch. On an invalid custom directory the dialog stays open with the
/// reason in the status bar.
fn start_operation(
    app: &AppWindow,
    op: Operation,
    predicate: impl Fn(&Path) -> bool,
    hide: impl Fn(&AppWindow),
) {
    let output_dir = match dialog_output_dir(app) {
        Ok(dir) => dir,
        Err(msg) => {
            app.set_status_text(msg.into());
            return;
        }
    };
    // A custom output directory always leaves the originals in place.
    let keep = output_dir.is_some() || app.get_op_keep_source();
    let targets = operation_targets(predicate);
    hide(app);
    if targets.is_empty() {
        return;
    }
    spawn_operation(app.as_weak(), op, targets, output_dir, keep);
}

/// Run a compress/decompress batch on a worker thread, reporting per-file
/// progress and marshaling each result back to the UI thread — a replaced file
/// updates its working-set row in place, a failed one is badged with the error.
fn spawn_operation(
    weak: Weak<AppWindow>,
    op: Operation,
    targets: Vec<PathBuf>,
    output_dir: Option<PathBuf>,
    keep: bool,
) {
    let _ = weak.upgrade_in_event_loop(|app| {
        app.set_busy(true);
        app.set_stage_text("".into());
    });
    std::thread::spawn(move || {
        let total = targets.len();
        let mut ok = 0usize;
        let mut failed = 0usize;
        for (i, input) in targets.into_iter().enumerate() {
            let status = format!(
                "{}: {} ({}/{})",
                op.progressive(),
                display_name(&input),
                i + 1,
                total
            );
            let weak_status = weak.clone();
            let _ = weak_status.upgrade_in_event_loop(move |app| app.set_status_text(status.into()));

            match process_one(op, &input, output_dir.as_deref(), keep) {
                Ok(new_path) => {
                    ok += 1;
                    // In replace mode (source removed) the working-set entry now
                    // points at the new file; refresh its row on the UI thread.
                    if !keep {
                        let _ = weak.upgrade_in_event_loop(move |app| {
                            replace_working_path(&app, &input, &new_path);
                        });
                    }
                }
                Err(e) => {
                    failed += 1;
                    let msg = e.to_string();
                    let _ = weak.upgrade_in_event_loop(move |_app| {
                        set_row_status(&input, "error", &msg);
                    });
                }
            }
        }
        let _ = weak.upgrade_in_event_loop(move |app| {
            app.set_busy(false);
            app.set_stage_text("".into());
            app.set_status_text(
                format!("{} {ok} file(s), {failed} failed", op.past()).into(),
            );
            update_memory(&app);
        });
    });
}

/// Compress or decompress one file: derive its output path, do the work via
/// `fitz-core`, write the result, and (in replace mode) delete the source.
/// Returns the path of the file that was written. Runs on a worker thread.
fn process_one(
    op: Operation,
    input: &Path,
    output_dir: Option<&Path>,
    keep: bool,
) -> Result<PathBuf> {
    let (output, out_fits) = match op {
        Operation::Compress(algorithm) => {
            let opts = fitz_core::compress::CompressOptions { algorithm };
            (
                compressed_output_path(input, output_dir),
                fitz_core::compress::compress(input, &opts)?,
            )
        }
        Operation::Decompress => (
            decompressed_output_path(input, output_dir),
            fitz_core::decompress::decompress(input)?,
        ),
    };
    out_fits
        .to_file(&output)
        .with_context(|| format!("cannot write {}", output.display()))?;
    // Replace mode removes the source, but never a file we just wrote onto.
    if !keep && output != input {
        std::fs::remove_file(input)
            .with_context(|| format!("cannot remove {}", input.display()))?;
    }
    Ok(output)
}

/// Point a working-set entry at the file that replaced it: update the path and
/// its list row, and drop the now-stale rendered preview from the cache. A
/// no-op if the old path is no longer in the set (e.g. it was cleared).
fn replace_working_path(app: &AppWindow, old: &Path, new: &Path) {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        if let Some(i) = st.paths.iter().position(|p| p == old) {
            st.paths[i] = new.to_path_buf();
            st.files_model.set_row_data(i, make_row(new));
        }
        st.cache.remove(&old.to_path_buf());
    });
    update_memory(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str) -> FileRow {
        FileRow {
            name: name.into(),
            status: "".into(),
            path: name.into(),
            error: "".into(),
            checked: false,
        }
    }

    #[test]
    fn toggle_check_row_flips_only_the_target_row() {
        let model = VecModel::from(vec![row("a"), row("b"), row("c")]);
        toggle_check_row(&model, 1);
        assert!(!model.row_data(0).unwrap().checked);
        assert!(model.row_data(1).unwrap().checked);
        assert!(!model.row_data(2).unwrap().checked);

        // Toggling again clears it; an out-of-range index is a no-op.
        toggle_check_row(&model, 1);
        assert!(!model.row_data(1).unwrap().checked);
        toggle_check_row(&model, 9);
        assert_eq!(model.row_count(), 3);
    }

    #[test]
    fn format_bytes_picks_a_sensible_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2 * 1024), "2 KB");
        assert_eq!(format_bytes(36 * 1024 * 1024), "36 MB");
        // The 1 GiB cache budget reads as "1.0 GB".
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(format_bytes(1536 * 1024 * 1024), "1.5 GB");
    }

    #[test]
    fn set_all_checked_sets_every_row() {
        let model = VecModel::from(vec![row("a"), row("b"), row("c")]);
        toggle_check_row(&model, 1); // start with a mixed state

        set_all_checked(&model, true);
        assert!((0..3).all(|i| model.row_data(i).unwrap().checked));

        set_all_checked(&model, false);
        assert!((0..3).all(|i| !model.row_data(i).unwrap().checked));
    }

    #[test]
    fn removal_targets_prefers_checked_else_highlighted() {
        // Checked rows win, sorted and de-duplicated, ignoring the highlight.
        assert_eq!(removal_targets([2, 0, 2].into_iter(), Some(1)), vec![0, 2]);
        // No checks → just the highlighted row.
        assert_eq!(removal_targets([].into_iter(), Some(3)), vec![3]);
        // Nothing checked and nothing highlighted → nothing to remove.
        assert_eq!(removal_targets([].into_iter(), None), Vec::<usize>::new());
    }

    #[test]
    fn next_selection_rehomes_the_highlight() {
        // The highlighted file survived → follow it to its new index.
        assert_eq!(next_selection(3, Some(1), Some(2)), Some(1));
        // It was removed → clamp the old index into the shrunken list.
        assert_eq!(next_selection(2, None, Some(5)), Some(1));
        assert_eq!(next_selection(3, None, Some(1)), Some(1));
        // Nothing highlighted before → land on the first row.
        assert_eq!(next_selection(3, None, None), Some(0));
        // The set emptied → clear the highlight.
        assert_eq!(next_selection(0, None, Some(0)), None);
    }

    #[test]
    fn algorithm_index_maps_to_compression_type() {
        assert!(matches!(algorithm_for_index(0), CompressionType::Rice1));
        assert!(matches!(algorithm_for_index(1), CompressionType::Gzip1));
        assert!(matches!(algorithm_for_index(2), CompressionType::Gzip2));
        // Out-of-range falls back to Rice.
        assert!(matches!(algorithm_for_index(99), CompressionType::Rice1));
    }

    /// Absolute path to a bundled `test-data/` fixture.
    fn test_data(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("test-data").join(name)
    }

    #[test]
    fn process_one_replaces_source_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("frame.fit");
        std::fs::copy(test_data("uncompressed.fit"), &input).unwrap();

        // Compress in replace mode: source removed, `.fz` written beside it.
        let compressed =
            process_one(Operation::Compress(CompressionType::Rice1), &input, None, false).unwrap();
        assert_eq!(compressed, tmp.path().join("frame.fit.fz"));
        assert!(compressed.is_file());
        assert!(!input.exists(), "source should be removed in replace mode");

        // Decompress it back in replace mode: `.fz` removed, `.fit` restored.
        let restored = process_one(Operation::Decompress, &compressed, None, false).unwrap();
        assert_eq!(restored, input);
        assert!(restored.is_file());
        assert!(!compressed.exists());
    }

    #[test]
    fn process_one_keep_mode_leaves_source_and_writes_to_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("frame.fit");
        std::fs::copy(test_data("uncompressed.fit"), &input).unwrap();
        let out_dir = tmp.path().join("out");
        std::fs::create_dir(&out_dir).unwrap();

        let compressed = process_one(
            Operation::Compress(CompressionType::Gzip1),
            &input,
            Some(&out_dir),
            true,
        )
        .unwrap();
        assert_eq!(compressed, out_dir.join("frame.fit.fz"));
        assert!(compressed.is_file());
        assert!(input.exists(), "source must be kept when writing to a directory");
    }
}
