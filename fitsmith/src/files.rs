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

/// The list names for `paths`: each path relative to the deepest folder they
/// all share, so one folder's files show bare names and a set spanning
/// `night1/Lights` and `night2/Lights` shows `night1/Lights/a.fit` and
/// `night2/Lights/c.fit`. Paths with nothing in common (different drives)
/// keep their full path.
pub fn relative_names(paths: &[PathBuf]) -> Vec<String> {
    let common = libfitz::files::common_dir(paths);
    paths
        .iter()
        .map(|p| match p.strip_prefix(&common) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
            _ => display_name(p),
        })
        .collect()
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

/// The suffix the Crop toolbar starts with, and the one used when it is left blank.
pub const DEFAULT_CROP_SUFFIX: &str = "_cropped";

/// Where the cropped copy of `input` is written, beside it. The suffix goes
/// before the extension, and a trailing `.fz` stays last
/// (`frame.fits.fz` → `frame_cropped.fits.fz`); a blank suffix falls back to
/// [`DEFAULT_CROP_SUFFIX`] so the copy can never land on the original. A
/// non-FITS `input` (a camera RAW) is written as FITS: `IMG_1.ARW` →
/// `IMG_1_cropped.fits`.
pub fn cropped_output_path(input: &Path, suffix: &str, fits: bool) -> PathBuf {
    let suffix = match suffix.trim() {
        "" => DEFAULT_CROP_SUFFIX,
        s => s,
    };
    let name = display_name(input);
    let (body, fz) = if is_compressed(input) {
        name.split_at(name.len() - ".fz".len())
    } else {
        (name.as_str(), "")
    };
    let cropped = match body.rsplit_once('.') {
        Some((stem, ext)) if fits => format!("{stem}{suffix}.{ext}{fz}"),
        Some((stem, _)) => format!("{stem}{suffix}.fits"),
        None if fits => format!("{body}{suffix}{fz}"),
        None => format!("{body}{suffix}.fits"),
    };
    input.with_file_name(cropped)
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
    fn cropped_output_path_puts_the_suffix_before_the_extension() {
        let p = |name: &str, suffix: &str, fits: bool| {
            cropped_output_path(Path::new("/d").join(name).as_path(), suffix, fits)
        };
        assert_eq!(
            p("frame.fits", "_cropped", true),
            Path::new("/d/frame_cropped.fits")
        );
        assert_eq!(p("frame.fit", "_c", true), Path::new("/d/frame_c.fit"));
        assert_eq!(
            p("frame.fits.fz", "_cropped", true),
            Path::new("/d/frame_cropped.fits.fz")
        );
        // Dots in the base name stay put.
        assert_eq!(
            p("M31.LRGB.fit", "_x", true),
            Path::new("/d/M31.LRGB_x.fit")
        );
        // A blank suffix never maps a file onto itself.
        assert_eq!(p("a.fits", "  ", true), Path::new("/d/a_cropped.fits"));
        // Camera RAW is written out as FITS.
        assert_eq!(
            p("IMG_1.ARW", "_cropped", false),
            Path::new("/d/IMG_1_cropped.fits")
        );
    }

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
    fn relative_names_are_bare_file_names_within_one_folder() {
        let p = |s: &str| PathBuf::from(s);
        assert!(relative_names(&[]).is_empty());
        assert_eq!(relative_names(&[p("/d/Lights/a.fit")]), ["a.fit"]);
        assert_eq!(
            relative_names(&[p("/d/Lights/a.fit"), p("/d/Lights/b.fit")]),
            ["a.fit", "b.fit"]
        );
    }

    #[test]
    fn relative_names_are_relative_to_the_common_parent() {
        let p = |s: &str| PathBuf::from(s);
        let names = relative_names(&[
            p("/d/night1/Lights/a.fit"),
            p("/d/night1/Lights/b.fit"),
            p("/d/night2/Lights/c.fit"),
            p("/d/night1/Darks/d.fit"),
        ]);
        assert_eq!(
            names,
            [
                "night1/Lights/a.fit",
                "night1/Lights/b.fit",
                "night2/Lights/c.fit",
                "night1/Darks/d.fit"
            ]
        );
        // A file in the shared folder itself keeps its bare name; a common
        // parent must match whole components (`/d/ab` is not under `/d/a`).
        assert_eq!(
            relative_names(&[p("/d/a/x.fit"), p("/d/top.fit"), p("/d/ab/y.fit")]),
            ["a/x.fit", "top.fit", "ab/y.fit"]
        );
        assert_eq!(
            relative_names(&[p("/x/b/a.fit"), p("/a.fit")]),
            ["x/b/a.fit", "a.fit"]
        );
    }

    #[test]
    fn relative_names_on_a_real_recursive_scan() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("n1/Lights")).unwrap();
        std::fs::create_dir_all(dir.path().join("n2/Lights")).unwrap();
        let data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../test-data");
        for (src, dst) in [
            ("uncompressed.fit", "n1/Lights/a.fit"),
            ("compressed.fits.fz", "n2/Lights/b.fits.fz"),
        ] {
            std::fs::copy(data.join(src), dir.path().join(dst)).unwrap();
        }
        let paths = scan_directory_recursive(dir.path());
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            relative_names(&paths),
            [
                format!("n1{sep}Lights{sep}a.fit"),
                format!("n2{sep}Lights{sep}b.fits.fz")
            ]
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
