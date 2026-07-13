//! Application logic bridging the Slint UI to `fitz-core`: opening a file
//! (loaded off the UI thread), caching the decoded image so the debayer/stretch
//! toggles re-render without touching disk, and pushing results back to the UI.

use std::cell::RefCell;
use std::path::Path;

use anyhow::Result;
use fitz_core::fits_image::find_image_hdu;
use fitz_core::fitskit::{FitsFile, Header, ImageData};
use fitz_core::preview::{PreviewImage, PreviewParams, render_preview};
use slint::ComponentHandle;

use crate::AppWindow;
use crate::image::preview_to_image;

/// A decoded FITS image kept resident so a toggle change only re-runs the
/// (in-memory) render, never a disk read.
struct LoadedDoc {
    header: Header,
    img: ImageData,
}

thread_local! {
    /// The currently displayed document. Lives on the UI thread; the load
    /// worker writes it through an event-loop closure (which also runs here),
    /// so no cross-thread sharing of the FITS data is needed.
    static DOC: RefCell<Option<LoadedDoc>> = const { RefCell::new(None) };
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

/// Read a FITS file and render it, returning the resident document alongside
/// the first preview. Runs on the load worker thread.
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

/// Prompt for a FITS file and load it. The heavy work (read + decompress +
/// debayer + stretch) runs on a worker thread; the result is marshaled back to
/// the UI thread via the Slint event loop.
pub fn open_file(app: &AppWindow) {
    let picked = rfd::FileDialog::new()
        .add_filter("FITS images", &["fit", "fits", "fts", "fz"])
        .add_filter("All files", &["*"])
        .pick_file();
    let Some(path) = picked else {
        return;
    };

    app.set_busy(true);
    app.set_status_text(format!("Loading {}…", path.display()).into());

    let weak = app.as_weak();
    let p = params(app);
    std::thread::spawn(move || {
        let outcome = load_and_render(&path, &p);
        let name = display_name(&path);
        let _ = weak.upgrade_in_event_loop(move |app| match outcome {
            Ok((doc, preview)) => {
                let (w, h) = (preview.width, preview.height);
                DOC.with(|d| *d.borrow_mut() = Some(doc));
                apply_preview(&app, &preview);
                app.set_busy(false);
                app.set_status_text(format!("{name}   {w}×{h}").into());
            }
            Err(e) => {
                app.set_busy(false);
                app.set_status_text(format!("Failed to open {name}: {e}").into());
            }
        });
    });
}

/// Re-render the cached document after a toggle change. Runs synchronously on
/// the UI thread; for large frames this briefly blocks — Milestone 4 moves it
/// onto a worker with stale-result coalescing.
pub fn rerender(app: &AppWindow) {
    let p = params(app);
    let rendered = DOC.with(|d| {
        d.borrow()
            .as_ref()
            .map(|doc| render_preview(&doc.header, &doc.img, &p))
    });
    match rendered {
        None => {} // nothing loaded yet
        Some(Ok(preview)) => apply_preview(app, &preview),
        Some(Err(e)) => app.set_status_text(format!("Render failed: {e}").into()),
    }
}

/// The file's base name for status text, falling back to the full path.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
