//! The `stretch` command: load a FITS image (debayering it first if needed),
//! apply an MTF/STF auto-stretch, and save the 16-bit result as FITS or TIFF.

use std::path::Path;

use anyhow::Result;
use fitz_core::debayer::OutputFormat;
use fitz_core::fits_image::{CFA_KEYWORDS, LoadRgbNotice, write_rgb16_fits, write_rgb16_tiff};

use crate::io_prompt::{ensure_can_write, print_progress, print_step};
use crate::options::StretchOptions;
use crate::terminal::print_warning;

pub fn stretch_file(input: &Path, output: &Path, opts: &StretchOptions) -> Result<()> {
    ensure_can_write(output, opts.yes)?;
    print_progress(opts.verbose, input, output);

    print_step(opts.verbose, "reading");
    let stretched = fitz_core::stretch::load_and_stretch(input, &opts.core)?;

    match stretched.notice {
        LoadRgbNotice::AlreadyDebayeredRgbCube => {
            println!(
                "{}: already debayered — skipping debayer step",
                input.display()
            );
        }
        LoadRgbNotice::AlreadyDebayeredMono => {
            print_warning(&format!(
                "{}: 1-channel image with no BAYERPAT header — treating it as an already-debayered \
                 monochrome image",
                input.display()
            ));
        }
        LoadRgbNotice::Demosaiced => {
            print_step(opts.verbose, "debayering");
        }
    }
    print_step(opts.verbose, "stretching");

    print_step(opts.verbose, "writing");
    match opts.format {
        OutputFormat::Tiff => {
            write_rgb16_tiff(output, stretched.width, stretched.height, &stretched.pixels)
        }
        OutputFormat::Fits => {
            let history = format!("stretched by fitz {}", env!("CARGO_PKG_VERSION"));
            write_rgb16_fits(
                output,
                stretched.width,
                stretched.height,
                &stretched.pixels,
                Some(&stretched.header),
                CFA_KEYWORDS,
                Some(&history),
            )
        }
    }
}
