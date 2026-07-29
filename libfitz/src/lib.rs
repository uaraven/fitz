//! Reusable FITS/image processing logic behind the `fitz` CLI: FITS I/O
//! (including transparent tile-compression), debayering, auto-stretch,
//! per-channel splitting, header inspection, and image resizing. Contains no
//! CLI parsing, terminal output, or interactive prompts — those live in the
//! `fitz` binary crate, which is a thin wrapper over this library.

pub use fitskit;

pub mod analytics;
pub mod compress;
pub mod copy_header;
pub mod debayer;
pub mod decompress;
pub mod export;
pub mod fits_image;
pub mod info;
pub mod inspect;
pub mod preview;
pub mod resize;
pub mod split_channel;
pub mod stars;
pub mod stretch;

mod errors;
pub mod data;
pub mod fits_file;
mod keywords;
mod fits_bayer;
#[cfg(test)]
pub(crate) mod test_support;
