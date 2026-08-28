//! Tools ▸ Aberration Inspector: a read-only 3×3 mosaic of crops sampled from
//! nine fixed regions of the *currently selected* frame (four corners, four
//! edge midpoints, the center), for judging corner-to-corner focus and optical
//! aberration at a glance.
//!
//! The crops come straight from the selected frame's resident preview — the same
//! debayered/stretched, full-resolution RGB8 buffer already on screen — so the
//! common case needs no file re-read: it just crops the cached preview. Only if
//! that frame has been evicted from the LRU cache (essentially never, since it
//! is the current selection) does it fall back to the viewer's off-thread load.
//! All the geometry and cropping lives in the tested [`libfitz::inspect`]
//! helpers; this module only wires them to the dialog's `[image]` model.

use std::path::PathBuf;
use std::rc::Rc;

use libfitz::inspect::{aberration_regions, aberration_tile_size, crop_rgb8};
use slint::{ComponentHandle, Image, ModelRc, VecModel};

use crate::AppWindow;
use crate::doc::FileMeta;
use crate::files::display_name;
use crate::image::tile_to_image;

use super::viewer::load_and_render;
use super::{PreviewKey, STATE, view_toggles};

/// Open the aberration inspector for the selected frame. Displays the nine tiles
/// from the frame's resident preview; if it isn't cached, loads it off-thread
/// first and opens the dialog on completion. A no-op with nothing selected (the
/// menu item is already disabled then).
pub fn open_aberration_dialog(app: &AppWindow) {
    let (debayer, stretch, invert) = view_toggles(app);
    let selected = STATE.with(|s| {
        let mut st = s.borrow_mut();
        let index = st.selected?;
        let path = st.paths.get(index)?.clone();
        let key = PreviewKey {
            path: path.clone(),
            debayer,
            stretch,
            invert,
        };
        let cached_preview = st.previews.get(&key).cloned();
        // Same rule as the viewer: metadata is resident per debayer state.
        let cached_meta = super::meta_lookup(&st.meta, &path, debayer);
        Some((path, cached_preview, cached_meta))
    });
    let Some((path, cached_preview, cached_meta)) = selected else {
        return;
    };

    if let Some(preview) = cached_preview {
        show_tiles(app, &preview);
        return;
    }
    // The current selection has been evicted (essentially never): read and
    // render it off the UI thread, cache it, then open the dialog.
    let weak = app.as_weak();
    app.set_busy(true);
    app.set_status_text(format!("Inspecting: {}", display_name(&path)).into());
    let need_meta = cached_meta.is_none();
    std::thread::spawn(move || {
        let outcome = load_and_render(&path, debayer, stretch, invert, need_meta, &|_| {});
        let _ = weak.upgrade_in_event_loop(move |app| {
            finish_open(&app, path, debayer, stretch, invert, outcome)
        });
    });
}

/// Apply an off-thread load: cache the preview (and metadata, if freshly
/// computed) and open the dialog, or report the read failure in the status bar.
fn finish_open(
    app: &AppWindow,
    path: PathBuf,
    debayer: bool,
    stretch: bool,
    invert: bool,
    outcome: anyhow::Result<(Option<FileMeta>, image::RgbImage)>,
) {
    app.set_busy(false);
    match outcome {
        Ok((meta, preview)) => {
            let cost = preview.as_raw().len();
            let rgb = Rc::new(preview);
            STATE.with(|s| {
                let mut st = s.borrow_mut();
                if let Some(m) = meta {
                    super::meta_store(&mut st.meta, &path, Rc::new(m));
                }
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
            });
            show_tiles(app, &rgb);
        }
        Err(e) => {
            app.set_status_text(format!("Failed to open {}: {e}", display_name(&path)).into());
        }
    }
}

/// Crop the nine aberration tiles from a preview and show the dialog, sizing the
/// grid so each crop draws 1:1.
fn show_tiles(app: &AppWindow, preview: &image::RgbImage) {
    let sz = aberration_tile_size(preview.width() as usize, preview.height() as usize);
    let tiles = aberration_tiles(preview, sz);
    app.set_aberration_tiles(ModelRc::new(VecModel::from(tiles)));
    app.set_aberration_tile_size(sz as f32);
    app.set_show_aberration(true);
}

/// Build the nine tile images (row-major: TL, TC, TR, ML, C, MR, BL, BC, BR)
/// from a rendered preview buffer, each an `sz × sz` crop.
fn aberration_tiles(preview: &image::RgbImage, sz: usize) -> Vec<Image> {
    let (w, h) = (preview.width() as usize, preview.height() as usize);
    aberration_regions(w, h, sz)
        .iter()
        .map(|&(x, y)| tile_to_image(&crop_rgb8(preview.as_raw(), w, h, x, y, sz)))
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

    #[test]
    fn nine_tiles_are_cropped_from_a_real_frame() {
        let image = libfitz::fits_file::load_fits(&test_data("uncompressed.fit")).unwrap();
        let preview = libfitz::preview::render_preview(&image).unwrap().to_rgb8();

        let sz = aberration_tile_size(preview.width() as usize, preview.height() as usize);
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
