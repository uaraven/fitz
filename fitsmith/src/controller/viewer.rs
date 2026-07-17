//! Selecting and navigating the working set, loading and rendering the selected
//! file off the UI thread, and the blink loop.
//!
//! Blink is load-aware: it never advances before the current frame is on
//! screen. Manually selecting a file while blinking simply continues the loop
//! from that file. Because the cache holds *rendered* documents, re-selecting or
//! blinking back to a file re-displays it straight from memory.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use libfitz::fits_image::find_image_hdu;
use libfitz::fitskit::FitsFile;
use libfitz::preview::{PreviewParams, PreviewStage, render_preview_with_progress};
use slint::{ComponentHandle, TimerMode, Weak};

use crate::doc::LoadedDoc;
use crate::files::{display_name, next_index};
use crate::{AppWindow, view};

use super::{STATE, params, set_row_status, update_memory};

/// How long a blink frame stays on screen *after it has finished loading*
/// before advancing to the next. The load is always awaited first, so blink
/// never runs ahead of the images it is showing.
const BLINK_DWELL: Duration = Duration::from_millis(400);

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
