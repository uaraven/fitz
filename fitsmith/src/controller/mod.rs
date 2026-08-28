//! Application logic bridging the Slint UI to `libfitz`. Files are decoded off
//! the UI thread; their headers/stats/star metrics ([`crate::doc::FileMeta`])
//! stay resident for the life of the working set, while rendered previews are
//! kept in a byte-budgeted LRU keyed by toggle state so re-selecting or
//! blinking back to a previously-seen combination redisplays instantly, with
//! no re-decode. A generation counter drops stale results when the user
//! scrubs faster than frames can render. Turning a document into UI
//! properties is [`crate::view`]'s job; this module owns state, threading and
//! blink.
//!
//! The controller is split by concern:
//!
//! - this module ([`mod@self`]) — the shared [`AppState`] and its thread-local,
//!   the memory readout, working-set management (open / add / remove / clear),
//!   the checkbox selection, and the helpers the other submodules lean on
//!   ([`operation_targets`], [`set_row_status`], [`algorithm_for_index`], …);
//! - [`viewer`] — selecting, navigating, loading/rendering off-thread, and blink;
//! - [`convert`] — the compress / decompress batch operations;
//! - [`export`] — the export dialog and its batch;
//! - [`analytics`] — the analytics batch and its time-series chart;
//! - [`delete_files`] — Tools ▸ Delete Files…: delete or rename the checked files.

mod analytics;
mod bad_frames;
mod convert;
mod delete_files;
mod export;
mod inspect;
pub(crate) mod metrics;
mod support;
mod viewer;

pub use analytics::*;
pub use bad_frames::*;
pub use convert::*;
pub use delete_files::*;
pub use export::*;
pub use inspect::*;
pub use viewer::*;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use libfitz::fitskit::CompressionType;
use metrics::{FileMetrics, MetricFamily};
use rayon::prelude::*;
use slint::{ComponentHandle, Model, ModelRc, Timer, VecModel, Weak};

use crate::controller::support::load_exposure;
use crate::doc::FileMeta;
use crate::files::{display_name, expand_inputs, is_compressed, scan_directory};
use crate::{AppWindow, FileRow, view};

/// Key for the rendered-preview cache: a path plus the toggle state it was
/// rendered under. Keying on the toggles (rather than clearing the cache on
/// every flip) means switching back to a previously-seen setting redisplays
/// instantly, and a toggle flip to a new combination is a plain reload — see
/// `viewer::load_and_render`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct PreviewKey {
    pub path: PathBuf,
    pub debayer: bool,
    pub stretch: bool,
    pub invert: bool,
}

