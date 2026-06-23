//! FITS image helpers shared by the `debayer` and `split` commands: locating
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
pub(crate) const BAYERPAT: &str = "BAYERPAT";
pub(crate) const BSCALE: &str = "BSCALE";
pub(crate) const BZERO: &str = "BZERO";

/// CFA-mosaic keywords that become meaningless once an image is debayered into
/// an RGB image. Dropped by the image commands (debayer/stretch/split) when
/// copying the source header, but not by decompress, which round-trips the
/// mosaic faithfully. `load_rgb` also relies on the absence of `BAYERPAT` to
/// detect an already-debayered 3-plane cube, so leaving it would break
/// re-processing the output.
pub(crate) const CFA_KEYWORDS: &[&str] =
    &["BAYERPAT", "XBAYROFF", "YBAYROFF", "BAYOFFX", "BAYOFFY"];

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
            "{}: already debayered — skipping debayer step",
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
/// round-trip. Metadata from `src_header` (minus `drop` and structural keywords)
/// is copied onto the output, and `history`, when present, is recorded as a
/// HISTORY card.
pub(crate) fn write_rgb16_fits(
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
pub(crate) fn write_pixel_fits(
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
pub(crate) fn copy_metadata(dest: &mut Header, src: &Header, extra_drop: &[&str]) {
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

/// Append a HISTORY provenance card to `dest`.
pub(crate) fn add_history(dest: &mut Header, text: &str) {
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

pub(crate) fn get_bayerpat(header: &Header) -> Option<&str> {
    header.get_string(BAYERPAT)
}

/// Serializes overwrite prompts so parallel batch runs don't interleave their
/// questions and answers on the shared terminal.
static PROMPT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Ensure `output` may be written. If it already exists and the user didn't
/// pass `--yes`, ask whether to overwrite it (when running interactively) and
/// bail if the answer is no.
pub(crate) fn ensure_can_write(output: &Path, assume_yes: bool) -> Result<()> {
    if !output.exists() || assume_yes {
        return Ok(());
    }
    if confirm_overwrite(output)? {
        Ok(())
    } else {
        bail!("{} already exists — skipped", output.display());
    }
}

/// Prompt on the terminal whether to overwrite an existing `output`. When stdin
/// isn't a terminal there's no one to ask, so refuse and point at `--yes`
/// (matching the old non-interactive guard).
fn confirm_overwrite(output: &Path) -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        bail!("{} already exists — use -y to overwrite", output.display());
    }

    // Hold the lock across the whole prompt/answer exchange.
    let _guard = PROMPT_LOCK.lock().unwrap();
    print!("{} already exists — overwrite? [y/N] ", output.display());
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES"))
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
