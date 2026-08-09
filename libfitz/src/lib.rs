//! Reusable FITS/image processing logic behind the `fitz` CLI: FITS I/O
//! (including transparent tile-compression), debayering, auto-stretch,
//! per-channel splitting, header inspection, and image resizing. Contains no
//! CLI parsing, terminal output, or interactive prompts — those live in the
//! `fitz` binary crate, which is a thin wrapper over this library.

pub use fitskit;

pub mod copy_header;
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
mod fits_bayer;
pub mod fits_file;
mod keywords;
pub mod raw_fits;
pub mod stats;
pub mod summary;

#[cfg(test)]
pub(crate) mod test_support;
