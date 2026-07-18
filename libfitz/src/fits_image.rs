//! FITS image helpers shared by the `debayer`/`stretch`/`split` logic: locating
//! the image HDU, resolving the Bayer pattern, demosaicing a mosaic into an
//! interleaved RGB buffer, and writing pixel data back out as FITS.

use std::borrow::Cow;
use std::fs::File;
use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use bayer::{BayerDepth, CFA, RasterDepth, RasterMut, run_demosaic};
use fitskit::{FitsFile, HduData, Header, HeaderValue, ImageData, Keyword, PixelData};
use rayon::prelude::*;
use tiff::encoder::{TiffEncoder, colortype};

/// FITS header keywords used across the debayer/split commands.
pub const BAYERPAT: &str = "BAYERPAT";
pub const BSCALE: &str = "BSCALE";
pub const BZERO: &str = "BZERO";

/// CFA-mosaic keywords that become meaningless once an image is debayered into
/// an RGB image. Dropped by the image commands (debayer/stretch/split) when
/// copying the source header, but not by decompress, which round-trips the
/// mosaic faithfully. `load_rgb` also relies on the absence of `BAYERPAT` to
/// detect an already-debayered 3-plane cube, so leaving it would break
/// re-processing the output.
pub const CFA_KEYWORDS: &[&str] = &["BAYERPAT", "XBAYROFF", "YBAYROFF", "BAYOFFX", "BAYOFFY"];

#[cfg(target_endian = "little")]
const NATIVE_BAYER_DEPTH16: BayerDepth = BayerDepth::Depth16LE;
#[cfg(target_endian = "big")]
const NATIVE_BAYER_DEPTH16: BayerDepth = BayerDepth::Depth16BE;

/// An interleaved (R, G, B, R, G, B, …) image, either 8- or 16-bit per sample
/// depending on the source's bit depth.
pub enum RgbBuffer {
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
pub fn find_image_hdu<'a>(
    fits: &'a FitsFile,
    input: &Path,
) -> Result<(&'a Header, Cow<'a, ImageData>)> {
    for hdu in &fits.hdus {
        if let HduData::Image(img) = &hdu.data {
            return Ok((&hdu.header, Cow::Borrowed(img)));
        }
        if let Some(cimg) = hdu.as_compressed_image() {
            let img = cimg
                .decompress()
                .with_context(|| format!("{}: decompression failed", input.display()))?;
            return Ok((&hdu.header, Cow::Owned(img)));
        }
    }
    Err(anyhow!("no image data found in {}", input.display()))
}

/// Locate the index of the HDU holding image data (a plain image HDU, or a
/// tile-compressed image extension), without decompressing it. Used by
/// commands that only need to inspect or edit the HDU's header, not its pixel
/// data (unlike [`find_image_hdu`], which decompresses eagerly).
pub fn find_image_hdu_index(fits: &FitsFile) -> Option<usize> {
    fits.hdus.iter().position(|hdu| {
        matches!(hdu.data, HduData::Image(_)) || hdu.as_compressed_image().is_some()
    })
}

