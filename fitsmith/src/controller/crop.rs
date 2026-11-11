//! Tools ▸ Crop: a crop rectangle edited on the preview (see
//! [`crate::crop_edit`] for the gestures) and applied, as the same pixel
//! rectangle, to every target file — losslessly for FITS, via `libfitz::crop`.
//!
//! The rectangle belongs to the working set rather than to a frame, so
//! selecting another file leaves it where it is; a frame too small to hold it
//! fails on its own row instead of being silently clamped. The batch reuses
//! the compress/decompress progress overlay.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use anyhow::{Result, ensure};
use libfitz::crop::CropRect;
use libfitz::files::{is_fits_path, is_openable_image};
use libfitz::loader::is_fits_file;
use slint::{ComponentHandle, Weak};

use crate::AppWindow;
use crate::crop_edit::aspect_for_index;
use crate::files::{cropped_output_path, cropping_temp_path, display_name};

use super::{
    STATE, batch_is_current, begin_file_batch, operation_targets, select_file, set_row_status,
    spawn_header_load, update_memory,
};

/// The size of the frame on screen, in image pixels — the crop rectangle's
/// bounds. Zero until a frame has been shown.
fn bounds(app: &AppWindow) -> (f32, f32) {
    (app.get_image_width(), app.get_image_height())
}

/// Copy the editor's rectangle into the window's `crop-*` properties (a zero
/// size means "no rectangle").
fn push_rect(app: &AppWindow) {
    let rect = STATE.with(|s| s.borrow().crop.rect());
    let (x, y, w, h) = rect.map_or((0.0, 0.0, 0.0, 0.0), |r| (r.x, r.y, r.w, r.h));
    app.set_crop_x(x);
    app.set_crop_y(y);
    app.set_crop_w(w);
    app.set_crop_h(h);
}

/// Crop mode was switched on or off (Tools ▸ Crop, or the toolbar's Cancel).
/// Turning it on offers a starting rectangle — kept from last time if there is
/// one; turning it off discards the rectangle.
pub fn crop_mode_changed(app: &AppWindow) {
    let on = app.get_crop_mode();
    let bounds = bounds(app);
    let aspect = aspect_for_index(app.get_crop_aspect());
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        if on {
            st.crop.set_aspect(aspect, bounds);
            st.crop.seed_default(bounds);
        } else {
            st.crop.clear();
        }
    });
    app.set_crop_cursor(0);
    push_rect(app);
}

/// The aspect-ratio dropdown changed: lock (or free) the rectangle's shape.
pub fn crop_aspect_changed(app: &AppWindow) {
    let aspect = aspect_for_index(app.get_crop_aspect());
    let bounds = bounds(app);
    STATE.with(|s| s.borrow_mut().crop.set_aspect(aspect, bounds));
    push_rect(app);
}

/// A pointer event on the preview while cropping, at image pixel `(x, y)`:
/// `kind` 0 is the press, 1 a move (hover or drag) and 2 the release; `tol` is
/// the handle grab distance in image pixels.
pub fn crop_pointer(app: &AppWindow, kind: i32, x: f32, y: f32, tol: f32) {
    let bounds = bounds(app);
    if bounds.0 <= 0.0 || bounds.1 <= 0.0 {
        return;
    }
    let p = (x, y);
    let cursor = STATE.with(|s| {
        let mut st = s.borrow_mut();
        let crop = &mut st.crop;
        match kind {
            0 => crop.press(p, tol, bounds),
            1 => {
                if crop.dragging() {
                    crop.drag_to(p, bounds);
                }
                crop.cursor_at(p, tol)
            }
            _ => {
                crop.release();
                crop.cursor_at(p, tol)
            }
        }
    });
    push_rect(app);
    app.set_crop_cursor(cursor);
}

/// A suffix that stays a plain file-name fragment.
fn suffix_is_valid(suffix: &str) -> bool {
    !suffix.contains(['/', '\\', '\0'])
}

/// The Crop button: check the settings, then — when the originals are to be
/// overwritten — ask first; otherwise start cropping.
pub fn run_crop(app: &AppWindow) {
    let Some(_) = STATE.with(|s| s.borrow().crop.crop_rect()) else {
        app.set_status_text("Draw a crop rectangle on the preview first".into());
        return;
    };
    let overwrite = app.get_crop_overwrite();
    let targets = operation_targets(is_openable_image);
    if targets.is_empty() {
        app.set_status_text("No images to crop".into());
        return;
    }
    // The suffix names every copy: all of them normally, only the camera RAW
    // ones (which are never overwritten) when overwriting.
    let uses_suffix = !overwrite || targets.iter().any(|p| !is_fits_path(p));
    if uses_suffix && !suffix_is_valid(app.get_crop_suffix().trim()) {
        app.set_status_text("The suffix can't contain path separators".into());
        return;
    }
    // Nothing is replaced unless a FITS file is, so only then is it worth asking.
    let replaced = if overwrite { fits_count(&targets) } else { 0 };
    if replaced > 0 {
        app.set_crop_confirm_count(replaced as i32);
        app.set_show_crop_confirm(true);
    } else {
        start_crop(app, targets);
    }
}

