//! FitSmith — a Slint GUI frontend for the `fitz` toolset. A working set of
//! files (open a file or a whole directory), click / blink to select, images
//! decoded off the UI thread and kept in an LRU cache so re-selection and blink
//! re-render from memory. The debayer/stretch toggles re-render the current
//! frame live; a Headers tab and a docked stats panel show the FITS metadata
//! and pixel statistics. The Tools menu's Export writes the working set out as
//! FITS, TIFF, JPEG or PNG, honoring the current debayer/stretch view.

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
    app.set_app_version(env!("CARGO_PKG_VERSION").into());
    controller::init(&app);

    // Every callback below just re-acquires the window from a weak handle and
    // forwards to a controller function; `forward!` wires one up without
    // repeating that upgrade boilerplate each time.
    macro_rules! forward {
        ($setter:ident, |$app:ident $(, $arg:ident)*| $body:expr) => {{
            let weak = app.as_weak();
            app.$setter(move |$($arg),*| {
                if let Some($app) = weak.upgrade() {
                    $body;
                }
            });
        }};
    }

    forward!(on_open, |app| controller::open_file(&app));
    forward!(on_open_directory, |app| controller::open_directory(&app));
    forward!(on_clear_files, |app| controller::clear_files(&app));
    forward!(on_remove_selected, |app| controller::remove_selected(&app));
    forward!(on_select_all, |app| controller::select_all(&app));
    forward!(on_deselect_all, |app| controller::deselect_all(&app));
    forward!(on_select_file, |app, index| controller::select_file(
        &app, index
    ));
    forward!(on_toggle_check, |app, index| controller::toggle_check(
        &app, index
    ));
    forward!(on_navigate, |app, delta| controller::navigate(&app, delta));
    forward!(on_navigate_first, |app| controller::navigate_edge(
        &app, false
    ));
    forward!(on_navigate_last, |app| controller::navigate_edge(
        &app, true
    ));
    forward!(on_toggles_changed, |app| controller::rerender(&app));
    forward!(on_blink_toggled, |app| controller::set_blinking(
        &app,
        app.get_blinking()
    ));
    forward!(on_open_compress_dialog, |app| {
        controller::open_compress_dialog(&app)
    });
    forward!(on_open_decompress_dialog, |app| {
        controller::open_decompress_dialog(&app)
    });
    forward!(on_browse_output_dir, |app| controller::browse_output_dir(
        &app
    ));
    forward!(on_run_compress, |app| controller::run_compress(&app));
    forward!(on_run_decompress, |app| controller::run_decompress(&app));
    forward!(on_open_export_dialog, |app| controller::open_export_dialog(
        &app
    ));
    forward!(on_browse_export_dir, |app| controller::browse_export_dir(
        &app
    ));
    forward!(on_run_export, |app| controller::run_export(&app));
    // Phase-2 placeholder: shows the dialog with the mock chart data baked
    // into app.slint. Phase 3 replaces this with controller::analytics
    // (gather targets, compute metrics off-thread, plot, and wire
    // metric-changed / export-png / export-csv / close-analytics).
    forward!(on_open_analytics_dialog, |app| app.set_show_analytics(true));

    app.on_request_exit(|| {
        let _ = slint::quit_event_loop();
    });

    // Seed the working set from any files / folders given on the command line.
    controller::open_args(&app, std::env::args_os().skip(1).map(PathBuf::from));

    app.run()?;
    Ok(())
}
