use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use libfitz::fits_image::{CFA_KEYWORDS, write_pixel_fits};
pub use libfitz::split_channel::ChannelFormat;
use libfitz::split_channel::encode_channel;

use crate::io_prompt::{ensure_can_write, print_progress, print_step};
use crate::options::SplitChannelOptions;

pub fn parse_channel_format(s: &str) -> Result<ChannelFormat, String> {
    s.parse()
}

pub fn split_channel_file(input: &Path, opts: &SplitChannelOptions) -> Result<()> {
    print_step(opts.verbose, "reading");
    let s = libfitz::split_channel::split_channels(input, &opts.core)
        .with_context(|| format!("{}: splitting failed", input.display()))?;

    print_step(opts.verbose, "splitting channels");

    let channels = [
        ("R", &s.r, opts.r_prefix.as_deref(), opts.r_dir.as_deref()),
        ("G", &s.g, opts.g_prefix.as_deref(), opts.g_dir.as_deref()),
        ("B", &s.b, opts.b_prefix.as_deref(), opts.b_dir.as_deref()),
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
        write_channel_fits(
            &output,
            s.width,
            s.height,
            values,
            opts.format,
            &s.header,
            channel,
        )?;
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
    width: usize,
    height: usize,
    values: &[f64],
    format: ChannelFormat,
    src_header: &libfitz::fitskit::Header,
    channel: &str,
) -> Result<()> {
    let (pixels, bzero) = encode_channel(values, format);
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
