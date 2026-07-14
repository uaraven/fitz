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
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use fitz_core::fits_image::find_image_hdu;
use fitz_core::fitskit::FitsFile;
use fitz_core::preview::{PreviewParams, PreviewStage, render_preview_with_progress};
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel, Weak};

use crate::doc::LoadedDoc;
use crate::files::{display_name, expand_inputs, is_compressed, next_index, scan_directory};
use crate::{AppWindow, FileRow, view};

/// Maximum total size of rendered previews to keep cached. Sized so a handful
/// of full-frame images stay resident for instant blink / re-selection.
/// TODO: make this user-configurable.
const CACHE_CAPACITY_BYTES: usize = 512 * 1024 * 1024;

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
        Self {
            paths: Vec::new(),
            files_model: Rc::new(VecModel::default()),
            cache: crate::cache::LruCache::new(CACHE_CAPACITY_BYTES),
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

/// Handle a debayer/stretch toggle change: the cached previews were rendered
/// with the old settings, so drop them all and re-render the current selection.
pub fn rerender(app: &AppWindow) {
    let selected = STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.cache.clear();
        st.selected
    });
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
            if is_current(req) {
                display_doc(app, &path, &doc);
            }
        }
        Err(e) => {
            set_row_status(&path, "error");
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

/// Update a file row's status badge (e.g. mark a failed load "error").
fn set_row_status(path: &Path, status: &str) {
    let target = path.to_string_lossy();
    STATE.with(|s| {
        let model = &s.borrow().files_model;
        for i in 0..model.row_count() {
            if let Some(mut row) = model.row_data(i)
                && row.path.as_str() == target.as_ref()
            {
                row.status = status.into();
                model.set_row_data(i, row);
                break;
            }
        }
    });
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
