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
use rayon::prelude::*;
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
    let (debayer, stretch, invert) = view_toggles(app);
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
            invert,
        };
        let cached_preview = st.previews.get(&key).cloned();
        let cached_meta = super::meta_lookup(&st.meta, &path, debayer);
        // The metadata built under the other debayer state of a CFA source:
        // shown as a placeholder while this state's statistics are measured
        // off-thread, so a toggle flip never re-renders the cached preview.
        let other_meta = cached_meta
            .is_none()
            .then(|| super::meta_lookup(&st.meta, &path, !debayer))
            .flatten();
        // The preview rendered for the same debayer/stretch state but the
        // opposite invert flag: an invert-only toggle is then a cheap byte
        // complement of an already-rendered buffer, not a reload.
        let invert_source = cached_preview.is_none().then(|| {
            st.previews
                .get(&PreviewKey {
                    path: path.clone(),
                    debayer,
                    stretch,
                    invert: !invert,
                })
                .cloned()
        });
        Some((
            index,
            path,
            cached_preview,
            cached_meta,
            other_meta,
            invert_source.flatten(),
            req,
        ))
    });
    let Some((index, path, cached_preview, cached_meta, other_meta, invert_source, req)) = action
    else {
        return;
    };

    app.set_selected_index(index as i32);
    if let Some(preview) = &cached_preview {
        if let Some(meta) = &cached_meta {
            display_doc(app, &path, meta, preview);
            return;
        }
        // Preview cached but the statistics describe the other debayer state
        // (a CFA source whose toggle just flipped): show the frame instantly
        // and only recompute the statistics, without reloading the image.
        if let Some(placeholder) = &other_meta {
            display_doc(app, &path, placeholder, preview);
            app.set_stage_text("Measuring".into());
            spawn_meta_recalc(app.as_weak(), path, debayer, stretch, invert, req);
            return;
        }
    }
    if let (Some(source), Some(meta)) = (&invert_source, &cached_meta) {
        let inverted = Rc::new(invert_rgb8(source));
        let cost = inverted.as_raw().len();
        STATE.with(|s| {
            s.borrow_mut().previews.put(
                PreviewKey {
                    path: path.clone(),
                    debayer,
                    stretch,
                    invert,
                },
                inverted.clone(),
                cost,
            )
        });
        update_memory(app);
        display_doc(app, &path, meta, &inverted);
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
        invert,
        cached_meta.is_none(),
        req,
    );
}

/// Complement every channel byte (`255 - b`) of an already-rendered preview.
/// This is exactly what re-rendering with the invert toggle flipped would
/// produce — the pixel-level invert (`Image::invert`) is a full-range
/// complement too — but without re-reading the file or redoing debayer/stretch.
fn invert_rgb8(preview: &image::RgbImage) -> image::RgbImage {
    let mut raw = preview.as_raw().clone();
    raw.par_iter_mut().for_each(|b| *b = 255 - *b);
    image::RgbImage::from_raw(preview.width(), preview.height(), raw)
        .expect("same dimensions as source")
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
/// a plain reload. A debayer flip on a CFA source whose preview is cached
/// keeps that preview on screen and only recomputes the statistics and star
/// metrics off-thread (see [`spawn_meta_recalc`]).
pub fn rerender(app: &AppWindow) {
    let selected = STATE.with(|s| s.borrow().selected);
    if let Some(index) = selected {
        select_file(app, index as i32);
    }
}

/// File ▸ Reload Current File (R): drop the selected file's resident metadata,
/// cached previews and analysis, then load it again from disk so the headers,
/// pixel statistics and star analytics reflect the file's current contents.
pub fn reload_current(app: &AppWindow) {
    let target = STATE.with(|s| {
        let mut st = s.borrow_mut();
        let index = st.selected?;
        let path = st.paths.get(index)?.clone();
        super::forget_path(&mut st, &path);
        st.analytics_cache.remove(&path);
        Some((index, path))
    });
    let Some((index, path)) = target else {
        return; // nothing selected — the menu item is disabled then anyway
    };
    // A stale "error" badge (or a stale error tooltip) no longer describes the
    // file about to be re-read; restore the plain / "compressed" badge.
    set_row_status(
        &path,
        if crate::files::is_compressed(&path) {
            "compressed"
        } else {
            ""
        },
        "",
    );
    update_memory(app);
    // The row's exposure readout comes from the header too — refresh it.
    super::spawn_exposure_load(app.as_weak(), vec![path]);
    select_file(app, index as i32);
}

/// Read a file, decode it, and render its preview on a worker thread, reporting
/// each pipeline stage to the right-hand status field and marshaling the final
/// result back to the UI thread.
fn spawn_load(
    weak: Weak<AppWindow>,
    path: PathBuf,
    debayer: bool,
    stretch: bool,
    invert: bool,
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
        let outcome = load_and_render(&path, debayer, stretch, invert, need_meta, &report);
        let _ = weak.upgrade_in_event_loop(move |app| {
            finish_load(&app, path, debayer, stretch, invert, outcome, req)
        });
    });
}

