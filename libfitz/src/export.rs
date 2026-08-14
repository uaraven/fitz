use crate::data::{Image, ImageType};
use crate::fits_file::{SaveOptions, save_fits};
use anyhow::anyhow;
use fitskit::{Bitpix, CompressOptions};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder};
use std::fs::File;
use std::path::Path;
use tiff::encoder::compression::DeflateLevel;
use tiff::encoder::{Compression, TiffEncoder, colortype};

#[derive(Clone, Copy)]
pub struct TiffOptions {
    pub bpp: u32,
    pub compress: bool,
}

#[derive(Clone, Copy)]
pub struct FitsOptions {
    pub bpp: i64,
    pub compress: bool,
}

#[derive(Clone, Copy)]
pub enum ExportFormat {
    Fits(FitsOptions),
    Tiff(TiffOptions),
    Jpeg(u8),
    Png,
}

impl Image {
    /// exports image to a file
    /// `target` - the path where to save the file
    /// `format` - determines the output format
    pub fn export(&self, target: &Path, format: ExportFormat) -> anyhow::Result<()> {
        match format {
            ExportFormat::Fits(opts) => save_fits(
                target,
                self,
                SaveOptions {
                    bitpix: Bitpix::from_i64(opts.bpp)?,
                    compress_options: if opts.compress {
                        Some(CompressOptions::default())
                    } else {
                        None
                    },
                    history: None,
                },
            ),
            ExportFormat::Tiff(opts) => export_as_tiff(target, self, opts),
            ExportFormat::Jpeg(level) => export_as_jpeg(target, self, level),
            ExportFormat::Png => export_as_png(target, self),
        }
    }
}

/// Save the image as JPEG
fn export_as_jpeg(target: &Path, img: &Image, quality: u8) -> anyhow::Result<()> {
    let mut file = File::create(target)?;
    let enc = JpegEncoder::new_with_quality(&mut file, quality);
    let (w, h) = (img.width as u32, img.height as u32);
    let rgb = img.image_type == ImageType::RGB;

    match rgb {
        true => enc.write_image(&img.pixels.as_u8(), w, h, ExtendedColorType::Rgb8),
        false => enc.write_image(&img.pixels.as_u8(), w, h, ExtendedColorType::L8),
    }?;

    Ok(())
}

pub fn export_as_png(target: &Path, img: &Image) -> anyhow::Result<()> {
    let mut file = File::create(target)?;
    let enc =
        PngEncoder::new_with_quality(&mut file, CompressionType::Default, FilterType::NoFilter);
    let (w, h) = (img.width as u32, img.height as u32);
    let rgb = img.image_type == ImageType::RGB;

    match rgb {
        true => enc.write_image(&img.pixels.as_u16_bytes(), w, h, ExtendedColorType::Rgb16),
        false => enc.write_image(&img.pixels.as_u16_bytes(), w, h, ExtendedColorType::L16),
    }?;

    Ok(())
}

/// Save the image as a TIFF file.
///
/// `bpp` (8, 16 or 32) selects the sample width; an [`ImageType::RGB`] image is
/// written as interleaved RGB, anything else as a single grayscale channel.
/// Unlike [`save_fits`] this carries no metadata — TIFF has nowhere for the
/// FITS header to go.
pub fn export_as_tiff(target: &Path, img: &Image, opts: TiffOptions) -> anyhow::Result<()> {
    let cannot =
        |what: &str, e: &dyn std::fmt::Display| anyhow!("cannot {what} {}: {e}", target.display());

    let file = File::create(target).map_err(|e| cannot("create", &e))?;
    let mut enc = TiffEncoder::new(file).map_err(|e| cannot("encode", &e))?;
    if opts.compress {
        enc = enc.with_compression(Compression::Deflate(DeflateLevel::Balanced));
    }
    let (w, h) = (img.width as u32, img.height as u32);
    let rgb = img.image_type == ImageType::RGB;

    let result = match (rgb, opts.bpp) {
        (true, 8) => enc.write_image::<colortype::RGB8>(w, h, &img.pixels.as_u8()),
        (true, 16) => enc.write_image::<colortype::RGB16>(w, h, &img.pixels.as_u16()),
        (true, 32) => enc.write_image::<colortype::RGB32>(w, h, &img.pixels.as_u32()),
        (false, 8) => enc.write_image::<colortype::Gray8>(w, h, &img.pixels.as_u8()),
        (false, 16) => enc.write_image::<colortype::Gray16>(w, h, &img.pixels.as_u16()),
        (false, 32) => enc.write_image::<colortype::Gray32>(w, h, &img.pixels.as_u32()),
        (_, other) => {
            return Err(anyhow!(
                "unsupported TIFF bit depth {other}, expected 8, 16 or 32"
            ));
        }
    };
    result.map_err(|e| cannot("write", &e))?;

    Ok(())
}

pub fn export_as_fits(target: &Path, img: &Image, opts: SaveOptions) -> anyhow::Result<()> {
    save_fits(target, img, opts)
}

#[cfg(test)]
mod test {
    use crate::export::{TiffOptions, export_as_jpeg, export_as_tiff};
    use crate::fits_file::load_fits;
    use crate::test_support::write_mosaic_fits;
    use tempfile::TempDir;

    #[test]
    fn save_jpeg_writes_rgb_and_grayscale() {
        // The JPEG encoder only supports 8-bit color types, so `export_as_jpeg`
        // must narrow the image's 16-bit samples down first rather than
        // handing it `Rgb16`/`L16` directly.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 16, 16, Some("RGGB"));

        let mono = load_fits(&input).unwrap();
        let rgb = mono.debayer().unwrap().unwrap();

        for (label, img) in [("mono", &mono), ("rgb", &rgb)] {
            let output = tmp.path().join(format!("{label}.jpg"));
            export_as_jpeg(&output, img, 90).unwrap();
            let data = std::fs::read(&output).unwrap();
            assert!(data.starts_with(&[0xFF, 0xD8]), "{label}");
        }
    }

    #[test]
    fn save_tiff_writes_rgb_and_grayscale_at_every_bit_depth() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 16, 16, Some("RGGB"));

        let mono = load_fits(&input).unwrap();
        let rgb = mono.debayer().unwrap().unwrap();

        for (label, img, channels) in [("mono", &mono, 1usize), ("rgb", &rgb, 3)] {
            let mut sizes = Vec::new();
            for bpp in [8u32, 16, 32] {
                let output = tmp.path().join(format!("{label}-{bpp}.tiff"));

                let opts = TiffOptions {
                    bpp,
                    compress: false,
                };
                export_as_tiff(&output, img, opts).unwrap();

                let data = std::fs::read(&output).unwrap();
                assert!(
                    data.starts_with(b"II") || data.starts_with(b"MM"),
                    "{label}"
                );
                // At least the raw samples, plus header and tags.
                let raw = 16 * 16 * channels * (bpp as usize / 8);
                assert!(
                    data.len() > raw,
                    "{label} {bpp}bpp is smaller than its pixels"
                );
                sizes.push(data.len());
            }
            // Wider samples make a bigger file, in order.
            assert!(
                sizes[0] < sizes[1] && sizes[1] < sizes[2],
                "{label} {sizes:?}"
            );
        }

        let err = export_as_tiff(
            &tmp.path().join("bad.tiff"),
            &rgb,
            TiffOptions {
                bpp: 12,
                compress: false,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("unsupported TIFF bit depth"));
    }
}
