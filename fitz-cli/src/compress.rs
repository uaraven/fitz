use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::io_prompt::{ensure_can_write, print_progress, print_step};
use crate::options::Options;

pub fn compress_file(input: &Path, output: &Path, opts: &Options) -> Result<()> {
    ensure_can_write(output, opts.yes)?;
    print_progress(opts.verbose, input, output);

    print_step(opts.verbose, "reading");
    print_step(opts.verbose, "compressing");
    let core_opts = libfitz::compress::CompressOptions {
        algorithm: opts.algorithm,
    };
    let out_fits = libfitz::compress::compress(input, &core_opts)?;

    print_step(opts.verbose, "writing");
    out_fits
        .to_file(output)
        .with_context(|| format!("cannot write {}", output.display()))?;

    if !opts.keep && opts.output.is_none() {
        fs::remove_file(input).with_context(|| format!("cannot remove {}", input.display()))?;
    }

    Ok(())
}
