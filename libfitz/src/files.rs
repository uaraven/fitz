//! Recognising the image files the tools can open: FITS (including
//! tile-compressed `.fz`) and camera RAW. The predicates are pure path logic;
//! [`find_images`] walks a folder tree but never opens or decodes a file.

use std::path::{Path, PathBuf};

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

/// Every openable image anywhere under `dir`, depth-first with the entries of
/// each folder in name order. No file is opened. Images whose extension is in
/// `skip_extensions` are left out. Folders that cannot be read are skipped;
/// symlinked folders are not followed (so a link cannot loop), symlinked files
/// are.
pub fn find_images(dir: &Path, skip_extensions: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_images(dir, skip_extensions, &mut out);
    out
}

fn collect_images(dir: &Path, skip_extensions: &[&str], out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_images(&path, skip_extensions, out);
        } else if (file_type.is_file() || (file_type.is_symlink() && path.is_file()))
            && is_openable_image(&path)
            && !has_extension(&path, skip_extensions)
        {
            out.push(path);
        }
    }
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

    #[test]
    fn find_images_walks_subfolders_in_name_order_and_honours_the_skip_list() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        for rel in ["b.fit", "a/z.fits", "a/y.tif", "a/deep/x.fz", "c/notes.txt"] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
        }
        let rel = |paths: Vec<PathBuf>| -> Vec<String> {
            paths
                .iter()
                .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
                .collect()
        };

        assert_eq!(
            rel(find_images(root, &[])),
            ["a/deep/x.fz", "a/y.tif", "a/z.fits", "b.fit"]
        );
        assert_eq!(
            rel(find_images(root, &["TIF"])),
            ["a/deep/x.fz", "a/z.fits", "b.fit"]
        );
        assert!(find_images(&root.join("missing"), &[]).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn appledouble_companions_are_not_images() {
        assert!(!is_fits_path(Path::new("/x/._frame.fits")));
        assert!(!is_raw_image(Path::new("/x/._IMG_0001.CR2")));
    }
}
