use crate::data::{Image, ImageType, PixelBuffer};
use anyhow::{Result, anyhow};
use std::borrow::Cow;
use std::path::Path;

use crate::fits_bayer::{cfa_str, parse_cfa};
use crate::keywords::{BAYERPAT, BSCALE, BZERO, CFA_KEYWORDS, add_history, copy_missing_metadata};
use fitskit::{
    Bitpix, CompressOptions, FitsFile, HduData, Header, HeaderValue, ImageData, PixelData,
};
use rayon::prelude::*;

/// Locate the index of the HDU holding image data (a plain image HDU, or a
/// tile-compressed image extension)
pub fn find_image_hdu_index(fits: &FitsFile) -> Option<usize> {
    fits.hdus.iter().position(is_image_hdu)
}

/// True if `hdu` carries image pixels, either plainly or tile-compressed.
fn is_image_hdu(hdu: &fitskit::Hdu) -> bool {
    matches!(hdu.data, HduData::Image(_)) || hdu.as_compressed_image().is_some()
}

/// True if `header` describes a 3-plane image (`NAXIS=3`, `NAXIS3=3`)
pub fn header_is_rgb_cube_shape(header: &Header) -> bool {
    header.get_int("NAXIS") == Some(3) && header.get_int("NAXIS3") == Some(3)
}

const MIN_U16: u32 = 0;
const MAX_U16: u32 = 65535;

const MIN_F32: f32 = 0.0;
const MAX_F32: f32 = 1.0;
fn fits_u8_to_u16(x: f32) -> u16 {
    (x as u32 * 257u32).clamp(MIN_U16, MAX_U16) as u16
}

fn fits_u16_to_u16(x: f32) -> u16 {
    (x as u32).clamp(MIN_U16, MAX_U16) as u16
}

fn fits_u32_to_f32(x: f32) -> f32 {
    (x / 4_294_967_295.0).clamp(MIN_F32, MAX_F32)
}

fn fits_u64_to_f32(x: f32) -> f32 {
    (x / 18_446_744_073_709_551_615.0).clamp(MIN_F32, MAX_F32)
}

/// Apply the image's BSCALE/BZERO scaling and clamp into a supported
/// `PixelBuffer`: integer sources map to `u16` over the full 0..=65535 range,
/// float/wide-integer sources to `f32` clamped to [0, 1]. Scaling and clamping
/// fuse into a single parallel pass, and the output type follows the sample
/// type directly.
fn load_pixels(img: &PixelData, b_scale: f32, b_zero: f32) -> PixelBuffer {
    let scale = |x: f32| b_zero + b_scale * x;
    let to_f32 = |x: f32| scale(x).clamp(0.0, 1.0);

    match img {
        PixelData::U8(v) => PixelBuffer::U16(
            v.par_iter()
                .map(|&x| fits_u8_to_u16(scale(x as f32)))
                .collect(),
        ),
        PixelData::I16(v) => PixelBuffer::U16(
            v.par_iter()
                .map(|&x| fits_u16_to_u16(scale(x as f32)))
                .collect(),
        ),
        PixelData::I32(v) => {
            PixelBuffer::F32(v.par_iter().map(|&x| fits_u32_to_f32(x as f32)).collect())
        }
        PixelData::I64(v) => {
            PixelBuffer::F32(v.par_iter().map(|&x| fits_u64_to_f32(x as f32)).collect())
        }
        PixelData::F32(v) => PixelBuffer::F32(v.par_iter().map(|&x| to_f32(x)).collect()),
        PixelData::F64(v) => PixelBuffer::F32(v.par_iter().map(|&x| to_f32(x as f32)).collect()),
    }
}

