//! Recognising the image files the tools can open: FITS (including
//! tile-compressed `.fz`) and camera RAW. Pure path logic — nothing here
//! touches the file system or decodes a file.

use std::path::Path;

/// Extensions treated as openable FITS images (`.fz` is a tile-compressed
/// FITS, transparently decompressed on read).
pub const FITS_EXTENSIONS: &[&str] = &["fit", "fits", "fts", "fz"];

/// Extensions treated as camera RAW images (decoded through LibRaw).
pub const RAW_EXTENSIONS: &[&str] = &[
    "3fr", "ari", "arw", "bay", "braw", "crw", "cr2", "cr3", "cap", "data", "dcs", "dcr", "dng",
    "drf", "eip", "erf", "fff", "gpr", "iiq", "k25", "kdc", "mdc", "mef", "mos", "mrw", "nef",
    "nrw", "obm", "orf", "pef", "ptx", "pxn", "r3d", "raf", "raw", "rwl", "rw2", "rwz", "sr2",
    "srf", "srw", "tif", "x3f",
];

/// Whether `path`'s extension is one of `candidates`, ignoring ASCII case.
pub fn has_extension(path: &Path, candidates: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| candidates.iter().any(|c| c.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Whether `path` looks like a FITS image we can open.
pub fn is_fits_path(path: &Path) -> bool {
    has_extension(path, FITS_EXTENSIONS) && !macos_special_file(path)
}

/// Whether `path` looks like a camera RAW image we can open.
pub fn is_raw_image(path: &Path) -> bool {
    has_extension(path, RAW_EXTENSIONS) && !macos_special_file(path)
}

/// Whether `path` is a FITS or RAW image we can open.
pub fn is_openable_image(path: &Path) -> bool {
    is_fits_path(path) || is_raw_image(path)
}

/// macOS litters non-native volumes with AppleDouble `._name` companions that
/// carry the original's extension but aren't images.
#[cfg(target_os = "macos")]
fn macos_special_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|f| f.to_str())
        .map(|f| f.starts_with("._"))
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn macos_special_file(_path: &Path) -> bool {
    false
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
    fn recognizes_raw_extensions_case_insensitively() {
        assert!(is_raw_image(Path::new("IMG_0001.CR2")));
        assert!(is_raw_image(Path::new("comet.ARW")));
        assert!(is_raw_image(Path::new("a.dng")));
        assert!(!is_raw_image(Path::new("a.fits")));
        assert!(!is_raw_image(Path::new("noext")));
    }

    #[test]
    fn openable_is_fits_or_raw() {
        assert!(is_openable_image(Path::new("a.fits")));
        assert!(is_openable_image(Path::new("a.nef")));
        assert!(!is_openable_image(Path::new("notes.txt")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn appledouble_companions_are_not_images() {
        assert!(!is_fits_path(Path::new("/x/._frame.fits")));
        assert!(!is_raw_image(Path::new("/x/._IMG_0001.CR2")));
    }
}
