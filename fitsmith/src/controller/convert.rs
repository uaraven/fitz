//! The Tools menu's Compress / Decompress batch operations: their dialogs, the
//! shared destination-folder handling, and the worker that rewrites each file
//! through `fitz-core`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fitz_core::fitskit::CompressionType;
use slint::{ComponentHandle, Model, Weak};

use crate::AppWindow;
use crate::files::{compressed_output_path, decompressed_output_path, display_name, is_compressed};

use super::{
    STATE, algorithm_for_index, make_row, operation_targets, require_existing_dir, set_row_status,
    update_memory,
};

/// The two file operations the Tools menu drives. `Compress` carries the
/// chosen tile-compression algorithm; `Decompress` needs no parameters.
#[derive(Clone, Copy)]
enum Operation {
    Compress(CompressionType),
    Decompress,
}

impl Operation {
    /// Present-progressive verb for the status bar ("Compressing" / …).
    fn progressive(self) -> &'static str {
        match self {
            Operation::Compress(_) => "Compressing",
            Operation::Decompress => "Decompressing",
        }
    }

    /// Past-tense noun for the completion summary ("Compressed" / …).
    fn past(self) -> &'static str {
        match self {
            Operation::Compress(_) => "Compressed",
            Operation::Decompress => "Decompressed",
        }
    }
}

/// Open the Compress dialog: count the files it would compress (every target
/// that isn't already `.fz`), reset the shared settings, and show it.
pub fn open_compress_dialog(app: &AppWindow) {
    let count = operation_targets(|p| !is_compressed(p)).len();
    app.set_compress_count(count as i32);
    reset_op_fields(app);
    app.set_show_compress(true);
}

/// Open the Decompress dialog: count the files it would decompress (every `.fz`
/// target), reset the shared settings, and show it.
pub fn open_decompress_dialog(app: &AppWindow) {
    let count = operation_targets(is_compressed).len();
    app.set_decompress_count(count as i32);
    reset_op_fields(app);
    app.set_show_decompress(true);
}

/// Restore the shared dialog settings to their defaults before showing a dialog.
fn reset_op_fields(app: &AppWindow) {
    app.set_op_algorithm(0);
    app.set_op_keep_source(true);
    app.set_op_use_custom_dir(false);
    app.set_op_output_dir("".into());
}

/// Open the native folder picker for the "different directory" field, writing
/// the chosen path back into the dialog's text field.
pub fn browse_output_dir(app: &AppWindow) {
    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
        app.set_op_output_dir(dir.to_string_lossy().into_owned().into());
    }
}

/// The output directory the dialog selected, or `None` for "beside the source".
/// Returns an error message when a custom directory is requested but the field
/// is empty or doesn't name an existing directory.
fn dialog_output_dir(app: &AppWindow) -> Result<Option<PathBuf>, String> {
    if !app.get_op_use_custom_dir() {
        return Ok(None);
    }
    require_existing_dir(&app.get_op_output_dir(), "Choose an output directory first").map(Some)
}

/// Confirm the Compress dialog: gather its settings and start the batch.
pub fn run_compress(app: &AppWindow) {
    let op = Operation::Compress(algorithm_for_index(app.get_op_algorithm()));
    start_operation(
        app,
        op,
        |p| !is_compressed(p),
        |app| app.set_show_compress(false),
    );
}

/// Confirm the Decompress dialog: gather its settings and start the batch.
pub fn run_decompress(app: &AppWindow) {
    start_operation(app, Operation::Decompress, is_compressed, |app| {
        app.set_show_decompress(false)
    });
}

/// Shared confirm handler: validate the destination, hide the dialog, and spawn
/// the batch. On an invalid custom directory the dialog stays open with the
/// reason in the status bar.
fn start_operation(
    app: &AppWindow,
    op: Operation,
    predicate: impl Fn(&Path) -> bool,
    hide: impl Fn(&AppWindow),
) {
    let output_dir = match dialog_output_dir(app) {
        Ok(dir) => dir,
        Err(msg) => {
            app.set_status_text(msg.into());
            return;
        }
    };
    // A custom output directory always leaves the originals in place.
    let keep = output_dir.is_some() || app.get_op_keep_source();
    let targets = operation_targets(predicate);
    hide(app);
    if targets.is_empty() {
        return;
    }
    spawn_operation(app.as_weak(), op, targets, output_dir, keep);
}

