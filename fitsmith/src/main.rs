//! FitSmith — a Slint GUI frontend for the `fitz` toolset. Milestone 4: a
//! working set of files (open a file or a whole directory), click / blink to
//! select, images decoded off the UI thread and kept in an LRU cache so
//! re-selection and blink re-render from memory. The debayer/stretch toggles
//! re-render the current frame live. Later milestones add header/stats tabs and
//! export.

mod cache;
mod controller;
mod files;
mod image;

use anyhow::Result;
use slint::ComponentHandle;

slint::include_modules!();

fn main() -> Result<()> {
    let app = AppWindow::new()?;
    app.set_status_text("No image — use File ▸ Open…".into());
    controller::init(&app);

    app.on_open({
        let weak = app.as_weak();
        move || {
            if let Some(app) = weak.upgrade() {
                controller::open_file(&app);
            }
        }
    });

    app.on_open_directory({
        let weak = app.as_weak();
        move || {
            if let Some(app) = weak.upgrade() {
                controller::open_directory(&app);
            }
        }
    });

    app.on_clear_files({
        let weak = app.as_weak();
        move || {
            if let Some(app) = weak.upgrade() {
                controller::clear_files(&app);
            }
        }
    });

    app.on_select_file({
        let weak = app.as_weak();
        move |index| {
            if let Some(app) = weak.upgrade() {
                controller::select_file(&app, index);
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

    app.on_blink_toggled({
        let weak = app.as_weak();
        move || {
            if let Some(app) = weak.upgrade() {
                controller::set_blinking(&app, app.get_blinking());
            }
        }
    });

    app.on_request_exit(|| {
        let _ = slint::quit_event_loop();
    });

    app.run()?;
    Ok(())
}