pub fn resolve_cfa(header: &Header, cli_pattern: Option<CFA>) -> Result<CFA> {
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
pub fn round_to_u16(v: f64) -> u16 {
    v.round().clamp(0.0, 65535.0) as u16
}

/// Narrow a 16-bit sample to 8-bit by keeping its high byte. The single source
/// of truth for the `>> 8` convention shared by 8-bpp debayer output and the
/// terminal preview (kitty and ANSI renderers).
pub fn high_byte(sample: u16) -> u8 {
    (sample >> 8) as u8
}

/// [`high_byte`] applied across an interleaved 16-bit RGB buffer.
pub fn rgb16_to_rgb8(src: &[u16]) -> Vec<u8> {
    src.par_iter().copied().map(high_byte).collect()
}

/// The BSCALE/BZERO scaling recorded in the header, defaulting to the FITS
/// no-op values (1.0/0.0) when the keywords are absent.
pub fn bscale_bzero(header: &Header) -> (f64, f64) {
    (
        header.get_float(BSCALE).unwrap_or(1.0),
        header.get_float(BZERO).unwrap_or(0.0),
    )
}

/// Read the image's physical pixel values, applying the BSCALE/BZERO scaling
/// recorded in the header (defaulting to 1.0/0.0 when absent).
pub fn scaled_pixels(header: &Header, img: &ImageData) -> Vec<f64> {
    let (bscale, bzero) = bscale_bzero(header);
    img.scaled_values(bscale, bzero)
}

/// A single-channel `f64` image to detect stars on, produced by
/// [`detection_plane`].
pub struct MonoPlane {
    pub width: usize,
    pub height: usize,
    /// Physical (BSCALE/BZERO-applied) values, row-major.
    pub values: Vec<f64>,
    /// The physical saturation level of the *source* samples — see
    /// [`sample_saturation`]. Not derivable from `values`, which are `f64` and
    /// have no ceiling of their own: taking the observed maximum instead would
    /// make the brightest star in every frame look clipped.
    /// [`f64::INFINITY`] when the source has no representable ceiling.
    pub saturation: f64,
}

/// The physical value of the largest representable raw sample: 65535 for the
/// unsigned-16 convention, 255 for `U8`. [`f64::INFINITY`] for sample types
/// with no ceiling worth clipping against — float samples, and the wide integer
/// types, which no realistic sensor fills.
///
/// This is the same quantity `info::stats_from_counts` derives from its
/// value-count array length, by the same BSCALE/BZERO map and under the same
/// `bscale > 0` guard `pixel_stats` applies before taking that fast path — the
/// two must agree, and a test pins that they do.
pub fn sample_saturation(header: &Header, img: &ImageData) -> f64 {
    let max_raw = match &img.pixels {
        PixelData::U8(_) => 255.0,
        PixelData::I16(_) => 32767.0,
        _ => return f64::INFINITY,
    };
    let (bscale, bzero) = bscale_bzero(header);
    if bscale <= 0.0 {
        return f64::INFINITY;
    }
    bzero + bscale * max_raw
}

/// Build the mono plane star detection runs on:
///   - a CFA mosaic (`BAYERPAT` present) → the green super-pixel plane, each
///     pixel the mean of one 2x2 cell's two green sites, so `(w/2 x h/2)`;
///   - a mono frame (2D, no `BAYERPAT`) → the frame's own scaled values;
///   - an already-debayered RGB cube (`NAXIS3=3`, no `BAYERPAT`) → its green
///     channel at full resolution ([`green_plane`]).
///
/// Anything else (e.g. a >3-plane cube) is an error.
///
/// **A CFA frame's measurements come out in half-resolution pixels.** An HFR or
/// FWHM measured here reads about half the number NINA reports for the same
/// frame. That is inherent to the super-pixel plane and is not a bug: every
/// frame in a session comes off the same sensor, so the trend — the only thing
/// a time series shows — is unaffected. A caller that reports absolute numbers
/// should say which plane they were measured on (compare the returned width
/// against the frame's). An RGB cube's green plane is *full* resolution, so its
/// numbers read about twice a raw mosaic's — the same caveat, opposite sign.
///
/// Why a super-pixel plane at all: a star profile sampled through a Bayer
/// filter is not a PSF, and the HFR measured from it is noise.
pub fn detection_plane(header: &Header, img: &ImageData) -> Result<MonoPlane> {
    if is_debayered_rgb_cube(header, img) {
        return Ok(green_plane(header, img));
    }
    if img.axes.len() != 2 {
        bail!(
            "star detection needs a single-plane image, not a {}-axis one",
            img.axes.len()
        );
    }
    let width = img.width().context("missing NAXIS1")?;
    let height = img.height().context("missing NAXIS2")?;
    let saturation = sample_saturation(header, img);
    let values = scaled_pixels(header, img);

    let Some(pattern) = get_bayerpat(header) else {
        return Ok(MonoPlane {
            width,
            height,
            values,
            saturation,
        });
    };
    let cfa =
        parse_cfa(pattern).ok_or_else(|| anyhow!("unrecognized BAYERPAT value {pattern:?}"))?;

    // A 2x2 cell is the quantum here, so an odd width or height drops its last
    // column/row.
    let (pw, ph) = (width / 2, height / 2);
    let [(ax, ay), (bx, by)] = green_sites(cfa);
    let plane = (0..ph)
        .into_par_iter()
        .flat_map_iter(|cy| {
            let values = &values;
            let site =
                move |gx: usize, gy: usize, cx: usize| values[(2 * cy + gy) * width + 2 * cx + gx];
            (0..pw).map(move |cx| (site(ax, ay, cx) + site(bx, by, cx)) / 2.0)
        })
        .collect();

    Ok(MonoPlane {
        width: pw,
        height: ph,
        values: plane,
        // Averaging two sites preserves the level: the mean of two samples
        // clipped at the ceiling is still the ceiling, so the source's
        // saturation carries over unchanged.
        saturation,
    })
}

/// Reduce an already-debayered 3-plane RGB cube to a single [`MonoPlane`] — its
/// green channel, at the frame's full resolution.
///
/// Green is the channel to measure on: a Bayer sensor has twice as many green
/// sites as red or blue, so green carries the most signal and the least noise,
/// and detecting on green keeps a debayered cube's numbers comparable to the
/// green super-pixel plane a raw mosaic uses ([`detection_plane`]). Unlike that
/// half-resolution mosaic plane, this is full resolution — see the caveat on
/// [`detection_plane`].
///
/// A **float** cube's green channel is quantized to the unsigned-16 range by a
/// fixed full-scale mapping — physical `1.0` → 65535 — so it analyzes like the
/// 16-bit CFA frames it sits alongside: normalized float data (drizzle output is
/// commonly `[0, 1]`) would otherwise plot near zero on an ADU chart and have no
/// meaningful saturation ceiling. The scale is deliberately *fixed*, not the
/// frame's own max → 65535 that the stretch path uses: a per-frame
/// max-normalization would rescale every frame differently, so one star would
/// report a different ADU from sub to sub and a session's chart would be
/// meaningless. Values above `1.0` clamp at the ceiling, as an over-range
/// integer would. Integer cubes already fit `[0, 65535]` and keep their physical
/// values (and the source sample type's ceiling).
///
/// The caller must have established the cube shape (this indexes `axes[0..2]`
/// and reads the middle third of the samples); [`detection_plane`] and
/// [`crate::info::pixel_stats`] gate on [`is_debayered_rgb_cube`] first.
pub fn green_plane(header: &Header, img: &ImageData) -> MonoPlane {
    let physical = green_plane_values(header, img);
    let (values, saturation) = match &img.pixels {
        PixelData::F32(_) | PixelData::F64(_) => {
            // Fixed full-scale map (1.0 → 65535), rounded and clamped into the
            // unsigned-16 range — not the stretch path's per-frame max scaling.
            let quantized = physical
                .par_iter()
                .map(|&v| f64::from(round_to_u16(v * f64::from(u16::MAX))))
                .collect();
            (quantized, f64::from(u16::MAX))
        }
        _ => (physical, sample_saturation(header, img)),
    };
    MonoPlane {
        width: img.axes[0],
        height: img.axes[1],
        values,
        saturation,
    }
}

/// The physical (BSCALE/BZERO-applied) values of a 3-plane RGB cube's green
/// channel — [`plane_values`] for the middle plane.
fn green_plane_values(header: &Header, img: &ImageData) -> Vec<f64> {
    plane_values(header, img, 1)
}

/// The physical (BSCALE/BZERO-applied) values of one plane of a planar image.
/// FITS stores a cube planar — the full first plane, then the second, and so on
/// — so plane `plane` is the samples `plane*width*height..(plane+1)*width*height`;
/// only those are scaled, never materializing the other planes as `f64`. The
/// caller must have established that the plane exists (e.g. via
/// [`is_rgb_cube_shape`]); this indexes `axes[0..2]` and slices the samples.
pub(crate) fn plane_values(header: &Header, img: &ImageData, plane: usize) -> Vec<f64> {
    let (bscale, bzero) = bscale_bzero(header);
    let plane_len = img.axes[0] * img.axes[1];
    let range = plane * plane_len..(plane + 1) * plane_len;
    let scale = |x: f64| bzero + bscale * x;
    match &img.pixels {
        PixelData::U8(v) => v[range].par_iter().map(|&x| scale(x as f64)).collect(),
        PixelData::I16(v) => v[range].par_iter().map(|&x| scale(x as f64)).collect(),
        PixelData::I32(v) => v[range].par_iter().map(|&x| scale(x as f64)).collect(),
        PixelData::I64(v) => v[range].par_iter().map(|&x| scale(x as f64)).collect(),
        PixelData::F32(v) => v[range].par_iter().map(|&x| scale(x as f64)).collect(),
        PixelData::F64(v) => v[range].par_iter().map(|&x| scale(x)).collect(),
    }
}

/// The two green sites within a 2x2 CFA cell, as `(x, y)` offsets.
fn green_sites(cfa: CFA) -> [(usize, usize); 2] {
    match cfa {
        CFA::RGGB | CFA::BGGR => [(1, 0), (0, 1)],
        CFA::GBRG | CFA::GRBG => [(0, 0), (1, 1)],
    }
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

pub fn demosaic_to_rgb(
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

/// What [`load_rgb`] had to do to produce its RGB buffer, so a caller can
/// report or otherwise react to the "already debayered" cases without
/// `load_rgb` itself doing any terminal I/O.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadRgbNotice {
    /// The source was a raw Bayer mosaic and was demosaiced.
    Demosaiced,
    /// A 3-plane image with no `BAYERPAT` header was treated as already
    /// debayered and reinterleaved as-is.
    AlreadyDebayeredRgbCube,
    /// A single-plane image with no `BAYERPAT` header was treated as an
    /// already-debayered monochrome image and replicated across channels.
    AlreadyDebayeredMono,
}

/// The result of [`load_rgb`]: the interleaved RGB buffer, its dimensions, and
/// a [`LoadRgbNotice`] describing which path produced it.
pub struct LoadedRgb {
    pub width: usize,
    pub height: usize,
    pub rgb: RgbBuffer,
    pub notice: LoadRgbNotice,
}

/// Load an image as an interleaved RGB buffer: demosaic a 2D Bayer mosaic, or
/// reinterleave an already-debayered 3-plane RGB cube as-is. Detection: a
/// 3-plane image with no BAYERPAT header is treated as already debayered
/// unless `force_demosaic` is set. Callers decide whether/how to surface the
/// returned [`LoadRgbNotice`] (a CLI prints a message, a GUI might show a
/// badge) — this function itself performs no I/O beyond the FITS data.
pub fn load_rgb(
    header: &Header,
    img: &ImageData,
    pattern: Option<CFA>,
    force_demosaic: bool,
) -> Result<LoadedRgb> {
    let already_debayered = !force_demosaic && is_debayered_rgb_cube(header, img);

    if already_debayered {
        let width = img.axes[0];
        let height = img.axes[1];
        let rgb = rgb_from_cube(header, img, width, height);
        return Ok(LoadedRgb {
            width,
            height,
            rgb,
            notice: LoadRgbNotice::AlreadyDebayeredRgbCube,
        });
    }

    if !force_demosaic && is_debayered_mono(header, img) {
        let width = img.axes[0];
        let height = img.axes[1];
        let rgb = rgb_from_mono(header, img, width, height);
        return Ok(LoadedRgb {
            width,
            height,
            rgb,
            notice: LoadRgbNotice::AlreadyDebayeredMono,
        });
    }

    if img.axes.len() != 2 {
        bail!("expected a 2D mosaic image, found {} axes", img.axes.len());
    }

    let cfa = resolve_cfa(header, pattern)
        .with_context(|| "cannot determine Bayer pattern".to_string())?;

    let (width, height, rgb) =
        demosaic_to_rgb(header, img, cfa).with_context(|| "debayering failed".to_string())?;
    Ok(LoadedRgb {
        width,
        height,
        rgb,
        notice: LoadRgbNotice::Demosaiced,
    })
}

/// Convert physical pixel values (obtained via [`scaled_pixels`]) to `u16`.
///
/// For float source data (`F32`/`F64`) the physical range can be anything — for
/// example drizzle output often uses [0, 1] — so plain rounding would clip
/// almost every value to 0.  In that case we scale linearly so the maximum
/// maps to `u16::MAX`, preserving the relative distribution for the stretch.
/// For integer sources the values already fit in [0, 65535], so we round directly.
fn scale_physical_to_u16(pixels: &PixelData, values: &[f64]) -> Vec<u16> {
    match pixels {
        PixelData::F32(_) | PixelData::F64(_) => {
            let max = values
                .par_iter()
                .copied()
                .filter(|v| v.is_finite())
                .reduce(|| 0.0_f64, f64::max);
            if max <= 0.0 {
                return vec![0u16; values.len()];
            }
            let factor = u16::MAX as f64 / max;
            values
                .par_iter()
                .map(|&v| round_to_u16(v.max(0.0) * factor))
                .collect()
        }
        _ => values.par_iter().map(|&v| round_to_u16(v)).collect(),
    }
}

/// Build an interleaved `RgbBuffer` for a `plane_len`-pixel image, `src` mapping
/// each output pixel `i` to the three source-sample indices (R, G, B) that feed
/// it. The single scaffolding behind [`rgb_from_cube`] and [`rgb_from_mono`]:
/// a `U8` source stays 8-bit and copies raw; anything else is scaled to `u16`
/// via [`scale_physical_to_u16`] first. Both take the same parallel interleave.
fn interleave_rgb(
    header: &Header,
    img: &ImageData,
    plane_len: usize,
    src: impl Fn(usize) -> [usize; 3] + Sync,
) -> RgbBuffer {
    fn fill<T: Copy + Default + Send + Sync>(
        samples: &[T],
        plane_len: usize,
        src: impl Fn(usize) -> [usize; 3] + Sync,
    ) -> Vec<T> {
        let mut out = vec![T::default(); plane_len * 3];
        out.par_chunks_mut(3).enumerate().for_each(|(i, px)| {
            let [r, g, b] = src(i);
            px[0] = samples[r];
            px[1] = samples[g];
            px[2] = samples[b];
        });
        out
    }

    if let PixelData::U8(v) = &img.pixels {
        return RgbBuffer::U8(fill(v, plane_len, src));
    }
    let u16_vals = scale_physical_to_u16(&img.pixels, &scaled_pixels(header, img));
    RgbBuffer::U16(fill(&u16_vals, plane_len, src))
}

/// Interleave an already-debayered 3-plane RGB cube into an `RgbBuffer`,
/// without running it through the demosaic algorithm.
fn rgb_from_cube(header: &Header, img: &ImageData, width: usize, height: usize) -> RgbBuffer {
    let plane_len = width * height;
    interleave_rgb(header, img, plane_len, |i| {
        [i, plane_len + i, 2 * plane_len + i]
    })
}

/// Replicate a single-plane (monochrome) image's samples across all three
/// channels, producing the same interleaved `RgbBuffer` shape [`rgb_from_cube`]
/// builds from a 3-plane cube.
fn rgb_from_mono(header: &Header, img: &ImageData, width: usize, height: usize) -> RgbBuffer {
    interleave_rgb(header, img, width * height, |i| [i, i, i])
}

/// Load a 2D image's raw pixel values as a grayscale `RgbBuffer` (the sample
/// replicated across all three channels), without running the demosaic
/// algorithm. Used by `fitz preview --no-debayer` to show a raw, not-yet-
/// debayered mosaic as-is rather than color-interpolating it.
pub fn load_mono_raw(header: &Header, img: &ImageData) -> Result<(usize, usize, RgbBuffer)> {
    if img.axes.len() != 2 {
        bail!(
            "expected a 2D mosaic image for a grayscale preview, found {} axes",
            img.axes.len()
        );
    }
    let width = img.axes[0];
    let height = img.axes[1];
    Ok((width, height, rgb_from_mono(header, img, width, height)))
}

/// Split an interleaved RGB sample buffer into concatenated R, G, B planes
/// (the same plane order [`rgb_from_cube`] expects when reading one back).
pub fn deinterleave_to_planes<T: Copy + Send + Sync>(v: &[T]) -> Vec<T> {
    let n = v.len() / 3;
    let mut out: Vec<T> = (0..n).into_par_iter().map(|i| v[i * 3]).collect();
    out.par_extend((0..n).into_par_iter().map(|i| v[i * 3 + 1]));
    out.par_extend((0..n).into_par_iter().map(|i| v[i * 3 + 2]));
    out
}

/// Write an interleaved 16-bit RGB image as an RGB16 TIFF.
pub fn write_rgb16_tiff(
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
/// round-trip. Metadata from `src_header` (minus `drop` and structural keywords)
/// is copied onto the output, and `history`, when present, is recorded as a
/// HISTORY card.
pub fn write_rgb16_fits(
    output: &Path,
    width: usize,
    height: usize,
    interleaved: &[u16],
    src_header: Option<&Header>,
    drop: &[&str],
    history: Option<&str>,
) -> Result<()> {
    let planes = deinterleave_to_planes(interleaved);
    let pixels = PixelData::I16(
        planes
            .par_iter()
            .map(|&p| (p as i32 - 32768) as i16)
            .collect(),
    );
    write_pixel_fits(
        output,
        vec![width, height, 3],
        pixels,
        1.0,
        32768.0,
        src_header,
        drop,
        history,
    )
}

/// Write a FITS file with the given pixel data, recording `bscale`/`bzero` in
/// the BSCALE/BZERO header keywords so readers can recover the original
/// (potentially unsigned or wider-range) physical values.
///
/// When `src_header` is given, its metadata keywords are copied onto the output
/// after the mandatory keywords and BSCALE/BZERO (see [`copy_metadata`]), with
/// `drop` naming extra keywords to skip. `history`, when present, is appended as
/// a HISTORY provenance card.
#[allow(clippy::too_many_arguments)]
pub fn write_pixel_fits(
    output: &Path,
    axes: Vec<usize>,
    pixels: PixelData,
    bscale: f64,
    bzero: f64,
    src_header: Option<&Header>,
    drop: &[&str],
    history: Option<&str>,
) -> Result<()> {
    let fits = build_pixel_fits(axes, pixels, bscale, bzero, src_header, drop, history);
    fits.to_file(output)
        .with_context(|| format!("cannot write {}", output.display()))?;
    Ok(())
}

/// Build (but do not write) the in-memory FITS file [`write_pixel_fits`] writes:
/// a primary-image HDU carrying `pixels`, the `bscale`/`bzero` scaling, and the
/// copied metadata / HISTORY card. Split out so callers that need to further
/// transform the file before writing (e.g. tile-compress it on export) can do so
/// without a write-then-reread round trip.
#[allow(clippy::too_many_arguments)]
pub fn build_pixel_fits(
    axes: Vec<usize>,
    pixels: PixelData,
    bscale: f64,
    bzero: f64,
    src_header: Option<&Header>,
    drop: &[&str],
    history: Option<&str>,
) -> FitsFile {
    let img = ImageData::new(axes, pixels);
    let mut fits = FitsFile::with_primary_image(img);

    {
        let header = &mut fits.primary_mut().header;
        if bscale != 1.0 || bzero != 0.0 {
            header.set(BZERO, HeaderValue::Float(bzero), None);
            header.set(BSCALE, HeaderValue::Float(bscale), None);
        }
        if let Some(src) = src_header {
            copy_metadata(header, src, drop);
        }
        if let Some(text) = history {
            add_history(header, text);
        }
    }

    fits
}

/// Copy metadata keywords from `src` onto `dest`, skipping structural/reserved
/// keywords (see [`is_reserved_keyword`]) plus any names in `extra_drop`. `dest`
/// is expected to already carry the mandatory keywords that
/// `FitsFile::with_primary_image` generated (and any BSCALE/BZERO the writer
/// set); survivors are appended after them, preserving FITS keyword ordering.
/// Commentary cards (COMMENT/HISTORY, whose value is `None`) are copied verbatim
/// via `push` rather than `set`.
pub fn copy_metadata(dest: &mut Header, src: &Header, extra_drop: &[&str]) {
    for kw in &src.keywords {
        if is_droppable(&kw.name, extra_drop) {
            continue;
        }
        dest.push(kw.clone());
    }
}

/// True if a keyword must not be carried onto an output header: either a
/// structural/reserved keyword (see [`is_reserved_keyword`]) or one the caller
/// explicitly named in `extra_drop`. Shared by [`copy_metadata`] and
/// [`copy_missing_metadata`] so both apply the same drop rule.
fn is_droppable(name: &str, extra_drop: &[&str]) -> bool {
    is_reserved_keyword(name) || extra_drop.iter().any(|d| d.eq_ignore_ascii_case(name))
}

/// Copy every non-structural keyword from `src` onto `dest` that `dest`
/// doesn't already carry a card for, returning the number of cards copied.
/// Used by the `copy-header` command to fill in only what a target file is
/// missing, rather than overwriting what it already has. `extra_drop` names
/// additional keywords to skip, on top of the reserved ones (see
/// [`copy_metadata`]) — `copy_header_file` uses this to keep a stale
/// `BAYERPAT` from a mosaic source landing on an already-debayered RGB target.
///
/// Structural/reserved keywords (see [`is_reserved_keyword`]) — `dest`'s own
/// resolution (`NAXIS*`), bit depth (`BITPIX`), pixel scaling
/// (`BSCALE`/`BZERO`), and similar data-layout keywords — are never copied,
/// since they describe `dest`'s own pixel data, not `src`'s. Commentary cards
/// (`COMMENT`/`HISTORY`, whose value is `None`) are always appended: multiple
/// independent annotations are normal, so they aren't deduplicated by name
/// like a regular keyword would be.
pub fn copy_missing_metadata(dest: &mut Header, src: &Header, extra_drop: &[&str]) -> usize {
    let mut copied = 0;
    for kw in &src.keywords {
        if is_droppable(&kw.name, extra_drop) {
            continue;
        }
        if kw.value.is_some() && dest.find(&kw.name).is_some() {
            continue;
        }
        dest.push(kw.clone());
        copied += 1;
    }
    copied
}

/// Pixel-scaling keywords that [`copy_metadata`] deliberately drops (the image
/// commands let their writer own scaling), but which a lossless compress /
/// decompress round-trip must preserve: the pixels are stored unchanged, so the
/// original `BSCALE`/`BZERO`/`BLANK` are still the correct physical
/// interpretation. Dropping `BZERO` would, for example, silently shift an
/// unsigned-16 image (`BZERO = 32768`) by 32768.
pub const SCALING_KEYWORDS: &[&str] = &[BSCALE, BZERO, "BLANK"];

/// Copy the [`SCALING_KEYWORDS`] present on `src` onto `dest`, used by the
/// compress/decompress container paths to keep the physical pixel scaling that
/// [`copy_metadata`] strips.
pub fn copy_scaling(dest: &mut Header, src: &Header) {
    for &name in SCALING_KEYWORDS {
        if let Some(kw) = src.find(name) {
            dest.push(kw.clone());
        }
    }
}

/// Append a HISTORY provenance card to `dest`.
pub fn add_history(dest: &mut Header, text: &str) {
    dest.push(Keyword::commentary("HISTORY", text));
}

/// True if `name` is `prefix` followed by at least one ASCII digit and nothing
/// else (e.g. `is_indexed("NAXIS3", "NAXIS")` is true, but `"NAXIS"` and
/// `"NAXISA"` are false).
fn is_indexed(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .map(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
}

/// True if `name` (uppercase, as fitskit stores keyword names) is a structural,
/// data-encoding, table, or tile-compression keyword that must not be copied
/// from a source header onto a freshly built output header: fitskit regenerates
/// the mandatory keywords for the new geometry, each writer sets its own
/// BSCALE/BZERO, and the table/`Z*` keywords only describe a compressed
/// container, not the image.
fn is_reserved_keyword(name: &str) -> bool {
    const EXACT: &[&str] = &[
        // Mandatory / structural.
        "SIMPLE", "BITPIX", "NAXIS", "EXTEND", "XTENSION", "PCOUNT", "GCOUNT",
        // Output encoding — owned by the writer.
        "BSCALE", "BZERO", // Data-dependent values tied to the old BITPIX / pixels.
        "BLANK", "DATAMIN", "DATAMAX", "CHECKSUM", "DATASUM",
        // BINTABLE structure (the compressed-image container).
        "TFIELDS", "THEAP", "EXTNAME", // Tile-compression scalar keywords.
        "ZIMAGE", "ZCMPTYPE", "ZBITPIX", "ZNAXIS", "ZQUANTIZ", "ZDITHER0", "ZBLANK", "ZMASKCMP",
        "ZSIMPLE", "ZEXTEND", "ZTENSION", "ZPCOUNT", "ZGCOUNT", "ZHECKSUM", "ZDATASUM",
        // Never copied as standalone cards.
        "END", "CONTINUE",
    ];
    if EXACT.contains(&name) {
        return true;
    }

    // Indexed families: <prefix> followed by one or more digits.
    const INDEXED: &[&str] = &[
        "NAXIS", "TFORM", "TTYPE", "TUNIT", "TSCAL", "TZERO", "TNULL", "TDIM", "TDISP", "ZNAXIS",
        "ZTILE", "ZNAME", "ZVAL",
    ];
    INDEXED.iter().any(|p| is_indexed(name, p))
}

/// The raw `BAYERPAT` header string, if present, without validating it as a
/// known CFA pattern (see [`resolve_cfa`] for that).
pub fn get_bayerpat(header: &Header) -> Option<&str> {
    header.get_string(BAYERPAT)
}

/// True if `img` has the shape of a debayered RGB cube: a 3-plane image
/// (`NAXIS3=3`).
pub fn is_rgb_cube_shape(img: &ImageData) -> bool {
    img.axes.len() == 3 && img.axes[2] == 3
}

/// True if `header` describes a 3-plane image (`NAXIS=3`, `NAXIS3=3`), reading
/// straight from the header keywords rather than decoded pixel data. A
/// tile-compressed HDU's header still carries the original `NAXIS*` alongside
/// its own `ZNAXIS*`, so this works without decompressing — needed by
/// `copy-header`, which edits headers only and never touches pixel data.
pub fn header_is_rgb_cube_shape(header: &Header) -> bool {
    header.get_int("NAXIS") == Some(3) && header.get_int("NAXIS3") == Some(3)
}

/// True if `img` should be treated as an already-debayered RGB cube rather
/// than a Bayer mosaic: a 3-plane image with no `BAYERPAT` header. Shared by
/// `load_rgb` (which uses it to skip demosaicing) and the `info` command
/// (which uses it to decide the channel count).
pub fn is_debayered_rgb_cube(header: &Header, img: &ImageData) -> bool {
    get_bayerpat(header).is_none() && is_rgb_cube_shape(img)
}

/// True if `img` should be treated as an already-debayered monochrome image
/// rather than an undebayered Bayer mosaic: a single-plane (2D) image with no
/// `BAYERPAT` header. A genuine mosaic always carries `BAYERPAT` (or needs
/// `--pattern`/`--force-demosaic`), so a 2D image without it is assumed to
/// already be monochrome rather than raw sensor data.
pub fn is_debayered_mono(header: &Header, img: &ImageData) -> bool {
    get_bayerpat(header).is_none() && img.axes.len() == 2
}

/// Copy `src`'s metadata and pixel-scaling keywords onto `dest` (see
/// [`copy_metadata`] and [`copy_scaling`]), the pairing the compress/decompress
/// container paths use to carry a source HDU's header onto its rebuilt output.
pub fn carry_over_metadata(dest: &mut Header, src: &Header) {
    copy_metadata(dest, src, &[]);
    copy_scaling(dest, src);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_data;

    /// A 4x4 frame whose values are powers of two, optionally tagged with a
    /// Bayer pattern. Deliberately not the sequential ramp the other fixtures
    /// use: on a ramp both diagonals of a 2x2 cell have the same mean, so a
    /// green-site mix-up between `RGGB` and `GRBG` would go unnoticed.
    fn powers_of_two_frame(pattern: Option<&str>) -> (Header, ImageData) {
        let pixels: Vec<i16> = vec![
            1, 2, 4, 8, //
            16, 32, 64, 128, //
            256, 512, 1024, 2048, //
            4096, 8192, 16384, 32767,
        ];
        let mut header = Header::default();
        if let Some(p) = pattern {
            header.set(BAYERPAT, HeaderValue::String(p.to_string()), None);
        }
        (header, ImageData::new(vec![4, 4], PixelData::I16(pixels)))
    }

    #[test]
    fn detection_plane_averages_the_green_sites_of_a_mosaic() {
        let (header, img) = powers_of_two_frame(Some("RGGB"));
        let plane = detection_plane(&header, &img).unwrap();

        // Half resolution: one pixel per 2x2 cell.
        assert_eq!((plane.width, plane.height), (2, 2));
        // RGGB puts green at (1,0) and (0,1) of each cell: (2+16)/2, (8+64)/2,
        // (512+4096)/2, (2048+16384)/2.
        assert_eq!(plane.values, vec![9.0, 36.0, 2304.0, 9216.0]);
    }

    #[test]
    fn detection_plane_locates_green_for_every_bayer_pattern() {
        // RGGB/BGGR share their green sites, as do GRBG/GBRG — but the two
        // pairs differ, and each pattern's plane is asserted in full.
        for pattern in ["RGGB", "BGGR"] {
            let (header, img) = powers_of_two_frame(Some(pattern));
            let plane = detection_plane(&header, &img).unwrap();
            assert_eq!(plane.values, vec![9.0, 36.0, 2304.0, 9216.0], "{pattern}");
        }
        for pattern in ["GRBG", "GBRG"] {
            let (header, img) = powers_of_two_frame(Some(pattern));
            let plane = detection_plane(&header, &img).unwrap();
            // Green at (0,0) and (1,1): (1+32)/2, (4+128)/2, (256+8192)/2,
            // (1024+32767)/2.
            assert_eq!(plane.values, vec![16.5, 66.0, 4224.0, 16895.5], "{pattern}");
        }
    }

    #[test]
    fn detection_plane_of_a_mono_frame_is_the_frame() {
        let (header, img) = powers_of_two_frame(None);
        let plane = detection_plane(&header, &img).unwrap();

        assert_eq!((plane.width, plane.height), (4, 4));
        assert_eq!(plane.values, scaled_pixels(&header, &img));
    }

    #[test]
    fn detection_plane_of_an_rgb_cube_is_its_green_plane() {
        // An already-debayered cube (no BAYERPAT) reduces to its green channel
        // at full resolution: the middle third of the planar samples, unchanged.
        // Planes are R = 1..=4, G = 10..=40, B = 100..=400.
        let img = ImageData::new(
            vec![2, 2, 3],
            PixelData::I16(vec![1, 2, 3, 4, 10, 20, 30, 40, 100, 200, 300, 400]),
        );
        let plane = detection_plane(&Header::default(), &img).unwrap();
        assert_eq!((plane.width, plane.height), (2, 2));
        assert_eq!(plane.values, vec![10.0, 20.0, 30.0, 40.0]);
    }

    #[test]
    fn green_plane_of_a_float_cube_maps_full_scale_to_the_unsigned16_range() {
        // A float cube's green channel is quantized by a fixed full-scale map
        // (physical 1.0 -> 65535), matching a 16-bit CFA frame's 0..=65535 axis —
        // *not* the frame's own max, which would rescale every frame differently.
        // Planes: R = 0, G = 0/0.25/0.5/1.0, B = 0. So 0.25 -> 16384,
        // 0.5 -> 32768, 1.0 -> 65535.
        let mut pixels = vec![0.0f32; 4]; // red plane
        pixels.extend_from_slice(&[0.0, 0.25, 0.5, 1.0]); // green plane
        pixels.extend_from_slice(&[0.0f32; 4]); // blue plane
        let img = ImageData::new(vec![2, 2, 3], PixelData::F32(pixels));

        let plane = green_plane(&Header::default(), &img);
        assert_eq!(plane.saturation, 65535.0);
        assert_eq!(plane.values, vec![0.0, 16384.0, 32768.0, 65535.0]);
    }

    #[test]
    fn green_plane_of_a_float_cube_clamps_values_above_full_scale() {
        // The fixed map is absolute, so a physical value above 1.0 clamps at the
        // 65535 ceiling rather than pulling the whole frame's scale down (which a
        // max-normalization would do) — an over-range sample reads as saturated,
        // exactly as an over-range integer would.
        let mut pixels = vec![0.0f32; 4];
        pixels.extend_from_slice(&[0.5, 1.0, 1.5, 2.0]); // green: two over full scale
        pixels.extend_from_slice(&[0.0f32; 4]);
        let img = ImageData::new(vec![2, 2, 3], PixelData::F32(pixels));

        let plane = green_plane(&Header::default(), &img);
        assert_eq!(plane.values, vec![32768.0, 65535.0, 65535.0, 65535.0]);
    }

    #[test]
    fn green_plane_of_an_integer_cube_keeps_its_physical_values() {
        // An integer cube already fits 0..=65535, so its green channel is left
        // as its physical values and carries the source sample type's ceiling
        // (65535 under the unsigned-16 convention) — no rescaling.
        let img = ImageData::new(
            vec![2, 2, 3],
            PixelData::I16(vec![1, 2, 3, 4, 10, 20, 30, 40, 100, 200, 300, 400]),
        );
        let mut header = Header::default();
        header.set(BZERO, HeaderValue::Float(32768.0), None);
        let plane = green_plane(&header, &img);
        assert_eq!(plane.saturation, 65535.0);
        // Physical = raw + 32768 (BZERO), no max-normalization.
        assert_eq!(plane.values, vec![32778.0, 32788.0, 32798.0, 32808.0]);
    }

    #[test]
    fn detection_plane_rejects_a_non_rgb_cube() {
        // A cube that isn't a 3-plane RGB frame has no green channel to select
        // and no super-pixel plane to build: still an error.
        let img = ImageData::new(vec![2, 2, 4], PixelData::I16(vec![0; 16]));
        assert!(detection_plane(&Header::default(), &img).is_err());
    }

    #[test]
    fn detection_plane_drops_an_odd_last_column_and_row() {
        // A 2x2 cell is the quantum, so a 3x3 mosaic detects on 1x1.
        let mut header = Header::default();
        header.set(BAYERPAT, HeaderValue::String("RGGB".to_string()), None);
        let img = ImageData::new(vec![3, 3], PixelData::I16((0..9).collect()));
        let plane = detection_plane(&header, &img).unwrap();

        assert_eq!((plane.width, plane.height), (1, 1));
        assert_eq!(plane.values, vec![2.0]); // (1 + 3) / 2
    }

    #[test]
    fn sample_saturation_follows_the_sample_type() {
        // The unsigned-16 convention: signed samples offset by BZERO 32768.
        let mut u16_header = Header::default();
        u16_header.set(BZERO, HeaderValue::Float(32768.0), None);
        let i16_img = ImageData::new(vec![2, 1], PixelData::I16(vec![0; 2]));
        assert_eq!(sample_saturation(&u16_header, &i16_img), 65535.0);

        // Plain signed 16, no scaling.
        assert_eq!(sample_saturation(&Header::default(), &i16_img), 32767.0);

        let u8_img = ImageData::new(vec![2, 1], PixelData::U8(vec![0; 2]));
        assert_eq!(sample_saturation(&Header::default(), &u8_img), 255.0);

        // Float samples have no representable ceiling to clip against, so no
        // star can be flat-topped and the rejection must never fire.
        let f32_img = ImageData::new(vec![2, 1], PixelData::F32(vec![0.0; 2]));
        assert_eq!(
            sample_saturation(&Header::default(), &f32_img),
            f64::INFINITY
        );
    }

    #[test]
    fn sample_saturation_agrees_with_pixel_stats() {
        // Two mechanisms, one truth: `sample_saturation` reads the PixelData
        // variant, `stats_from_counts` reads its value-count array's length.
        // This is the guard against them drifting apart.
        let mut u16_header = Header::default();
        u16_header.set(BZERO, HeaderValue::Float(32768.0), None);
        let cases: [(Header, ImageData); 3] = [
            (
                Header::default(),
                ImageData::new(vec![2, 2], PixelData::I16(vec![1, 2, 3, 4])),
            ),
            (
                u16_header,
                ImageData::new(vec![2, 2], PixelData::I16(vec![1, 2, 3, 4])),
            ),
            (
                Header::default(),
                ImageData::new(vec![2, 2], PixelData::U8(vec![1, 2, 3, 4])),
            ),
        ];
        for (header, img) in &cases {
            assert_eq!(
                sample_saturation(header, img),
                crate::info::pixel_stats(header, img).saturation
            );
        }
    }

    #[test]
    fn detection_plane_of_the_real_mosaic_is_half_resolution() {
        // uncompressed.fit is a 3008x3008 GRBG unsigned-16 mosaic.
        let path = test_data("uncompressed.fit");
        let fits = FitsFile::from_file(&path).unwrap();
        let (header, img) = find_image_hdu(&fits, &path).unwrap();
        let plane = detection_plane(header, img.as_ref()).unwrap();

        assert_eq!((plane.width, plane.height), (1504, 1504));
        assert_eq!(plane.values.len(), 1504 * 1504);
        assert_eq!(plane.saturation, 65535.0);
        assert!(plane.values.iter().all(|v| (0.0..=65535.0).contains(v)));
    }

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
    fn is_debayered_mono_true_only_for_2d_without_bayerpat() {
        let mut header = Header::default();
        let img2d = ImageData::new(vec![4, 3], PixelData::I16(vec![0; 12]));
        assert!(is_debayered_mono(&header, &img2d));

        header.set(BAYERPAT, HeaderValue::String("RGGB".to_string()), None);
        assert!(!is_debayered_mono(&header, &img2d));

        let cube = ImageData::new(vec![4, 3, 3], PixelData::I16(vec![0; 36]));
        assert!(!is_debayered_mono(&Header::default(), &cube));
    }

    #[test]
    fn load_rgb_replicates_mono_channel_across_rgb_with_notice() {
        let header = Header::default();
        let img = ImageData::new(vec![2, 2], PixelData::I16(vec![0, 1, 2, 3]));

        let loaded = load_rgb(&header, &img, None, false).unwrap();

        assert_eq!((loaded.width, loaded.height), (2, 2));
        assert_eq!(loaded.notice, LoadRgbNotice::AlreadyDebayeredMono);
        match loaded.rgb {
            RgbBuffer::U16(v) => {
                assert_eq!(v, vec![0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3]);
            }
            RgbBuffer::U8(_) => panic!("expected a u16 rgb buffer"),
        }
    }

    #[test]
    fn load_rgb_force_demosaic_rejects_mono_without_pattern() {
        let header = Header::default();
        let img = ImageData::new(vec![2, 2], PixelData::I16(vec![0, 1, 2, 3]));

        let err = match load_rgb(&header, &img, None, true) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(err.to_string().contains("Bayer pattern"));
    }

    #[test]
    fn load_mono_raw_replicates_raw_samples_without_demosaicing() {
        // Even with a BAYERPAT header present (a genuine raw mosaic),
        // `load_mono_raw` must skip demosaicing and just replicate the raw
        // per-pixel value across channels, unlike `load_rgb`.
        let mut header = Header::default();
        header.set(BAYERPAT, HeaderValue::String("RGGB".to_string()), None);
        let img = ImageData::new(vec![2, 2], PixelData::I16(vec![0, 1, 2, 3]));

        let (width, height, rgb) = load_mono_raw(&header, &img).unwrap();

        assert_eq!((width, height), (2, 2));
        match rgb {
            RgbBuffer::U16(v) => {
                assert_eq!(v, vec![0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3]);
            }
            RgbBuffer::U8(_) => panic!("expected a u16 rgb buffer"),
        }
    }

    #[test]
    fn load_mono_raw_rejects_non_2d_image() {
        let header = Header::default();
        let cube = ImageData::new(vec![2, 2, 3], PixelData::I16(vec![0; 12]));

        let err = match load_mono_raw(&header, &cube) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(err.to_string().contains("2D"));
    }

    #[test]
    fn rgb16_to_rgb8_takes_high_byte() {
        assert_eq!(high_byte(0xFF00), 255);
        assert_eq!(high_byte(0x0100), 1);
        assert_eq!(rgb16_to_rgb8(&[0xFF00, 0x8000, 0x0100]), vec![255, 128, 1]);
    }

    #[test]
    fn is_indexed_matches_prefix_plus_digits_only() {
        assert!(is_indexed("NAXIS3", "NAXIS"));
        assert!(is_indexed("TFORM12", "TFORM"));
        assert!(is_indexed("ZNAXIS2", "ZNAXIS"));
        // bare prefix, non-digit suffix, and unrelated names do not match
        assert!(!is_indexed("NAXIS", "NAXIS"));
        assert!(!is_indexed("TFORMAT", "TFORM"));
        assert!(!is_indexed("OBJECT", "NAXIS"));
    }

    #[test]
    fn is_reserved_keyword_covers_structural_table_and_compression() {
        for kw in [
            "SIMPLE", "BITPIX", "NAXIS", "NAXIS1", "NAXIS3", "BSCALE", "BZERO", "BLANK", "DATAMIN",
            "CHECKSUM", "TFIELDS", "TFORM1", "TTYPE3", "EXTNAME", "ZIMAGE", "ZNAXIS", "ZNAXIS2",
            "ZTILE1", "ZVAL1", "END",
        ] {
            assert!(is_reserved_keyword(kw), "{kw} should be reserved");
        }
        for kw in [
            "OBJECT", "DATE-OBS", "CRVAL1", "BAYERPAT", "GAIN", "COMMENT", "HISTORY",
        ] {
            assert!(!is_reserved_keyword(kw), "{kw} should not be reserved");
        }
    }

    #[test]
    fn copy_metadata_keeps_metadata_and_drops_reserved_and_extra() {
        let mut src = Header::default();
        // Structural/table/compression keywords that must be dropped.
        src.set("SIMPLE", HeaderValue::Logical(true), None);
        src.set("BITPIX", HeaderValue::Integer(16), None);
        src.set("NAXIS1", HeaderValue::Integer(8), None);
        src.set("BSCALE", HeaderValue::Float(1.0), None);
        src.set("BZERO", HeaderValue::Float(32768.0), None);
        src.set("TFORM1", HeaderValue::String("1PB".to_string()), None);
        src.set("ZIMAGE", HeaderValue::Logical(true), None);
        src.set("ZNAXIS2", HeaderValue::Integer(8), None);
        src.set(
            "EXTNAME",
            HeaderValue::String("COMPRESSED_IMAGE".to_string()),
            None,
        );
        // Metadata that must survive.
        src.set("OBJECT", HeaderValue::String("M31".to_string()), None);
        src.set("CRVAL1", HeaderValue::Float(10.68), None);
        src.set(BAYERPAT, HeaderValue::String("RGGB".to_string()), None);
        src.push(Keyword::commentary("COMMENT", "hi"));

        let mut dest = Header::default();
        copy_metadata(&mut dest, &src, CFA_KEYWORDS);

        assert_eq!(dest.get_string("OBJECT"), Some("M31"));
        assert_eq!(dest.get_float("CRVAL1"), Some(10.68));
        assert!(dest.iter().any(|k| k.name == "COMMENT"));
        // CFA + reserved keywords are gone.
        assert!(dest.find(BAYERPAT).is_none());
        for kw in [
            "SIMPLE", "BITPIX", "NAXIS1", "BSCALE", "BZERO", "TFORM1", "ZIMAGE", "ZNAXIS2",
            "EXTNAME",
        ] {
            assert!(dest.find(kw).is_none(), "{kw} leaked into output");
        }
    }
}