/// Loads FITS file
///
/// If the file is compressed it will be automatically decompressed
/// The resulting file is converted either to 16-bit unsigned or 32-bit float pixel format
pub fn load_fits(source: &Path) -> Result<Image> {
    let fits_file = FitsFile::from_file(source)?;
    let hdu_opt = fits_file.iter().find(|hdu| is_image_hdu(hdu));

    if let Some(hdu) = hdu_opt {
        // Borrow a plain image HDU, but decompress a tile-compressed one into an owned buffer.
        let img: Cow<ImageData> = if let Some(compressed) = hdu.as_compressed_image() {
            Cow::Owned(compressed.decompress()?)
        } else if let HduData::Image(img) = &hdu.data {
            Cow::Borrowed(img)
        } else {
            return Err(anyhow!("Invalid image type"));
        };

        let bayer_pat = hdu.header.get_string(BAYERPAT).and_then(parse_cfa);

        let image_type = match img.axes.len() {
            // A cube is only an image if it has exactly the three colour
            // planes. Anything wider (a data cube, a stack) has no meaning
            // here, and treating it as RGB would silently mangle it.
            3 if img.axes[2] == 3 => ImageType::RGB,
            2 => match bayer_pat {
                Some(cfa) => ImageType::CFA(cfa),
                None => ImageType::Grayscale,
            },
            _ => {
                return Err(anyhow!("unsupported image shape {:?}", img.axes));
            }
        };

        let width = img.width().ok_or_else(|| anyhow!("No width"))?;
        let height = img.height().ok_or_else(|| anyhow!("No height"))?;

        let b_scale = hdu.header.get_float(BSCALE).unwrap_or(1.0) as f32;
        let b_zero = hdu.header.get_float(BZERO).unwrap_or(0.0) as f32;

        let pixels = load_pixels(&img.pixels, b_scale, b_zero);
        // FITS stores a cube planar; every in-memory RGB buffer is interleaved.
        let pixels = match image_type {
            ImageType::RGB => pixels.interleave_planes(),
            _ => pixels,
        };

        Ok(Image {
            image_type,
            header: hdu.header.clone(),
            width,
            height,
            pixels,
        })
    } else {
        Err(anyhow!("Image HDU not found"))
    }
}

/// Options for saving the image
/// Defines the target pixel format, the compression, and an optional
/// provenance card
pub struct SaveOptions {
    pub bitpix: Bitpix,
    pub compress_options: Option<fitskit::CompressOptions>,
    /// Recorded as a HISTORY card on the image HDU, naming what produced it
    /// (e.g. `"debayered by fitz 0.2.0"`).
    pub history: Option<String>,
}

impl Default for SaveOptions {
    fn default() -> Self {
        SaveOptions {
            bitpix: Bitpix::I16,
            compress_options: None,
            history: None,
        }
    }
}

impl SaveOptions {
    pub fn default_compressed() -> Self {
        SaveOptions {
            compress_options: Some(CompressOptions::default()),
            ..SaveOptions::default()
        }
    }

    /// This `SaveOptions` with a HISTORY provenance card attached.
    pub fn with_history(self, history: impl Into<String>) -> Self {
        SaveOptions {
            history: Some(history.into()),
            ..self
        }
    }
}

pub fn image_to_fits(img: &Image, options: SaveOptions) -> Result<FitsFile> {
    // An RGB `Image` is interleaved in memory but a FITS cube is planar, so the
    // samples are reordered once here, before any type conversion.
    let planar;
    let pixels = match img.image_type {
        ImageType::RGB => {
            planar = img.pixels.deinterleave_planes();
            &planar
        }
        _ => &img.pixels,
    };

    let (bscale, bzero, pixel_data) = match options.bitpix {
        Bitpix::U8 => (1.0, 0.0, PixelData::U8(pixels.as_u8())),
        Bitpix::I16 => {
            let (bscale, bzero, data) = pixels.as_i16();
            (bscale, bzero, PixelData::I16(data))
        }
        Bitpix::I32 => {
            let (bscale, bzero, data) = pixels.as_i32();
            (bscale, bzero, PixelData::I32(data))
        }
        Bitpix::I64 => {
            let (bscale, bzero, data) = pixels.as_i64();
            (bscale, bzero, PixelData::I64(data))
        }
        Bitpix::F32 => (1.0, 0.0, PixelData::F32(pixels.as_f32())),
        Bitpix::F64 => (1.0, 0.0, PixelData::F64(pixels.as_f64())),
    };

    let img_data = match img.image_type {
        ImageType::RGB => ImageData::new(vec![img.width, img.height, 3], pixel_data),
        ImageType::Grayscale => ImageData::new(vec![img.width, img.height], pixel_data),
        ImageType::CFA(_) => ImageData::new(vec![img.width, img.height], pixel_data),
    };

    // Every keyword below describes the *image*, so it has to land on the header
    // of the HDU that carries it — the primary for a plain image, but the pushed
    // extension for a tile-compressed one, whose empty primary `load_fits` never
    // looks at. Writing them to the primary in the compressed case silently
    // dropped BZERO and BAYERPAT, so a compressed file read back as an all-zero
    // grayscale frame.
    let (mut dst_file, image_hdu) = if let Some(compress_options) = &options.compress_options {
        let mut compressed_fits = FitsFile::with_empty_primary();
        let co = fitskit::CompressOptions {
            algorithm: compress_options.algorithm,
            ..fitskit::CompressOptions::default()
        };
        compressed_fits.push_extension(img_data.compress(&co)?);
        let extension = compressed_fits.hdus.len() - 1;
        (compressed_fits, extension)
    } else {
        (FitsFile::with_primary_image(img_data), 0)
    };

    let header = &mut dst_file.hdus[image_hdu].header;
    header.set(BSCALE, HeaderValue::Integer(bscale as i64), None);
    header.set(BZERO, HeaderValue::Integer(bzero as i64), None);
    if let ImageType::CFA(cfa) = img.image_type {
        header.set(
            BAYERPAT,
            HeaderValue::String(cfa_str(cfa).to_string()),
            None,
        );
    }
    if let Some(history) = &options.history {
        add_history(header, history);
    }

    // Only a mosaic has a Bayer pattern, and the writer set that one itself
    // just above. Carrying a source BAYERPAT onto a debayered cube (or onto one
    // split-out channel) would make every other tool read it as raw sensor data
    // again.
    let drop: &[&str] = match img.image_type {
        ImageType::CFA(_) => &[],
        _ => CFA_KEYWORDS,
    };
    // Runs after the writer's own keywords: `copy_missing_metadata` skips the
    // reserved names (BSCALE/BZERO among them) and never overwrites a card the
    // destination already carries, so what was just set above stands.
    copy_missing_metadata(header, &img.header, drop);
    Ok(dst_file)
}

