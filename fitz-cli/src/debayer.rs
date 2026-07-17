use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result};
pub use libfitz::debayer::OutputFormat;
use libfitz::debayer::{DebayeredImage, OutputSamples, encode_rgb_as_source, to_output_samples};
use libfitz::fits_image::{CFA_KEYWORDS, write_pixel_fits};
use tiff::encoder::{TiffEncoder, colortype};

use crate::io_prompt::{ensure_can_write, print_load_rgb_notice, print_progress, print_step};
use crate::options::DebayerOptions;

pub fn parse_output_format(s: &str) -> Result<OutputFormat, String> {
    s.parse()
}

pub fn debayer_file(input: &Path, output: &Path, opts: &DebayerOptions) -> Result<()> {
    ensure_can_write(output, opts.yes)?;
    print_progress(opts.verbose, input, output);

    print_step(opts.verbose, "reading");
    let d = libfitz::debayer::debayer(input, &opts.core)?;

    print_load_rgb_notice(opts.verbose, input, d.notice);

    print_step(opts.verbose, "writing");
    match opts.core.format {
        OutputFormat::Tiff => {
            let (width, height) = (d.width, d.height);
            let samples = to_output_samples(d.rgb, opts.core.bpp);
            write_tiff(output, width, height, samples)?;
        }
        OutputFormat::Fits => write_fits(output, d)?,
    }

    Ok(())
}

fn write_tiff(output: &Path, width: usize, height: usize, samples: OutputSamples) -> Result<()> {
    let file =
        File::create(output).with_context(|| format!("cannot create {}", output.display()))?;
    let mut enc = TiffEncoder::new(file)
        .with_context(|| format!("cannot create TIFF encoder for {}", output.display()))?;

    let result = match samples {
        OutputSamples::U8(v) => enc.write_image::<colortype::RGB8>(width as u32, height as u32, &v),
        OutputSamples::U16(v) => {
            enc.write_image::<colortype::RGB16>(width as u32, height as u32, &v)
        }
        OutputSamples::U32(v) => {
            enc.write_image::<colortype::RGB32>(width as u32, height as u32, &v)
        }
    };

    result.with_context(|| format!("cannot write {}", output.display()))?;

    Ok(())
}

/// Write the debayered RGB cube as FITS using the same pixel format (BITPIX
/// and BSCALE/BZERO scaling) as the source image, rather than a fixed bit
/// depth — `--bpp` only governs TIFF output.
fn write_fits(output: &Path, d: DebayeredImage) -> Result<()> {
    let (width, height, header, source) = (d.width, d.height, d.header, d.source);
    let pixels = encode_rgb_as_source(d.rgb, &source);
    let history = format!("debayered by fitz {}", env!("CARGO_PKG_VERSION"));
    write_pixel_fits(
        output,
        vec![width, height, 3],
        pixels,
        source.bscale,
        source.bzero,
        Some(&header),
        CFA_KEYWORDS,
        Some(&history),
    )
}
