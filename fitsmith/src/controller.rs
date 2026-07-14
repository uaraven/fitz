//! Application logic bridging the Slint UI to `fitz-core`. Milestone 4 adds a
//! working set of files with click / blink selection: files are decoded off the
//! UI thread, kept in an LRU cache so re-selecting or blinking re-renders from
//! memory, and a generation counter drops stale results when the user scrubs
//! faster than frames can render.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use fitz_core::fits_image::find_image_hdu;
use fitz_core::fitskit::{FitsFile, Header, ImageData};
use fitz_core::preview::{PreviewImage, PreviewParams, render_preview};
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel, Weak};

use crate::files::{display_name, is_compressed, next_index, scan_directory};
use crate::image::preview_to_image;
use crate::{AppWindow, FileRow};

/// How many decoded frames to keep resident. Small: the goal is smooth blink /
/// re-selection over a handful of frames, not caching a whole session.
const CACHE_CAPACITY: usize = 8;

/// Blink advance interval.
const BLINK_INTERVAL: Duration = Duration::from_millis(600);

/// A decoded FITS image kept resident so a toggle change or re-selection only
/// re-runs the (in-memory) render, never a disk read. Shared via `Arc` so the
/// render worker can borrow it without copying multi-MB pixel data.
struct LoadedDoc {
    header: Header,
    img: ImageData,
}

/// All UI-thread application state. Lives in a thread-local because Slint is
/// single-threaded: every mutation happens either from a callback or from a
/// worker's `upgrade_in_event_loop` closure, both of which run here.
struct AppState {
    /// The working set, in list order.
    paths: Vec<PathBuf>,
    /// The `[FileRow]` model backing the list view (mirrors `paths`).
    files_model: Rc<VecModel<FileRow>>,
    /// Decoded frames, keyed by path.
    cache: crate::cache::LruCache<PathBuf, Arc<LoadedDoc>>,
    /// Currently selected index into `paths`, if any.
    selected: Option<usize>,
    /// Bumped on every selection/re-render request; a worker result is applied
    /// only if its captured generation still matches (stale-result coalescing).
    generation: u64,
    /// Drives blink; stopped by default.
    blink_timer: Timer,
}

