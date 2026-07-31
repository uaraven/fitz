//! Copy FITS header keywords from a source image onto a target image, filling
//! in only the keywords the target doesn't already carry.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use fitskit::FitsFile;

use crate::fits_image::{
    CFA_KEYWORDS, copy_missing_metadata, find_image_hdu_index, header_is_rgb_cube_shape,
};

/// Merge `source`'s header onto `target`'s, filling in only what `target`
/// doesn't already carry — its own resolution, bit depth, channel count,
/// pixel scaling, and any other keyword it already has are left untouched. A
/// `BAYERPAT` (and related CFA keywords) from `source` is skipped when
/// `target` is already a debayered 3-plane cube, so it doesn't start looking
/// like undebayered raw sensor data again. Returns `target`'s `FitsFile` with
/// the merged header already applied (ready for the caller to write out) and
/// the number of keywords copied.
pub fn copy_header(source: &Path, target: &Path) -> Result<(FitsFile, usize)> {
    let src_fits =
        FitsFile::from_file(source).with_context(|| format!("cannot read {}", source.display()))?;
    let mut dst_fits =
        FitsFile::from_file(target).with_context(|| format!("cannot read {}", target.display()))?;

    let src_idx = find_image_hdu_index(&src_fits)
        .ok_or_else(|| anyhow!("no image data found in {}", source.display()))?;
    let dst_idx = find_image_hdu_index(&dst_fits)
        .ok_or_else(|| anyhow!("no image data found in {}", target.display()))?;

    // A target that's already a debayered 3-plane cube must keep looking
    // debayered: a stale BAYERPAT copied from a mosaic source would make
    // `is_debayered_rgb_cube` (used by `info`/`debayer`/`stretch`) mistake it
    // for raw sensor data again.
    let extra_drop: &[&str] = if header_is_rgb_cube_shape(&dst_fits.hdus[dst_idx].header) {
        CFA_KEYWORDS
    } else {
        &[]
    };

    let copied = copy_missing_metadata(
        &mut dst_fits.hdus[dst_idx].header,
        &src_fits.hdus[src_idx].header,
        extra_drop,
    );

    Ok((dst_fits, copied))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_mosaic_fits_with_metadata;
    use fitskit::{FitsFile, HeaderValue, ImageData, Keyword, PixelData};
    use tempfile::TempDir;

    /// Write a plain 2D FITS file with no metadata beyond the mandatory
    /// keywords, for asserting on what `copy_header` fills in.
    fn write_bare_fits(path: &Path, width: usize, height: usize) {
        let pixels: Vec<i16> = (0..(width * height) as i16).collect();
        let img = ImageData::new(vec![width, height], PixelData::I16(pixels));
        FitsFile::with_primary_image(img).to_file(path).unwrap();
    }

    #[test]
    fn copy_header_fills_in_missing_metadata() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source.fits");
        write_mosaic_fits_with_metadata(&source, 8, 6, Some("RGGB"));

        let target = tmp.path().join("target.fits");
        write_bare_fits(&target, 8, 6);

        let (fits, copied) = copy_header(&source, &target).unwrap();
        assert!(copied > 0);
        let header = &fits.primary().header;
        assert_eq!(header.get_string("OBJECT"), Some("M31"));
        assert_eq!(header.get_float("CRVAL1"), Some(10.68));
        assert_eq!(header.get_string("BAYERPAT").map(str::trim), Some("RGGB"));
        assert!(header.iter().any(|k| k.name == "COMMENT"));
    }

    #[test]
    fn copy_header_keeps_bayerpat_off_an_already_debayered_target() {
        use crate::test_support::write_rgb_cube_fits;

        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source.fits");
        write_mosaic_fits_with_metadata(&source, 8, 6, Some("RGGB"));

        let target = tmp.path().join("target.fits");
        write_rgb_cube_fits(&target, 8, 6);

        let (fits, _) = copy_header(&source, &target).unwrap();
        let header = &fits.primary().header;
        // Metadata still comes across...
        assert_eq!(header.get_string("OBJECT"), Some("M31"));
        // ...but BAYERPAT must not, or `info`/`debayer`/`stretch` would mistake
        // this already-debayered cube for undebayered raw sensor data again.
        assert!(header.find("BAYERPAT").is_none());
    }

    #[test]
    fn copy_header_never_overwrites_targets_own_structural_keywords() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source.fits");
        write_mosaic_fits_with_metadata(&source, 8, 6, Some("RGGB"));

        let target = tmp.path().join("target.fits");
        write_bare_fits(&target, 4, 3);

        let (fits, _) = copy_header(&source, &target).unwrap();
        let header = &fits.primary().header;
        // The target's own resolution/bit depth must survive unchanged, not be
        // clobbered by the (differently sized) source's.
        assert_eq!(header.get_int("NAXIS1"), Some(4));
        assert_eq!(header.get_int("NAXIS2"), Some(3));
    }

    #[test]
    fn copy_header_does_not_overwrite_a_value_target_already_has() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source.fits");
        write_mosaic_fits_with_metadata(&source, 8, 6, Some("RGGB"));

        let target = tmp.path().join("target.fits");
        write_bare_fits(&target, 8, 6);
        {
            let mut fits = FitsFile::from_file(&target).unwrap();
            fits.primary_mut().header.set(
                "OBJECT",
                HeaderValue::String("keep-me".to_string()),
                None,
            );
            fits.to_file(&target).unwrap();
        }

        let (fits, _) = copy_header(&source, &target).unwrap();
        assert_eq!(fits.primary().header.get_string("OBJECT"), Some("keep-me"));
    }

    #[test]
    fn copy_header_appends_multiple_history_cards_without_deduping() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source.fits");
        {
            let img = ImageData::new(vec![2, 2], PixelData::I16(vec![0, 1, 2, 3]));
            let mut fits = FitsFile::with_primary_image(img);
            fits.primary_mut()
                .header
                .push(Keyword::commentary("HISTORY", "first"));
            fits.to_file(&source).unwrap();
        }

        let target = tmp.path().join("target.fits");
        write_bare_fits(&target, 2, 2);
        {
            let mut fits = FitsFile::from_file(&target).unwrap();
            fits.primary_mut()
                .header
                .push(Keyword::commentary("HISTORY", "second"));
            fits.to_file(&target).unwrap();
        }

        let (fits, _) = copy_header(&source, &target).unwrap();
        let history: Vec<&str> = fits
            .primary()
            .header
            .iter()
            .filter(|k| k.name == "HISTORY")
            .filter_map(|k| k.comment.as_deref())
            .collect();
        assert_eq!(history, vec!["second", "first"]);
    }

    #[test]
    fn copy_header_errors_when_source_has_no_image() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("empty.fits");
        let fits = FitsFile {
            hdus: vec![fitskit::Hdu::primary_empty()],
        };
        fits.to_file(&source).unwrap();

        let target = tmp.path().join("target.fits");
        write_bare_fits(&target, 4, 3);

        let err = match copy_header(&source, &target) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(err.to_string().contains("no image data"));
    }
}
