use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use libfitz::data::Image;
use libfitz::fits_file::{SaveOptions, load_fits, save_fits};
pub use libfitz::split_channel::ChannelFormat;

use crate::io_prompt::{ensure_can_write, print_progress, print_step};
use crate::options::SplitChannelOptions;

pub fn parse_channel_format(s: &str) -> Result<ChannelFormat, String> {
    s.parse()
}

pub fn split_channel_file(input: &Path, opts: &SplitChannelOptions) -> Result<()> {
    print_step(opts.verbose, "reading");
    let image = load_fits(input).with_context(|| format!("{}: reading failed", input.display()))?;

    print_step(opts.verbose, "splitting channels");
    let [r, g, b] = if opts.cfa {
        image
            .split_cfa()
            .with_context(|| format!("{}: splitting failed", input.display()))?
    } else {
        image
            .split_channels()
            .with_context(|| format!("{}: splitting failed", input.display()))?
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
    for (default_prefix, image, prefix, dir) in channels {
        if any_configured && prefix.is_none() && dir.is_none() {
            continue;
        }

        let output = channel_output_path(input, default_prefix, prefix, dir)?;
        outputs.push((output, image, default_prefix));
    }

    // Check all outputs before writing any, so a pre-existing file doesn't
    // leave a partial set of channels written to disk.
    for (output, _, _) in &outputs {
        ensure_can_write(output, opts.yes)?;
    }

    for (output, image, channel) in outputs {
        print_progress(opts.verbose, input, &output);
        print_step(opts.verbose, "writing");
        write_channel_fits(&output, image, opts.format, channel)?;
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

fn write_channel_fits(
    output: &Path,
    image: &Image,
    format: ChannelFormat,
    channel: &str,
) -> Result<()> {
    let history = format!(
        "split channel {} by fitz {}",
        channel,
        env!("CARGO_PKG_VERSION")
    );
    save_fits(
        output,
        image,
        SaveOptions {
            bitpix: format.into(),
            compress_options: None,
            history: Some(history),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfitz::data::ImageType;
    use std::fs;
    use tempfile::TempDir;

    use crate::test_support::test_data;

    /// Copies a bundled fixture into `dir` so the default beside-the-input
    /// output paths land in a scratch directory instead of `test-data/`.
    fn copy_fixture(name: &str, dir: &Path) -> PathBuf {
        let dest = dir.join(name);
        fs::copy(test_data(name), &dest).unwrap();
        dest
    }

    fn opts() -> SplitChannelOptions {
        SplitChannelOptions {
            yes: true,
            ..SplitChannelOptions::default()
        }
    }

    #[test]
    fn split_channel_file_splits_a_debayered_rgb_cube() {
        let tmp = TempDir::new().unwrap();
        let input = copy_fixture("rgb.fits", tmp.path());
        let source = load_fits(&input).unwrap();
        assert_eq!(source.image_type, ImageType::RGB);
        let expected = source.split_channels().unwrap();

        split_channel_file(&input, &opts()).unwrap();

        let names = ["R-rgb.fits", "G-rgb.fits", "B-rgb.fits"];
        for (name, expected_channel) in names.iter().zip(&expected) {
            let channel = load_fits(&tmp.path().join(name)).unwrap();
            assert_eq!(channel.image_type, ImageType::Grayscale);
            assert_eq!(
                (channel.width, channel.height),
                (expected_channel.width, expected_channel.height)
            );
        }

        // The three channels of a real colour image are genuinely different,
        // not three copies of the same plane.
        let r = load_fits(&tmp.path().join("R-rgb.fits")).unwrap();
        let b = load_fits(&tmp.path().join("B-rgb.fits")).unwrap();
        assert_ne!(r.pixels, b.pixels);
    }

    #[test]
    fn split_channel_file_debayers_a_raw_mosaic_before_splitting() {
        let tmp = TempDir::new().unwrap();
        let input = copy_fixture("cfa_orion.fits", tmp.path());
        let source = load_fits(&input).unwrap();
        assert_eq!(source.image_type, ImageType::CFA(bayer::CFA::RGGB));
        // The default `--format i16` round-trips a u16-sourced split exactly
        // (the FITS output uses the unsigned-16 BZERO convention), so the
        // written channels can be compared byte-for-byte against the split
        // computed directly from the source.
        let [expected_r, expected_g, expected_b] = source.split_channels().unwrap();

        split_channel_file(&input, &opts()).unwrap();

        let r = load_fits(&tmp.path().join("R-cfa_orion.fits")).unwrap();
        let g = load_fits(&tmp.path().join("G-cfa_orion.fits")).unwrap();
        let b = load_fits(&tmp.path().join("B-cfa_orion.fits")).unwrap();
        for channel in [&r, &g, &b] {
            assert_eq!(channel.image_type, ImageType::Grayscale);
            assert_eq!(
                (channel.width, channel.height),
                (source.width, source.height)
            );
        }
        assert_eq!(r.pixels, expected_r.pixels);
        assert_eq!(g.pixels, expected_g.pixels);
        assert_eq!(b.pixels, expected_b.pixels);
    }

    #[test]
    fn split_channel_file_cfa_flag_splits_the_raw_mosaic_without_debayering() {
        let tmp = TempDir::new().unwrap();
        let input = copy_fixture("cfa_orion.fits", tmp.path());
        let source = load_fits(&input).unwrap();
        let [expected_r, expected_g, expected_b] = source.split_cfa().unwrap();

        split_channel_file(
            &input,
            &SplitChannelOptions {
                cfa: true,
                ..opts()
            },
        )
        .unwrap();

        let r = load_fits(&tmp.path().join("R-cfa_orion.fits")).unwrap();
        let g = load_fits(&tmp.path().join("G-cfa_orion.fits")).unwrap();
        let b = load_fits(&tmp.path().join("B-cfa_orion.fits")).unwrap();
        for channel in [&r, &g, &b] {
            // Undebayered split channels are half the mosaic's width/height.
            assert_eq!(
                (channel.width, channel.height),
                (source.width / 2, source.height / 2)
            );
        }
        assert_eq!(r.pixels, expected_r.pixels);
        assert_eq!(g.pixels, expected_g.pixels);
        assert_eq!(b.pixels, expected_b.pixels);
    }

    #[test]
    fn split_channel_file_writes_only_the_configured_channels() {
        let tmp = TempDir::new().unwrap();
        let input = copy_fixture("rgb.fits", tmp.path());

        split_channel_file(
            &input,
            &SplitChannelOptions {
                r_prefix: Some("red".to_string()),
                ..opts()
            },
        )
        .unwrap();

        assert!(tmp.path().join("red-rgb.fits").exists());
        assert!(!tmp.path().join("G-rgb.fits").exists());
        assert!(!tmp.path().join("B-rgb.fits").exists());
    }

    #[test]
    fn split_channel_file_writes_a_configured_channel_to_its_own_directory() {
        let tmp = TempDir::new().unwrap();
        let input = copy_fixture("cfa_orion.fits", tmp.path());
        let blue_dir = tmp.path().join("blue-out");

        split_channel_file(
            &input,
            &SplitChannelOptions {
                cfa: true,
                b_dir: Some(blue_dir.clone()),
                ..opts()
            },
        )
        .unwrap();

        // A per-channel `--dir` keeps the original filename rather than
        // prefixing it.
        let output = blue_dir.join("cfa_orion.fits");
        assert!(output.exists());
        let channel = load_fits(&output).unwrap();
        assert_eq!(channel.image_type, ImageType::Grayscale);
        assert_eq!(
            (channel.width, channel.height),
            (source_dims(&input).0 / 2, source_dims(&input).1 / 2)
        );

        assert!(!tmp.path().join("R-cfa_orion.fits").exists());
        assert!(!tmp.path().join("G-cfa_orion.fits").exists());
    }

    fn source_dims(input: &Path) -> (usize, usize) {
        let image = load_fits(input).unwrap();
        (image.width, image.height)
    }
}
