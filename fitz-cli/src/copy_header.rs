//! The `copy-header` command: copy FITS header keywords from a source image
//! onto a target image, filling in only the keywords the target doesn't
//! already carry (its own resolution, bit depth, channel count, pixel
//! scaling, and any other keyword it already has are left untouched). A
//! `BAYERPAT` (and related CFA keywords) from the source is also skipped when
//! the target is already a debayered 3-plane cube, so it doesn't start looking
//! like undebayered raw sensor data again.

use std::path::Path;

use anyhow::{Context, Result};

use crate::io_prompt::{ensure_can_write, print_progress, print_step};
use crate::options::CopyHeaderOptions;

pub fn copy_header_file(source: &Path, target: &Path, opts: &CopyHeaderOptions) -> Result<()> {
    print_step(opts.verbose, "reading");
    print_step(opts.verbose, "copying header");
    let (out_fits, copied) = libfitz::copy_header::copy_header(source, target)?;

    let output = opts.output.clone().unwrap_or_else(|| target.to_path_buf());
    // Overwriting the target in place is the whole point of this command and
    // must not trip the "already exists" guard, the same way decompress
    // handles its default in-place output.
    if output != target {
        ensure_can_write(&output, opts.yes)?;
    }
    print_progress(opts.verbose, source, &output);

    print_step(opts.verbose, "writing");
    out_fits
        .to_file(&output)
        .with_context(|| format!("cannot write {}", output.display()))?;

    if opts.verbose {
        println!("copied {copied} header keyword(s)");
    }

    Ok(())
}