impl AppState {
    fn new() -> Self {
        Self {
            paths: Vec::new(),
            files_model: Rc::new(VecModel::default()),
            cache: crate::cache::LruCache::new(CACHE_CAPACITY),
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

/// Push a rendered preview onto the window: the image plus its natural size,
/// which the `ImageView` needs to compute fit/zoom.
fn apply_preview(app: &AppWindow, preview: &PreviewImage) {
    app.set_preview_image(preview_to_image(preview));
    app.set_image_width(preview.width as f32);
    app.set_image_height(preview.height as f32);
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
    let picked = rfd::FileDialog::new()
        .add_filter("FITS images", &["fit", "fits", "fts", "fz"])
        .add_filter("All files", &["*"])
        .pick_file();
    let Some(path) = picked else {
        return;
    };
    let target = add_paths(vec![path.clone()]).or_else(|| index_of(&path));
    if let Some(index) = target {
        select_file(app, index as i32);
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
    let first = paths[0].clone();
    let target = add_paths(paths).or_else(|| index_of(&first));
    if let Some(index) = target {
        select_file(app, index as i32);
    }
}

/// Remove every file from the working set and reset the view. Bumping the
/// generation makes any in-flight load/render land as stale and be dropped.
pub fn clear_files(app: &AppWindow) {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.blink_timer.stop();
        st.paths.clear();
        st.files_model.set_vec(Vec::new());
        st.cache = crate::cache::LruCache::new(CACHE_CAPACITY);
        st.selected = None;
        st.generation += 1;
    });
    app.set_blinking(false);
    app.set_selected_index(-1);
    app.set_preview_image(slint::Image::default());
    app.set_image_width(0.0);
    app.set_image_height(0.0);
    app.set_busy(false);
    app.set_status_text("No image — use File ▸ Add File…".into());
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
/// render from the cache (off-thread) or load it from disk.
pub fn select_file(app: &AppWindow, index: i32) {
    let action = STATE.with(|s| {
        let mut st = s.borrow_mut();
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
    app.set_busy(true);
    app.set_status_text(format!("Loading {}…", display_name(&path)).into());
    let p = params(app);
    match cached {
        Some(doc) => spawn_render(app.as_weak(), doc, p, req),
        None => spawn_load(app.as_weak(), path, p, req),
    }
}

/// Re-render the currently selected document after a toggle change, off-thread
/// with the same coalescing. No-op when nothing is selected or cached.
pub fn rerender(app: &AppWindow) {
    let action = STATE.with(|s| {
        let mut st = s.borrow_mut();
        let idx = st.selected?;
        let path = st.paths.get(idx)?.clone();
        let doc = st.cache.get(&path).cloned()?;
        st.generation += 1;
        Some((doc, st.generation))
    });
    if let Some((doc, req)) = action {
        app.set_busy(true);
        spawn_render(app.as_weak(), doc, params(app), req);
    }
}

/// Render a cached document on a worker thread, marshaling the result back.
fn spawn_render(weak: Weak<AppWindow>, doc: Arc<LoadedDoc>, p: PreviewParams, req: u64) {
    std::thread::spawn(move || {
        let outcome = render_preview(&doc.header, &doc.img, &p);
        let _ = weak.upgrade_in_event_loop(move |app| finish_render(&app, outcome, req));
    });
}

/// Read a file, decode it, and render its first preview on a worker thread.
fn spawn_load(weak: Weak<AppWindow>, path: PathBuf, p: PreviewParams, req: u64) {
    std::thread::spawn(move || {
        let outcome = load_and_render(&path, &p);
        let _ = weak.upgrade_in_event_loop(move |app| finish_load(&app, path, outcome, req));
    });
}

/// Read a FITS file into a resident document and render it. Runs on a worker.
fn load_and_render(path: &Path, p: &PreviewParams) -> Result<(LoadedDoc, PreviewImage)> {
    let fits = FitsFile::from_file(path)?;
    let (header, img) = find_image_hdu(&fits, path)?;
    let doc = LoadedDoc {
        header: header.clone(),
        img: img.into_owned(),
    };
    let preview = render_preview(&doc.header, &doc.img, p)?;
    Ok((doc, preview))
}

/// Whether `req` is still the latest request; stale results are dropped so the
/// newest selection always wins.
fn is_current(req: u64) -> bool {
    STATE.with(|s| s.borrow().generation == req)
}

/// Base name of the currently selected file, for status text.
fn selected_name() -> Option<String> {
    STATE.with(|s| {
        let st = s.borrow();
        st.selected
            .and_then(|i| st.paths.get(i))
            .map(|p| display_name(p))
    })
}

/// Apply a re-render result (from the cache) if it isn't stale.
fn finish_render(app: &AppWindow, outcome: Result<PreviewImage>, req: u64) {
    if !is_current(req) {
        return;
    }
    match outcome {
        Ok(preview) => {
            apply_preview(app, &preview);
            app.set_busy(false);
            let name = selected_name().unwrap_or_default();
            app.set_status_text(format!("{name}   {}×{}", preview.width, preview.height).into());
        }
        Err(e) => {
            app.set_busy(false);
            app.set_status_text(format!("Render failed: {e}").into());
        }
    }
}

/// Cache a freshly loaded document (always, so the work isn't wasted) and
/// display it only if the selection hasn't moved on.
fn finish_load(
    app: &AppWindow,
    path: PathBuf,
    outcome: Result<(LoadedDoc, PreviewImage)>,
    req: u64,
) {
    match outcome {
        Ok((doc, preview)) => {
            STATE.with(|s| s.borrow_mut().cache.put(path.clone(), Arc::new(doc)));
            if is_current(req) {
                apply_preview(app, &preview);
                app.set_busy(false);
                let name = display_name(&path);
                app.set_status_text(
                    format!("{name}   {}×{}", preview.width, preview.height).into(),
                );
            }
        }
        Err(e) => {
            set_row_status(&path, "error");
            if is_current(req) {
                app.set_busy(false);
                app.set_status_text(format!("Failed to open {}: {e}", display_name(&path)).into());
            }
        }
    }
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

/// Start or stop the blink timer, which advances the selection across the
/// working set. Re-render is cached, so blink stays responsive.
pub fn set_blinking(app: &AppWindow, on: bool) {
    STATE.with(|s| {
        let st = s.borrow();
        if on {
            let weak = app.as_weak();
            st.blink_timer
                .start(TimerMode::Repeated, BLINK_INTERVAL, move || {
                    if let Some(app) = weak.upgrade() {
                        advance_blink(&app);
                    }
                });
        } else {
            st.blink_timer.stop();
        }
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
