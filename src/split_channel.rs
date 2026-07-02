use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use fitskit::{FitsFile, Header, PixelData};
use rayon::prelude::*;

use crate::fits_image::{
    CFA_KEYWORDS, RgbBuffer, demosaic_to_rgb, ensure_can_write, find_image_hdu, get_bayerpat,
    is_rgb_cube_shape, print_progress, print_step, resolve_cfa, scaled_pixels, write_pixel_fits,
};
use crate::options::SplitChannelOptions;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChannelFormat {
    I8,
    I16,
    I32,
    F32,
    F64,
}

pub fn parse_channel_format(s: &str) -> Result<ChannelFormat, String> {
    match s.to_ascii_lowercase().as_str() {
        "i8" => Ok(ChannelFormat::I8),
        "i16" => Ok(ChannelFormat::I16),
        "i32" => Ok(ChannelFormat::I32),
        "f32" => Ok(ChannelFormat::F32),
        "f64" => Ok(ChannelFormat::F64),
        _ => Err("format must be one of: i8, i16, i32, f32, f64".to_string()),
    }
}

pub fn split_channel_file(input: &Path, opts: &SplitChannelOptions) -> Result<()> {
    print_step(opts.verbose, "reading");
    let fits =
        FitsFile::from_file(input).with_context(|| format!("cannot read {}", input.display()))?;

    let (header, img) = find_image_hdu(&fits, input, opts.verbose)?;
    let img = img.as_ref();

    let try_demosaic = opts.force_demosaic || get_bayerpat(header).is_some();

    let (width, height, r, g, b) = if try_demosaic {
        if img.axes.len() != 2 {
            bail!(
                "{}: expected a 2D mosaic image, found {} axes",
                input.display(),
                img.axes.len()
            );
        }

        let cfa = resolve_cfa(header, opts.pattern)
            .with_context(|| format!("{}: cannot determine Bayer pattern", input.display()))?;

        print_step(opts.verbose, "debayering");
        let (width, height, rgb) = demosaic_to_rgb(header, img, cfa)
            .with_context(|| format!("{}: debayering failed", input.display()))?;

        print_step(opts.verbose, "splitting channels");
        let (r, g, b) = deinterleave(rgb);
        (width, height, r, g, b)
    } else {
        if !is_rgb_cube_shape(img) {
            bail!(
                "{}: no BAYERPAT header and image is not a 3-plane RGB cube (NAXIS3=3)",
                input.display()
            );
        }

        print_step(opts.verbose, "splitting channels");
        let width = img.axes[0];
        let height = img.axes[1];
        let scaled = scaled_pixels(header, img);

        let plane_len = width * height;
        let r = scaled[0..plane_len].to_vec();
        let g = scaled[plane_len..2 * plane_len].to_vec();
        let b = scaled[2 * plane_len..3 * plane_len].to_vec();
        (width, height, r, g, b)
    };

    let channels = [
        ("R", &r, opts.r_prefix.as_deref(), opts.r_dir.as_deref()),
        ("G", &g, opts.g_prefix.as_deref(), opts.g_dir.as_deref()),
        ("B", &b, opts.b_prefix.as_deref(), opts.b_dir.as_deref()),
    ];

    // With no per-channel prefix/dir options, write all three; otherwise write
    // only the channels the user explicitly configured.
    let any_configured = channels
        .iter()
        .any(|(_, _, prefix, dir)| prefix.is_some() || dir.is_some());

    let mut outputs = Vec::with_capacity(channels.len());
    for (default_prefix, values, prefix, dir) in channels {
        if any_configured && prefix.is_none() && dir.is_none() {
            continue;
        }

        let output = channel_output_path(input, default_prefix, prefix, dir)?;
        outputs.push((output, values, default_prefix));
    }

    // Check all outputs before writing any, so a pre-existing file doesn't
    // leave a partial set of channels written to disk.
    for (output, _, _) in &outputs {
        ensure_can_write(output, opts.yes)?;
    }

    for (output, values, channel) in outputs {
        print_progress(opts.verbose, input, &output);
        print_step(opts.verbose, "writing");
        write_channel_fits(&output, width, height, values, opts.format, header, channel)?;
    }

    Ok(())
}

