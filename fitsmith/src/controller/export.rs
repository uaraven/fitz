//! The View ▸ Export batch: the dialog's per-format settings, the destination
//! folder, and the worker that renders each file through the preview pipeline
//! and encodes it to the chosen format.

use std::path::PathBuf;

use libfitz::export::{ExportFormat, FitsOptions, TiffOptions};
use slint::{ComponentHandle, Weak};

use crate::AppWindow;
use crate::files::export_output_path;

use super::batch::{Overlay, spawn_batch};
use super::viewer::apply_pipeline;
use super::{operation_targets, raise_batch_cancel, require_existing_dir, view_toggles};

/// Map a FITS bit-depth dialog index (the ComboBox order) to a `BITPIX`.
/// Falls back to 16-bit integer for any out-of-range index.
fn fits_bpp_for_index(index: i32) -> i64 {
    match index {
        0 => 8,
        2 => -32, // FITS float BITPIX
        _ => 16,
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

/// The file extension for `format`, feeding [`export_output_path`].
fn export_extension(format: &ExportFormat) -> &'static str {
    match format {
        ExportFormat::Fits(_) => "fits",
        ExportFormat::Tiff(_) => "tiff",
        ExportFormat::Jpeg(_) => "jpg",
        ExportFormat::Png => "png",
    }
}

/// Assemble the [`ExportFormat`] (with its per-format options) from the export
/// dialog's current settings.
fn export_format(app: &AppWindow) -> ExportFormat {
    match app.get_export_format() {
        1 => ExportFormat::Tiff(TiffOptions {
            bpp: tiff_bpp_for_index(app.get_export_tiff_bpp()),
            compress: app.get_export_tiff_deflate(),
        }),
        2 => ExportFormat::Jpeg(app.get_export_jpeg_quality().clamp(1, 100) as u8),
        3 => ExportFormat::Png,
        // 0 and any unexpected index: FITS.
        _ => ExportFormat::Fits(FitsOptions {
            bpp: fits_bpp_for_index(app.get_export_fits_bitpix()),
            compress: app.get_export_fits_compress(),
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
    let (debayer, stretch, invert) = view_toggles(app);
    spawn_export(
        app.as_weak(),
        targets,
        dir,
        format,
        debayer,
        stretch,
        invert,
    );
}

/// Cancel the running export batch from its progress overlay: take the overlay
/// down at once and raise the cancel flag; the worker stops after the file in
/// flight and reports how far it got in the status bar.
pub fn cancel_export(app: &AppWindow) {
    raise_batch_cancel();
    app.set_export_in_progress(false);
    app.set_busy(false);
    app.set_status_text("Canceling export…".into());
}

/// Export one file: load it, apply the current debayer/stretch settings, and
/// encode it to `format` at `output`.
fn export_one(
    input: &std::path::Path,
    output: &std::path::Path,
    format: ExportFormat,
    debayer: bool,
    stretch: bool,
    invert: bool,
) -> anyhow::Result<()> {
    let image = libfitz::fits_file::load_fits(input)?;
    let image = apply_pipeline(image, debayer, stretch, invert, &|_| {})?;
    image.export(output, format)
}

/// Run an export batch, rendering each file through the preview pipeline and
/// encoding it to the chosen format in `dir`, behind the cancellable modal
/// progress overlay. A failed file is badged with its error but does not abort
/// the batch, and a cancel stops the batch between files.
fn spawn_export(
    weak: Weak<AppWindow>,
    targets: Vec<PathBuf>,
    dir: PathBuf,
    format: ExportFormat,
    debayer: bool,
    stretch: bool,
    invert: bool,
) {
    let ext = export_extension(&format);
    spawn_batch(
        weak,
        Overlay::Export,
        String::new(),
        targets,
        move |input| {
            let output = export_output_path(input, &dir, ext);
            export_one(input, &output, format, debayer, stretch, invert)
        },
        |app, run, current| {
            if !current {
                return;
            }
            let (ok, failed) = (run.done.len(), run.failed);
            let summary = if run.canceled {
                format!("Export canceled — {ok} exported, {failed} failed")
            } else {
                format!("Exported {ok} file(s), {failed} failed")
            };
            app.set_status_text(summary.into());
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_index_mappings_have_expected_defaults_and_fallbacks() {
        assert_eq!(fits_bpp_for_index(0), 8);
        assert_eq!(fits_bpp_for_index(1), 16);
        assert_eq!(fits_bpp_for_index(2), -32);
        // Out-of-range falls back to 16-bit integer.
        assert_eq!(fits_bpp_for_index(7), 16);

        assert_eq!(tiff_bpp_for_index(0), 8);
        assert_eq!(tiff_bpp_for_index(1), 16);
        assert_eq!(tiff_bpp_for_index(2), 32);
        // Out-of-range falls back to 16.
        assert_eq!(tiff_bpp_for_index(7), 16);
    }

    #[test]
    fn export_extension_matches_the_format() {
        assert_eq!(
            export_extension(&ExportFormat::Fits(FitsOptions {
                bpp: 16,
                compress: false
            })),
            "fits"
        );
        assert_eq!(
            export_extension(&ExportFormat::Tiff(TiffOptions {
                bpp: 16,
                compress: false
            })),
            "tiff"
        );
        assert_eq!(export_extension(&ExportFormat::Jpeg(90)), "jpg");
        assert_eq!(export_extension(&ExportFormat::Png), "png");
    }
}