/// Run a compress/decompress batch on a worker thread, reporting per-file
/// progress and marshaling each result back to the UI thread — a replaced file
/// updates its working-set row in place, a failed one is badged with the error.
fn spawn_operation(
    weak: Weak<AppWindow>,
    op: Operation,
    targets: Vec<PathBuf>,
    output_dir: Option<PathBuf>,
    keep: bool,
) {
    let _ = weak.upgrade_in_event_loop(|app| {
        app.set_busy(true);
        app.set_stage_text("".into());
    });
    std::thread::spawn(move || {
        let total = targets.len();
        let mut ok = 0usize;
        let mut failed = 0usize;
        for (i, input) in targets.into_iter().enumerate() {
            let status = format!(
                "{}: {} ({}/{})",
                op.progressive(),
                display_name(&input),
                i + 1,
                total
            );
            let weak_status = weak.clone();
            let _ =
                weak_status.upgrade_in_event_loop(move |app| app.set_status_text(status.into()));

            match process_one(op, &input, output_dir.as_deref(), keep) {
                Ok(new_path) => {
                    ok += 1;
                    // In replace mode (source removed) the working-set entry now
                    // points at the new file; refresh its row on the UI thread.
                    if !keep {
                        let _ = weak.upgrade_in_event_loop(move |app| {
                            replace_working_path(&app, &input, &new_path);
                        });
                    }
                }
                Err(e) => {
                    failed += 1;
                    let msg = e.to_string();
                    let _ = weak.upgrade_in_event_loop(move |_app| {
                        set_row_status(&input, "error", &msg);
                    });
                }
            }
        }
        let _ = weak.upgrade_in_event_loop(move |app| {
            app.set_busy(false);
            app.set_stage_text("".into());
            app.set_status_text(format!("{} {ok} file(s), {failed} failed", op.past()).into());
            update_memory(&app);
        });
    });
}

/// Compress or decompress one file: derive its output path, do the work via
/// `fitz-core`, write the result, and (in replace mode) delete the source.
/// Returns the path of the file that was written. Runs on a worker thread.
fn process_one(
    op: Operation,
    input: &Path,
    output_dir: Option<&Path>,
    keep: bool,
) -> Result<PathBuf> {
    let (output, out_fits) = match op {
        Operation::Compress(algorithm) => {
            let opts = fitz_core::compress::CompressOptions { algorithm };
            (
                compressed_output_path(input, output_dir),
                fitz_core::compress::compress(input, &opts)?,
            )
        }
        Operation::Decompress => (
            decompressed_output_path(input, output_dir),
            fitz_core::decompress::decompress(input)?,
        ),
    };
    out_fits
        .to_file(&output)
        .with_context(|| format!("cannot write {}", output.display()))?;
    // Replace mode removes the source, but never a file we just wrote onto.
    if !keep && output != input {
        std::fs::remove_file(input)
            .with_context(|| format!("cannot remove {}", input.display()))?;
    }
    Ok(output)
}

/// Point a working-set entry at the file that replaced it: update the path and
/// its list row, and drop the now-stale rendered preview from the cache. A
/// no-op if the old path is no longer in the set (e.g. it was cleared).
fn replace_working_path(app: &AppWindow, old: &Path, new: &Path) {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        if let Some(i) = st.paths.iter().position(|p| p == old) {
            st.paths[i] = new.to_path_buf();
            st.files_model.set_row_data(i, make_row(new));
        }
        st.cache.remove(&old.to_path_buf());
    });
    update_memory(app);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_data;

    #[test]
    fn process_one_replaces_source_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("frame.fit");
        std::fs::copy(test_data("uncompressed.fit"), &input).unwrap();

        // Compress in replace mode: source removed, `.fz` written beside it.
        let compressed = process_one(
            Operation::Compress(CompressionType::Rice1),
            &input,
            None,
            false,
        )
        .unwrap();
        assert_eq!(compressed, tmp.path().join("frame.fit.fz"));
        assert!(compressed.is_file());
        assert!(!input.exists(), "source should be removed in replace mode");

        // Decompress it back in replace mode: `.fz` removed, `.fit` restored.
        let restored = process_one(Operation::Decompress, &compressed, None, false).unwrap();
        assert_eq!(restored, input);
        assert!(restored.is_file());
        assert!(!compressed.exists());
    }

    #[test]
    fn process_one_keep_mode_leaves_source_and_writes_to_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("frame.fit");
        std::fs::copy(test_data("uncompressed.fit"), &input).unwrap();
        let out_dir = tmp.path().join("out");
        std::fs::create_dir(&out_dir).unwrap();

        let compressed = process_one(
            Operation::Compress(CompressionType::Gzip1),
            &input,
            Some(&out_dir),
            true,
        )
        .unwrap();
        assert_eq!(compressed, out_dir.join("frame.fit.fz"));
        assert!(compressed.is_file());
        assert!(
            input.exists(),
            "source must be kept when writing to a directory"
        );
    }
}
