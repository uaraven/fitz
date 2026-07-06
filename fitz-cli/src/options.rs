use std::path::PathBuf;

use fitz_core::fitskit::CompressionType;

pub struct Options {
    pub keep: bool,
    pub yes: bool,
    pub verbose: bool,
    pub output: Option<PathBuf>,
    pub algorithm: CompressionType,
    pub multi_file: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            keep: false,
            yes: false,
            verbose: false,
            output: None,
            algorithm: CompressionType::Rice1,
            multi_file: false,
        }
    }
}

#[derive(Default)]
pub struct DebayerOptions {
    pub core: fitz_core::debayer::DebayerOptions,
    pub yes: bool,
    pub verbose: bool,
    pub output: Option<PathBuf>,
    pub multi_file: bool,
}

pub struct StretchOptions {
    pub core: fitz_core::stretch::StretchOptions,
    pub yes: bool,
    pub verbose: bool,
    pub format: fitz_core::debayer::OutputFormat,
    pub output: Option<PathBuf>,
    pub multi_file: bool,
}

impl Default for StretchOptions {
    fn default() -> Self {
        StretchOptions {
            core: fitz_core::stretch::StretchOptions::default(),
            yes: false,
            verbose: false,
            format: fitz_core::debayer::OutputFormat::Fits,
            output: None,
            multi_file: false,
        }
    }
}

#[derive(Default)]
pub struct InfoOptions {
    pub verbose: bool,
    /// Read (decompressing if needed) the pixel data and report pixel
    /// statistics. Without it, `info` reports header-derived metadata only.
    pub pixel: bool,
    /// Use a logarithmic vertical axis for the pixel histogram. Only meaningful
    /// together with `pixel`, which is what triggers the histogram.
    pub log: bool,
    /// Print the raw FITS header cards of the image HDU instead of the formatted
    /// summary.
    pub headers: bool,
}

pub struct PreviewOptions {
    pub verbose: bool,
    pub core: fitz_core::stretch::StretchOptions,
    /// Force kitty graphics protocol rendering, bypassing auto-detection.
    pub force_kitty: bool,
    /// Force true-color ASCII rendering, bypassing auto-detection.
    pub force_truecolor: bool,
    /// Fallback to most compatible ASCII rendering mode, with only 216 colours.
    pub fallback: bool,
}

pub struct SplitChannelOptions {
    pub core: fitz_core::split_channel::SplitChannelOptions,
    pub yes: bool,
    pub verbose: bool,
    pub format: fitz_core::split_channel::ChannelFormat,
    pub r_prefix: Option<String>,
    pub r_dir: Option<PathBuf>,
    pub g_prefix: Option<String>,
    pub g_dir: Option<PathBuf>,
    pub b_prefix: Option<String>,
    pub b_dir: Option<PathBuf>,
}

impl Default for SplitChannelOptions {
    fn default() -> Self {
        SplitChannelOptions {
            core: fitz_core::split_channel::SplitChannelOptions::default(),
            yes: false,
            verbose: false,
            format: fitz_core::split_channel::ChannelFormat::I16,
            r_prefix: None,
            r_dir: None,
            g_prefix: None,
            g_dir: None,
            b_prefix: None,
            b_dir: None,
        }
    }
}

#[derive(Default)]
pub struct CopyHeaderOptions {
    pub yes: bool,
    pub verbose: bool,
    /// Write the result to this file instead of overwriting the target in place.
    pub output: Option<PathBuf>,
}
