//! Application logic bridging the Slint UI to `libfitz`. Files are decoded and
//! rendered off the UI thread into display-ready [`LoadedDoc`]s (preview +
//! headers + stats), kept in a byte-budgeted LRU cache so re-selecting or
//! blinking back to a file re-displays instantly, with no re-decode. A
//! generation counter drops stale results when the user scrubs faster than
//! frames can render. Turning a document into UI properties is [`crate::view`]'s
//! job; this module owns state, threading and blink.
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
//! - [`analytics`] — the analytics batch and its time-series chart.

mod analytics;
mod convert;
mod export;
mod viewer;

pub use analytics::*;
pub use convert::*;
pub use export::*;
pub use viewer::*;

use std::cell::RefCell;
use std::cmp::max;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use libfitz::analytics::{FileMetrics, MetricFamily};
use libfitz::fitskit::CompressionType;
use libfitz::preview::PreviewParams;
use slint::{Model, ModelRc, Timer, VecModel};

use crate::doc::LoadedDoc;
use crate::files::{display_name, expand_inputs, is_compressed, scan_directory};
use crate::{AppWindow, FileRow, view};

/// Maximum total size of rendered previews to keep cached. Sized so a handful
/// of full-frame images stay resident for instant blink / re-selection.
/// TODO: make this user-configurable.
const CACHE_CAPACITY_BYTES: usize = 1024 * 1024 * 1024;

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
    /// Every analyzed frame's metrics for the open Analytics dialog, collected
    /// once so switching the plotted metric needs no file re-read. Cleared when
    /// the dialog closes.
    analytics: Vec<FileMetrics>,
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
}

impl AppState {
    fn new() -> Self {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_memory();
        let max_mem = sys.available_memory();
        let cache_capacity = max(CACHE_CAPACITY_BYTES, (max_mem * 4 / 5) as usize);

        Self {
            paths: Vec::new(),
            files_model: Rc::new(VecModel::default()),
            cache: crate::cache::LruCache::new(cache_capacity),
            selected: None,
            generation: 0,
            blink_timer: Timer::default(),
            analytics: Vec::new(),
            analytics_generation: 0,
            analytics_cancel: Arc::new(AtomicBool::new(false)),
            analytics_family: MetricFamily::Pixel,
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

/// Snapshot the debayer/stretch toggle state from the UI into preview
/// parameters. Shared by the [`viewer`] load path and the [`export`] batch, so
/// an exported file matches what the viewer is showing.
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
        st.cache.clear();
        st.selected = None;
        st.generation += 1;
    });
    show_empty(app);
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
        None => show_empty(app),
    }
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
}

/// Flip the `checked` flag on one row of the file model. A no-op for an
/// out-of-range index. Split out from [`toggle_check`] so it needs no window.
fn toggle_check_row(model: &VecModel<FileRow>, index: usize) {
    if let Some(mut row) = model.row_data(index) {
        row.checked = !row.checked;
        model.set_row_data(index, row);
    }
}

/// Check every row in the working set — Tools ▸ Select All (Ctrl/Cmd+A).
pub fn select_all(_app: &AppWindow) {
    STATE.with(|s| set_all_checked(&s.borrow().files_model, true));
}

/// Uncheck every row in the working set — Tools ▸ Deselect All (Ctrl/Cmd+D).
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
/// submodules' tests so they exercise real FITS frames.
#[cfg(test)]
fn test_data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("test-data")
        .join(name)
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
}