/// How many of `targets` an overwriting crop would replace. Camera RAW files
/// are never overwritten (a FITS can't stand in for one), so only the FITS
/// targets count.
fn fits_count(targets: &[PathBuf]) -> usize {
    targets.iter().filter(|p| is_fits_path(p)).count()
}

/// The overwrite confirmation was accepted.
pub fn confirm_crop(app: &AppWindow) {
    app.set_show_crop_confirm(false);
    start_crop(app, operation_targets(is_openable_image));
}

fn start_crop(app: &AppWindow, targets: Vec<PathBuf>) {
    let Some(rect) = STATE.with(|s| s.borrow().crop.crop_rect()) else {
        return;
    };
    spawn_crop(
        app.as_weak(),
        targets,
        rect,
        app.get_crop_suffix().trim().to_string(),
        app.get_crop_overwrite(),
    );
}

/// Crop `targets` on a worker thread behind the cancellable progress overlay,
/// marshaling each result back to the UI thread. A failed file is badged with
/// its error; a cancel stops the batch between files. Every file written joins
/// the working set (see [`register_outputs`]), and the crop mode ends once the
/// batch is done.
fn spawn_crop(
    weak: Weak<AppWindow>,
    targets: Vec<PathBuf>,
    rect: CropRect,
    suffix: String,
    overwrite: bool,
) {
    let (generation, cancel) = begin_file_batch();
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_convert_title("Cropping…".into());
        app.set_convert_in_progress(true);
        app.set_convert_progress(0.0);
        app.set_convert_progress_text("".into());
        app.set_convert_progress_detail("".into());
        app.set_busy(true);
        app.set_stage_text("".into());
    });
    std::thread::spawn(move || {
        let total = targets.len();
        let mut ok = 0usize;
        let mut failed = 0usize;
        let mut snapped = false;
        let mut overwritten = 0usize;
        let mut written = Vec::new();
        let mut canceled = false;
        for (i, input) in targets.into_iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                canceled = true;
                break;
            }
            let progress = i as f32 / total as f32;
            let text = display_name(&input);
            let detail = format!("{}/{}", i + 1, total);
            let weak_progress = weak.clone();
            let _ = weak_progress.upgrade_in_event_loop(move |app| {
                app.set_convert_progress(progress);
                app.set_convert_progress_text(text.into());
                app.set_convert_progress_detail(detail.into());
            });

            match process_one(&input, rect, &suffix, overwrite) {
                Ok((output, used)) => {
                    ok += 1;
                    snapped |= used != rect;
                    overwritten += usize::from(output == input);
                    written.push(output);
                }
                Err(e) => {
                    failed += 1;
                    let msg = format!("{e:#}");
                    let _ = weak.upgrade_in_event_loop(move |_app| {
                        set_row_status(&input, "error", &msg);
                    });
                }
            }
        }
        let _ = weak.upgrade_in_event_loop(move |app| {
            // The files are on disk whatever happened to the batch since, so
            // the working set follows them even if a later batch owns the UI.
            let stale_index = register_outputs(&app, written);
            // A later batch owns the UI now; leave its overlay and status alone.
            if !batch_is_current(generation) {
                return;
            }
            app.set_convert_in_progress(false);
            app.set_convert_progress(1.0);
            app.set_busy(false);
            app.set_stage_text("".into());
            let mut summary = if canceled {
                format!("Crop canceled — {ok} done, {failed} failed")
            } else {
                format!("Cropped {ok} file(s), {failed} failed")
            };
            if snapped {
                summary.push_str(" (mosaic frames cut on the colour-pattern grid)");
            }
            app.set_status_text(summary.into());
            // A finished crop ends the mode (dropping the rectangle). A canceled
            // one keeps it so the rectangle can be adjusted — unless files were
            // already overwritten, which the rectangle no longer describes.
            if !canceled || overwritten > 0 {
                app.set_crop_mode(false);
            }
            if let Some(index) = stale_index {
                // The frame on screen was replaced by a smaller one; its caches
                // are already dropped, so selecting it again re-reads it.
                select_file(&app, index as i32);
            }
            update_memory(&app);
        });
    });
}

