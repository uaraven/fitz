//! Tools ▸ Session Summary…: group the working set's files (the checked ones,
//! or all of them when none are checked — see [`super::operation_targets`])
//! with [`libfitz::session::summarize_files`] and show their sessions, frame
//! types and filters as a read-only tree with frame counts and integration
//! times, and copy that report — in the layout `fitz summary` prints — to the
//! clipboard.
//!
//! Reading the headers runs on a worker thread; its result is applied only if
//! the dialog is still showing that run (a generation guard, like the other
//! batches). The grouping itself is `libfitz`'s — this module only flattens
//! the [`Summary`] into display rows.

use libfitz::session::{Summary, format_duration, format_timestamp, summarize_files};
use libfitz::session_report::format_summary;
use slint::ComponentHandle;

use super::{STATE, operation_targets};
use crate::{AppWindow, SessionRow};

/// Flatten `summary` depth-first into display lines: each session (depth 0),
/// then its frame types (1), then each type's filters (2). A session's time is
/// its light integration; every other line shows its own total.
fn flatten(summary: &Summary) -> Vec<SessionRow> {
    let row = |depth: i32, label: String, count: usize, integration: f64| SessionRow {
        depth,
        label: label.into(),
        count: frame_count(count).into(),
        time: format_duration(integration).into(),
    };
    let mut rows = Vec::new();
    for session in &summary.sessions {
        let mut label = session.label();
        if let (Some(start), Some(end)) = (session.start, session.end) {
            label = format!("{label}  {} – {}", clock(start), clock(end));
        }
        rows.push(row(0, label, session.count, session.integration));
        for group in &session.types {
            let label = group.frame_type.label().to_string();
            rows.push(row(1, label, group.count, group.integration));
            for filter in &group.filters {
                let label = filter.label().to_string();
                rows.push(row(2, label, filter.count, filter.integration));
            }
        }
    }
    rows
}

/// The time-of-day part of a timestamp (`"21:30"` from `"2026-06-22 21:30"`).
fn clock(epoch: f64) -> String {
    let stamp = format_timestamp(epoch);
    stamp
        .split_once(' ')
        .map_or(stamp.clone(), |(_, time)| time.to_string())
}

/// `"1 frame"` / `"92 frames"`.
fn frame_count(count: usize) -> String {
    format!("{count} frame{}", if count == 1 { "" } else { "s" })
}

/// The totals line under the tree: sessions, light frames, light integration
/// and — when some images could not be read — how many were left out.
fn summary_line(summary: &Summary) -> String {
    let mut line = format!(
        "{}, {} light, {} integration",
        match summary.sessions.len() {
            1 => "1 session".to_string(),
            n => format!("{n} sessions"),
        },
        frame_count(summary.light_count()),
        format_duration(summary.integration),
    );
    if !summary.errors.is_empty() {
        line.push_str(&format!(" · {} unreadable", summary.errors.len()));
    }
    line
}

/// Open the dialog on the working set's files — the checked ones, or all when
/// none are checked — and read their headers on a worker thread. A no-op for an
/// empty working set (the menu item is disabled then anyway).
pub fn open_sessions_dialog(app: &AppWindow) {
    let targets = operation_targets(|_| true);
    if targets.is_empty() {
        return;
    }
    let total = STATE.with(|s| s.borrow().paths.len());
    let generation = STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.sessions_generation += 1;
        st.sessions_report.clear();
        st.sessions_model.set_vec(Vec::new());
        st.sessions_generation
    });
    let source = if targets.len() == total {
        format!("All {} in the working set", files_noun(total))
    } else {
        format!("{} checked in the working set", files_noun(targets.len()))
    };
    app.set_sessions_source(source.into());
    app.set_sessions_summary("Reading headers…".into());
    app.set_sessions_notice("".into());
    app.set_show_sessions(true);

    let weak = app.as_weak();
    std::thread::spawn(move || {
        let summary = summarize_files(&targets);
        let _ = weak.upgrade_in_event_loop(move |app| apply_summary(&app, generation, summary));
    });
}

/// `"1 file"` / `"12 files"`.
fn files_noun(count: usize) -> String {
    format!("{count} file{}", if count == 1 { "" } else { "s" })
}

/// Show a finished summary — unless the dialog was closed or reopened
/// meanwhile, in which case the result is stale and dropped.
fn apply_summary(app: &AppWindow, generation: u64, summary: Summary) {
    if STATE.with(|s| s.borrow().sessions_generation) != generation {
        return;
    }
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.sessions_model.set_vec(flatten(&summary));
        st.sessions_report = format_summary(&summary);
    });
    let line = if summary.sessions.is_empty() {
        "No readable images".to_string()
    } else {
        summary_line(&summary)
    };
    app.set_sessions_summary(line.into());
}

