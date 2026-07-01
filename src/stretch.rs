//! The `stretch` command: load a FITS image (debayering it first if needed),
//! apply an MTF/STF auto-stretch, and save the 16-bit result as FITS or TIFF.

use std::path::Path;

use anyhow::{Context, Result};
use bayer::CFA;
use fitskit::{FitsFile, Header};
use rayon::prelude::*;

use crate::debayer::OutputFormat;
use crate::fits_image::{
    CFA_KEYWORDS, RgbBuffer, ensure_can_write, find_image_hdu, load_rgb, print_progress,
    print_step, round_to_u16, write_rgb16_fits, write_rgb16_tiff,
};
use crate::options::StretchOptions;

/// Working sample type for the stretch math. `f32` is more than precise enough
/// for a 16-bit result and halves the normalized-image buffer versus `f64`.
type Sample = f32;

/// Shadows clipping point, in units of (normalized) MAD below the median.
const SHADOWS_CLIP: Sample = -2.8;
/// Target mean background the stretched median is pulled towards.
const TARGET_BG: Sample = 0.25;
/// Scale factor turning the median absolute deviation into a robust estimate of
/// the standard deviation for a normal distribution.
const MAD_NORM: Sample = 1.4826;

const OUT_MAX: Sample = u16::MAX as Sample;

pub fn stretch_file(input: &Path, output: &Path, opts: &StretchOptions) -> Result<()> {
    ensure_can_write(output, opts.yes)?;
    print_progress(opts.verbose, input, output);

    let (width, height, stretched, header) = load_and_stretch(
        input,
        opts.pattern,
        opts.force_demosaic,
        opts.linked,
        opts.verbose,
    )?;

    print_step(opts.verbose, "writing");
    match opts.format {
        OutputFormat::Tiff => write_rgb16_tiff(output, width, height, &stretched),
        OutputFormat::Fits => {
            let history = format!("stretched by fitz {}", env!("CARGO_PKG_VERSION"));
            write_rgb16_fits(
                output,
                width,
                height,
                &stretched,
                Some(&header),
                CFA_KEYWORDS,
                Some(&history),
            )
        }
    }
}

/// Load a FITS image (debayering if needed) and apply the auto-stretch, returning
/// the interleaved 16-bit result and its `(width, height)`. Shared by the
/// `stretch` and `preview` commands, which differ only in what they do with the
/// stretched buffer (write it to a file vs. render it to the terminal).
pub(crate) fn load_and_stretch(
    input: &Path,
    pattern: Option<CFA>,
    force_demosaic: bool,
    linked: bool,
    verbose: bool,
) -> Result<(usize, usize, Vec<u16>, Header)> {
    print_step(verbose, "reading");
    let fits =
        FitsFile::from_file(input).with_context(|| format!("cannot read {}", input.display()))?;

    let (header, img) = find_image_hdu(&fits, input, verbose)?;
    let img = img.as_ref();

    let (width, height, rgb) = load_rgb(header, img, input, pattern, force_demosaic, verbose)?;

    print_step(verbose, "stretching");
    let stretched = auto_stretch(&rgb, linked);
    Ok((width, height, stretched, header.clone()))
}

/// Apply an MTF/STF auto-stretch to an interleaved RGB image, returning
/// interleaved 16-bit samples in `[0, 65535]`. With `linked`, one set of stretch
/// parameters (derived from all channels together) is applied to every channel;
/// otherwise each channel is stretched from its own statistics, which also acts
/// as an automatic background neutralization.
///
/// This is a pure, in-memory transform so callers can do something other than
/// write the result to a file.
pub fn auto_stretch(rgb: &RgbBuffer, linked: bool) -> Vec<u16> {
    let mut samples = to_normalized(rgb);

    // Linked mode derives one stretch from all samples; otherwise the
    // interleaved R,G,B channels are each stretched from their own statistics.
    // The transfer of one channel never reads another, so every channel's
    // params are derived from the original normalized samples — which lets us
    // compute the three independent channels' params in parallel and then apply
    // the transfer in a single parallel pass. The math (and thus the output) is
    // identical to processing the channels one after another.
    if linked {
        let (shadows, midtones) = find_params(&mut samples.clone());
        samples
            .par_iter_mut()
            .for_each(|v| *v = transfer(*v, shadows, midtones));
    } else {
        let params: Vec<(Sample, Sample)> = (0..3usize)
            .into_par_iter()
            .map(|start| {
                let mut chan: Vec<Sample> =
                    samples.iter().skip(start).step_by(3).copied().collect();
                find_params(&mut chan)
            })
            .collect();
        samples.par_chunks_mut(3).for_each(|px| {
            for (c, v) in px.iter_mut().enumerate() {
                let (shadows, midtones) = params[c];
                *v = transfer(*v, shadows, midtones);
            }
        });
    }

    samples
        .par_iter()
        .map(|&v| round_to_u16((v * OUT_MAX) as f64))
        .collect()
}