/// Crop one file. FITS is cropped losslessly and, when `overwrite` is set,
/// replaces the original; a camera RAW is always written as a new
/// `<name><suffix>.fits` (a FITS can't stand in for it). The result goes to a
/// hidden temp file first and is renamed into place, so a failure never leaves
/// a half-written file — or damages the original. Returns the path written and
/// the rectangle actually used (a mosaic frame's is snapped to its colour-pattern
/// grid). Runs on a worker thread.
fn process_one(
    input: &Path,
    rect: CropRect,
    suffix: &str,
    overwrite: bool,
) -> Result<(PathBuf, CropRect)> {
    let fits = is_fits_file(input);
    let target = if overwrite && fits {
        input.to_path_buf()
    } else {
        // Checked by `run_crop` too; this keeps the file name from ever
        // reaching outside the source folder, whoever calls.
        ensure!(
            suffix_is_valid(suffix),
            "the suffix can't contain path separators"
        );
        cropped_output_path(input, suffix, fits)
    };
    let temp = cropping_temp_path(&target);
    let used = match libfitz::crop::crop_file(input, &temp, rect) {
        Ok(used) => used,
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            return Err(e);
        }
    };
    if let Err(e) = std::fs::rename(&temp, &target) {
        let _ = std::fs::remove_file(&temp);
        return Err(anyhow::Error::new(e).context(format!("cannot write {}", target.display())));
    }
    Ok((target, used))
}

/// Split the files a crop batch wrote into those already in the working set
/// (`set`) — overwritten originals, or copies from an earlier run — and those
/// that are new to it, each listed once.
fn split_written(set: &[PathBuf], written: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut known: Vec<PathBuf> = Vec::new();
    let mut new: Vec<PathBuf> = Vec::new();
    for path in written {
        let list = if set.contains(&path) {
            &mut known
        } else {
            &mut new
        };
        if !list.contains(&path) {
            list.push(path);
        }
    }
    (known, new)
}

/// Bring the working set in line with the files a crop batch wrote: a file
/// already in it was rewritten, so its cached previews, metadata and analytics
/// are dropped (its row and checkbox stay); a new file is appended to the list.
/// The selection is left alone. Returns the selected row's index when that file
/// was among those rewritten, i.e. when the frame on screen is stale.
fn register_outputs(app: &AppWindow, written: Vec<PathBuf>) -> Option<usize> {
    let (known, new) = STATE.with(|s| split_written(&s.borrow().paths, written));
    for path in &known {
        refresh_rewritten(app, path);
    }
    let (_, added) = super::add_paths(new);
    if !added.is_empty() {
        super::refresh_rows(app);
        super::update_exposure(app);
        spawn_header_load(app.as_weak(), added);
    }
    STATE.with(|s| {
        let st = s.borrow();
        stale_selection(&st.paths, st.selected, &known)
    })
}

/// The index of the selected file if it is one of `rewritten`.
fn stale_selection(
    paths: &[PathBuf],
    selected: Option<usize>,
    rewritten: &[PathBuf],
) -> Option<usize> {
    selected.filter(|&i| paths.get(i).is_some_and(|p| rewritten.contains(p)))
}

