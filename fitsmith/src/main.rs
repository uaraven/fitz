//! FitSmith — a Slint GUI frontend for the `fitz` toolset. Milestone 3: open a
//! FITS file (loaded off the UI thread through `fitz-core`), display it in a
//! zoomable/pannable view, and re-render live as the debayer/stretch toggles
//! change. Later milestones add the file list, header/stats tabs, and export.

mod controller;
mod image;

use anyhow::Result;
use slint::ComponentHandle;

slint::include_modules!();

fn main() -> Result<()> {
    let app = AppWindow::new()?;
    app.set_status_text("No image — use File ▸ Open…".into());

    app.on_open({
        let weak = app.as_weak();
        move || {
            if let Some(app) = weak.upgrade() {
                controller::open_file(&app);
            }
        }
    });

    app.on_toggles_changed({
        let weak = app.as_weak();
        move || {
            if let Some(app) = weak.upgrade() {
                controller::rerender(&app);
            }
        }
    });

    app.on_request_exit(|| {
        let _ = slint::quit_event_loop();
    });

    app.run()?;
    Ok(())
}
