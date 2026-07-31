//! The `stretch` command: load a FITS image (debayering it first if needed),
//! apply an MTF/STF auto-stretch, and save the 16-bit result as FITS or TIFF.

use std::path::Path;

use crate::io_prompt::{ensure_can_write, print_load_rgb_notice, print_progress, print_step};
use crate::options::StretchOptions;
use anyhow::Result;
use libfitz::data::PixelBuffer;
use libfitz::debayer::OutputFormat;
use libfitz::fits_file::SaveOptions;
use libfitz::fits_image::{CFA_KEYWORDS, write_rgb16_fits, write_rgb16_tiff};
use libfitz::fitskit::Bitpix;

pub fn stretch_file(input: &Path, output: &Path, opts: &StretchOptions) -> Result<()> {
    ensure_can_write(output, opts.yes)?;
    print_progress(opts.verbose, input, output);

    print_step(opts.verbose, "reading");
    let image = libfitz::fits_file::load_fits(input)?;

    print_step(opts.verbose, "stretching");
    let stretched = image.stretch(opts.core.linked, opts.core.brightness);

    print_step(opts.verbose, "writing");
    match opts.format {
        OutputFormat::Tiff => {
            // TODO: Write tiff
            // write_rgb16_tiff(output, stretched.width, stretched.height, &stretched.pixels)
            Ok(())
        }
        OutputFormat::Fits => {
            let history = format!("stretched by fitz {}", env!("CARGO_PKG_VERSION"));
            let options = SaveOptions {
                bitpix: match image.pixels {
                    PixelBuffer::U16(_) => Bitpix::I16,
                    PixelBuffer::F32(_) => Bitpix::F32,
                },
                compress_options: None,
            };
            libfitz::fits_file::save_fits(output, &stretched, options)?;
            Ok(())
        }
    }
}
