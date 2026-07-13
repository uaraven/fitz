//! Build a display-ready preview of a FITS image from already-loaded pixel
//! data, honoring independent "debayer" and "stretch" toggles. This is the
//! pixel pipeline shared by the CLI's terminal `preview` and a GUI frontend:
//! both need to turn a raw mosaic / RGB cube / mono frame into an image, they
//! only differ in how they finally paint it (ANSI/kitty vs. an on-screen
//! surface).
//!
//! Two entry points:
//!
//! - [`preview_rgb`] resolves the *debayer* toggle into an un-stretched
//!   [`RgbBuffer`] (demosaic a raw mosaic, reinterleave an already-debayered
//!   cube, or show a raw mosaic as grayscale). The CLI uses this and keeps
//!   working on the 16-bit buffer so its terminal output is byte-for-byte
//!   unchanged.
//! - [`render_preview`] additionally resolves the *stretch* toggle and widens
//!   the result to an interleaved **RGBA8** buffer, the form a GUI toolkit
//!   (e.g. Slint's `Image::from_rgba8`) consumes directly.

use anyhow::Result;
use bayer::CFA;
use fitskit::{Header, ImageData};
use rayon::prelude::*;

use crate::fits_image::{
    LoadRgbNotice, RgbBuffer, high_byte, is_debayered_mono, is_debayered_rgb_cube, load_mono_raw,
    load_rgb,
};
use crate::stretch::{DEFAULT_BRIGHTNESS, auto_stretch};

/// How a preview's RGB buffer was produced, so a caller can surface it (the CLI
/// prints a note/warning, a GUI might show a badge). Mirrors
/// [`LoadRgbNotice`] but adds [`PreviewSource::RawMono`], the case unique to a
/// preview: the debayer toggle is off, so a raw mosaic is shown as grayscale
/// without color interpolation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PreviewSource {
    /// A raw Bayer mosaic was demosaiced into color.
    Demosaiced,
    /// A 3-plane image with no `BAYERPAT` header, reinterleaved as-is.
    AlreadyDebayeredRgbCube,
    /// A single-plane image with no `BAYERPAT` header, replicated across RGB.
    AlreadyDebayeredMono,
    /// Debayer disabled on a raw mosaic: raw sensor values shown as grayscale.
    RawMono,
}

impl From<LoadRgbNotice> for PreviewSource {
    fn from(n: LoadRgbNotice) -> Self {
        match n {
            LoadRgbNotice::Demosaiced => PreviewSource::Demosaiced,
            LoadRgbNotice::AlreadyDebayeredRgbCube => PreviewSource::AlreadyDebayeredRgbCube,
            LoadRgbNotice::AlreadyDebayeredMono => PreviewSource::AlreadyDebayeredMono,
        }
    }
}

/// An un-stretched preview RGB buffer plus its dimensions and [`PreviewSource`].
pub struct PreviewRgb {
    pub width: usize,
    pub height: usize,
    pub rgb: RgbBuffer,
    pub source: PreviewSource,
}

/// Resolve the *debayer* toggle into an un-stretched [`RgbBuffer`].
///
/// With `debayer` on, a raw mosaic is demosaiced and an already-debayered image
/// is reinterleaved as-is (via [`load_rgb`]). With `debayer` off, a raw mosaic
/// is instead shown as grayscale from its raw sensor values ([`load_mono_raw`]);
/// an already-debayered image has nothing to skip, so it falls back to
/// [`load_rgb`] (the caller can notice this from [`PreviewSource`] and warn that
/// the toggle had no effect).
pub fn preview_rgb(
    header: &Header,
    img: &ImageData,
    debayer: bool,
    pattern: Option<CFA>,
    force_demosaic: bool,
) -> Result<PreviewRgb> {
    // Debayer on, or an already-debayered image (which has nothing to skip):
    // let load_rgb do the right thing and report how it did it.
    if debayer || is_debayered_rgb_cube(header, img) || is_debayered_mono(header, img) {
        let loaded = load_rgb(header, img, pattern, force_demosaic)?;
        return Ok(PreviewRgb {
            width: loaded.width,
            height: loaded.height,
            rgb: loaded.rgb,
            source: loaded.notice.into(),
        });
    }

    // Debayer off on a genuine raw mosaic: grayscale, no color interpolation.
    let (width, height, rgb) = load_mono_raw(header, img)?;
    Ok(PreviewRgb {
        width,
        height,
        rgb,
        source: PreviewSource::RawMono,
    })
}

