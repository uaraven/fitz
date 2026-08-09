//! Selecting and navigating the working set, loading and rendering the selected
//! file off the UI thread, and the blink loop.
//!
//! Blink is load-aware: it never advances before the current frame is on
//! screen. Manually selecting a file while blinking simply continues the loop
//! from that file. A rendered preview is cached per `(path, debayer, stretch)`
//! (see [`super::PreviewKey`]), so re-selecting or blinking back to a
//! previously-seen combination re-displays it straight from memory; a new
//! combination is a plain reload.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use libfitz::data::Image;
use slint::{ComponentHandle, TimerMode, Weak};

use crate::doc::FileMeta;
use crate::files::{display_name, next_index};
use crate::{AppWindow, view};

use super::{PreviewKey, STATE, set_row_status, update_memory, view_toggles};

/// How long a blink frame stays on screen *after it has finished loading*
/// before advancing to the next. The load is always awaited first, so blink
/// never runs ahead of the images it is showing.
const BLINK_DWELL: Duration = Duration::from_millis(400);

/// Select the file at `index`: mark it current, bump the generation, and either
/// display it straight from the preview cache or load it from disk off-thread.
/// Any pending blink advance is cancelled here; the newly shown frame schedules
/// the next one, so blink continues from whatever the user just selected.
pub fn select_file(app: &AppWindow, index: i32) {
    let (debayer, stretch) = view_toggles(app);
    let action = STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.blink_timer.stop();
        let index = usize::try_from(index).ok()?;
        let path = st.paths.get(index)?.clone();
        st.selected = Some(index);
        st.generation += 1;
        let req = st.generation;
        let key = PreviewKey {
            path: path.clone(),
            debayer,
            stretch,
        };
        let cached_preview = st.previews.get(&key).cloned();
        let cached_meta = st.meta.get(&path).cloned();
        Some((index, path, cached_preview, cached_meta, req))
    });
    let Some((index, path, cached_preview, cached_meta, req)) = action else {
        return;
    };

    app.set_selected_index(index as i32);
    if let (Some(preview), Some(meta)) = (&cached_preview, &cached_meta) {
        display_doc(app, &path, meta, preview);
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
    spawn_load(
        app.as_weak(),
        path,
        debayer,
        stretch,
        cached_meta.is_none(),
        req,
    );
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

/// Handle a debayer/stretch toggle change: re-run the load-or-display path for
/// the current selection under the new toggle state. A previously-seen
/// combination redisplays instantly from the preview cache; an unseen one is
/// a plain reload — there is nothing to invalidate.
pub fn rerender(app: &AppWindow) {
    let selected = STATE.with(|s| s.borrow().selected);
    if let Some(index) = selected {
        select_file(app, index as i32);
    }
}

/// Read a file, decode it, and render its preview on a worker thread, reporting
/// each pipeline stage to the right-hand status field and marshaling the final
/// result back to the UI thread.
fn spawn_load(
    weak: Weak<AppWindow>,
    path: PathBuf,
    debayer: bool,
    stretch: bool,
    need_meta: bool,
    req: u64,
) {
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
        let outcome = load_and_render(&path, debayer, stretch, need_meta, &report);
        let _ = weak.upgrade_in_event_loop(move |app| {
            finish_load(&app, path, debayer, stretch, outcome, req)
        });
    });
}

/// Read a FITS file and render its preview under `debayer`/`stretch`, calling
/// `report` with the name of each stage as it starts. `FileMeta` (headers,
/// stats, star metrics) is only computed when `need_meta` is set — the
/// resident cache already has it otherwise, and it doesn't depend on the
/// preview toggles anyway. Runs on a worker.
pub(super) fn load_and_render(
    path: &Path,
    debayer: bool,
    stretch: bool,
    need_meta: bool,
    report: &dyn Fn(&'static str),
) -> Result<(Option<FileMeta>, image::RgbaImage)> {
    report("Reading");
    let image = libfitz::fits_file::load_fits(path)?;
    let meta = need_meta.then(|| FileMeta::build(&image));
    let processed = apply_pipeline(image, debayer, stretch, report)?;
    report("Rendering");
    let rgba = libfitz::preview::render_preview(&processed)?.to_rgba8();
    Ok((meta, rgba))
}

/// Apply the debayer/stretch toggles to a freshly loaded image, reporting
/// each stage that actually runs. Shared by the load path above and the
/// export batch, so an exported file matches what's on screen. Takes `image`
/// by value (rather than cloning it) since `Image` doesn't implement `Clone`.
pub(super) fn apply_pipeline(
    image: Image,
    debayer: bool,
    stretch: bool,
    report: &dyn Fn(&'static str),
) -> Result<Image> {
    let debayered = debayer.then(|| image.debayer()).flatten();
    let mut img = match debayered {
        Some(result) => {
            report("Debayering");
            result?
        }
        // Toggle off, or nothing to demosaic (already RGB/grayscale).
        None => image,
    };
    if stretch {
        report("Stretching");
        img = img.stretch(true, libfitz::stretch::DEFAULT_BRIGHTNESS);
    }
    Ok(img)
}

/// Whether `req` is still the latest request; stale results are dropped so the
/// newest selection always wins.
fn is_current(req: u64) -> bool {
    STATE.with(|s| s.borrow().generation == req)
}

/// Cache a freshly loaded preview (and its metadata, if this load computed
/// one) and display it only if the selection hasn't moved on.
fn finish_load(
    app: &AppWindow,
    path: PathBuf,
    debayer: bool,
    stretch: bool,
    outcome: Result<(Option<FileMeta>, image::RgbaImage)>,
    req: u64,
) {
    match outcome {
        Ok((meta, rgba)) => {
            let cost = rgba.as_raw().len();
            let rgba = Rc::new(rgba);
            let meta = STATE.with(|s| {
                let mut st = s.borrow_mut();
                let meta = match meta {
                    Some(m) => {
                        let m = Rc::new(m);
                        st.meta.insert(path.clone(), m.clone());
                        m
                    }
                    // `need_meta` was false, meaning the caller already found
                    // this path resident.
                    None => st
                        .meta
                        .get(&path)
                        .cloned()
                        .expect("meta must be resident when not recomputed"),
                };
                st.previews.put(
                    PreviewKey {
                        path: path.clone(),
                        debayer,
                        stretch,
                    },
                    rgba.clone(),
                    cost,
                );
                meta
            });
            update_memory(app);
            if is_current(req) {
                display_doc(app, &path, &meta, &rgba);
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
fn display_doc(app: &AppWindow, path: &Path, meta: &FileMeta, preview: &image::RgbaImage) {
    view::show_doc(app, meta, preview);
    app.set_busy(false);
    app.set_status_text(
        format!(
            "{}   {}×{}",
            display_name(path),
            preview.width(),
            preview.height()
        )
        .into(),
    );
    app.set_stage_text("".into()); // pipeline finished for this frame
    schedule_next_blink(app);
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