/// Put the report on the system clipboard and say so (or why not) in the
/// dialog. A no-op while the headers are still being read.
pub fn copy_sessions(app: &AppWindow) {
    let notice = STATE.with(|s| {
        let mut st = s.borrow_mut();
        let st = &mut *st; // split the borrow between the report and the clipboard
        if st.sessions_report.is_empty() {
            return None;
        }
        let result = set_clipboard_text(&mut st.clipboard, &st.sessions_report);
        Some(match result {
            Ok(()) => "Copied to clipboard".to_string(),
            Err(e) => format!("Copy failed: {e}"),
        })
    });
    if let Some(notice) = notice {
        app.set_sessions_notice(notice.into());
    }
}

/// Set the clipboard's text, opening the clipboard on first use and keeping
/// the handle in `slot` afterwards.
fn set_clipboard_text(
    slot: &mut Option<arboard::Clipboard>,
    text: &str,
) -> Result<(), arboard::Error> {
    if slot.is_none() {
        *slot = Some(arboard::Clipboard::new()?);
    }
    slot.as_mut().expect("just opened").set_text(text)
}

/// Dialog dismissed: forget the rows and make any read still running land as
/// stale.
pub fn close_sessions(_app: &AppWindow) {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.sessions_generation += 1;
        st.sessions_report.clear();
        st.sessions_model.set_vec(Vec::new());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_data;

    #[test]
    fn real_frames_flatten_into_a_session_type_filter_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let lights = tmp.path().join("Lights");
        let darks = tmp.path().join("DARKS");
        std::fs::create_dir_all(&lights).unwrap();
        std::fs::create_dir_all(&darks).unwrap();
        let files = [
            lights.join("a.fit"),
            lights.join("b.fits"),
            darks.join("IMG_0001.ARW"),
        ];
        std::fs::copy(test_data("uncompressed.fit"), &files[0]).unwrap();
        std::fs::copy(test_data("bad-image/good1.fits"), &files[1]).unwrap();
        std::fs::copy(test_data("comet.ARW"), &files[2]).unwrap();

        let summary = summarize_files(&files);
        let rows = flatten(&summary);

        // Depths only ever step down by one, starting at a session.
        assert_eq!(rows[0].depth, 0);
        assert!(rows.windows(2).all(|w| w[1].depth <= w[0].depth + 1));
        let line = |label: &str| rows.iter().find(|r| r.label == label).unwrap();
        // The two light fixtures were taken on different nights, so each night
        // has its own "Lights" line; together they hold both frames.
        let frames = |r: &SessionRow| r.count.split(' ').next().unwrap().parse::<usize>().unwrap();
        let lights: Vec<&SessionRow> = rows.iter().filter(|r| r.label == "Lights").collect();
        assert!(lights.iter().all(|r| r.depth == 1));
        assert_eq!(lights.iter().map(|r| frames(r)).sum::<usize>(), 2);
        assert_eq!(line("Darks").count, "1 frame");
        assert_eq!(line("Narrowband 7nm").depth, 2);
        assert_eq!(line("Narrowband 7nm").time, "0h 03m 00s"); // EXPTIME 180 s
        assert_eq!(line("HaOIII 7nm").count, "1 frame");
        assert_eq!(line("(no filter)").depth, 2); // the RAW dark

        // Every frame is counted once at the filter level.
        let leaf_frames: usize = rows.iter().filter(|r| r.depth == 2).map(frames).sum();
        assert_eq!(leaf_frames, 3);
    }

    #[test]
    fn totals_line_counts_lights_and_flags_unreadable_files() {
        let tmp = tempfile::tempdir().unwrap();
        let (good, broken) = (tmp.path().join("a.fit"), tmp.path().join("broken.fits"));
        std::fs::copy(test_data("uncompressed.fit"), &good).unwrap();
        std::fs::write(&broken, b"not a fits file").unwrap();

        let summary = summarize_files(&[good, broken]);
        let line = summary_line(&summary);

        assert!(line.starts_with("1 session, 1 frame light, "), "{line}");
        assert!(line.contains("0h 03m 00s integration"), "{line}"); // EXPTIME 180 s
        assert!(line.ends_with("· 1 unreadable"), "{line}");
    }

    #[test]
    fn clock_is_the_time_of_day() {
        let epoch = libfitz::fits_file::parse_date_obs("2026-06-22T21:30:45").unwrap();
        assert_eq!(clock(epoch), "21:30");
    }
}
