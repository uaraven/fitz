//! The `summary` command: scan a directory tree of FITS / camera RAW images
//! and print how they group into observing sessions, frame types and filters,
//! with frame counts and integration times. All of the scanning and grouping is
//! `libfitz::session`, the report layout is `libfitz::session_report`; this
//! module only prints the result.

use std::path::Path;

use crate::io_prompt::print_step;
use anyhow::Result;
use libfitz::session::summarize_dir;
use libfitz::session_report::format_summary;

/// Scan `dir` and print its summary, headed by the directory's path when
/// `show_dir` is set (several directories in one run). Unreadable images go to
/// stderr in the usual `fitz: <path>: <err>` form. Returns whether the run was
/// clean: `false` if some image could not be read or no image was found at all.
pub fn summary_dir(dir: &Path, show_dir: bool, verbose: bool) -> Result<bool> {
    print_step(verbose, &format!("scanning {}", dir.display()));
    let summary = summarize_dir(dir)?;

    for (path, err) in &summary.errors {
        eprintln!("fitz: {}: {err}", path.display());
    }
    if summary.sessions.is_empty() && summary.errors.is_empty() {
        eprintln!("fitz: no images found in {}", dir.display());
        return Ok(false);
    }

    if show_dir {
        println!("{}:", dir.display());
    }
    print!("{}", format_summary(&summary));
    Ok(summary.errors.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_data;
    use libfitz::test_frames::write_frame_fits;

    #[test]
    fn a_tree_of_fits_and_raw_frames_is_reported_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let lights = root.join("Lights");
        let darks = root.join("DARKS");
        std::fs::create_dir_all(&lights).unwrap();
        std::fs::create_dir_all(&darks).unwrap();
        let ha = |name: &str, time: &str| {
            write_frame_fits(&lights.join(name), Some("Ha"), 300.0, Some(time), None)
        };
        ha("a.fit", "2026-06-22T22:00:00");
        ha("b.fit", "2026-06-22T22:10:00");
        std::fs::copy(test_data("comet.ARW"), darks.join("IMG_0001.ARW")).unwrap();

        let summary = summarize_dir(root).unwrap();
        let report = format_summary(&summary);

        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        assert!(
            report.contains(
                "Session 2026-06-22 - 0h 10m 00s light integration\n    2 light frames\n"
            ),
            "{report}"
        );
        assert!(
            report.contains("    Ha    2x300s    0h 10m 00s\n"),
            "{report}"
        );
        // The RAW frame is a dark, counted but not broken down by filter; its
        // night comes from its own EXIF time.
        assert!(report.contains("    1 dark frame\n"), "{report}");
        assert!(
            report.ends_with("total integration 0h 10m 00s\n"),
            "{report}"
        );
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        assert!(summary_dir(Path::new("/no/such/dir"), false, false).is_err());
    }

    #[test]
    fn an_empty_directory_is_not_a_clean_run() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!summary_dir(tmp.path(), false, false).unwrap());
    }
}