/// All UI-thread application state. Lives in a thread-local because Slint is
/// single-threaded: every mutation happens either from a callback or from a
/// worker's `upgrade_in_event_loop` closure, both of which run here.
struct AppState {
    /// The working set, in list order.
    paths: Vec<PathBuf>,
    /// The `[FileRow]` model backing the list view (mirrors `paths`).
    files_model: Rc<VecModel<FileRow>>,
    /// Headers, curated info, pixel statistics and star metrics, keyed by
    /// path *and* the debayer state they describe — resident for as long as a
    /// path stays in the working set, never evicted by an LRU (see
    /// `doc::FileMeta`). Keyed like [`PreviewKey`] (rather than invalidated on
    /// a flip) so toggling debayer back and forth on a CFA source measures
    /// each rendering once; a non-CFA source's toggle-independent metadata is
    /// stored under both keys. Access via [`meta_lookup`] / [`meta_store`].
    meta: HashMap<(PathBuf, bool), Rc<FileMeta>>,
    /// Rendered display buffers, keyed by path *and* the debayer/stretch
    /// toggle state they were rendered under. Byte-budgeted LRU; a toggle
    /// flip to an unseen combination is a plain reload, not a cache
    /// invalidation.
    previews: crate::cache::LruCache<PreviewKey, Rc<image::RgbImage>>,
    /// Currently selected index into `paths`, if any.
    selected: Option<usize>,
    /// Bumped on every selection/re-render request; a worker result is applied
    /// only if its captured generation still matches (stale-result coalescing).
    generation: u64,
    /// One-shot timer that advances blink after the current frame's dwell.
    blink_timer: Timer,
    /// Every analyzed frame's metrics for the open Analytics dialog, collected
    /// once so switching the plotted metric needs no file re-read. Cleared when
    /// the dialog closes.
    analytics: Vec<FileMetrics>,
    /// Per-file analysis results, kept for the lifetime of the working set so
    /// reopening the Analysis dialog re-reads nothing. Each entry is stamped
    /// with the file's size/mtime and revalidated on lookup, so a file rewritten
    /// under us reads as a miss.
    ///
    /// Deliberately unbounded and deliberately absent from the `update_memory`
    /// readout: one entry is a couple hundred bytes against a cached preview's
    /// megabytes, so even ten thousand frames sit below that readout's noise
    /// floor, and evicting would reintroduce the re-reads this cache exists to
    /// remove.
    analytics_cache: analytics::AnalyticsCache,
    /// Guards analytics batches specifically — kept apart from `generation` so
    /// that merely selecting a file mid-batch doesn't discard its results.
    analytics_generation: u64,
    /// Raised to ask the running analytics worker to stop between files. Each
    /// batch gets a fresh flag, so cancelling one can't silence the next.
    analytics_cancel: Arc<AtomicBool>,
    /// Which family the open chart dialog is showing — i.e. which of the two
    /// menu entries opened it. Decides the dropdown's metrics, whether the
    /// batch detects stars, and the export file-name prefix.
    analytics_family: MetricFamily,
    /// Every measured frame behind the open "Detect bad frames" dialog, so a
    /// knob change re-evaluates in memory with no file re-read. Cleared when
    /// the dialog closes.
    bad_frames: Vec<FileMetrics>,
    /// The paths the bad-frame dialog currently flags, in working-set order —
    /// what its Select button checks in the file list.
    bad_frame_flagged: Vec<PathBuf>,
    /// Raised to ask the running export / compress / decompress batch worker to
    /// stop between files. Each batch gets a fresh flag so cancelling one can't
    /// silence the next; the batches are modal (each runs behind a blocking
    /// progress overlay), so at most one is live and one flag serves them all.
    batch_cancel: Arc<AtomicBool>,
    /// Bumped when a file batch starts, so a superseded worker's final summary
    /// can't touch the UI of a later batch.
    batch_generation: u64,
}

