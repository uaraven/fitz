//! Pure, UI-free helpers for the working set of files: recognizing FITS paths,
//! scanning a directory, deriving display names, and stepping the blink
//! selection. Kept free of Slint so the logic is unit-testable without an
//! event loop.

use std::path::{Path, PathBuf};

// The extension lists and FITS/RAW path predicates live in `libfitz` so the
// CLI's session scan recognises exactly the same files; re-exported here so
// existing `crate::files::…` callers are unchanged.
use libfitz::files::has_extension;
pub use libfitz::files::{FITS_EXTENSIONS, RAW_EXTENSIONS, is_fits_path, is_openable_image};

/// Whether `path` is a tile-compressed FITS (`.fz`), used to badge the file row.
pub fn is_compressed(path: &Path) -> bool {
    has_extension(path, &["fz"])
}

/// The file's base name, falling back to the full path when it has none. The
/// full name is returned untouched — callers that need to fit a narrow widget
/// elide it in the UI layer.
pub fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The FITS files directly inside `dir`, sorted by path for a stable list
/// order. A directory that can't be read yields an empty set rather than an
/// error — the caller surfaces "no images" the same way either way.
pub fn scan_directory(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_openable_image(p))
        .collect();
    paths.sort();
    paths
}

/// Every openable image anywhere under `dir`, subfolders included, depth-first
/// in name order. Same extensions as [`scan_directory`]; unreadable folders are
/// skipped and symlinked folders are not followed.
pub fn scan_directory_recursive(dir: &Path) -> Vec<PathBuf> {
    libfitz::files::find_images(dir, &[])
}

/// Whether `dir` directly contains at least one subfolder. Decides whether
/// File ▸ Open Folder offers to include subfolders.
pub fn has_subdirectories(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .filter_map(|e| e.ok())
            .any(|e| e.file_type().is_ok_and(|t| t.is_dir()))
    })
}

/// The next selection index for blink / arrow scrubbing, wrapping around the
/// working set. Returns 0 for an empty set (nothing to select).
pub fn next_index(current: usize, len: usize) -> usize {
    if len == 0 { 0 } else { (current + 1) % len }
}

/// Where a compressed copy of `input` is written. Compression appends `.fz` to
/// the whole file name (`frame.fit` → `frame.fit.fz`), matching the `fitz` CLI.
/// With `output_dir` set the file lands in that directory (same base name);
/// otherwise it lands beside the source.
pub fn compressed_output_path(input: &Path, output_dir: Option<&Path>) -> PathBuf {
    let mut name = input
        .file_name()
        .map(|n| n.to_owned())
        .unwrap_or_else(|| input.as_os_str().to_owned());
    name.push(".fz");
    match output_dir {
        Some(dir) => dir.join(name),
        None => input.with_file_name(name),
    }
}

/// Where a decompressed copy of `input` is written. Decompression drops a
/// trailing `.fz` (`frame.fit.fz` → `frame.fit`); a name without `.fz` is kept
/// as-is. With `output_dir` set the (stripped) name lands in that directory;
/// otherwise it lands beside the source.
pub fn decompressed_output_path(input: &Path, output_dir: Option<&Path>) -> PathBuf {
    let stripped = if is_compressed(input) {
        input.with_extension("")
    } else {
        input.to_path_buf()
    };
    match output_dir {
        Some(dir) => {
            let name = stripped.file_name().unwrap_or(stripped.as_os_str());
            dir.join(name)
        }
        None => stripped,
    }
}

/// Where an exported copy of `input` is written: into `dir`, with the input's
/// base name (all FITS extensions stripped, including a `.fz` suffix) and the
/// export format's `ext`. So `frame.fit` → `<dir>/frame.<ext>` and
/// `frame.fit.fz` → `<dir>/frame.<ext>`. A base name containing dots (e.g.
/// `M31.LRGB.fit`) keeps them (`M31.LRGB.<ext>`).
pub fn export_output_path(input: &Path, dir: &Path, ext: &str) -> PathBuf {
    // Drop a trailing `.fz`, then the FITS extension, leaving the bare stem.
    let mut base = input.to_path_buf();
    if is_compressed(&base) {
        base.set_extension("");
    }
    if is_fits_path(&base) {
        base.set_extension("");
    }
    let mut name = base
        .file_name()
        .map(|n| n.to_owned())
        .unwrap_or_else(|| input.as_os_str().to_owned());
    name.push(".");
    name.push(ext);
    dir.join(name)
}

/// Where an "ignored" copy of `input`'s name lives: `.ignored` appended to the
/// whole file name (`frame.fits` → `frame.fits.ignored`), matching
/// [`compressed_output_path`]'s pattern. Tools ▸ Delete Files…'s "Rename to
/// .ignored" action uses this to take a file out of the working set without
/// deleting it.
pub fn ignored_output_path(input: &Path) -> PathBuf {
    let mut name = input
        .file_name()
        .map(|n| n.to_owned())
        .unwrap_or_else(|| input.as_os_str().to_owned());
    name.push(".ignored");
    input.with_file_name(name)
}