/// A working-set file was rewritten in place: forget everything cached from its
/// old contents and re-read its header. Unlike a compress/decompress rewrite
/// the path (and so the row, with its checkbox) stays as it is.
fn refresh_rewritten(app: &AppWindow, path: &Path) {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        super::forget_path(&mut st, path);
        st.analytics_cache.remove(path);
    });
    super::clear_row_status(path);
    spawn_header_load(app.as_weak(), vec![path.to_path_buf()]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_data;
    use libfitz::fitskit::FitsFile;

    fn rect(x: usize, y: usize, width: usize, height: usize) -> CropRect {
        CropRect {
            x,
            y,
            width,
            height,
        }
    }

    fn naxis(path: &Path) -> (i64, i64) {
        let header = FitsFile::from_file(path).unwrap().hdus[0].header.clone();
        (
            header.get_int("NAXIS1").unwrap(),
            header.get_int("NAXIS2").unwrap(),
        )
    }

    #[test]
    fn suffix_mode_writes_a_new_file_and_keeps_the_original() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("frame.fit");
        std::fs::copy(test_data("uncompressed.fit"), &input).unwrap();
        let before = std::fs::read(&input).unwrap();

        let (output, used) = process_one(&input, rect(100, 50, 640, 480), "_c", false).unwrap();
        assert_eq!(output, tmp.path().join("frame_c.fit"));
        assert_eq!(used, rect(100, 50, 640, 480));
        assert_eq!(naxis(&output), (640, 480));
        assert_eq!(std::fs::read(&input).unwrap(), before, "original untouched");
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("cropping"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    #[test]
    fn overwrite_mode_replaces_the_original_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("frame.fit");
        std::fs::copy(test_data("uncompressed.fit"), &input).unwrap();

        let (output, _) = process_one(&input, rect(11, 7, 401, 303), "_ignored", true).unwrap();
        assert_eq!(output, input);
        // The Bayer origin and size were snapped to even pixels.
        assert_eq!(naxis(&input), (400, 302));
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_frame_too_small_for_the_rectangle_fails_and_leaves_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("frame.fit");
        std::fs::copy(test_data("uncompressed.fit"), &input).unwrap();
        let before = std::fs::read(&input).unwrap();

        let err = process_one(&input, rect(0, 0, 9000, 9000), "_c", true).unwrap_err();
        assert!(err.to_string().contains("does not fit"), "{err}");
        assert_eq!(std::fs::read(&input).unwrap(), before);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_camera_raw_is_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("comet.ARW");
        std::fs::copy(test_data("comet.ARW"), &input).unwrap();

        let (output, _) = process_one(&input, rect(0, 0, 600, 400), "_cropped", true).unwrap();
        assert_eq!(output, tmp.path().join("comet_cropped.fits"));
        assert!(input.is_file(), "the RAW original must survive");
        assert_eq!(naxis(&output), (600, 400));
    }

    #[test]
    fn written_files_split_into_known_and_new_without_repeats() {
        let p = |s: &str| PathBuf::from(s);
        let set = vec![p("/d/a.fits"), p("/d/b.fits"), p("/d/b_cropped.fits")];
        let written = vec![
            p("/d/a.fits"),         // overwritten original
            p("/d/b_cropped.fits"), // copy from an earlier run
            p("/d/c_cropped.fits"), // brand new
            p("/d/c_cropped.fits"),
        ];
        let (known, new) = split_written(&set, written);
        assert_eq!(known, vec![p("/d/a.fits"), p("/d/b_cropped.fits")]);
        assert_eq!(new, vec![p("/d/c_cropped.fits")]);
    }

    #[test]
    fn a_suffix_that_leaves_the_folder_is_refused_even_when_overwriting() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("frames");
        std::fs::create_dir(&folder).unwrap();
        let input = folder.join("comet.ARW");
        std::fs::copy(test_data("comet.ARW"), &input).unwrap();

        // A RAW file takes the suffix even in overwrite mode.
        let err = process_one(&input, rect(0, 0, 600, 400), "/../escaped", true).unwrap_err();
        assert!(err.to_string().contains("path separators"), "{err}");
        let entries = std::fs::read_dir(tmp.path()).unwrap().count()
            + std::fs::read_dir(&folder).unwrap().count();
        assert_eq!(
            entries, 2,
            "nothing written anywhere: just `frames` and the RAW"
        );

        // An overwritten FITS never uses the suffix, so it isn't judged.
        let fits = folder.join("frame.fit");
        std::fs::copy(test_data("uncompressed.fit"), &fits).unwrap();
        assert!(process_one(&fits, rect(0, 0, 100, 100), "/../x", true).is_ok());
    }

    #[test]
    fn only_the_selected_file_being_rewritten_makes_the_view_stale() {
        let p = |s: &str| PathBuf::from(s);
        let paths = vec![p("/d/a.fits"), p("/d/b.fits")];
        let rewritten = vec![p("/d/b.fits")];
        assert_eq!(stale_selection(&paths, Some(1), &rewritten), Some(1));
        assert_eq!(stale_selection(&paths, Some(0), &rewritten), None);
        assert_eq!(stale_selection(&paths, None, &rewritten), None);
        assert_eq!(stale_selection(&paths, Some(9), &rewritten), None);
    }

    #[test]
    fn only_fits_targets_count_towards_the_overwrite_warning() {
        let targets: Vec<PathBuf> = ["a.fits", "b.fit.fz", "IMG_1.ARW", "c.cr2"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(fits_count(&targets), 2);
        assert_eq!(fits_count(&targets[2..]), 0);
    }

    #[test]
    fn suffixes_with_path_separators_are_rejected() {
        assert!(suffix_is_valid("_cropped"));
        assert!(suffix_is_valid(""));
        assert!(!suffix_is_valid("../x"));
        assert!(!suffix_is_valid("a\\b"));
    }
}