impl AppState {
    fn new() -> Self {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_memory();
        let max_mem = sys.available_memory();
        // Budget the preview cache at 80% of the memory available at startup,
        // so plenty of full-frame images stay resident for instant blink /
        // re-selection without ever budgeting more than the machine can give.
        // The `.max(1)` only satisfies the LRU's positive-capacity assert when
        // the available memory can't be read at all. `meta` is deliberately
        // unbudgeted — see its field comment.
        // TODO: make this user-configurable.
        let cache_capacity = ((max_mem * 4 / 5) as usize).max(1);

        Self {
            paths: Vec::new(),
            files_model: Rc::new(VecModel::default()),
            meta: HashMap::new(),
            previews: crate::cache::LruCache::new(cache_capacity),
            selected: None,
            generation: 0,
            blink_timer: Timer::default(),
            analytics: Vec::new(),
            analytics_cache: analytics::AnalyticsCache::new(),
            analytics_generation: 0,
            analytics_cancel: Arc::new(AtomicBool::new(false)),
            analytics_family: MetricFamily::Pixel,
            bad_frames: Vec::new(),
            bad_frame_flagged: Vec::new(),
            batch_cancel: Arc::new(AtomicBool::new(false)),
            batch_generation: 0,
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
        (st.previews.total_bytes(), st.previews.capacity())
    });
    app.set_memory_text(
        format!(
            "Memory: {} / {}",
            format_bytes(used),
            format_bytes(capacity)
        )
        .into(),
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

/// The resident metadata valid under the given debayer toggle state, if any.
fn meta_lookup(
    meta: &HashMap<(PathBuf, bool), Rc<FileMeta>>,
    path: &Path,
    debayer: bool,
) -> Option<Rc<FileMeta>> {
    meta.get(&(path.to_path_buf(), debayer)).cloned()
}

/// Store freshly built metadata under every debayer state it is valid for:
/// just the one it was built under for a CFA source, both for anything else
/// (where the toggle is a no-op and one measurement serves both states).
fn meta_store(meta: &mut HashMap<(PathBuf, bool), Rc<FileMeta>>, path: &Path, m: Rc<FileMeta>) {
    match m.debayered {
        Some(state) => {
            meta.insert((path.to_path_buf(), state), m);
        }
        None => {
            meta.insert((path.to_path_buf(), false), m.clone());
            meta.insert((path.to_path_buf(), true), m);
        }
    }
}

/// Snapshot the debayer/stretch toggle state from the UI as `(debayer,
/// stretch, invert)`
fn view_toggles(app: &AppWindow) -> (bool, bool, bool) {
    (
        app.get_debayer_enabled(),
        app.get_stretch_enabled(),
        app.get_invert_enabled(),
    )
}

/// Build the list row for a path: base name plus a "compressed" badge for `.fz`.
fn make_row(path: &Path, exposure: f32) -> FileRow {
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
        exposure,
    }
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

// --- opening files -------------------------------------------------------

/// Prompt for one or more FITS files, add them to the working set, and select
/// the first newly added one.
pub fn open_file(app: &AppWindow) {
    if let Some(paths) = rfd::FileDialog::new()
        .add_filter("FITS images", &["fit", "fits", "fts", "fz"])
        .add_filter("All files", &["*"])
        .pick_files()
    {
        add_and_select(app, paths);
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
    let (target, added) = add_paths(paths);
    let target = target.or_else(|| index_of(&first));
    if let Some(index) = target {
        select_file(app, index as i32);
    }
    update_exposure(app);
    spawn_exposure_load(app.as_weak(), added);
}

/// Append any paths not already in the working set to both `paths` and the list
/// model. Returns the index of the first newly added path (for auto-select, or
/// `None` if every path was already present), plus every newly added path so
/// the caller can kick off an async exposure load for them.
fn add_paths(new_paths: Vec<PathBuf>) -> (Option<usize>, Vec<PathBuf>) {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        let mut first_added = None;
        let mut added = Vec::new();
        for path in new_paths {
            if st.paths.iter().any(|p| p == &path) {
                continue;
            }
            // Exposure is filled in asynchronously once its header has been
            // read off the UI thread — see `spawn_exposure_load`.
            st.files_model.push(make_row(&path, 0.0));
            st.paths.push(path.clone());
            first_added.get_or_insert(st.paths.len() - 1);
            added.push(path);
        }
        (first_added, added)
    })
}

fn index_of(path: &Path) -> Option<usize> {
    STATE.with(|s| s.borrow().paths.iter().position(|p| p == path))
}

/// Read the header of each newly added path off the UI thread
/// and fill in its row's exposure once done, then refresh the status
/// bar totals. A no-op for an empty list.
fn spawn_exposure_load(weak: Weak<AppWindow>, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let loaded: Vec<(PathBuf, f32)> = paths
            .into_par_iter()
            .map(|path| {
                let exposure = load_exposure(&path);
                (path, exposure)
            })
            .collect();
        let _ = weak.upgrade_in_event_loop(move |app| apply_loaded_exposures(&app, loaded));
    });
}

/// Apply the results of `spawn_exposure_load` to the rows that are still in
/// the working set — a row removed, or the set cleared, while its header was
/// still loading is looked up by path and silently skipped, since the working
/// set may have changed shape by the time this runs — then refresh the
/// total/checked exposure readouts.
fn apply_loaded_exposures(app: &AppWindow, loaded: Vec<(PathBuf, f32)>) {
    STATE.with(|s| {
        let st = s.borrow();
        for (path, exposure) in loaded {
            if let Some(i) = st.paths.iter().position(|p| *p == path)
                && let Some(mut row) = st.files_model.row_data(i)
            {
                row.exposure = exposure;
                st.files_model.set_row_data(i, row);
            }
        }
    });
    update_exposure(app);
    app.window().request_redraw();
}

// --- removing / clearing files ------------------------------------------

/// Reset the window chrome to the empty state: no selection, not busy, the
/// "add files" prompt in the status bar, and a cleared view. Shared by the
/// clear-all path and the remove-that-empties-the-set path.
fn show_empty(app: &AppWindow) {
    app.set_blinking(false);
    app.set_selected_index(-1);
    app.set_busy(false);
    app.set_status_text("No image — add files to view".into());
    app.set_stage_text("".into());
    view::clear(app);
}

