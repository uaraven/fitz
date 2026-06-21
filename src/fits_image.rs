//! FITS image helpers shared by the `debayer` and `split` commands: locating
//! the image HDU, resolving the Bayer pattern, demosaicing a mosaic into an
//! interleaved RGB buffer, and writing pixel data back out as FITS.

use std::borrow::Cow;
use std::fs::File;
use std::io::Cursor;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use bayer::{run_demosaic, BayerDepth, RasterDepth, RasterMut, CFA};
use fitskit::{FitsFile, HduData, Header, HeaderValue, ImageData, PixelData};
use rayon::prelude::*;
use tiff::encoder::{colortype, TiffEncoder};

/// FITS header keywords used across the debayer/split commands.
pub(crate) const BAYERPAT: &str = "BAYERPAT";
pub(crate) const BSCALE: &str = "BSCALE";
pub(crate) const BZERO: &str = "BZERO";

#[cfg(target_endian = "little")]
const NATIVE_BAYER_DEPTH16: BayerDepth = BayerDepth::Depth16LE;
#[cfg(target_endian = "big")]
const NATIVE_BAYER_DEPTH16: BayerDepth = BayerDepth::Depth16BE;

/// An interleaved (R, G, B, R, G, B, …) image, either 8- or 16-bit per sample
/// depending on the source's bit depth.
pub(crate) enum RgbBuffer {
    U8(Vec<u8>),
    U16(Vec<u16>),
}

/// Find the first image in a FITS file, returning its header and pixels.
///
/// A plain image HDU is borrowed directly; a tile-compressed image HDU
/// (`ZIMAGE`) is transparently decompressed into an owned [`ImageData`], so
/// every command works on both raw and `.fz`-compressed inputs. For a
/// compressed image the returned header is the compressed HDU's own header,
/// which carries the original keywords (BAYERPAT, BSCALE/BZERO, RA/DEC, …)
/// alongside the `Z*` compression keywords.
pub(crate) fn find_image_hdu<'a>(
    fits: &'a FitsFile,
    input: &Path,
    verbose: bool,
) -> Result<(&'a Header, Cow<'a, ImageData>)> {
    for hdu in &fits.hdus {
        if let HduData::Image(img) = &hdu.data {
            return Ok((&hdu.header, Cow::Borrowed(img)));
        }
        if let Some(cimg) = hdu.as_compressed_image() {
            print_step(verbose, "decompressing");
            let img = cimg
                .decompress()
                .with_context(|| format!("{}: decompression failed", input.display()))?;
            return Ok((&hdu.header, Cow::Owned(img)));
        }
    }
    Err(anyhow!("no image data found in {}", input.display()))
}

pub(crate) fn resolve_cfa(header: &Header, cli_pattern: Option<CFA>) -> Result<CFA> {
    if let Some(p) = cli_pattern {
        return Ok(p);
    }

    let s = header
        .get_string(BAYERPAT)
        .ok_or_else(|| anyhow!("no BAYERPAT keyword in FITS header and no --pattern given"))?;
    parse_cfa(s).ok_or_else(|| anyhow!("unrecognized BAYERPAT value {s:?}"))
}

fn parse_cfa(s: &str) -> Option<CFA> {
    match s.trim().to_ascii_uppercase().as_str() {
        "RGGB" => Some(CFA::RGGB),
        "GBRG" => Some(CFA::GBRG),
        "BGGR" => Some(CFA::BGGR),
        "GRBG" => Some(CFA::GRBG),
        _ => None,
    }
}

/// Round a physical pixel value to the nearest u16, clamping to its range.
pub(crate) fn round_to_u16(v: f64) -> u16 {
    v.round().clamp(0.0, 65535.0) as u16
}

/// Narrow a 16-bit sample to 8-bit by keeping its high byte. The single source
/// of truth for the `>> 8` convention shared by 8-bpp debayer output and the
/// terminal preview (kitty and ANSI renderers).
pub(crate) fn high_byte(sample: u16) -> u8 {
    (sample >> 8) as u8
}

/// [`high_byte`] applied across an interleaved 16-bit RGB buffer.
pub(crate) fn rgb16_to_rgb8(src: &[u16]) -> Vec<u8> {
    src.par_iter().copied().map(high_byte).collect()
}

/// The BSCALE/BZERO scaling recorded in the header, defaulting to the FITS
/// no-op values (1.0/0.0) when the keywords are absent.
pub(crate) fn bscale_bzero(header: &Header) -> (f64, f64) {
    (
        header.get_float(BSCALE).unwrap_or(1.0),
        header.get_float(BZERO).unwrap_or(0.0),
    )
}

/// Read the image's physical pixel values, applying the BSCALE/BZERO scaling
/// recorded in the header (defaulting to 1.0/0.0 when absent).
pub(crate) fn scaled_pixels(header: &Header, img: &ImageData) -> Vec<f64> {
    let (bscale, bzero) = bscale_bzero(header);
    img.scaled_values(bscale, bzero)
}

