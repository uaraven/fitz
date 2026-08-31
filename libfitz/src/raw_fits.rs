//!! Operations on raw fits files
//!! These operations do not process the images on load and do not convert them into internal
//! format for additional processing

use crate::fits_file::find_image_hdu_index;
use crate::keywords::{COMPRESSION_KEYWORDS, copy_missing_metadata};
use anyhow::anyhow;
use fitskit::{CompressOptions, CompressionType, FitsFile, Hdu, HduData, ImageData, Quantize};
use std::borrow::Cow;
use std::path::Path;

/// Loads the FitsFile uncompressing it if necessary
pub fn load_raw(source: &Path) -> anyhow::Result<FitsFile> {
    let ff = FitsFile::from_file(source)?;
    let hdu_idx = find_image_hdu_index(&ff)
        .ok_or_else(|| anyhow!("No image data found in {}", source.display()))?;
    let compressed_hdu = &ff.hdus[hdu_idx];
    if let Some(cimg) = compressed_hdu.as_compressed_image() {
        let image = cimg.decompress()?;
        let mut u_hdu = Hdu::primary_image(image);
        copy_missing_metadata(
            &mut u_hdu.header,
            &compressed_hdu.header,
            COMPRESSION_KEYWORDS,
        );
        Ok(FitsFile { hdus: vec![u_hdu] })
    } else {
        Ok(ff)
    }
}

/// Copies headers from image HDU of `source_fits` file into the image HDU of `target_fits` file
/// Returns the number of headers copied
pub fn copy_headers_raw(
    source_fits: &FitsFile,
    target_fits: &mut FitsFile,
) -> anyhow::Result<usize> {
    let source_image_hdu_index = find_image_hdu_index(source_fits)
        .ok_or_else(|| anyhow!("No image data found in source file"))?;
    let source_hdu = &source_fits.hdus[source_image_hdu_index];

    let target_image_hdu_index = find_image_hdu_index(target_fits)
        .ok_or_else(|| anyhow!("No image data found in target file"))?;
    let target_hdu = &mut target_fits.hdus[target_image_hdu_index];

    Ok(copy_missing_metadata(
        &mut target_hdu.header,
        &source_hdu.header,
        &[],
    ))
}

pub enum CompressionSettings {
    NoCompression,
    Rice1,
    Gzip1,
    Gzip2,
}

/// Map a `fitskit` compression algorithm onto the [`CompressionSettings`]
/// [`save_raw`] accepts, falling back to [`CompressionSettings::NoCompression`]
/// for any algorithm this crate doesn't otherwise support (e.g. Hcompress1).
pub fn compression_settings_for(algorithm: CompressionType) -> CompressionSettings {
    match algorithm {
        CompressionType::Rice1 => CompressionSettings::Rice1,
        CompressionType::Gzip1 => CompressionSettings::Gzip1,
        CompressionType::Gzip2 => CompressionSettings::Gzip2,
        _ => CompressionSettings::NoCompression,
    }
}

