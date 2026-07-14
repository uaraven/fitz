//! Pure, UI-free helpers for the working set of files: recognizing FITS paths,
//! scanning a directory, deriving display names, and stepping the blink
//! selection. Kept free of Slint and `fitz-core` so the logic is unit-testable
//! without an event loop.

use std::path::{Path, PathBuf};

/// Extensions FitSmith treats as openable FITS images (`.fz` is a
/// tile-compressed FITS, transparently decompressed on read).
const FITS_EXTENSIONS: &[&str] = &["fit", "fits", "fts", "fz"];

fn has_extension(path: &Path, candidates: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| candidates.iter().any(|c| c.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Whether `path` looks like a FITS image we can open.
pub fn is_fits_path(path: &Path) -> bool {
    has_extension(path, FITS_EXTENSIONS)
}

/// Whether `path` is a tile-compressed FITS (`.fz`), used to badge the file row.
pub fn is_compressed(path: &Path) -> bool {
    has_extension(path, &["fz"])
}

/// The file's base name, falling back to the full path when it has none.
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
        .filter(|p| p.is_file() && is_fits_path(p))
        .collect();
    paths.sort();
    paths
}

/// The next selection index for blink / arrow scrubbing, wrapping around the
/// working set. Returns 0 for an empty set (nothing to select).
pub fn next_index(current: usize, len: usize) -> usize {
    if len == 0 { 0 } else { (current + 1) % len }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_fits_extensions_case_insensitively() {
        assert!(is_fits_path(Path::new("a.fit")));
        assert!(is_fits_path(Path::new("a.FITS")));
        assert!(is_fits_path(Path::new("a.Fts")));
        assert!(is_fits_path(Path::new("a.fz")));
        assert!(!is_fits_path(Path::new("a.png")));
        assert!(!is_fits_path(Path::new("noext")));
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
}