/// Raw single-channel pixel bytes ready to feed into the demosaic algorithm,
/// along with the depths needed to interpret them.
fn raw_bytes_for_demosaic(header: &Header, img: &ImageData) -> (BayerDepth, RasterDepth, Vec<u8>) {
    if let PixelData::U8(v) = &img.pixels {
        return (BayerDepth::Depth8, RasterDepth::Depth8, v.clone());
    }

    (
        NATIVE_BAYER_DEPTH16,
        RasterDepth::Depth16,
        scaled_u16_bytes(header, img),
    )
}

/// Map each physical pixel value (BSCALE/BZERO applied) straight to a
/// native-endian `u16` and emit its bytes, the form the 16-bit demosaic path
/// consumes. Folding the scale-and-round over the raw pixels avoids the
/// intermediate `Vec<f64>` (8 bytes/pixel) that [`scaled_pixels`] would
/// allocate before rounding back down to 2 bytes/pixel.
fn scaled_u16_bytes(header: &Header, img: &ImageData) -> Vec<u8> {
    let (bscale, bzero) = bscale_bzero(header);
    let scale = |x: f64| round_to_u16(bzero + bscale * x).to_ne_bytes();
    match &img.pixels {
        PixelData::U8(v) => v.par_iter().flat_map_iter(|&x| scale(x as f64)).collect(),
        PixelData::I16(v) => v.par_iter().flat_map_iter(|&x| scale(x as f64)).collect(),
        PixelData::I32(v) => v.par_iter().flat_map_iter(|&x| scale(x as f64)).collect(),
        PixelData::I64(v) => v.par_iter().flat_map_iter(|&x| scale(x as f64)).collect(),
        PixelData::F32(v) => v.par_iter().flat_map_iter(|&x| scale(x as f64)).collect(),
        PixelData::F64(v) => v.par_iter().flat_map_iter(|&x| scale(x)).collect(),
    }
}

pub(crate) fn demosaic_to_rgb(
    header: &Header,
    img: &ImageData,
    cfa: CFA,
) -> Result<(usize, usize, RgbBuffer)> {
    let width = img.width().context("missing NAXIS1")?;
    let height = img.height().context("missing NAXIS2")?;

    let (bayer_depth, raster_depth, raw) = raw_bytes_for_demosaic(header, img);

    let bytes_per_pixel = match raster_depth {
        RasterDepth::Depth8 => 3,
        RasterDepth::Depth16 => 6,
    };
    let mut out_buf = vec![0u8; bytes_per_pixel * width * height];
    {
        let mut raster = RasterMut::new(width, height, raster_depth, &mut out_buf);
        run_demosaic(
            &mut Cursor::new(&raw[..]),
            bayer_depth,
            cfa,
            bayer::Demosaic::Linear,
            &mut raster,
        )
        .map_err(|e| anyhow!("{e}"))?;
    }

    let rgb = match raster_depth {
        RasterDepth::Depth8 => RgbBuffer::U8(out_buf),
        RasterDepth::Depth16 => RgbBuffer::U16(
            out_buf
                .par_chunks_exact(2)
                .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                .collect(),
        ),
    };

    Ok((width, height, rgb))
}

/// Load an image as an interleaved RGB buffer: demosaic a 2D Bayer mosaic, or
/// reinterleave an already-debayered 3-plane RGB cube as-is. Detection mirrors
/// the `debayer` command: a 3-plane image with no BAYERPAT header is treated as
/// already debayered unless `force_demosaic` is set.
pub(crate) fn load_rgb(
    header: &Header,
    img: &ImageData,
    input: &Path,
    pattern: Option<CFA>,
    force_demosaic: bool,
    verbose: bool,
) -> Result<(usize, usize, RgbBuffer)> {
    let already_debayered = !force_demosaic
        && get_bayerpat(header).is_none()
        && img.axes.len() == 3
        && img.axes[2] == 3;

    if already_debayered {
        println!(
            "{}: already debayered (no BAYERPAT header, found a 3-plane RGB cube) — skipping debayer step",
            input.display()
        );
        let width = img.axes[0];
        let height = img.axes[1];
        let rgb = rgb_from_cube(header, img, width, height);
        return Ok((width, height, rgb));
    }

    if img.axes.len() != 2 {
        bail!(
            "{}: expected a 2D mosaic image, found {} axes",
            input.display(),
            img.axes.len()
        );
    }

    let cfa = resolve_cfa(header, pattern)
        .with_context(|| format!("{}: cannot determine Bayer pattern", input.display()))?;

    print_step(verbose, "debayering");
    demosaic_to_rgb(header, img, cfa)
        .with_context(|| format!("{}: debayering failed", input.display()))
}

/// Interleave an already-debayered 3-plane RGB cube into an `RgbBuffer`,
/// without running it through the demosaic algorithm.
fn rgb_from_cube(header: &Header, img: &ImageData, width: usize, height: usize) -> RgbBuffer {
    let plane_len = width * height;

    if let PixelData::U8(v) = &img.pixels {
        let mut out = vec![0u8; plane_len * 3];
        out.par_chunks_mut(3).enumerate().for_each(|(i, px)| {
            px[0] = v[i];
            px[1] = v[plane_len + i];
            px[2] = v[2 * plane_len + i];
        });
        return RgbBuffer::U8(out);
    }

    let scaled = scaled_pixels(header, img);

    let mut out = vec![0u16; plane_len * 3];
    out.par_chunks_mut(3).enumerate().for_each(|(i, px)| {
        px[0] = round_to_u16(scaled[i]);
        px[1] = round_to_u16(scaled[plane_len + i]);
        px[2] = round_to_u16(scaled[2 * plane_len + i]);
    });
    RgbBuffer::U16(out)
}