/// Options controlling how an in-memory image is rendered to a display buffer.
pub struct PreviewParams {
    /// Demosaic a raw mosaic into color; when false a raw mosaic is shown as
    /// grayscale (see [`preview_rgb`]).
    pub debayer: bool,
    /// Apply the MTF/STF auto-stretch. When false the linear pixel values are
    /// shown as-is (a raw astronomical frame will look nearly black).
    pub stretch: bool,
    /// Bayer pattern override; takes precedence over the FITS `BAYERPAT` header.
    pub pattern: Option<CFA>,
    /// Always demosaic, even if the image looks already debayered.
    pub force_demosaic: bool,
    /// Auto-stretch target background level in `(0, 1)`; higher is brighter.
    pub brightness: f32,
    /// Stretch all channels together instead of independently.
    pub linked: bool,
}

impl Default for PreviewParams {
    fn default() -> Self {
        PreviewParams {
            debayer: true,
            stretch: true,
            pattern: None,
            force_demosaic: false,
            brightness: DEFAULT_BRIGHTNESS,
            linked: false,
        }
    }
}

/// A display-ready preview: an interleaved RGBA8 buffer and its dimensions,
/// plus how it was produced.
pub struct PreviewImage {
    pub width: usize,
    pub height: usize,
    /// Interleaved (R, G, B, A) bytes, `width * height * 4` long, alpha 255.
    pub rgba8: Vec<u8>,
    pub source: PreviewSource,
}

/// Render an in-memory image to an RGBA8 display buffer, honoring the debayer
/// and stretch toggles in `p`. No I/O — the caller has already loaded the
/// `(header, img)` (see [`crate::fits_image::find_image_hdu`]).
pub fn render_preview(header: &Header, img: &ImageData, p: &PreviewParams) -> Result<PreviewImage> {
    let pr = preview_rgb(header, img, p.debayer, p.pattern, p.force_demosaic)?;

    let rgba8 = if p.stretch {
        // A raw-mono preview is grayscale (all channels equal), so linked vs.
        // unlinked is moot; honor the caller's choice for color images.
        let stretched = auto_stretch(&pr.rgb, p.linked, p.brightness);
        rgb16_to_rgba8(&stretched)
    } else {
        rgb_buffer_to_rgba8(&pr.rgb)
    };

    Ok(PreviewImage {
        width: pr.width,
        height: pr.height,
        rgba8,
        source: pr.source,
    })
}

/// Widen an interleaved 16-bit RGB buffer to RGBA8, keeping each sample's high
/// byte (the same `>> 8` convention as the rest of the display path) and a
/// fully opaque alpha.
fn rgb16_to_rgba8(rgb: &[u16]) -> Vec<u8> {
    rgb.par_chunks_exact(3)
        .flat_map_iter(|px| [high_byte(px[0]), high_byte(px[1]), high_byte(px[2]), 255])
        .collect()
}

