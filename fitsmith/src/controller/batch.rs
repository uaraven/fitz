//! The skeleton shared by the modal file batches (compress / decompress, export
//! and crop): put up the progress overlay, walk the target files on a worker
//! thread — checking for a cancel between files, reporting progress, badging
//! the files that fail — then hand the results back to the UI thread.
//!
//! A batch supplies only what is its own: what to do to one file, and what to
//! do with the results ([`spawn_batch`]). The loop itself ([`run_files`]) knows
//! nothing about the window, so it is unit-tested directly.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use slint::Weak;

use crate::AppWindow;
use crate::files::display_name;

use super::{batch_is_current, begin_file_batch, set_row_status};

/// Which of the window's progress-overlay property sets a batch drives.
/// Compress, decompress and crop share one (only one batch runs at a time);
/// export has its own.
#[derive(Clone, Copy)]
pub(super) enum Overlay {
    Convert,
    Export,
}

impl Overlay {
    /// Raise the overlay at 0%, mark the window busy, and — for the shared
    /// overlay — say what is running. (The export overlay has a fixed title.)
    fn show(self, app: &AppWindow, title: &str) {
        match self {
            Overlay::Convert => {
                app.set_convert_title(title.into());
                app.set_convert_in_progress(true);
                app.set_convert_progress(0.0);
                app.set_convert_progress_text("".into());
                app.set_convert_progress_detail("".into());
            }
            Overlay::Export => {
                app.set_export_in_progress(true);
                app.set_export_progress(0.0);
                app.set_export_progress_text("".into());
                app.set_export_progress_detail("".into());
            }
        }
        app.set_busy(true);
        app.set_stage_text("".into());
    }

    /// Show the file being processed: how far along the batch is, its name, and
    /// its "3/10" position.
    fn progress(self, app: &AppWindow, fraction: f32, text: String, detail: String) {
        match self {
            Overlay::Convert => {
                app.set_convert_progress(fraction);
                app.set_convert_progress_text(text.into());
                app.set_convert_progress_detail(detail.into());
            }
            Overlay::Export => {
                app.set_export_progress(fraction);
                app.set_export_progress_text(text.into());
                app.set_export_progress_detail(detail.into());
            }
        }
    }

    /// Take the overlay down and clear the busy state.
    fn hide(self, app: &AppWindow) {
        match self {
            Overlay::Convert => {
                app.set_convert_in_progress(false);
                app.set_convert_progress(1.0);
            }
            Overlay::Export => {
                app.set_export_in_progress(false);
                app.set_export_progress(1.0);
            }
        }
        app.set_busy(false);
        app.set_stage_text("".into());
    }
}

/// What a batch did: the files it handled, with each one's result, how many
/// failed, and whether it was cut short by a cancel.
pub(super) struct Run<R> {
    pub done: Vec<(PathBuf, R)>,
    pub failed: usize,
    pub canceled: bool,
}

/// Walk `targets`, calling `step` on each. Before every file it checks `cancel`
/// (a raised flag ends the walk, leaving the rest untouched) and calls
/// `on_progress(index, total, file)`; a file whose `step` fails is reported to
/// `on_failure` with the full error chain and the walk goes on.
pub(super) fn run_files<R>(
    targets: Vec<PathBuf>,
    cancel: &AtomicBool,
    mut step: impl FnMut(&Path) -> Result<R>,
    mut on_progress: impl FnMut(usize, usize, &Path),
    mut on_failure: impl FnMut(&Path, String),
) -> Run<R> {
    let total = targets.len();
    let mut run = Run {
        done: Vec::new(),
        failed: 0,
        canceled: false,
    };
    for (i, input) in targets.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            run.canceled = true;
            break;
        }
        on_progress(i, total, &input);
        match step(&input) {
            Ok(result) => run.done.push((input, result)),
            Err(e) => {
                run.failed += 1;
                on_failure(&input, format!("{e:#}"));
            }
        }
    }
    run
}