fn channel_output_path(
    input: &Path,
    default_prefix: &str,
    prefix: Option<&str>,
    dir: Option<&Path>,
) -> Result<PathBuf> {
    let filename = input
        .file_name()
        .ok_or_else(|| anyhow!("{}: path has no file name", input.display()))?;

    let path = match dir {
        Some(dir) => {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create directory {}", dir.display()))?;
            dir.join(filename)
        }
        None => {
            let prefix = prefix.unwrap_or(default_prefix);
            let mut name = OsString::from(format!("{prefix}-"));
            name.push(filename);
            crate::place_beside(input, name)
        }
    };
    Ok(path)
}

fn deinterleave(rgb: RgbBuffer) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
    match rgb {
        RgbBuffer::U8(v) => deinterleave_channels(&v),
        RgbBuffer::U16(v) => deinterleave_channels(&v),
    }
}

fn deinterleave_channels<T: Copy + Into<f64> + Send + Sync>(
    v: &[T],
) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
    let n = v.len() / 3;
    let r = (0..n).into_par_iter().map(|i| v[i * 3].into()).collect();
    let g = (0..n)
        .into_par_iter()
        .map(|i| v[i * 3 + 1].into())
        .collect();
    let b = (0..n)
        .into_par_iter()
        .map(|i| v[i * 3 + 2].into())
        .collect();
    (r, g, b)
}