/// Read a FITS file and run it through the debayer stage, returning the image
/// alongside the `FileMeta::debayered` state to record for it: `Some(toggle)`
/// when the source was a CFA mosaic, `None` otherwise.
fn load_debayered(
    path: &Path,
    debayer: bool,
    report: &dyn Fn(&'static str),
) -> Result<(Image, Option<bool>)> {
    report("Reading");
    let image = libfitz::fits_file::load_fits(path)?;
    let is_cfa = matches!(image.image_type, libfitz::data::ImageType::CFA(_));
    let image = apply_debayer(image, debayer, report)?;
    Ok((image, is_cfa.then_some(debayer)))
}

/// Read a FITS file and render its preview under `debayer`/`stretch`, calling
/// `report` with the name of each stage as it starts. `FileMeta` (headers,
/// stats, star metrics) is only computed when `need_meta` is set — the
/// resident cache already has a valid one otherwise. It is built *after* the
/// debayer stage (but before the stretch, which is display-only), so a CFA
/// frame's statistics and star metrics describe the rendering on screen;
/// the recorded state feeds [`super::meta_store`]. Runs on a worker.
pub(super) fn load_and_render(
    path: &Path,
    debayer: bool,
    stretch: bool,
    invert: bool,
    need_meta: bool,
    report: &dyn Fn(&'static str),
) -> Result<(Option<FileMeta>, image::RgbImage)> {
    let (image, debayered) = load_debayered(path, debayer, report)?;
    let meta = need_meta.then(|| FileMeta::build(&image, debayered));
    let processed = apply_stretch(image, stretch, invert, report);
    report("Rendering");
    let rgb = libfitz::preview::render_preview(&processed)?.to_rgb8();
    Ok((meta, rgb))
}

/// The metadata-only counterpart of [`load_and_render`]: read and debayer the
/// file, measure it, and skip the stretch/render — the rendered preview for
/// this combination is already cached. Runs on a worker.
fn load_meta(path: &Path, debayer: bool, report: &dyn Fn(&'static str)) -> Result<FileMeta> {
    let (image, debayered) = load_debayered(path, debayer, report)?;
    Ok(FileMeta::build(&image, debayered))
}

/// Apply the debayer/stretch toggles to a freshly loaded image, reporting
/// each stage that actually runs. Shared by the load path above and the
/// export batch, so an exported file matches what's on screen. Takes `image`
/// by value (rather than cloning it) since `Image` doesn't implement `Clone`.
pub(super) fn apply_pipeline(
    image: Image,
    debayer: bool,
    stretch: bool,
    invert: bool,
    report: &dyn Fn(&'static str),
) -> Result<Image> {
    Ok(apply_stretch(
        apply_debayer(image, debayer, report)?,
        stretch,
        invert,
        report,
    ))
}

/// The pipeline's debayer stage: demosaic when the toggle is on and there is
/// something to demosaic, else pass the image through.
fn apply_debayer(image: Image, debayer: bool, report: &dyn Fn(&'static str)) -> Result<Image> {
    match debayer.then(|| image.debayer()).flatten() {
        Some(result) => {
            report("Debayering");
            result
        }
        // Toggle off, or nothing to demosaic (already RGB/grayscale).
        None => Ok(image),
    }
}

/// The pipeline's stretch/invert stage: auto-stretch when the toggle is on,
/// then invert regardless of the stretch state.
fn apply_stretch(image: Image, stretch: bool, invert: bool, report: &dyn Fn(&'static str)) -> Image {
    let image = if stretch {
        report("Stretching");
        image.stretch(false, libfitz::stretch::DEFAULT_BRIGHTNESS)
    } else {
        image
    };
    if invert {
        report("Inverting");
        image.invert().unwrap()
    } else {
        image
    }
}

/// Whether `req` is still the latest request; stale results are dropped so the
/// newest selection always wins.
fn is_current(req: u64) -> bool {
    STATE.with(|s| s.borrow().generation == req)
}

/// Recompute a frame's statistics for a flipped debayer state on a worker,
/// while the already-cached preview stays on screen. The decoded pixels are
/// not kept resident, so this re-reads the file — but the displayed image is
/// never reloaded or re-rendered, and the result is cached under its state,
/// so each rendering of a CFA source is measured at most once.
fn spawn_meta_recalc(
    weak: Weak<AppWindow>,
    path: PathBuf,
    debayer: bool,
    stretch: bool,
    invert: bool,
    req: u64,
) {
    std::thread::spawn(move || {
        let outcome = load_meta(&path, debayer, &|_| {});
        let _ = weak.upgrade_in_event_loop(move |app| {
            finish_meta_recalc(&app, path, debayer, stretch, invert, outcome, req)
        });
    });
}

/// Cache freshly recomputed metadata and refresh the panels if the selection
/// (and toggle state) hasn't moved on. The preview it belongs with is looked
/// up again — in the unlikely case it was evicted meanwhile, fall back to the
/// normal select path, which now finds the metadata resident and only
/// re-renders.
fn finish_meta_recalc(
    app: &AppWindow,
    path: PathBuf,
    debayer: bool,
    stretch: bool,
    invert: bool,
    outcome: Result<FileMeta>,
    req: u64,
) {
    match outcome {
        Ok(meta) => {
            let meta = Rc::new(meta);
            let preview = STATE.with(|s| {
                let mut st = s.borrow_mut();
                super::meta_store(&mut st.meta, &path, meta.clone());
                st.previews
                    .get(&PreviewKey {
                        path: path.clone(),
                        debayer,
                        stretch,
                        invert,
                    })
                    .cloned()
            });
            if is_current(req) {
                match preview {
                    Some(preview) => display_doc(app, &path, &meta, &preview),
                    None => {
                        if let Some(index) = super::index_of(&path) {
                            select_file(app, index as i32);
                        }
                    }
                }
            }
        }
        Err(e) => {
            set_row_status(&path, "error", &e.to_string());
            if is_current(req) {
                app.set_status_text(
                    format!("Failed to measure {}: {e}", display_name(&path)).into(),
                );
                app.set_stage_text("".into());
            }
        }
    }
}

/// Cache a freshly loaded preview (and its metadata, if this load computed
/// one) and display it only if the selection hasn't moved on.
fn finish_load(
    app: &AppWindow,
    path: PathBuf,
    debayer: bool,
    stretch: bool,
    invert: bool,
    outcome: Result<(Option<FileMeta>, image::RgbImage)>,
    req: u64,
) {
    match outcome {
        Ok((meta, rgb)) => {
            let cost = rgb.as_raw().len();
            let rgb = Rc::new(rgb);
            let meta = STATE.with(|s| {
                let mut st = s.borrow_mut();
                let meta = match meta {
                    Some(m) => {
                        let m = Rc::new(m);
                        super::meta_store(&mut st.meta, &path, m.clone());
                        m
                    }
                    // `need_meta` was false, meaning the caller already found
                    // this path resident under this debayer state.
                    None => super::meta_lookup(&st.meta, &path, debayer)
                        .expect("meta must be resident when not recomputed"),
                };
                st.previews.put(
                    PreviewKey {
                        path: path.clone(),
                        debayer,
                        stretch,
                        invert,
                    },
                    rgb.clone(),
                    cost,
                );
                meta
            });
            update_memory(app);
            if is_current(req) {
                display_doc(app, &path, &meta, &rgb);
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
fn display_doc(app: &AppWindow, path: &Path, meta: &FileMeta, preview: &image::RgbImage) {
    view::show_doc(app, meta, preview, app.get_show_stars());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_data;

    #[test]
    fn cfa_meta_follows_the_debayer_toggle() {
        let path = test_data("cfa_orion.fits");

        // Toggle off: the statistics describe the raw mosaic (one channel).
        let (meta, _) = load_and_render(&path, false, false, false, true, &|_| {}).unwrap();
        let meta = meta.unwrap();
        assert_eq!(meta.stats.channels.len(), 1);
        assert_eq!(meta.debayered, Some(false));

        // Toggle on: the demosaiced RGB (three channels), recorded under its
        // own state so both measurements can be cached side by side.
        let (meta, _) = load_and_render(&path, true, false, false, true, &|_| {}).unwrap();
        let meta = meta.unwrap();
        assert_eq!(meta.stats.channels.len(), 3);
        assert_eq!(meta.debayered, Some(true));
    }

    #[test]
    fn non_cfa_meta_ignores_the_debayer_toggle() {
        // An already-debayered RGB cube: the toggle is a no-op, so one
        // measurement serves both states.
        let (meta, _) =
            load_and_render(&test_data("rgb.fits"), true, false, false, true, &|_| {}).unwrap();
        let meta = meta.unwrap();
        assert_eq!(meta.debayered, None);
    }

    #[test]
    fn load_meta_measures_without_rendering() {
        // The meta-only path used when a debayer flip finds the preview
        // already cached: same measurements as the full load, no render.
        let path = test_data("cfa_orion.fits");
        let mosaic = load_meta(&path, false, &|_| {}).unwrap();
        assert_eq!(mosaic.stats.channels.len(), 1);
        assert_eq!(mosaic.debayered, Some(false));

        let rgb = load_meta(&path, true, &|_| {}).unwrap();
        assert_eq!(rgb.stats.channels.len(), 3);
        assert_eq!(rgb.debayered, Some(true));
    }
}