/// Saves the raw fits file, applying compression of requested
pub fn save_raw(
    fits: &FitsFile,
    target: &Path,
    save_compression: CompressionSettings,
) -> anyhow::Result<()> {
    let hdu_index = find_image_hdu_index(fits)
        .ok_or_else(|| anyhow!("No image data found in {}", target.display()))?;
    let hdu = &fits.hdus[hdu_index];
    let img: Cow<ImageData> = if let Some(compressed) = hdu.as_compressed_image() {
        Cow::Owned(compressed.decompress()?)
    } else if let HduData::Image(img) = &hdu.data {
        Cow::Borrowed(img)
    } else {
        return Err(anyhow!("Invalid image type"));
    };
    if let CompressionSettings::NoCompression = save_compression {
        fits.to_file(target)?;
        Ok(())
    } else {
        let compress_options = match save_compression {
            CompressionSettings::Rice1 => CompressOptions::default(),
            CompressionSettings::Gzip1 => CompressOptions {
                algorithm: CompressionType::Gzip1,
                tile: None,
                quantize: Some(4.0),
                dither: Quantize::SubtractiveDither1,
                dither_seed: Some(1),
                blocksize: 32,
            },
            CompressionSettings::Gzip2 => CompressOptions {
                algorithm: CompressionType::Gzip2,
                tile: None,
                quantize: Some(4.0),
                dither: Quantize::SubtractiveDither1,
                dither_seed: Some(1),
                blocksize: 32,
            },
            _ => unreachable!(),
        };
        let mut compressed_fits = FitsFile::with_empty_primary();
        let mut compressed_hdu = img.compress(&compress_options)?;
        copy_missing_metadata(&mut compressed_hdu.header, &hdu.header, &[]);
        compressed_fits.push_extension(compressed_hdu);
        compressed_fits.to_file(target)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{write_mosaic_fits, write_mosaic_fits_with_metadata};
    use fitskit::Header;
    use tempfile::TempDir;

    fn assert_metadata_preserved(header: &Header) {
        assert_eq!(header.get_string("OBJECT"), Some("M31"));
        assert_eq!(header.get_string("DATE-OBS"), Some("2026-06-22T00:00:00"));
        assert_eq!(header.get_float("CRVAL1"), Some(10.68));
        assert_eq!(header.get_float("CRVAL2"), Some(41.27));
    }

    /// Build a compressed fixture (with metadata) by round-tripping a plain one
    /// through `load_raw`/`save_raw`, since `write_mosaic_fits_with_metadata`
    /// only writes plain FITS.
    fn write_compressed_fixture(tmp: &TempDir, name: &str) -> std::path::PathBuf {
        let plain = tmp.path().join("source_plain.fits");
        write_mosaic_fits_with_metadata(&plain, 8, 8, None);
        let loaded = load_raw(&plain).unwrap();
        let compressed = tmp.path().join(name);
        save_raw(&loaded, &compressed, CompressionSettings::Rice1).unwrap();
        compressed
    }

    #[test]
    fn uncompressed_to_compressed_keeps_metadata() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("in.fits");
        write_mosaic_fits_with_metadata(&src, 8, 8, None);

        let fits = load_raw(&src).unwrap();
        assert!(fits.hdus[0].as_compressed_image().is_none());
        assert_metadata_preserved(&fits.hdus[0].header);

        let out = tmp.path().join("out.fits.fz");
        save_raw(&fits, &out, CompressionSettings::Rice1).unwrap();

        let result = FitsFile::from_file(&out).unwrap();
        let hdu_idx = find_image_hdu_index(&result).unwrap();
        assert!(result.hdus[hdu_idx].as_compressed_image().is_some());
        assert_metadata_preserved(&result.hdus[hdu_idx].header);
    }

    #[test]
    fn compressed_to_uncompressed_keeps_metadata() {
        let tmp = TempDir::new().unwrap();
        let compressed = write_compressed_fixture(&tmp, "in.fits.fz");

        let fits = load_raw(&compressed).unwrap();
        assert!(fits.hdus[0].as_compressed_image().is_none());
        assert_metadata_preserved(&fits.hdus[0].header);

        let out = tmp.path().join("out.fits");
        save_raw(&fits, &out, CompressionSettings::NoCompression).unwrap();

        let result = FitsFile::from_file(&out).unwrap();
        let hdu_idx = find_image_hdu_index(&result).unwrap();
        assert!(result.hdus[hdu_idx].as_compressed_image().is_none());
        assert_metadata_preserved(&result.hdus[hdu_idx].header);
    }

    #[test]
    fn compressed_to_compressed_keeps_metadata() {
        let tmp = TempDir::new().unwrap();
        let compressed = write_compressed_fixture(&tmp, "in.fits.fz");

        let fits = load_raw(&compressed).unwrap();
        assert_metadata_preserved(&fits.hdus[0].header);

        let out = tmp.path().join("out.fits.fz");
        save_raw(&fits, &out, CompressionSettings::Gzip1).unwrap();

        let result = FitsFile::from_file(&out).unwrap();
        let hdu_idx = find_image_hdu_index(&result).unwrap();
        assert!(result.hdus[hdu_idx].as_compressed_image().is_some());
        assert_metadata_preserved(&result.hdus[hdu_idx].header);
    }

    #[test]
    fn copy_headers_raw_fills_in_missing_metadata() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.fits");
        write_mosaic_fits_with_metadata(&src, 8, 8, None);
        let target = tmp.path().join("target.fits");
        write_mosaic_fits(&target, 8, 8, None);

        let source_fits = FitsFile::from_file(&src).unwrap();
        let mut target_fits = FitsFile::from_file(&target).unwrap();
        assert!(target_fits.primary().header.get_string("OBJECT").is_none());

        copy_headers_raw(&source_fits, &mut target_fits).unwrap();

        let hdu_idx = find_image_hdu_index(&target_fits).unwrap();
        assert_metadata_preserved(&target_fits.hdus[hdu_idx].header);
    }

    #[test]
    fn copy_headers_raw_does_not_overwrite_existing_metadata() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.fits");
        write_mosaic_fits_with_metadata(&src, 8, 8, None);

        let target = tmp.path().join("target.fits");
        write_mosaic_fits_with_metadata(&target, 8, 8, None);

        let source_fits = FitsFile::from_file(&src).unwrap();
        let mut target_fits = FitsFile::from_file(&target).unwrap();
        let hdu_idx = find_image_hdu_index(&target_fits).unwrap();
        target_fits.hdus[hdu_idx].header.set(
            "OBJECT",
            fitskit::HeaderValue::String("Target".to_string()),
            None,
        );

        copy_headers_raw(&source_fits, &mut target_fits).unwrap();

        let hdu_idx = find_image_hdu_index(&target_fits).unwrap();
        assert_eq!(
            target_fits.hdus[hdu_idx].header.get_string("OBJECT"),
            Some("Target"),
            "a keyword the target already carries must not be overwritten"
        );
    }

    #[test]
    fn copy_headers_raw_errors_when_source_has_no_image() {
        let source_fits = FitsFile::with_empty_primary();
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target.fits");
        write_mosaic_fits_with_metadata(&target, 8, 8, None);
        let mut target_fits = FitsFile::from_file(&target).unwrap();

        assert!(copy_headers_raw(&source_fits, &mut target_fits).is_err());
    }

    #[test]
    fn compression_settings_for_maps_known_algorithms_and_falls_back() {
        assert!(matches!(
            compression_settings_for(CompressionType::Rice1),
            CompressionSettings::Rice1
        ));
        assert!(matches!(
            compression_settings_for(CompressionType::Gzip1),
            CompressionSettings::Gzip1
        ));
        assert!(matches!(
            compression_settings_for(CompressionType::Gzip2),
            CompressionSettings::Gzip2
        ));
        assert!(matches!(
            compression_settings_for(CompressionType::Hcompress1),
            CompressionSettings::NoCompression
        ));
    }

    #[test]
    fn uncompressed_to_uncompressed_keeps_metadata() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("in.fits");
        write_mosaic_fits_with_metadata(&src, 8, 8, None);

        let fits = load_raw(&src).unwrap();
        assert_metadata_preserved(&fits.hdus[0].header);

        let out = tmp.path().join("out.fits");
        save_raw(&fits, &out, CompressionSettings::NoCompression).unwrap();

        let result = FitsFile::from_file(&out).unwrap();
        let hdu_idx = find_image_hdu_index(&result).unwrap();
        assert!(result.hdus[hdu_idx].as_compressed_image().is_none());
        assert_metadata_preserved(&result.hdus[hdu_idx].header);
    }
}
