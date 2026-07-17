//! The View ▸ Export batch: the dialog's per-format settings, the destination
//! folder, and the worker that renders each file through the preview pipeline
//! and encodes it to the chosen format.

use std::path::PathBuf;

use libfitz::export::{
    ExportFormat, FitsBitpix, FitsExportOptions, JpegExportOptions, TiffExportOptions,
};
use libfitz::preview::PreviewParams;
use slint::{ComponentHandle, Weak};

use crate::AppWindow;
use crate::files::{display_name, export_output_path};

use super::{algorithm_for_index, operation_targets, params, require_existing_dir, set_row_status};

/// Map a FITS bit-depth dialog index (the ComboBox order) to a [`FitsBitpix`].
/// Falls back to 16-bit integer for any out-of-range index.
fn fits_bitpix_for_index(index: i32) -> FitsBitpix {
    match index {
        0 => FitsBitpix::I8,
        2 => FitsBitpix::F32,
        _ => FitsBitpix::I16,
    }
}

/// Map a TIFF bits-per-pixel dialog index (the ComboBox order) to a sample
/// bit depth. Falls back to 16 for any out-of-range index.
fn tiff_bpp_for_index(index: i32) -> u32 {
    match index {
        0 => 8,
        2 => 32,
        _ => 16,
    }
}

/// Assemble the [`ExportFormat`] (with its per-format options) from the export
/// dialog's current settings.
fn export_format(app: &AppWindow) -> ExportFormat {
    match app.get_export_format() {
        1 => ExportFormat::Tiff(TiffExportOptions {
            bpp: tiff_bpp_for_index(app.get_export_tiff_bpp()),
            deflate: app.get_export_tiff_deflate(),
        }),
        2 => ExportFormat::Jpeg(JpegExportOptions {
            quality: app.get_export_jpeg_quality().clamp(1, 100) as u8,
        }),
        3 => ExportFormat::Png,
        // 0 and any unexpected index: FITS.
        _ => ExportFormat::Fits(FitsExportOptions {
            bitpix: fits_bitpix_for_index(app.get_export_fits_bitpix()),
            compression: app
                .get_export_fits_compress()
                .then(|| algorithm_for_index(app.get_export_fits_algorithm())),
        }),
    }
}

/// Open the Export dialog: count the files it would export (every target — the
/// checked rows, or all when none are checked), reset the settings, and show it.
pub fn open_export_dialog(app: &AppWindow) {
    let count = operation_targets(|_| true).len();
    app.set_export_count(count as i32);
    reset_export_fields(app);
    app.set_show_export(true);
}

/// Restore the export dialog settings to their defaults before showing it.
fn reset_export_fields(app: &AppWindow) {
    app.set_export_output_dir("".into());
    app.set_export_format(0);
    app.set_export_fits_bitpix(1); // 16-bit integer
    app.set_export_fits_compress(false);
    app.set_export_fits_algorithm(0);
    app.set_export_tiff_bpp(1); // 16 bits per pixel
    app.set_export_tiff_deflate(false);
    app.set_export_jpeg_quality(90);
}

/// Open the native folder picker for the export destination, writing the chosen
/// path back into the dialog's field.
pub fn browse_export_dir(app: &AppWindow) {
    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
        app.set_export_output_dir(dir.to_string_lossy().into_owned().into());
    }
}

/// Confirm the Export dialog: validate the destination folder, gather the
/// settings, hide the dialog, and start the batch. On an invalid folder the
/// dialog stays open with the reason in the status bar.
pub fn run_export(app: &AppWindow) {
    let dir = match require_existing_dir(
        &app.get_export_output_dir(),
        "Choose a destination folder first",
    ) {
        Ok(dir) => dir,
        Err(msg) => {
            app.set_status_text(msg.into());
            return;
        }
    };
    let format = export_format(app);
    let targets = operation_targets(|_| true);
    app.set_show_export(false);
    if targets.is_empty() {
        return;
    }
    // The current view settings (debayer/stretch) are exported, so the file
    // matches what the viewer shows.
    spawn_export(app.as_weak(), targets, dir, format, params(app));
}

/// Run an export batch on a worker thread, rendering each file through the
/// preview pipeline and encoding it to the chosen format in `dir`. Drives the
/// modal progress overlay; a failed file is badged with its error but does not
/// abort the batch.
fn spawn_export(
    weak: Weak<AppWindow>,
    targets: Vec<PathBuf>,
    dir: PathBuf,
    format: ExportFormat,
    params: PreviewParams,
) {
    let ext = format.extension();
    let _ = weak.upgrade_in_event_loop(|app| {
        app.set_export_in_progress(true);
        app.set_export_progress(0.0);
        app.set_export_progress_text("".into());
        app.set_busy(true);
        app.set_stage_text("".into());
    });
    std::thread::spawn(move || {
        let total = targets.len();
        let mut ok = 0usize;
        let mut failed = 0usize;
        for (i, input) in targets.iter().enumerate() {
            let progress = i as f32 / total as f32;
            let text = format!("{} ({}/{})", display_name(input), i + 1, total);
            let weak_progress = weak.clone();
            let _ = weak_progress.upgrade_in_event_loop(move |app| {
                app.set_export_progress(progress);
                app.set_export_progress_text(text.into());
            });

            let output = export_output_path(input, &dir, ext);
            match libfitz::export::export_file(input, &output, &params, &format) {
                Ok(()) => ok += 1,
                Err(e) => {
                    failed += 1;
                    let input = input.clone();
                    let msg = e.to_string();
                    let _ = weak.upgrade_in_event_loop(move |_app| {
                        set_row_status(&input, "error", &msg);
                    });
                }
            }
        }
        let _ = weak.upgrade_in_event_loop(move |app| {
            app.set_export_in_progress(false);
            app.set_export_progress(1.0);
            app.set_busy(false);
            app.set_stage_text("".into());
            app.set_status_text(format!("Exported {ok} file(s), {failed} failed").into());
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_index_mappings_have_expected_defaults_and_fallbacks() {
        assert_eq!(fits_bitpix_for_index(0), FitsBitpix::I8);
        assert_eq!(fits_bitpix_for_index(1), FitsBitpix::I16);
        assert_eq!(fits_bitpix_for_index(2), FitsBitpix::F32);
        // Out-of-range falls back to 16-bit integer.
        assert_eq!(fits_bitpix_for_index(7), FitsBitpix::I16);

        assert_eq!(tiff_bpp_for_index(0), 8);
        assert_eq!(tiff_bpp_for_index(1), 16);
        assert_eq!(tiff_bpp_for_index(2), 32);
        // Out-of-range falls back to 16.
        assert_eq!(tiff_bpp_for_index(7), 16);
    }
}