/// Remove every file from the working set and reset the view. Bumping the
/// generation makes any in-flight load land as stale and be dropped.
pub fn clear_files(app: &AppWindow) {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.blink_timer.stop();
        st.paths.clear();
        st.files_model.set_vec(Vec::new());
        st.meta.clear();
        st.previews.clear();
        // The analyses are kept for the lifetime of the working set, and this
        // is the end of one.
        st.analytics_cache.clear();
        st.selected = None;
        st.generation += 1;
    });
    show_empty(app);
    update_memory(app);
    update_exposure(app);
    update_checked_count(app);
}

/// Drop a path's resident metadata (both debayer states) and every cached
/// rendered preview for it (all eight debayer/stretch/invert combinations) —
/// e.g. when it leaves the working set, or is rewritten in place by
/// compress/decompress.
fn forget_path(st: &mut AppState, path: &Path) {
    for debayer in [false, true] {
        st.meta.remove(&(path.to_path_buf(), debayer));
        for stretch in [false, true] {
            for invert in [false, true] {
                st.previews.remove(&PreviewKey {
                    path: path.to_path_buf(),
                    debayer,
                    stretch,
                    invert,
                });
            }
        }
    }
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
fn next_selection(
    new_len: usize,
    survived: Option<usize>,
    old_index: Option<usize>,
) -> Option<usize> {
    if new_len == 0 {
        None
    } else if let Some(i) = survived {
        Some(i)
    } else {
        Some(old_index.unwrap_or(0).min(new_len - 1))
    }
}

/// Drop the given row indices from the working set (order and duplicates
/// don't matter — sorted and de-duplicated first, then removed
/// high-index-first so earlier indices stay valid), evicting each removed
/// file's cached preview. Any in-flight load is orphaned by the generation
/// bump. Returns the row to reselect afterward, or `None` if `targets` was
/// empty (nothing removed).
fn drop_rows(mut targets: Vec<usize>) -> Option<Option<usize>> {
    if targets.is_empty() {
        return None;
    }
    targets.sort_unstable();
    targets.dedup();

    STATE.with(|s| {
        let mut st = s.borrow_mut();
        let old_index = st.selected;
        let selected_path = st.selected.and_then(|i| st.paths.get(i).cloned());

        st.blink_timer.stop();
        for &i in targets.iter().rev() {
            let path = st.paths.remove(i);
            st.files_model.remove(i);
            forget_path(&mut st, &path);
            // Not needed for correctness — the stamp would catch a rewrite
            // anyway — but it stops a long session accumulating analyses for
            // files nobody has open.
            st.analytics_cache.remove(&path);
        }
        st.generation += 1;
        st.selected = None;

        let len = st.paths.len();
        let survived = selected_path.and_then(|p| st.paths.iter().position(|q| q == &p));
        Some(next_selection(len, survived, old_index))
    })
}

/// The shared epilogue after [`drop_rows`] actually removed something:
/// refresh the memory/exposure/checked readouts and re-home the highlight to
/// the row it resolved on (or show the empty state when the set emptied).
fn finish_removal(app: &AppWindow, target: Option<usize>) {
    update_memory(app);
    update_exposure(app);
    update_checked_count(app);
    match target {
        // `select_file` re-displays a surviving file straight from the cache.
        Some(index) => select_file(app, index as i32),
        None => show_empty(app),
    }
}

/// Remove the checked rows from the working set — or, when nothing is checked,
/// the highlighted row.
pub fn remove_selected(app: &AppWindow) {
    let targets = STATE.with(|s| {
        let st = s.borrow();
        let checked = (0..st.files_model.row_count())
            .filter(|&i| st.files_model.row_data(i).is_some_and(|r| r.checked));
        removal_targets(checked, st.selected)
    });
    let Some(target) = drop_rows(targets) else {
        return; // nothing was checked or highlighted
    };
    finish_removal(app, target);
}

// --- checkbox selection --------------------------------------------------

/// Flip a file row's `checked` (selection) state. Driven by the row's checkbox
/// click and by pressing Space on the highlighted row; the toggled state feeds
/// straight back into the list via the model binding.
pub fn toggle_check(_app: &AppWindow, index: i32) {
    if index < 0 {
        return;
    }
    STATE.with(|s| toggle_check_row(&s.borrow().files_model, index as usize));
    update_checked_count(_app);
    update_exposure(_app);
}

/// Flip the `checked` flag on one row of the file model. A no-op for an
/// out-of-range index. Split out from [`toggle_check`] so it needs no window.
fn toggle_check_row(model: &VecModel<FileRow>, index: usize) {
    if let Some(mut row) = model.row_data(index) {
        row.checked = !row.checked;
        model.set_row_data(index, row);
    }
}

pub fn count_exposure(files: &VecModel<FileRow>) -> (f32, f32) {
    files.iter().fold((0.0, 0.0), |acc, f| {
        (
            acc.0 + f.exposure,
            if f.checked { acc.1 + f.exposure } else { acc.1 },
        )
    })
}

pub fn count_checked_files(files: &VecModel<FileRow>) -> usize {
    files.iter().filter(|x| x.checked).count()
}

pub fn update_checked_count(_app: &AppWindow) {
    STATE.with(|s| {
        let count = count_checked_files(&s.borrow().files_model);
        _app.set_checked_file_count(count as i32);
    })
}

pub fn update_exposure(_app: &AppWindow) {
    STATE.with(|s| {
        let exp = count_exposure(&s.borrow().files_model);
        _app.set_total_exposure(exp.0 as i32);
        _app.set_checked_exposure(exp.1 as i32)
    })
}

/// Check every row in the working set — Tools ▸ Select All (Ctrl/Cmd+A).
pub fn select_all(_app: &AppWindow) {
    STATE.with(|s| set_all_checked(&s.borrow().files_model, true));
    update_checked_count(_app);
    update_exposure(_app);
}

/// Uncheck every row in the working set — Tools ▸ Deselect All (Ctrl/Cmd+D).
pub fn deselect_all(_app: &AppWindow) {
    STATE.with(|s| set_all_checked(&s.borrow().files_model, false));
    update_checked_count(_app);
    update_exposure(_app);
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

/// Check exactly the rows whose path is in `paths` and uncheck every other
/// row, only rewriting rows that actually change (like [`set_all_checked`]).
/// The bad-frame dialog's Select button uses this to turn its verdict into the
/// file-list selection.
fn set_checked_paths(model: &VecModel<FileRow>, paths: &std::collections::HashSet<String>) {
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i) {
            let want = paths.contains(row.path.as_str());
            if row.checked != want {
                row.checked = want;
                model.set_row_data(i, row);
            }
        }
    }
}

