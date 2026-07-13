//! FitSmith — a Slint GUI frontend for the `fitz` toolset. This walking
//! skeleton loads a bundled sample FITS through `fitz-core`'s preview pipeline
//! and shows it, proving the core → RGBA8 → Slint path end to end. Later
//! milestones add the file list, menus, toolbar, header/stats tabs, and zoom.

mod image;

use std::path::Path;

use anyhow::Result;
use fitz_core::fits_image::find_image_hdu;
use fitz_core::fitskit::FitsFile;
use fitz_core::preview::{PreviewParams, render_preview};

slint::include_modules!();

/// Load the bundled sample frame and render it with the default preview
/// settings (debayer + stretch), returning a ready-to-display Slint image.
fn load_sample_preview() -> Result<slint::Image> {
    let path = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-data/uncompressed.fit"
    ));
    let fits = FitsFile::from_file(path)?;
    let (header, img) = find_image_hdu(&fits, path)?;
    let preview = render_preview(header, img.as_ref(), &PreviewParams::default())?;
    Ok(image::preview_to_image(&preview))
}

fn main() -> Result<()> {
    let app = AppWindow::new()?;

    let preview = load_sample_preview().unwrap_or_else(|e| {
        eprintln!("fitsmith: could not load sample image ({e}); showing placeholder");
        image::placeholder_image()
    });
    app.set_preview_image(preview);

    app.run()?;
    Ok(())
}
