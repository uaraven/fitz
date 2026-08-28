//! The Tools ▸ Delete Files… dialog: delete the checked files from disk, or
//! rename them out of the pipeline (`frame.fits` → `frame.fits.ignored`), then
//! drop the affected rows from the working set. Unlike the other Tools-menu
//! batch operations ([`super::convert`], [`super::export`]) this never falls
//! back to "all files" when nothing is checked — it only ever touches checked
//! rows, and does its (cheap, syscall-only) work synchronously rather than on
//! a worker thread.

use std::path::{Path, PathBuf};

use slint::Model;

use crate::AppWindow;
use crate::files::ignored_output_path;

use super::{STATE, count_checked_files, drop_rows, finish_removal, set_row_status};

/// Open the dialog: count the checked files it would act on and reset the
/// action dropdown to the safe default ("rename") before showing it.
pub fn open_delete_files_dialog(app: &AppWindow) {
    let count = STATE.with(|s| count_checked_files(&s.borrow().files_model));
    app.set_delete_files_count(count as i32);
    app.set_delete_files_action(0);
    app.set_show_delete_files(true);
}

/// The checked files' paths — never falls back to "all files" like
/// [`super::operation_targets`], since this action must only ever touch what
/// the user explicitly checked.
fn checked_targets() -> Vec<PathBuf> {
    STATE.with(|s| {
        let st = s.borrow();
        (0..st.files_model.row_count())
            .filter(|&i| st.files_model.row_data(i).is_some_and(|r| r.checked))
            .filter_map(|i| st.paths.get(i).cloned())
            .collect()
    })
}

/// Confirm the dialog: delete or rename every checked file on disk. A file
/// that fails keeps its row, badged with the error (matching the convert/
/// export convention); only files that succeed are dropped from the list.
pub fn run_delete_files(app: &AppWindow) {
    app.set_show_delete_files(false);
    let rename = app.get_delete_files_action() == 0;
    let targets = checked_targets();
    if targets.is_empty() {
        return;
    }

    let mut removed = Vec::new();
    let mut failed = 0usize;
    for path in &targets {
        match delete_one(path, rename) {
            Ok(()) => removed.push(path.clone()),
            Err(e) => {
                failed += 1;
                set_row_status(path, "error", &e.to_string());
            }
        }
    }

    drop_paths(app, &removed);

    let verb = if rename { "Renamed" } else { "Deleted" };
    let summary = if failed == 0 {
        format!("{verb} {} file(s)", removed.len())
    } else {
        format!("{verb} {} file(s), {failed} failed", removed.len())
    };
    app.set_status_text(summary.into());
}

/// Delete or rename one file on disk per the dialog's chosen action. Runs
/// synchronously — a filesystem delete/rename is a cheap syscall, unlike the
/// full FITS I/O compress/decompress does.
fn delete_one(path: &Path, rename: bool) -> std::io::Result<()> {
    if rename {
        std::fs::rename(path, ignored_output_path(path))
    } else {
        std::fs::remove_file(path)
    }
}

/// Drop the given paths from the working set, resolving each to its current
/// index by path (rather than a captured index) since that's what the
/// controller already does for a mutation driven by outside-the-model work —
/// see `convert::replace_working_path`/`apply_loaded_exposures`.
fn drop_paths(app: &AppWindow, paths: &[PathBuf]) {
    let targets = STATE.with(|s| {
        let st = s.borrow();
        paths
            .iter()
            .filter_map(|p| st.paths.iter().position(|q| q == p))
            .collect::<Vec<_>>()
    });
    if let Some(target) = drop_rows(targets) {
        finish_removal(app, target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_data;

    #[test]
    fn delete_one_removes_file_permanently() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("frame.fit");
        std::fs::copy(test_data("uncompressed.fit"), &input).unwrap();

        delete_one(&input, false).unwrap();

        assert!(!input.exists());
    }

    #[test]
    fn delete_one_renames_to_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("frame.fit");
        std::fs::copy(test_data("uncompressed.fit"), &input).unwrap();

        delete_one(&input, true).unwrap();

        assert!(!input.exists());
        assert!(tmp.path().join("frame.fit.ignored").is_file());
    }

    #[test]
    fn delete_one_reports_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.fit");

        assert!(delete_one(&missing, false).is_err());
        assert!(delete_one(&missing, true).is_err());
    }
}