// --- shared batch helpers ------------------------------------------------

/// Map a compress-dialog algorithm index (the ComboBox order) to a fitskit
/// compression type. Falls back to Rice for any out-of-range index. Shared by
/// the [`convert`] and [`export`] dialogs.
fn algorithm_for_index(index: i32) -> CompressionType {
    match index {
        1 => CompressionType::Gzip1,
        2 => CompressionType::Gzip2,
        _ => CompressionType::Rice1,
    }
}

/// Validate a user-typed output directory: trim it, reject empty (with
/// `empty_msg`), and require it to name an existing directory. Returns the path
/// or a message for the status bar. Shared by the [`convert`] and [`export`]
/// dialogs' destination fields.
fn require_existing_dir(text: &str, empty_msg: &'static str) -> Result<PathBuf, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(empty_msg.into());
    }
    let dir = PathBuf::from(trimmed);
    if !dir.is_dir() {
        return Err(format!("Not a directory: {trimmed}"));
    }
    Ok(dir)
}

/// Absolute path to a bundled `test-data/` fixture. Shared by the controller
/// submodules' and [`crate::doc`]'s tests so they exercise real FITS frames.
#[cfg(test)]
pub(crate) fn test_data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("test-data")
        .join(name)
}

/// Start a new modal file batch (export / compress / decompress): stop any
/// straggling worker, and hand back the new generation and its fresh cancel
/// flag. The worker checks the flag between files and gates its final summary
/// on the generation still being current.
fn begin_file_batch() -> (u64, Arc<AtomicBool>) {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.batch_cancel.store(true, Ordering::Relaxed);
        st.batch_generation += 1;
        st.batch_cancel = Arc::new(AtomicBool::new(false));
        (st.batch_generation, st.batch_cancel.clone())
    })
}