/// Widen an interleaved [`RgbBuffer`] to RGBA8 without stretching: 8-bit samples
/// pass through, 16-bit samples keep their high byte, alpha is 255.
fn rgb_buffer_to_rgba8(rgb: &RgbBuffer) -> Vec<u8> {
    match rgb {
        RgbBuffer::U8(v) => v
            .par_chunks_exact(3)
            .flat_map_iter(|px| [px[0], px[1], px[2], 255])
            .collect(),
        RgbBuffer::U16(v) => rgb16_to_rgba8(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fits_image::find_image_hdu;
    use crate::test_support::{write_mono_f32_fits, write_mosaic_fits, write_rgb_cube_fits};
    use fitskit::FitsFile;
    use tempfile::TempDir;

    /// Load a fixture and hand its `(header, img)` to `f`, mirroring how a
    /// caller uses `find_image_hdu` before the in-memory preview functions.
    fn with_image<T>(path: &std::path::Path, f: impl FnOnce(&Header, &ImageData) -> T) -> T {
        let fits = FitsFile::from_file(path).unwrap();
        let (header, img) = find_image_hdu(&fits, path).unwrap();
        f(header, img.as_ref())
    }

    #[test]
    fn render_debayer_and_stretch_produces_opaque_rgba() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("mosaic.fit");
        write_mosaic_fits(&path, 8, 8, Some("RGGB"));

        let img = with_image(&path, |h, i| {
            render_preview(h, i, &PreviewParams::default()).unwrap()
        });

        assert_eq!(img.source, PreviewSource::Demosaiced);
        assert_eq!((img.width, img.height), (8, 8));
        // RGBA: four bytes per pixel, every alpha byte fully opaque.
        assert_eq!(img.rgba8.len(), 8 * 8 * 4);
        assert!(img.rgba8.chunks_exact(4).all(|px| px[3] == 255));
    }

    #[test]
    fn debayer_off_shows_raw_mosaic_as_gray() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("mosaic.fit");
        write_mosaic_fits(&path, 6, 4, Some("RGGB"));

        let img = with_image(&path, |h, i| {
            let p = PreviewParams {
                debayer: false,
                ..PreviewParams::default()
            };
            render_preview(h, i, &p).unwrap()
        });

        assert_eq!(img.source, PreviewSource::RawMono);
        assert_eq!(img.rgba8.len(), 6 * 4 * 4);
        // Grayscale: R == G == B for every pixel.
        assert!(
            img.rgba8
                .chunks_exact(4)
                .all(|px| px[0] == px[1] && px[1] == px[2])
        );
    }

    #[test]
    fn already_debayered_cube_is_detected_regardless_of_toggle() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("cube.fit");
        write_rgb_cube_fits(&path, 5, 3);

        for debayer in [true, false] {
            let pr = with_image(&path, |h, i| {
                preview_rgb(h, i, debayer, None, false).unwrap()
            });
            assert_eq!(pr.source, PreviewSource::AlreadyDebayeredRgbCube);
            assert_eq!((pr.width, pr.height), (5, 3));
        }
    }

    #[test]
    fn no_stretch_differs_from_stretch() {
        // A near-linear frame rendered without a stretch stays dark; with the
        // auto-stretch it brightens. The two buffers must therefore differ.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("mono.fit");
        write_mono_f32_fits(&path, 16, 16);

        let (plain, stretched) = with_image(&path, |h, i| {
            let plain = render_preview(
                h,
                i,
                &PreviewParams {
                    debayer: false,
                    stretch: false,
                    ..PreviewParams::default()
                },
            )
            .unwrap();
            let stretched = render_preview(
                h,
                i,
                &PreviewParams {
                    debayer: false,
                    stretch: true,
                    ..PreviewParams::default()
                },
            )
            .unwrap();
            (plain.rgba8, stretched.rgba8)
        });

        assert_eq!(plain.len(), stretched.len());
        assert_ne!(plain, stretched, "stretch toggle must change the pixels");
    }

    #[test]
    fn already_debayered_mono_replicates_channels() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("mono.fit");
        // A 2D image with no BAYERPAT is treated as already-debayered mono.
        write_mosaic_fits(&path, 4, 4, None);

        let img = with_image(&path, |h, i| {
            render_preview(h, i, &PreviewParams::default()).unwrap()
        });

        assert_eq!(img.source, PreviewSource::AlreadyDebayeredMono);
        assert!(
            img.rgba8
                .chunks_exact(4)
                .all(|px| px[0] == px[1] && px[1] == px[2])
        );
    }
}
