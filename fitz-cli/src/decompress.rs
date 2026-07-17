use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::io_prompt::{ensure_can_write, print_progress, print_step};
use crate::options::Options;

pub fn decompress_file(input: &Path, output: &Path, opts: &Options) -> Result<()> {
    // Decompressing in place (output == input) is allowed and must not trip
    // the "already exists" guard.
    if output != input {
        ensure_can_write(output, opts.yes)?;
    }
    print_progress(opts.verbose, input, output);

    print_step(opts.verbose, "reading");
    print_step(opts.verbose, "decompressing");
    let out_fits = libfitz::decompress::decompress(input)?;

    print_step(opts.verbose, "writing");
    out_fits
        .to_file(output)
        .with_context(|| format!("cannot write {}", output.display()))?;

    if !opts.keep && opts.output.is_none() && output != input {
        fs::remove_file(input).with_context(|| format!("cannot remove {}", input.display()))?;
    }

    Ok(())
}
