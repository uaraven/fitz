//! Reusable FITS/image processing logic behind the `fitz` CLI: FITS I/O
//! (including transparent tile-compression), debayering, auto-stretch,
//! per-channel splitting, header inspection, and image resizing. Contains no
//! CLI parsing, terminal output, or interactive prompts — those live in the
//! `fitz` binary crate, which is a thin wrapper over this library.

pub use fitskit;

// On the MSVC target LibRaw (via rsraw-sys) is compiled with plain clang — see
// `.cargo/config.toml` — which, unlike cl/clang-cl, doesn't embed a default-lib
// directive for oldnames.lib, the UCRT's source of the POSIX aliases LibRaw
// calls (stricmp, strnicmp, swab). Declared here rather than as a rustflag so
// it also reaches rustdoc's doctest links and any downstream crate.
#[cfg(all(windows, target_env = "msvc"))]
#[link(name = "oldnames")]
unsafe extern "C" {}

pub mod copy_header;
pub mod crop;
pub mod debayer;
pub mod inspect;
pub mod preview;
pub mod resize;
pub mod split_channel;
pub mod stars;
pub mod stretch;

mod convert;
pub mod data;
pub mod export;
pub mod files;
mod fits_bayer;
pub mod fits_file;
mod keywords;
pub mod raw_fits;
pub mod session;
pub mod session_report;
pub mod stats;
pub mod summary;
mod xtrans;

pub mod contrast;
mod invert;
pub mod loader;
pub mod raw_file;
#[cfg(any(test, feature = "test-support"))]
pub mod test_frames;
#[cfg(test)]
pub(crate) mod test_support;

/// `Some(trimmed)` unless `s` is empty once trimmed — a header/EXIF string
/// field is blank rather than absent as often as it's simply missing.
pub(crate) fn non_blank(s: &str) -> Option<&str> {
    let s = s.trim();
    (!s.is_empty()).then_some(s)
}