/// Save the FITS file
///
/// Saves the FITS image in `img` to the file at the `target` path
/// `options` contain [SaveOptions] struct with save options
///
/// The headers are copied from the source image
pub fn save_fits(target: &Path, img: &Image, options: SaveOptions) -> Result<()> {
    let dst_file = image_to_fits(img, options)?;

    // Streams HDU by HDU through a `BufWriter`. Going via `to_bytes` instead
    // would hold the entire encoded file — every pixel again — in memory purely
    // to hand it to `fs::write`.
    dst_file
        .to_file(target)
        .map_err(|e| anyhow!("cannot write {}: {e}", target.display()))?;

    Ok(())
}

/// What [`load_header`] reports: everything a caller can learn about an image
/// without decoding its pixels.
pub struct ImageMeta {
    pub header: Header,
    pub image_type: ImageType,
    pub width: usize,
    pub height: usize,
    /// The image's own BITPIX. For a tile-compressed HDU this is `ZBITPIX` —
    /// the *image's* bit depth — not the binary table container's.
    pub bitpix: i64,
}

/// Parse a `DATE-OBS`/`DATE-LOC`-style header value
/// (`"2026-06-22T22:00:00"`, optionally with fractional seconds) into a
/// Unix-epoch timestamp. The string carries no timezone, so it is read as
/// UTC verbatim — no zone conversion is applied. `None` for anything that
/// doesn't parse as `YYYY-MM-DDTHH:MM:SS[.ffffff]`.
pub fn parse_date_obs(s: &str) -> Option<f64> {
    let (date, time) = s.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() {
        return None;
    }

    let mut time_parts = time.split(':');
    let hour: f64 = time_parts.next()?.parse().ok()?;
    let minute: f64 = time_parts.next()?.parse().ok()?;
    let second: f64 = time_parts.next()?.parse().ok()?;
    if time_parts.next().is_some() {
        return None;
    }

    let days = days_from_civil(year, month, day);
    Some(days as f64 * 86400.0 + hour * 3600.0 + minute * 60.0 + second)
}