/// Run a batch behind the progress overlay: `step` is applied to every target
/// on a worker thread (see [`run_files`]; a failed file is badged with its
/// error), then `finish` runs on the UI thread with the results and whether
/// this batch is still the live one. `finish` must apply the results to the
/// working set either way, but leave the status bar alone when it is not —
/// a later batch owns the UI then.
pub(super) fn spawn_batch<R: Send + 'static>(
    weak: Weak<AppWindow>,
    overlay: Overlay,
    title: String,
    targets: Vec<PathBuf>,
    step: impl FnMut(&Path) -> Result<R> + Send + 'static,
    finish: impl FnOnce(&AppWindow, Run<R>, bool) + Send + 'static,
) {
    let (generation, cancel) = begin_file_batch();
    let _ = weak.upgrade_in_event_loop(move |app| overlay.show(&app, &title));
    std::thread::spawn(move || {
        let run = run_files(
            targets,
            &cancel,
            step,
            |i, total, input| {
                let fraction = i as f32 / total as f32;
                let text = display_name(input);
                let detail = format!("{}/{}", i + 1, total);
                let _ = weak.upgrade_in_event_loop(move |app| {
                    overlay.progress(&app, fraction, text, detail)
                });
            },
            |input, message| {
                let input = input.to_path_buf();
                let _ = weak.upgrade_in_event_loop(move |_app| {
                    set_row_status(&input, "error", &message);
                });
            },
        );
        let _ = weak.upgrade_in_event_loop(move |app| {
            let current = batch_is_current(generation);
            if current {
                overlay.hide(&app);
            }
            finish(&app, run, current);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn every_file_is_stepped_with_progress_and_failures_do_not_stop_the_walk() {
        let cancel = AtomicBool::new(false);
        let mut progress = Vec::new();
        let mut failures = Vec::new();
        let run = run_files(
            paths(&["a", "bad1", "c", "bad2"]),
            &cancel,
            |p| {
                if p.to_string_lossy().starts_with("bad") {
                    bail!("cannot read {}", p.display());
                }
                Ok(p.to_string_lossy().len())
            },
            |i, total, p| progress.push((i, total, p.to_path_buf())),
            |p, msg| failures.push((p.to_path_buf(), msg)),
        );

        assert_eq!(
            run.done,
            vec![(PathBuf::from("a"), 1), (PathBuf::from("c"), 1)]
        );
        assert_eq!(run.failed, 2);
        assert!(!run.canceled);
        assert_eq!(progress.len(), 4);
        assert_eq!(progress[2], (2, 4, PathBuf::from("c")));
        assert_eq!(
            failures,
            vec![
                (PathBuf::from("bad1"), "cannot read bad1".to_string()),
                (PathBuf::from("bad2"), "cannot read bad2".to_string()),
            ]
        );
    }

    #[test]
    fn a_cancel_stops_before_the_next_file_and_keeps_what_was_done() {
        let cancel = AtomicBool::new(false);
        let mut progress = 0;
        let run = run_files(
            paths(&["a", "b", "c", "d"]),
            &cancel,
            |p| {
                // The user cancels while the second file is in flight.
                if p == Path::new("b") {
                    cancel.store(true, Ordering::Relaxed);
                }
                Ok(())
            },
            |_, _, _| progress += 1,
            |_, _| {},
        );

        assert!(run.canceled);
        // The file in flight finishes; the rest are not started.
        assert_eq!(run.done.len(), 2);
        assert_eq!(progress, 2);
    }

    #[test]
    fn a_batch_canceled_before_it_starts_does_nothing() {
        let cancel = AtomicBool::new(true);
        let run = run_files(
            paths(&["a"]),
            &cancel,
            |_| -> Result<()> { unreachable!("no file should be stepped") },
            |_, _, _| unreachable!("no progress should be reported"),
            |_, _| {},
        );
        assert!(run.canceled);
        assert!(run.done.is_empty());
        assert_eq!(run.failed, 0);
    }

    #[test]
    fn a_failure_message_carries_the_whole_error_chain() {
        let cancel = AtomicBool::new(false);
        let mut messages = Vec::new();
        run_files(
            paths(&["a"]),
            &cancel,
            |_| -> Result<()> {
                Err(anyhow::anyhow!("disk full").context("cannot write a_cropped.fits"))
            },
            |_, _, _| {},
            |_, msg| messages.push(msg),
        );
        assert_eq!(messages, vec!["cannot write a_cropped.fits: disk full"]);
    }
}
