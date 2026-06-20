//! FITS image helpers shared by the `debayer` and `split` commands: locating
//! the image HDU, resolving the Bayer pattern, demosaicing a mosaic into an
//! interleaved RGB buffer, and writing pixel data back out as FITS.

use std::io::Cursor;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use bayer::{run_demosaic, BayerDepth, RasterDepth, RasterMut, CFA};
use fitskit::{FitsFile, HduData, Header, HeaderValue, ImageData, PixelData};

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

/// Find the first image HDU in a FITS file, returning its header and pixels.
pub(crate) fn find_image_hdu<'a>(
    fits: &'a FitsFile,
    input: &Path,
) -> Result<(&'a Header, &'a ImageData)> {
    let hdu = fits
        .hdus
        .iter()
        .find(|h| matches!(h.data, HduData::Image(_)))
        .ok_or_else(|| anyhow!("no image data found in {}", input.display()))?;

    match &hdu.data {
        HduData::Image(img) => Ok((&hdu.header, img)),
        _ => unreachable!(),
    }
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

    let bytes = scaled_pixels(header, img)
        .iter()
        .flat_map(|&v| round_to_u16(v).to_ne_bytes())
        .collect();

    (NATIVE_BAYER_DEPTH16, RasterDepth::Depth16, bytes)
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
                .chunks_exact(2)
                .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                .collect(),
        ),
    };

    Ok((width, height, rgb))
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
}
