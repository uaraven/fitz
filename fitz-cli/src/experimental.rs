//! Hidden `experimental` command: unstable features not yet ready for the
//! stable command surface. Not shown in `fitz --help`, but its subcommands
//! are listed normally by `fitz experimental --help`.

use std::path::Path;

use anyhow::Result;

pub(crate) fn contrast_file(input: &Path) -> Result<()> {
    let image = libfitz::fits_file::load_fits(input)?;
    let contrast = image.contrast();
    println!("{}: {contrast}", input.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_data;

    #[test]
    fn contrast_file_runs_on_real_data() {
        let input = test_data("uncompressed.fit");
        contrast_file(&input).unwrap();
    }

    #[test]
    fn contrast_file_runs_on_compressed_input() {
        let input = test_data("compressed.fits.fz");
        contrast_file(&input).unwrap();
    }
}