/// Ask the running file batch to stop. The worker still finishes the file in
/// flight, then reports how far it got in the status bar.
fn raise_batch_cancel() {
    STATE.with(|s| s.borrow().batch_cancel.store(true, Ordering::Relaxed));
}

/// Whether `generation` is still the live file batch; a superseded worker's
/// final summary is dropped so it can't touch a later batch's UI.
fn batch_is_current(generation: u64) -> bool {
    STATE.with(|s| s.borrow().batch_generation == generation)
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
            exposure: 0.0,
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
    fn count_exposure_sums_total_and_checked_only() {
        let model = VecModel::from(vec![
            FileRow {
                exposure: 30.0,
                ..row("a")
            },
            FileRow {
                checked: true,
                exposure: 60.0,
                ..row("b")
            },
            FileRow {
                checked: true,
                exposure: 10.0,
                ..row("c")
            },
        ]);
        // Total sums every row; the checked total only the checked ones.
        assert_eq!(count_exposure(&model), (100.0, 70.0));
    }

    #[test]
    fn count_exposure_is_zero_for_an_empty_or_all_unchecked_model() {
        let empty = VecModel::from(Vec::<FileRow>::new());
        assert_eq!(count_exposure(&empty), (0.0, 0.0));

        let none_checked = VecModel::from(vec![
            FileRow {
                exposure: 30.0,
                ..row("a")
            },
            FileRow {
                exposure: 60.0,
                ..row("b")
            },
        ]);
        assert_eq!(count_exposure(&none_checked), (90.0, 0.0));
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
    fn set_checked_paths_checks_exactly_the_named_rows() {
        let model = VecModel::from(vec![row("a"), row("b"), row("c")]);
        toggle_check_row(&model, 0); // "a" starts checked and must be cleared

        let paths: std::collections::HashSet<String> =
            ["b".to_string(), "missing".to_string()].into();
        set_checked_paths(&model, &paths);
        assert!(!model.row_data(0).unwrap().checked);
        assert!(model.row_data(1).unwrap().checked);
        assert!(!model.row_data(2).unwrap().checked);

        // An empty set clears everything.
        set_checked_paths(&model, &std::collections::HashSet::new());
        assert!((0..3).all(|i| !model.row_data(i).unwrap().checked));
    }

    #[test]
    fn cfa_meta_is_cached_per_debayer_state_and_non_cfa_serves_both() {
        // Real frames: a CFA mosaic and an already-debayered RGB cube.
        let cfa_path = test_data("cfa_orion.fits");
        let rgb_path = test_data("rgb.fits");
        let mosaic = libfitz::fits_file::load_fits(&cfa_path).unwrap();
        let rgb = libfitz::fits_file::load_fits(&rgb_path).unwrap();

        let mut meta = HashMap::new();

        // A CFA measurement lands only under the state it was built for.
        let m = Rc::new(FileMeta::build(&mosaic, Some(false)));
        meta_store(&mut meta, &cfa_path, m.clone());
        assert!(Rc::ptr_eq(
            &meta_lookup(&meta, &cfa_path, false).unwrap(),
            &m
        ));
        assert!(meta_lookup(&meta, &cfa_path, true).is_none());

        // The other state's measurement is kept alongside, not replacing it.
        let d = Rc::new(FileMeta::build(
            &mosaic.debayer().unwrap().unwrap(),
            Some(true),
        ));
        meta_store(&mut meta, &cfa_path, d.clone());
        assert!(Rc::ptr_eq(
            &meta_lookup(&meta, &cfa_path, false).unwrap(),
            &m
        ));
        assert!(Rc::ptr_eq(
            &meta_lookup(&meta, &cfa_path, true).unwrap(),
            &d
        ));

        // A non-CFA measurement serves both toggle states.
        let r = Rc::new(FileMeta::build(&rgb, None));
        meta_store(&mut meta, &rgb_path, r.clone());
        assert!(Rc::ptr_eq(
            &meta_lookup(&meta, &rgb_path, false).unwrap(),
            &r
        ));
        assert!(Rc::ptr_eq(
            &meta_lookup(&meta, &rgb_path, true).unwrap(),
            &r
        ));
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
}
