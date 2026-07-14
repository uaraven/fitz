//! FitSmith — a Slint GUI frontend for the `fitz` toolset. A working set of
//! files (open a file or a whole directory), click / blink to select, images
//! decoded off the UI thread and kept in an LRU cache so re-selection and blink
//! re-render from memory. The debayer/stretch toggles re-render the current
//! frame live; a Headers tab and a docked stats panel show the FITS metadata
//! and pixel statistics. A later milestone adds export.

mod cache;
mod controller;
mod doc;
mod files;
mod image;
mod view;

use std::path::PathBuf;

use anyhow::Result;
use slint::ComponentHandle;

slint::include_modules!();

/// On macOS, AppKit auto-populates a menu it recognizes as the "View" menu with
/// "Show Tab Bar" / "Show All Tabs" whenever automatic window tabbing is enabled
/// (the default). FitSmith doesn't use tabbed windows, so disable it — a
/// process-global class setting that must be applied before the menu is built.
/// "Enter Full Screen" (also auto-added to the View menu) is unaffected.
#[cfg(target_os = "macos")]
fn disable_automatic_window_tabbing() {
    use objc2::runtime::Bool;
    use objc2::{class, msg_send};
    // Safety: a class message to the AppKit-provided NSWindow with a BOOL arg.
    unsafe {
        let _: () = msg_send![class!(NSWindow), setAllowsAutomaticWindowTabbing: Bool::new(false)];
    }
}

fn main() -> Result<()> {
    #[cfg(target_os = "macos")]
    disable_automatic_window_tabbing();

    let app = AppWindow::new()?;
    app.set_status_text("No image — add files to view".into());
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

    app.on_navigate({
        let weak = app.as_weak();
        move |delta| {
            if let Some(app) = weak.upgrade() {
                controller::navigate(&app, delta);
            }
        }
    });

    app.on_navigate_first({
        let weak = app.as_weak();
        move || {
            if let Some(app) = weak.upgrade() {
                controller::navigate_edge(&app, false);
            }
        }
    });

    app.on_navigate_last({
        let weak = app.as_weak();
        move || {
            if let Some(app) = weak.upgrade() {
                controller::navigate_edge(&app, true);
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

    // Seed the working set from any files / folders given on the command line.
    controller::open_args(&app, std::env::args_os().skip(1).map(PathBuf::from));

    app.run()?;
    Ok(())
}