/// Days since the Unix epoch (1970-01-01) for a civil date, exact over the
/// whole proleptic Gregorian calendar. Howard Hinnant's `days_from_civil`.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // Mar=0 .. Feb=11
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Read `source`'s image metadata without decoding (or decompressing) its
/// pixels — the header-only counterpart to [`load_fits`], for callers that only
/// report on a frame rather than process it.
pub fn load_header(source: &Path) -> Result<ImageMeta> {
    let fits_file = FitsFile::from_file(source)?;
    let hdu = fits_file
        .iter()
        .find(|hdu| is_image_hdu(hdu))
        .ok_or_else(|| anyhow!("Image HDU not found"))?;

    // A tile-compressed HDU describes the image it holds in its `Z*` keywords;
    // its own NAXIS/BITPIX describe the binary table wrapping it.
    let (naxis, bitpix) = match hdu.as_compressed_image() {
        Some(_) => ("ZNAXIS", "ZBITPIX"),
        None => ("NAXIS", "BITPIX"),
    };
    let axis = |i: usize| hdu.header.get_int(&format!("{naxis}{i}"));

    let axes = hdu.header.get_int(naxis).unwrap_or(0);
    let bayer_pat = hdu.header.get_string(BAYERPAT).and_then(parse_cfa);
    let image_type = if axes == 3 && axis(3) == Some(3) {
        ImageType::RGB
    } else {
        match bayer_pat {
            Some(cfa) => ImageType::CFA(cfa),
            None => ImageType::Grayscale,
        }
    };

    Ok(ImageMeta {
        header: hdu.header.clone(),
        image_type,
        width: axis(1).unwrap_or(0) as usize,
        height: axis(2).unwrap_or(0) as usize,
        bitpix: hdu.header.get_int(bitpix).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{ImageType, PixelBuffer};
    use crate::test_support::{
        output_header, test_data, write_mosaic_fits, write_rgb_cube_f32_fits, write_rgb_cube_fits,
    };
    use bayer::CFA;
    use fitskit::{Bitpix, CompressOptions};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    #[test]
    fn test_load_scaled_i16_image() {
        let input = test_data("cfa_orion.fits");
        let loaded = load_fits(&input).unwrap();

        assert_eq!(loaded.image_type, ImageType::CFA(CFA::RGGB));
        assert_eq!(loaded.width, 3856);
        assert_eq!(loaded.height, 2180);
        // An I16 source scales into the u16 pixel buffer.
        assert!(matches!(loaded.pixels, PixelBuffer::U16(_)));
    }

    #[test]
    fn test_load_compressed_file() {
        let input = test_data("compressed.fits.fz");
        let loaded = load_fits(&input).unwrap();

        assert_eq!(loaded.image_type, ImageType::CFA(CFA::GRBG));
        assert_eq!(loaded.width, 3008);
        assert_eq!(loaded.height, 3008);
        // An I16 source scales into the u16 pixel buffer.
        assert!(matches!(loaded.pixels, PixelBuffer::U16(_)));
    }

    #[test]
    fn test_save_uncompressed_file() {
        let input = test_data("cfa_orion.fits");
        let loaded = load_fits(&input).unwrap();

        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("raw.fits");

        save_fits(
            &output,
            &loaded,
            SaveOptions {
                bitpix: Bitpix::I16,
                ..SaveOptions::default()
            },
        )
        .unwrap();

        let actual = format!("{:x}", Sha256::digest(std::fs::read(&output).unwrap()));
        assert_eq!(
            actual,
            "e5b5ff8800c404719862765593d39857559552120d656f41ac2ea85f85f4f7f3"
        );
    }

    #[test]
    fn test_save_compressed_file() {
        let input = test_data("cfa_orion.fits");
        let loaded = load_fits(&input).unwrap();

        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("raw.fits.fz");
        // let output = test_data("test.fits.fz");

        save_fits(
            &output,
            &loaded,
            SaveOptions {
                compress_options: Some(CompressOptions::default()),
                ..SaveOptions::default()
            },
        )
        .unwrap();

        // Re-pinned when the writer started putting BSCALE/BZERO/BAYERPAT and the
        // copied metadata on the compressed extension's header instead of the
        // empty primary; `compressed_save_round_trips_pixels_and_bayer_pattern`
        // is what vouches for these bytes meaning the right thing.
        let actual = format!("{:x}", Sha256::digest(std::fs::read(&output).unwrap()));
        assert_eq!(
            actual,
            "7bf9f18f351d1c1a1335306dbd5b550c6e6dfc877b9f20bdfdf00eeea1215c1b"
        );
    }

    /// The SHA-256 fixtures above pin the bytes we write but never read them
    /// back, so they cannot see a file that is well-formed and still means
    /// something different on the way in. These round-trips do.
    #[test]
    fn compressed_save_round_trips_pixels_and_bayer_pattern() {
        let loaded = load_fits(&test_data("cfa_orion.fits")).unwrap();

        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("roundtrip.fits.fz");
        save_fits(&output, &loaded, SaveOptions::default_compressed()).unwrap();

        let reloaded = load_fits(&output).unwrap();

        // The Bayer pattern lives on the compressed extension's header, not the
        // empty primary: losing it turns the frame into an undebayerable mono image.
        assert_eq!(reloaded.image_type, ImageType::CFA(CFA::RGGB));
        assert_eq!(
            (reloaded.width, reloaded.height),
            (loaded.width, loaded.height)
        );
        // Without BZERO the i16 samples read back negative and clamp to 0.
        assert_eq!(reloaded.pixels, loaded.pixels);
    }

    #[test]
    fn uncompressed_save_round_trips_pixels_and_bayer_pattern() {
        let loaded = load_fits(&test_data("cfa_orion.fits")).unwrap();

        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("roundtrip.fits");
        save_fits(&output, &loaded, SaveOptions::default()).unwrap();

        let reloaded = load_fits(&output).unwrap();

        assert_eq!(reloaded.image_type, ImageType::CFA(CFA::RGGB));
        assert_eq!(
            (reloaded.width, reloaded.height),
            (loaded.width, loaded.height)
        );
        assert_eq!(reloaded.pixels, loaded.pixels);
    }

    /// `as_u8` must emit one sample per pixel. Emitting both bytes of each u16
    /// wrote twice the data NAXIS declares, which overran the data block and
    /// made the next card unparseable on read.
    #[test]
    fn u8_save_writes_one_sample_per_pixel() {
        let loaded = load_fits(&test_data("cfa_orion.fits")).unwrap();

        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("eight_bit.fits");
        save_fits(
            &output,
            &loaded,
            SaveOptions {
                bitpix: Bitpix::U8,
                ..SaveOptions::default()
            },
        )
        .unwrap();

        let reloaded = load_fits(&output).unwrap();

        assert_eq!(
            (reloaded.width, reloaded.height),
            (loaded.width, loaded.height)
        );
        let PixelBuffer::U16(samples) = &reloaded.pixels else {
            panic!("an 8-bit source loads into the u16 pixel buffer");
        };
        assert_eq!(samples.len(), loaded.width * loaded.height);

        // Each sample kept its source pixel's high byte, widened back by 257.
        let PixelBuffer::U16(source) = &loaded.pixels else {
            panic!("expected u16 source pixels");
        };
        for (i, (&got, &want)) in samples.iter().zip(source.iter()).enumerate().take(4096) {
            assert_eq!(got, (want >> 8) * 257, "sample {i}");
        }
    }

    /// FITS stores a cube planar (the whole R plane, then G, then B) while
    /// every in-memory RGB buffer is interleaved. Reading one without
    /// reordering scrambled the channels into thirds of the frame; writing one
    /// laid interleaved samples down under a planar `NAXIS3=3` declaration.
    #[test]
    fn rgb_cube_is_interleaved_on_load_and_planar_on_save() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("rgb.fits");
        // `write_rgb_cube_fits` fills plane `c` with the run `c*n .. (c+1)*n`.
        write_rgb_cube_fits(&path, 4, 3);
        let n = 4 * 3;

        let loaded = load_fits(&path).unwrap();
        assert_eq!(loaded.image_type, ImageType::RGB);

        // Interleaved in memory: pixel `i` is the triple (i, n+i, 2n+i).
        let expected: Vec<u16> = (0..n)
            .flat_map(|i| [i as u16, (n + i) as u16, (2 * n + i) as u16])
            .collect();
        assert_eq!(loaded.pixels, PixelBuffer::U16(expected));

        // ...and planar again on the way out, so the file round-trips.
        let output = tmp.path().join("out.fits");
        save_fits(&output, &loaded, SaveOptions::default()).unwrap();
        assert_eq!(load_fits(&output).unwrap().pixels, loaded.pixels);

        // Read the written samples raw, so the planar layout is pinned against
        // the file itself and not merely against this crate's own reader.
        let written = FitsFile::from_file(&output).unwrap();
        let HduData::Image(img) = &written.primary().data else {
            panic!("expected a plain image HDU");
        };
        assert_eq!(img.axes, vec![4, 3, 3]);
        let PixelData::I16(samples) = &img.pixels else {
            panic!("expected I16 samples");
        };
        // The three runs are contiguous, exactly as they came in — offset by
        // the unsigned-16 convention's BZERO.
        let expected: Vec<i16> = (0..3 * n).map(|i| (i - 32768) as i16).collect();
        assert_eq!(samples, &expected);
    }

    /// The float path interleaves too — a drizzle-processed cube is normalized
    /// `[0, 1]` floats, so it loads into an `F32` buffer rather than a `U16` one.
    #[test]
    fn a_float_rgb_cube_is_interleaved_on_load() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("rgb32.fits");
        write_rgb_cube_f32_fits(&path, 4, 3);
        let n = 4 * 3;

        let loaded = load_fits(&path).unwrap();
        assert_eq!(loaded.image_type, ImageType::RGB);

        let PixelBuffer::F32(samples) = &loaded.pixels else {
            panic!("an F32 source loads into the f32 pixel buffer");
        };
        let expected: Vec<f32> = (0..n)
            .flat_map(|i| [i, n + i, 2 * n + i].map(|v| v as f32 / (3 * n) as f32))
            .collect();
        assert_eq!(samples, &expected);
    }

    /// A cube with anything other than three planes is not an image this
    /// library understands; treating it as RGB would silently mangle it.
    #[test]
    fn a_cube_that_is_not_three_planes_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("stack.fits");
        let img = ImageData::new(vec![4, 3, 5], PixelData::I16(vec![0i16; 4 * 3 * 5]));
        FitsFile::with_primary_image(img).to_file(&path).unwrap();

        let err = load_fits(&path).unwrap_err();
        assert!(err.to_string().contains("unsupported image shape"));
    }

    /// A debayered cube must not carry the source mosaic's BAYERPAT, or every
    /// other tool reads it back as raw sensor data.
    #[test]
    fn cfa_keywords_are_dropped_from_a_debayered_output() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 8, 6, Some("RGGB"));

        let rgb = load_fits(&input).unwrap().debayer().unwrap().unwrap();
        let output = tmp.path().join("rgb.fits");
        save_fits(&output, &rgb, SaveOptions::default()).unwrap();

        assert!(output_header(&output).find("BAYERPAT").is_none());

        // One split-out channel is a plain mono frame and must lose it too.
        let green = rgb.plane(1).unwrap();
        let channel = tmp.path().join("green.fits");
        save_fits(&channel, &green, SaveOptions::default()).unwrap();
        assert!(output_header(&channel).find("BAYERPAT").is_none());

        // A mosaic, though, keeps its pattern: the writer sets it itself.
        let mosaic = tmp.path().join("mosaic-out.fits");
        save_fits(&mosaic, &load_fits(&input).unwrap(), SaveOptions::default()).unwrap();
        assert_eq!(
            output_header(&mosaic).get_string("BAYERPAT").map(str::trim),
            Some("RGGB")
        );
    }

    #[test]
    fn save_options_history_lands_on_the_image_hdu() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&input, 8, 6, Some("RGGB"));
        let loaded = load_fits(&input).unwrap();

        let output = tmp.path().join("out.fits");
        save_fits(
            &output,
            &loaded,
            SaveOptions::default().with_history("debayered by fitz tests"),
        )
        .unwrap();

        assert!(output_header(&output).iter().any(|k| {
            k.name == "HISTORY"
                && k.comment
                    .as_deref()
                    .is_some_and(|c| c.contains("debayered by fitz"))
        }));
    }

    #[test]
    fn parse_date_obs_reads_utc_wall_clock_with_no_zone_shift() {
        // No fractional seconds.
        let t = parse_date_obs("2026-06-22T22:00:00").unwrap();
        assert_eq!(
            t,
            days_from_civil(2026, 6, 22) as f64 * 86400.0 + 22.0 * 3600.0
        );

        // Fractional seconds survive.
        let t = parse_date_obs("2026-05-31T04:57:09.004664").unwrap();
        let expected =
            days_from_civil(2026, 5, 31) as f64 * 86400.0 + 4.0 * 3600.0 + 57.0 * 60.0 + 9.004664;
        assert!((t - expected).abs() < 1e-6);
    }

    #[test]
    fn parse_date_obs_rejects_malformed_input() {
        assert_eq!(parse_date_obs(""), None);
        assert_eq!(parse_date_obs("2026-06-22"), None); // no time component
        assert_eq!(parse_date_obs("2026-06-22T22:00"), None); // missing seconds
        assert_eq!(parse_date_obs("not-a-date T 00:00:00"), None);
    }

    #[test]
    fn days_from_civil_matches_known_epoch_offsets() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
    }

    #[test]
    fn load_header_matches_load_fits_without_reading_pixels() {
        for name in ["uncompressed.fit", "compressed.fits.fz", "cfa_orion.fits"] {
            let path = test_data(name);
            let meta = load_header(&path).unwrap();
            let full = load_fits(&path).unwrap();

            assert_eq!(meta.image_type, full.image_type, "{name}");
            assert_eq!(
                (meta.width, meta.height),
                (full.width, full.height),
                "{name}"
            );
            // A tile-compressed HDU's own BITPIX describes its binary table;
            // the image's own bit depth is in ZBITPIX.
            assert_eq!(meta.bitpix, 16, "{name}");
        }
    }
}
