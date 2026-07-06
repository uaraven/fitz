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
    fits.hdus
        .iter()
        .position(|hdu| matches!(hdu.data, HduData::Image(_)) || hdu.as_compressed_image().is_some())
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
        bail!(
            "expected a 2D mosaic image, found {} axes",
            img.axes.len()
        );
    }

    let cfa =
        resolve_cfa(header, pattern).with_context(|| "cannot determine Bayer pattern".to_string())?;

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
    let u16_vals = scale_physical_to_u16(&img.pixels, &scaled);

    let mut out = vec![0u16; plane_len * 3];
    out.par_chunks_mut(3).enumerate().for_each(|(i, px)| {
        px[0] = u16_vals[i];
        px[1] = u16_vals[plane_len + i];
        px[2] = u16_vals[2 * plane_len + i];
    });
    RgbBuffer::U16(out)
}

/// Replicate a single-plane (monochrome) image's samples across all three
/// channels, producing the same interleaved `RgbBuffer` shape [`rgb_from_cube`]
/// builds from a 3-plane cube.
fn rgb_from_mono(header: &Header, img: &ImageData, width: usize, height: usize) -> RgbBuffer {
    let plane_len = width * height;

    if let PixelData::U8(v) = &img.pixels {
        let mut out = vec![0u8; plane_len * 3];
        out.par_chunks_mut(3).enumerate().for_each(|(i, px)| {
            px[0] = v[i];
            px[1] = v[i];
            px[2] = v[i];
        });
        return RgbBuffer::U8(out);
    }

    let scaled = scaled_pixels(header, img);
    let u16_vals = scale_physical_to_u16(&img.pixels, &scaled);

    let mut out = vec![0u16; plane_len * 3];
    out.par_chunks_mut(3).enumerate().for_each(|(i, px)| {
        let v = u16_vals[i];
        px[0] = v;
        px[1] = v;
        px[2] = v;
    });
    RgbBuffer::U16(out)
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
pub fn write_rgb16_tiff(output: &Path, width: usize, height: usize, interleaved: &[u16]) -> Result<()> {
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
    let pixels = PixelData::I16(planes.iter().map(|&p| (p as i32 - 32768) as i16).collect());
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

    fits.to_file(output)
        .with_context(|| format!("cannot write {}", output.display()))?;

    Ok(())
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
        if is_reserved_keyword(&kw.name) {
            continue;
        }
        if extra_drop.iter().any(|d| d.eq_ignore_ascii_case(&kw.name)) {
            continue;
        }
        dest.push(kw.clone());
    }
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
        if is_reserved_keyword(&kw.name) {
            continue;
        }
        if extra_drop.iter().any(|d| d.eq_ignore_ascii_case(&kw.name)) {
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