fn write_channel_fits(
    output: &Path,
    width: usize,
    height: usize,
    values: &[f64],
    format: ChannelFormat,
    src_header: &Header,
    channel: &str,
) -> Result<()> {
    let (pixels, bzero) = match format {
        ChannelFormat::I8 => (
            PixelData::U8(
                values
                    .par_iter()
                    .map(|&v| v.round().clamp(0.0, 255.0) as u8)
                    .collect(),
            ),
            0.0,
        ),
        ChannelFormat::I16 => (
            PixelData::I16(
                values
                    .par_iter()
                    .map(|&v| (v.round().clamp(0.0, 65535.0) - 32768.0) as i16)
                    .collect(),
            ),
            32768.0,
        ),
        ChannelFormat::I32 => (
            PixelData::I32(
                values
                    .par_iter()
                    .map(|&v| (v.round().clamp(0.0, 4294967295.0) - 2147483648.0) as i32)
                    .collect(),
            ),
            2147483648.0,
        ),
        ChannelFormat::F32 => (
            PixelData::F32(values.par_iter().map(|&v| v as f32).collect()),
            0.0,
        ),
        ChannelFormat::F64 => (PixelData::F64(values.to_vec()), 0.0),
    };

    let history = format!(
        "split channel {} by fitz {}",
        channel,
        env!("CARGO_PKG_VERSION")
    );
    write_pixel_fits(
        output,
        vec![width, height],
        pixels,
        1.0,
        bzero,
        Some(src_header),
        CFA_KEYWORDS,
        Some(&history),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        copy_to_temp, test_data, write_mosaic_fits, write_mosaic_fits_with_metadata,
        write_rgb_cube_fits,
    };
    use fitskit::HduData;
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    #[test]
    fn split_channel_preserves_metadata_and_drops_bayerpat() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits_with_metadata(&input, 8, 6, Some("RGGB"));

        split_channel_file(&input, &SplitChannelOptions::default()).unwrap();

        for (channel, file) in [
            ("R", "R-raw.fits"),
            ("G", "G-raw.fits"),
            ("B", "B-raw.fits"),
        ] {
            let header = FitsFile::from_file(tmp.path().join(file))
                .unwrap()
                .primary()
                .header
                .clone();
            assert_eq!(header.get_string("OBJECT"), Some("M31"), "{channel}");
            assert_eq!(header.get_float("CRVAL1"), Some(10.68), "{channel}");
            assert!(header.find("BAYERPAT").is_none(), "{channel}");
            let marker = format!("split channel {channel} by fitz");
            assert!(
                header.iter().any(|k| k.name == "HISTORY"
                    && k.comment.as_deref().is_some_and(|c| c.contains(&marker))),
                "missing HISTORY for {channel}"
            );
        }
    }

    #[test]
    fn split_channel_fz_output_does_not_leak_container_keywords() {
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);

        split_channel_file(&input, &SplitChannelOptions::default()).unwrap();

        let header = FitsFile::from_file(tmp.path().join("R-compressed.fits.fz"))
            .unwrap()
            .primary()
            .header
            .clone();
        for kw in [
            "TFORM1", "TFIELDS", "ZIMAGE", "ZCMPTYPE", "ZNAXIS1", "XTENSION", "EXTNAME", "BAYERPAT",
        ] {
            assert!(header.find(kw).is_none(), "{kw} leaked into split output");
        }
    }

    #[test]
    fn split_channel_default_writes_all_three_with_default_prefixes() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 8, 6, Some("RGGB"));

        split_channel_file(&input, &SplitChannelOptions::default()).unwrap();

        assert!(tmp.path().join("R-raw.fits").exists());
        assert!(tmp.path().join("G-raw.fits").exists());
        assert!(tmp.path().join("B-raw.fits").exists());
    }

    #[test]
    fn split_channel_only_configured_channels_are_saved() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 8, 6, Some("RGGB"));

        let opts = SplitChannelOptions {
            r_prefix: Some("Red".to_string()),
            ..SplitChannelOptions::default()
        };
        split_channel_file(&input, &opts).unwrap();

        assert!(tmp.path().join("Red-raw.fits").exists());
        assert!(!tmp.path().join("G-raw.fits").exists());
        assert!(!tmp.path().join("B-raw.fits").exists());
    }

    #[test]
    fn split_channel_dir_keeps_original_filename() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 8, 6, Some("RGGB"));

        let r_dir = tmp.path().join("red");
        std::fs::create_dir(&r_dir).unwrap();

        let opts = SplitChannelOptions {
            r_dir: Some(r_dir.clone()),
            ..SplitChannelOptions::default()
        };
        split_channel_file(&input, &opts).unwrap();

        assert!(r_dir.join("raw.fits").exists());
    }

    #[test]
    fn split_channel_creates_missing_dir() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 8, 6, Some("RGGB"));

        // Directory does not exist beforehand — it must be created automatically.
        let r_dir = tmp.path().join("red");
        assert!(!r_dir.exists());

        let opts = SplitChannelOptions {
            r_dir: Some(r_dir.clone()),
            ..SplitChannelOptions::default()
        };
        split_channel_file(&input, &opts).unwrap();

        assert!(r_dir.join("raw.fits").exists());
    }

    #[test]
    fn split_channel_creates_missing_nested_dir() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 8, 6, Some("RGGB"));

        // Neither "out" nor "out/red" exist yet — both levels must be created.
        let r_dir = tmp.path().join("out").join("red");
        assert!(!r_dir.exists());

        let opts = SplitChannelOptions {
            r_dir: Some(r_dir.clone()),
            ..SplitChannelOptions::default()
        };
        split_channel_file(&input, &opts).unwrap();

        assert!(r_dir.join("raw.fits").exists());
    }

    #[test]
    fn split_channel_creates_missing_dirs_for_all_channels() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 8, 6, Some("RGGB"));

        let r_dir = tmp.path().join("r");
        let g_dir = tmp.path().join("g");
        let b_dir = tmp.path().join("b");
        assert!(!r_dir.exists() && !g_dir.exists() && !b_dir.exists());

        let opts = SplitChannelOptions {
            r_dir: Some(r_dir.clone()),
            g_dir: Some(g_dir.clone()),
            b_dir: Some(b_dir.clone()),
            ..SplitChannelOptions::default()
        };
        split_channel_file(&input, &opts).unwrap();

        assert!(r_dir.join("raw.fits").exists());
        assert!(g_dir.join("raw.fits").exists());
        assert!(b_dir.join("raw.fits").exists());
    }

    #[test]
    fn split_channel_skips_debayer_for_existing_rgb_cube() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 4, 3);

        split_channel_file(&input, &SplitChannelOptions::default()).unwrap();

        let r = FitsFile::from_file(tmp.path().join("R-rgb.fits")).unwrap();
        let bscale = r.primary().header.get_float("BSCALE").unwrap_or(1.0);
        let bzero = r.primary().header.get_float("BZERO").unwrap_or(0.0);
        if let HduData::Image(img) = &r.primary().data {
            let scaled = img.scaled_values(bscale, bzero);
            assert_eq!(scaled, (0..12).map(|x| x as f64).collect::<Vec<_>>());
        } else {
            panic!("expected image data");
        }
    }

    #[test]
    fn split_channel_force_demosaic_rejects_3_plane_cube_instead_of_guessing() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&input, 4, 3);

        let opts = SplitChannelOptions {
            force_demosaic: true,
            pattern: Some(bayer::CFA::RGGB),
            ..SplitChannelOptions::default()
        };
        let err = split_channel_file(&input, &opts).unwrap_err();
        assert!(err.to_string().contains("2D mosaic image"));
    }

    #[test]
    fn split_channel_errors_without_bayerpat_or_rgb_cube() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("raw.fits");
        write_mosaic_fits(&input, 4, 4, None);

        let err = split_channel_file(&input, &SplitChannelOptions::default()).unwrap_err();
        assert!(err.to_string().contains("3-plane RGB cube"));
    }

    #[test]
    fn split_channel_handles_tile_compressed_input() {
        // A compressed .fz input must be decompressed before debayering and
        // splitting into the three per-channel files.
        let tmp = TempDir::new().unwrap();
        let input = copy_to_temp("compressed.fits.fz", &tmp);

        split_channel_file(&input, &SplitChannelOptions::default()).unwrap();

        assert!(tmp.path().join("R-compressed.fits.fz").exists());
        assert!(tmp.path().join("G-compressed.fits.fz").exists());
        assert!(tmp.path().join("B-compressed.fits.fz").exists());
    }

    fn assert_split_channel_matches_known_hashes(format: ChannelFormat, suffix: &str) {
        let tmp = TempDir::new().unwrap();
        let input = test_data("uncompressed.fit");

        let r_dir = tmp.path().join("r");
        let g_dir = tmp.path().join("g");
        let b_dir = tmp.path().join("b");
        std::fs::create_dir_all(&r_dir).unwrap();
        std::fs::create_dir_all(&g_dir).unwrap();
        std::fs::create_dir_all(&b_dir).unwrap();

        let opts = SplitChannelOptions {
            r_dir: Some(r_dir.clone()),
            g_dir: Some(g_dir.clone()),
            b_dir: Some(b_dir.clone()),
            format,
            ..SplitChannelOptions::default()
        };
        split_channel_file(&input, &opts).unwrap();

        for (channel, dir) in [("r", &r_dir), ("g", &g_dir), ("b", &b_dir)] {
            let hash_file = format!("split/uncompressed-{suffix}-{channel}.sha256");
            let expected = std::fs::read_to_string(test_data(&hash_file))
                .unwrap()
                .trim()
                .to_string();

            let actual_path = dir.join("uncompressed.fit");
            let actual = format!("{:x}", Sha256::digest(std::fs::read(&actual_path).unwrap()));
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn split_channel_uncompressed_fit_i16_matches_known_hash() {
        assert_split_channel_matches_known_hashes(ChannelFormat::I16, "i16");
    }
}