/// Split an interleaved RGB sample buffer into concatenated R, G, B planes
/// (the same plane order [`rgb_from_cube`] expects when reading one back).
pub(crate) fn deinterleave_to_planes<T: Copy + Send + Sync>(v: &[T]) -> Vec<T> {
    let n = v.len() / 3;
    let mut out: Vec<T> = (0..n).into_par_iter().map(|i| v[i * 3]).collect();
    out.par_extend((0..n).into_par_iter().map(|i| v[i * 3 + 1]));
    out.par_extend((0..n).into_par_iter().map(|i| v[i * 3 + 2]));
    out
}

/// Write an interleaved 16-bit RGB image as an RGB16 TIFF.
pub(crate) fn write_rgb16_tiff(
    output: &Path,
    width: usize,
    height: usize,
    interleaved: &[u16],
) -> Result<()> {
    let file =
        File::create(output).with_context(|| format!("cannot create {}", output.display()))?;
    let mut enc = TiffEncoder::new(file)
        .with_context(|| format!("cannot create TIFF encoder for {}", output.display()))?;

    enc.write_image::<colortype::RGB16>(width as u32, height as u32, interleaved)
        .with_context(|| format!("cannot write {}", output.display()))?;

    Ok(())
}

/// Write an interleaved 16-bit RGB image as a 3-plane FITS cube, using the FITS
/// unsigned-16 convention (BITPIX 16 with BZERO 32768) so values in 0..=65535
/// round-trip.
pub(crate) fn write_rgb16_fits(
    output: &Path,
    width: usize,
    height: usize,
    interleaved: &[u16],
) -> Result<()> {
    let planes = deinterleave_to_planes(interleaved);
    let pixels = PixelData::I16(planes.iter().map(|&p| (p as i32 - 32768) as i16).collect());
    write_pixel_fits(output, vec![width, height, 3], pixels, 1.0, 32768.0)
}

/// Write a FITS file with the given pixel data, recording `bscale`/`bzero` in
/// the BSCALE/BZERO header keywords so readers can recover the original
/// (potentially unsigned or wider-range) physical values.
pub(crate) fn write_pixel_fits(
    output: &Path,
    axes: Vec<usize>,
    pixels: PixelData,
    bscale: f64,
    bzero: f64,
) -> Result<()> {
    let img = ImageData::new(axes, pixels);
    let mut fits = FitsFile::with_primary_image(img);

    if bscale != 1.0 || bzero != 0.0 {
        let header = &mut fits.primary_mut().header;
        header.set(BZERO, HeaderValue::Float(bzero), None);
        header.set(BSCALE, HeaderValue::Float(bscale), None);
    }

    fits.to_file(output)
        .with_context(|| format!("cannot write {}", output.display()))?;

    Ok(())
}

pub(crate) fn get_bayerpat(header: &Header) -> Option<&str> {
    header.get_string(BAYERPAT)
}

/// Bail if `output` already exists and the user didn't pass `--force`.
pub(crate) fn ensure_can_write(output: &Path, force: bool) -> Result<()> {
    if output.exists() && !force {
        bail!("{} already exists — use -f to overwrite", output.display());
    }
    Ok(())
}

/// Print the `input -> output` mapping when verbose mode is enabled.
pub(crate) fn print_progress(verbose: bool, input: &Path, output: &Path) {
    if verbose {
        println!("{} -> {}", input.display(), output.display());
    }
}

/// Print the name of an operation (reading, debayering, …) when verbose mode is
/// enabled.
pub(crate) fn print_step(verbose: bool, step: &str) {
    if verbose {
        println!("  {step}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_cfa_prefers_cli_pattern_over_header() {
        let mut header = Header::default();
        header.set(BAYERPAT, HeaderValue::String("RGGB".to_string()), None);

        let cfa = resolve_cfa(&header, Some(CFA::BGGR)).unwrap();
        assert_eq!(cfa, CFA::BGGR);
    }

    #[test]
    fn resolve_cfa_falls_back_to_header_when_no_cli_pattern() {
        let mut header = Header::default();
        header.set(BAYERPAT, HeaderValue::String("RGGB".to_string()), None);

        let cfa = resolve_cfa(&header, None).unwrap();
        assert_eq!(cfa, CFA::RGGB);
    }

    #[test]
    fn rgb16_to_rgb8_takes_high_byte() {
        assert_eq!(high_byte(0xFF00), 255);
        assert_eq!(high_byte(0x0100), 1);
        assert_eq!(rgb16_to_rgb8(&[0xFF00, 0x8000, 0x0100]), vec![255, 128, 1]);
    }
}