/// Expand command-line arguments into a flat list of input files: directories
/// are scanned for FITS files, existing files are kept as-is (a file named
/// explicitly is honored regardless of extension), and missing paths are
/// dropped.
pub fn expand_inputs(args: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for arg in args {
        if arg.is_dir() {
            out.extend(scan_directory(&arg));
        } else if arg.is_file() {
            out.push(arg);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_compressed_only_for_fz() {
        assert!(is_compressed(Path::new("a.FZ")));
        assert!(!is_compressed(Path::new("a.fits")));
    }

    #[test]
    fn display_name_uses_basename() {
        assert_eq!(display_name(Path::new("/x/y/frame.fit")), "frame.fit");
    }

    #[test]
    fn display_name_keeps_long_names_untouched() {
        let name = format!("{}.fit", "a".repeat(60)); // long, but not shortened
        assert_eq!(display_name(&PathBuf::from(format!("/x/{name}"))), name);
    }

    #[test]
    fn scan_directory_returns_sorted_fits_only() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["b.fits", "a.fit", "notes.txt", "c.fz"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        std::fs::create_dir(dir.path().join("sub.fits")).unwrap(); // a dir, not a file

        let found: Vec<String> = scan_directory(dir.path())
            .iter()
            .map(|p| display_name(p))
            .collect();
        assert_eq!(found, vec!["a.fit", "b.fits", "c.fz"]);
    }

    #[test]
    fn recursive_scan_finds_real_images_in_subfolders() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = |name: &str| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("test-data")
                .join(name)
        };
        std::fs::create_dir_all(dir.path().join("Lights/night1")).unwrap();
        std::fs::copy(fixture("uncompressed.fit"), dir.path().join("top.fit")).unwrap();
        std::fs::copy(
            fixture("compressed.fits.fz"),
            dir.path().join("Lights/night1/deep.fits.fz"),
        )
        .unwrap();
        std::fs::copy(fixture("comet.ARW"), dir.path().join("Lights/raw.ARW")).unwrap();
        std::fs::write(dir.path().join("Lights/notes.txt"), b"x").unwrap();

        let flat = scan_directory(dir.path());
        let deep: Vec<String> = scan_directory_recursive(dir.path())
            .iter()
            .map(|p| p.strip_prefix(dir.path()).unwrap().display().to_string())
            .collect();

        assert_eq!(flat.len(), 1);
        assert_eq!(
            deep,
            ["Lights/night1/deep.fits.fz", "Lights/raw.ARW", "top.fit"]
        );
    }

    #[test]
    fn has_subdirectories_only_for_real_subfolders() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.fit"), b"x").unwrap();
        assert!(!has_subdirectories(dir.path()));
        assert!(!has_subdirectories(Path::new("/no/such/dir/here")));

        std::fs::create_dir(dir.path().join("sub")).unwrap();
        assert!(has_subdirectories(dir.path()));
    }

    #[test]
    fn scan_missing_directory_is_empty() {
        assert!(scan_directory(Path::new("/no/such/dir/here")).is_empty());
    }

    #[test]
    fn next_index_wraps_and_handles_empty() {
        assert_eq!(next_index(0, 3), 1);
        assert_eq!(next_index(2, 3), 0);
        assert_eq!(next_index(0, 1), 0);
        assert_eq!(next_index(0, 0), 0);
    }

    #[test]
    fn compressed_output_appends_fz() {
        assert_eq!(
            compressed_output_path(Path::new("/x/frame.fit"), None),
            PathBuf::from("/x/frame.fit.fz")
        );
        assert_eq!(
            compressed_output_path(Path::new("/x/frame.fit"), Some(Path::new("/out"))),
            PathBuf::from("/out/frame.fit.fz")
        );
    }

    #[test]
    fn ignored_output_appends_ignored_to_whole_name() {
        assert_eq!(
            ignored_output_path(Path::new("/x/frame.fits")),
            PathBuf::from("/x/frame.fits.ignored")
        );
        assert_eq!(
            ignored_output_path(Path::new("/x/frame.fit.fz")),
            PathBuf::from("/x/frame.fit.fz.ignored")
        );
    }

    #[test]
    fn decompressed_output_strips_fz() {
        assert_eq!(
            decompressed_output_path(Path::new("/x/frame.fit.fz"), None),
            PathBuf::from("/x/frame.fit")
        );
        assert_eq!(
            decompressed_output_path(Path::new("/x/frame.fit.fz"), Some(Path::new("/out"))),
            PathBuf::from("/out/frame.fit")
        );
        // A name without a .fz extension is kept as-is (only the directory moves).
        assert_eq!(
            decompressed_output_path(Path::new("/x/frame.fits"), None),
            PathBuf::from("/x/frame.fits")
        );
    }

    #[test]
    fn export_output_path_strips_fits_extensions_and_applies_format_ext() {
        let dir = Path::new("/out");
        assert_eq!(
            export_output_path(Path::new("/x/frame.fit"), dir, "tiff"),
            PathBuf::from("/out/frame.tiff")
        );
        // A .fz-compressed input drops both the .fz and the FITS extension.
        assert_eq!(
            export_output_path(Path::new("/x/frame.fit.fz"), dir, "png"),
            PathBuf::from("/out/frame.png")
        );
        // Dots inside the base name are preserved.
        assert_eq!(
            export_output_path(Path::new("/x/M31.LRGB.fits"), dir, "jpg"),
            PathBuf::from("/out/M31.LRGB.jpg")
        );
        // A name without a FITS extension keeps its whole name as the base.
        assert_eq!(
            export_output_path(Path::new("/x/frame"), dir, "fits"),
            PathBuf::from("/out/frame.fits")
        );
    }

    #[test]
    fn expand_inputs_scans_dirs_keeps_files_drops_missing() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["b.fits", "a.fit", "notes.txt"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let loose = dir.path().join("notes.txt"); // a file named explicitly
        let missing = dir.path().join("gone.fits");

        let found: Vec<String> = expand_inputs(vec![
            dir.path().to_path_buf(), // scanned → a.fit, b.fits (FITS only, sorted)
            loose,                    // explicit file → kept despite .txt
            missing,                  // does not exist → dropped
        ])
        .iter()
        .map(|p| display_name(p))
        .collect();

        assert_eq!(found, vec!["a.fit", "b.fits", "notes.txt"]);
    }
}
