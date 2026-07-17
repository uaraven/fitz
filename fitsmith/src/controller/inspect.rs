//! Tools ▸ Aberration Inspector: a read-only 3×3 mosaic of crops sampled from
//! nine fixed regions of the *currently selected* frame (four corners, four
//! edge midpoints, the center), for judging corner-to-corner focus and optical
//! aberration at a glance.
//!
//! The crops come straight from the selected frame's resident preview — the same
//! debayered/stretched, full-resolution RGBA8 buffer already on screen — so the
//! common case needs no file re-read: it just crops the cached
//! [`LoadedDoc::preview`]. Only if that frame has been evicted from the LRU cache
//! (essentially never, since it is the current selection) does it fall back to
//! the viewer's off-thread load. All the geometry and cropping lives in the
//! tested [`libfitz::inspect`] helpers; this module only wires them to the
//! dialog's `[image]` model.

use std::path::PathBuf;
use std::rc::Rc;

use libfitz::inspect::{aberration_regions, aberration_tile_size, crop_rgba8};
use libfitz::preview::PreviewImage;
use slint::{ComponentHandle, Image, ModelRc, VecModel};

use crate::AppWindow;
use crate::doc::LoadedDoc;
use crate::files::display_name;
use crate::image::tile_to_image;

use super::viewer::load_and_render;
use super::{STATE, params};

/// Open the aberration inspector for the selected frame. Displays the nine tiles
/// from the frame's resident preview; if it isn't cached, loads it off-thread
/// first and opens the dialog on completion. A no-op with nothing selected (the
/// menu item is already disabled then).
pub fn open_aberration_dialog(app: &AppWindow) {
    let selected = STATE.with(|s| {
        let mut st = s.borrow_mut();
        let index = st.selected?;
        let path = st.paths.get(index)?.clone();
        let cached = st.cache.get(&path).cloned();
        Some((path, cached))
    });
    let Some((path, cached)) = selected else {
        return;
    };

    if let Some(doc) = cached {
        show_tiles(app, &doc.preview);
        return;
    }
    // The current selection has been evicted (essentially never): read and
    // render it off the UI thread, cache it, then open the dialog.
    let weak = app.as_weak();
    let p = params(app);
    app.set_busy(true);
    app.set_status_text(format!("Inspecting: {}", display_name(&path)).into());
    std::thread::spawn(move || {
        let outcome = load_and_render(&path, &p, &|_| {});
        let _ = weak.upgrade_in_event_loop(move |app| finish_open(&app, path, outcome));
    });
}

/// Apply an off-thread load: cache the document and open the dialog, or report
/// the read failure in the status bar.
fn finish_open(app: &AppWindow, path: PathBuf, outcome: anyhow::Result<LoadedDoc>) {
    app.set_busy(false);
    match outcome {
        Ok(doc) => {
            let doc = Rc::new(doc);
            let cost = doc.preview.rgba8.len();
            STATE.with(|s| s.borrow_mut().cache.put(path, doc.clone(), cost));
            show_tiles(app, &doc.preview);
        }
        Err(e) => {
            app.set_status_text(format!("Failed to open {}: {e}", display_name(&path)).into());
        }
    }
}

/// Crop the nine aberration tiles from a preview and show the dialog, sizing the
/// grid so each crop draws 1:1.
fn show_tiles(app: &AppWindow, preview: &PreviewImage) {
    let sz = aberration_tile_size(preview.width, preview.height);
    let tiles = aberration_tiles(preview, sz);
    app.set_aberration_tiles(ModelRc::new(VecModel::from(tiles)));
    app.set_aberration_tile_size(sz as f32);
    app.set_show_aberration(true);
}

/// Build the nine tile images (row-major: TL, TC, TR, ML, C, MR, BL, BC, BR)
/// from a rendered preview buffer, each an `sz × sz` crop.
fn aberration_tiles(preview: &PreviewImage, sz: usize) -> Vec<Image> {
    let (w, h) = (preview.width, preview.height);
    aberration_regions(w, h, sz)
        .iter()
        .map(|&(x, y)| tile_to_image(&crop_rgba8(&preview.rgba8, w, h, x, y, sz)))
        .collect()
}

/// Close the inspector: hide the dialog and drop its tile images.
pub fn close_aberration(app: &AppWindow) {
    app.set_show_aberration(false);
    app.set_aberration_tiles(ModelRc::new(VecModel::<Image>::default()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_data;
    use libfitz::fits_image::find_image_hdu;
    use libfitz::fitskit::FitsFile;
    use libfitz::preview::{PreviewParams, render_preview};

    #[test]
    fn nine_tiles_are_cropped_from_a_real_frame() {
        let fits = FitsFile::from_file(test_data("uncompressed.fit")).unwrap();
        let (header, img) = find_image_hdu(&fits, &test_data("uncompressed.fit")).unwrap();
        let preview = render_preview(header, &img, &PreviewParams::default()).unwrap();

        let sz = aberration_tile_size(preview.width, preview.height);
        let tiles = aberration_tiles(&preview, sz);
        assert_eq!(tiles.len(), 9);
        // Every tile is the square SZ×SZ crop the geometry promises.
        assert!(
            tiles
                .iter()
                .all(|t| t.size().width == sz as u32 && t.size().height == sz as u32)
        );
    }
}