/// Normalize the interleaved samples to `[0, 1]` based on the source bit depth.
fn to_normalized(rgb: &RgbBuffer) -> Vec<Sample> {
    match rgb {
        RgbBuffer::U8(v) => v
            .par_iter()
            .map(|&x| x as Sample / u8::MAX as Sample)
            .collect(),
        RgbBuffer::U16(v) => v
            .par_iter()
            .map(|&x| x as Sample / u16::MAX as Sample)
            .collect(),
    }
}

/// Derive the `(shadows, midtones)` STF parameters from a set of normalized
/// samples. `samples` is consumed as scratch: it's reordered by the median
/// selection and then overwritten in place with absolute deviations.
fn find_params(samples: &mut [Sample]) -> (Sample, Sample) {
    let med = median(samples);

    for v in samples.iter_mut() {
        *v = (*v - med).abs();
    }
    let mad = median(samples) * MAD_NORM;

    let shadows = (med + SHADOWS_CLIP * mad).clamp(0.0, 1.0);
    // Keep the midtone strictly inside (0, 1) as `mtf` requires: degenerate
    // inputs (a near-constant image, or one with a very large spread) can push
    // `med - shadows` to 0 or >= 1, where `mtf` would otherwise return exactly
    // 0 or 1 and collapse the whole stretch to solid white or black.
    let midtones = mtf(TARGET_BG, med - shadows).clamp(Sample::EPSILON, 1.0 - Sample::EPSILON);

    (shadows, midtones)
}

/// Rescale a sample against the shadows clip, then apply the midtones transfer.
fn transfer(v: Sample, shadows: Sample, midtones: Sample) -> Sample {
    let denom = 1.0 - shadows;
    let rescaled = if denom > 0.0 {
        ((v - shadows) / denom).clamp(0.0, 1.0)
    } else {
        0.0
    };
    mtf(midtones, rescaled)
}

/// The midtones transfer function: a monotonic curve on `[0, 1]` with
/// `mtf(m, 0) = 0`, `mtf(m, 1) = 1`, and `mtf(m, m) = 0.5`. The midtone `m` is
/// expected to lie in `(0, 1)`, which is guaranteed for the values
/// [`find_params`] derives.
fn mtf(m: Sample, x: Sample) -> Sample {
    if x <= 0.0 {
        0.0
    } else if x >= 1.0 {
        1.0
    } else if (x - m).abs() < Sample::EPSILON {
        0.5
    } else {
        ((m - 1.0) * x) / ((2.0 * m - 1.0) * x - m)
    }
}

/// The median of `values`, selecting in place. For an even count this averages
/// the two central elements. Returns 0.0 for an empty slice.
fn median(values: &mut [Sample]) -> Sample {
    let n = values.len();
    if n == 0 {
        return 0.0;
    }

    let mid = n / 2;
    let hi = *select_nth(values, mid);
    if n % 2 == 1 {
        hi
    } else {
        let lo = *select_nth(values, mid - 1);
        (lo + hi) / 2.0
    }
}

