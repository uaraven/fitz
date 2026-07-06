use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use fitz_core::fits_image::find_image_hdu;
use fitz_core::fitskit::FitsFile;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy)]
pub(crate) enum HashTarget {
    File,
    Header,
    Image,
}

pub(crate) fn hash_file(input: &Path, target: HashTarget) -> Result<()> {
    let hash = match target {
        HashTarget::File => {
            let data = std::fs::read(input)
                .with_context(|| format!("cannot read {}", input.display()))?;
            hex(Sha256::digest(&data))
        }
        HashTarget::Header => {
            let fits = FitsFile::from_file(input)
                .with_context(|| format!("cannot read {}", input.display()))?;
            let (header, _) = find_image_hdu(&fits, input)?;
            let mut buf = Vec::new();
            header
                .write_to(&mut buf)
                .with_context(|| format!("cannot serialize header of {}", input.display()))?;
            hex(Sha256::digest(&buf))
        }
        HashTarget::Image => {
            let fits = FitsFile::from_file(input)
                .with_context(|| format!("cannot read {}", input.display()))?;
            let (_, img) = find_image_hdu(&fits, input)?;
            hex(Sha256::digest(img.pixels.to_bytes()))
        }
    };

    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{hash}  {}", input.display());
    Ok(())
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().fold(String::new(), |mut s, b| {
        let _ = std::fmt::write(&mut s, format_args!("{b:02x}"));
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_data;

    #[test]
    fn file_hash_is_stable() {
        let input = test_data("uncompressed.fit");
        hash_file(&input, HashTarget::File).unwrap();
    }

    #[test]
    fn header_hash_is_stable() {
        let input = test_data("uncompressed.fit");
        hash_file(&input, HashTarget::Header).unwrap();
    }

    #[test]
    fn image_hash_is_stable() {
        let input = test_data("uncompressed.fit");
        hash_file(&input, HashTarget::Image).unwrap();
    }

    #[test]
    fn image_hash_stable_for_compressed_input() {
        let input = test_data("compressed.fits.fz");
        hash_file(&input, HashTarget::Image).unwrap();
    }
}