/// Partition `values` so the element at `k` is the one that belongs there in
/// sorted order, returning a reference to it (a total order is fine: samples are
/// always finite).
fn select_nth(values: &mut [Sample], k: usize) -> &Sample {
    let (_, nth, _) = values.select_nth_unstable_by(k, |a, b| a.total_cmp(b));
    nth
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        output_header, test_data, write_mosaic_fits, write_mosaic_fits_with_metadata,
        write_rgb_cube_fits,
    };
    use fitskit::HduData;
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    #[test]
    fn stretch_fits_preserves_metadata_and_drops_bayerpat() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits_with_metadata(&input, 8, 6, Some("RGGB"));

        let output = tmp.path().join("out.fits");
        stretch_file(&input, &output, &StretchOptions::default()).unwrap();

        let header = output_header(&output);
        assert_eq!(header.get_string("OBJECT"), Some("M31"));
        assert_eq!(header.get_float("CRVAL1"), Some(10.68));
        assert!(header.find("BAYERPAT").is_none());
        assert!(header.iter().any(|k| {
            k.name == "HISTORY"
                && k.comment
                    .as_deref()
                    .is_some_and(|c| c.contains("stretched by fitz"))
        }));
    }

    #[test]
    fn stretch_fz_output_does_not_leak_container_keywords() {
        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("out.fits");
        stretch_file(
            &test_data("compressed.fits.fz"),
            &output,
            &StretchOptions::default(),
        )
        .unwrap();

        let header = output_header(&output);
        for kw in [
            "TFORM1", "TFIELDS", "ZIMAGE", "ZCMPTYPE", "ZNAXIS1", "XTENSION", "EXTNAME", "BAYERPAT",
        ] {
            assert!(header.find(kw).is_none(), "{kw} leaked into stretch output");
        }
    }

    #[test]
    fn mtf_hits_its_anchor_points() {
        let m = 0.25;
        assert_eq!(mtf(m, 0.0), 0.0);
        assert_eq!(mtf(m, 1.0), 1.0);
        assert!((mtf(m, m) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn mtf_is_monotonic() {
        let m = 0.2;
        let mut prev = mtf(m, 0.0);
        for i in 1..=100 {
            let cur = mtf(m, i as Sample / 100.0);
            assert!(cur >= prev, "mtf decreased at {i}: {cur} < {prev}");
            prev = cur;
        }
    }

    #[test]
    fn median_of_even_and_odd_counts() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    /// Build a gradient mosaic, debayer + stretch it, and return the
    /// interleaved 16-bit output along with its dimensions.
    fn stretched_gradient(width: usize, height: usize, linked: bool) -> Vec<u16> {
        let max = (width * height) as Sample;
        let samples: Vec<u16> = (0..width * height * 3)
            .map(|i| ((i % (width * height)) as Sample / max * OUT_MAX) as u16)
            .collect();
        auto_stretch(&RgbBuffer::U16(samples), linked)
    }

    #[test]
    fn output_stays_within_16_bit_range() {
        // Range is guaranteed by the u16 type; assert the stretch actually
        // spreads values out (not all clamped to one end).
        let out = stretched_gradient(16, 16, false);
        assert!(out.iter().any(|&v| v > 0));
        assert!(out.iter().any(|&v| v < u16::MAX));
    }

    #[test]
    fn high_spread_image_does_not_collapse_to_black() {
        // Half the pixels black, half white: the spread is so large that
        // `med - shadows` exceeds 1, where an unclamped midtone would drive the
        // whole stretch to solid black. The clamp in `find_params` must keep
        // both extremes present in the output.
        let n = 1024usize;
        let samples: Vec<u16> = (0..n)
            .flat_map(|i| {
                let v = if i % 2 == 0 { 0 } else { u16::MAX };
                [v, v, v]
            })
            .collect();
        let out = auto_stretch(&RgbBuffer::U16(samples), false);
        assert!(out.iter().any(|&v| v > 0), "output collapsed to all black");
        assert!(
            out.iter().any(|&v| v < u16::MAX),
            "output collapsed to all white"
        );
    }

    #[test]
    fn constant_image_does_not_panic_or_collapse() {
        // A perfectly flat image has zero MAD, so `med == shadows`; the clamp
        // keeps `mtf` away from its degenerate `m = 0` (solid white) case.
        let n = 256usize;
        let samples: Vec<u16> = (0..n).flat_map(|_| [20000u16, 20000, 20000]).collect();
        let out = auto_stretch(&RgbBuffer::U16(samples), false);
        assert!(
            out.iter().any(|&v| v < u16::MAX),
            "flat image went all white"
        );
    }

    #[test]
    fn stretch_preserves_intra_channel_ordering() {
        // A single ascending channel must stay non-decreasing after stretch:
        // both the shadow rescale and the MTF are monotonic.
        let n = 256usize;
        let samples: Vec<u16> = (0..n)
            .flat_map(|i| {
                let v = (i as Sample / n as Sample * OUT_MAX) as u16;
                [v, v, v]
            })
            .collect();
        let out = auto_stretch(&RgbBuffer::U16(samples), false);
        let reds: Vec<u16> = out.iter().step_by(3).copied().collect();
        assert!(reds.windows(2).all(|w| w[1] >= w[0]));
    }

    #[test]
    fn stretch_pulls_median_towards_target_background() {
        // A faint image (low median, small spread) should have its median pulled
        // up close to the target background of ~0.25 * 65535.
        let n = 4096usize;
        let samples: Vec<u16> = (0..n)
            .flat_map(|i| {
                // values clustered near the low end: 100..356
                let v = 100 + (i % 256) as u16;
                [v, v, v]
            })
            .collect();
        let mut out: Vec<Sample> = auto_stretch(&RgbBuffer::U16(samples), false)
            .iter()
            .step_by(3)
            .map(|&v| v as Sample)
            .collect();
        let med = median(&mut out);
        let target = TARGET_BG * OUT_MAX;
        assert!(
            (med - target).abs() < 0.05 * OUT_MAX,
            "median {med} not near target {target}"
        );
    }

    #[test]
    fn linked_and_per_channel_differ_on_imbalanced_color() {
        // Strong red, weak blue: per-channel neutralizes the balance while
        // linked preserves it, so the two outputs must differ.
        let n = 32usize;
        let samples: Vec<u16> = (0..n)
            .flat_map(|i| {
                let r = 40000 + (i % 100) as u16;
                let g = 8000 + (i % 100) as u16;
                let b = 1000 + (i % 100) as u16;
                [r, g, b]
            })
            .collect();
        let buf = RgbBuffer::U16(samples);
        let per_channel = auto_stretch(&buf, false);
        let linked = auto_stretch(&buf, true);
        assert_ne!(per_channel, linked);
    }

    #[test]
    fn stretch_mosaic_produces_three_plane_fits() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 8, 6, Some("RGGB"));

        let output = tmp.path().join("out.fits");
        stretch_file(&input, &output, &StretchOptions::default()).unwrap();

        let fits = FitsFile::from_file(&output).unwrap();
        match &fits.primary().data {
            HduData::Image(img) => assert_eq!(img.axes, vec![8, 6, 3]),
            _ => panic!("expected image data"),
        }
    }

    #[test]
    fn stretch_already_debayered_cube_produces_tiff() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 4, 3);

        let output = tmp.path().join("out.tiff");
        let opts = StretchOptions {
            format: OutputFormat::Tiff,
            ..StretchOptions::default()
        };
        stretch_file(&input, &output, &opts).unwrap();

        let data = std::fs::read(&output).unwrap();
        assert!(data.starts_with(b"II") || data.starts_with(b"MM"));
    }

    #[test]
    fn stretch_default_format_is_fits() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 4, 4, Some("RGGB"));

        let output = tmp.path().join("out.fits");
        stretch_file(&input, &output, &StretchOptions::default()).unwrap();

        let fits = FitsFile::from_file(&output).unwrap();
        assert!(matches!(fits.primary().data, HduData::Image(_)));
    }

    #[test]
    fn stretch_yes_overwrites_existing_output() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 4, 4, Some("RGGB"));

        let output = tmp.path().join("out.fits");
        std::fs::write(&output, b"dummy").unwrap();

        let opts = StretchOptions {
            yes: true,
            ..StretchOptions::default()
        };
        stretch_file(&input, &output, &opts).unwrap();
        // A real FITS file is far bigger than the 5-byte dummy.
        assert!(output.metadata().unwrap().len() > 5);
    }

    #[test]
    fn stretch_errors_without_bayer_pattern() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 4, 4, None);

        let output = tmp.path().join("out.fits");
        let err = stretch_file(&input, &output, &StretchOptions::default()).unwrap_err();
        assert!(err.to_string().contains("Bayer pattern"));
    }

    #[test]
    fn stretch_handles_tile_compressed_input() {
        // A compressed .fz input must be decompressed and stretched into a
        // 3-plane cube just like its uncompressed equivalent.
        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("out.fits");
        stretch_file(
            &test_data("compressed.fits.fz"),
            &output,
            &StretchOptions::default(),
        )
        .unwrap();

        let fits = FitsFile::from_file(&output).unwrap();
        match &fits.primary().data {
            HduData::Image(img) => assert_eq!(img.axes, vec![3008, 3008, 3]),
            _ => panic!("expected image data"),
        }
    }

    fn assert_stretch_matches_known_hash(format: OutputFormat, hash_file: &str) {
        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("out");

        let opts = StretchOptions {
            format,
            ..StretchOptions::default()
        };
        stretch_file(&test_data("uncompressed.fit"), &output, &opts).unwrap();

        let expected = std::fs::read_to_string(test_data(hash_file))
            .unwrap()
            .trim()
            .to_string();
        let actual = format!("{:x}", Sha256::digest(std::fs::read(&output).unwrap()));
        assert_eq!(actual, expected);
    }

    #[test]
    fn stretch_uncompressed_fit_tiff_matches_known_hash() {
        assert_stretch_matches_known_hash(OutputFormat::Tiff, "stretch/uncompressed.tiff.sha256");
    }

    #[test]
    fn stretch_uncompressed_fit_fits_matches_known_hash() {
        assert_stretch_matches_known_hash(OutputFormat::Fits, "stretch/uncompressed.fits.sha256");
    }
}
