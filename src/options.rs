use std::path::PathBuf;

use bayer::CFA;
use fitskit::CompressionType;

use crate::debayer::OutputFormat;
use crate::split_channel::ChannelFormat;

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

pub struct DebayerOptions {
    pub yes: bool,
    pub verbose: bool,
    pub bpp: u32,
    /// Bayer pattern override; takes precedence over the FITS headers.
    pub pattern: Option<CFA>,
    /// Always demosaic, even if the input looks like an already-debayered
    /// RGB image. Use this when an input is a genuine raw mosaic that 
    /// happens to have 3 channels for some other reason, so it isn't 
    /// silently misread as RGB.
    pub force_demosaic: bool,
    pub format: OutputFormat,
    pub output: Option<PathBuf>,
    pub multi_file: bool,
}

impl Default for DebayerOptions {
    fn default() -> Self {
        DebayerOptions {
            yes: false,
            verbose: false,
            bpp: 16,
            pattern: None,
            force_demosaic: false,
            format: OutputFormat::Fits,
            output: None,
            multi_file: false,
        }
    }
}

pub struct StretchOptions {
    pub yes: bool,
    pub verbose: bool,
    /// Apply one shared set of stretch parameters to all channels instead of
    /// stretching each channel independently.
    pub linked: bool,
    /// Bayer pattern override; takes precedence over the FITS headers.
    pub pattern: Option<CFA>,
    /// Always demosaic, even if the input looks like an already-debayered
    /// RGB image. See `DebayerOptions::force_demosaic`.
    pub force_demosaic: bool,
    pub format: OutputFormat,
    pub output: Option<PathBuf>,
    pub multi_file: bool,
}

impl Default for StretchOptions {
    fn default() -> Self {
        StretchOptions {
            yes: false,
            verbose: false,
            linked: false,
            pattern: None,
            force_demosaic: false,
            format: OutputFormat::Fits,
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
    /// Apply one shared set of stretch parameters to all channels instead of
    /// stretching each channel independently.
    pub linked: bool,
    /// Bayer pattern override; takes precedence over the FITS headers.
    pub pattern: Option<CFA>,
    /// Always demosaic, even if the input looks like an already-debayered
    /// RGB image. See `DebayerOptions::force_demosaic`.
    pub force_demosaic: bool,
    /// Force kitty graphics protocol rendering, bypassing auto-detection.
    pub force_kitty: bool,
    /// Force true-color ASCII rendering, bypassing auto-detection.
    pub force_truecolor: bool,
    /// Fallback to most compatible ASCII rendering mode, with only 216 colours.
    pub fallback: bool,
}

pub struct SplitChannelOptions {
    pub yes: bool,
    pub verbose: bool,
    pub format: ChannelFormat,
    /// Bayer pattern override; takes precedence over the FITS headers.
    pub pattern: Option<CFA>,
    /// Always demosaic, even if the input looks like an already-debayered
    /// RGB image. See `DebayerOptions::force_demosaic`.
    pub force_demosaic: bool,
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
            yes: false,
            verbose: false,
            format: ChannelFormat::I16,
            pattern: None,
            force_demosaic: false,
            r_prefix: None,
            r_dir: None,
            g_prefix: None,
            g_dir: None,
            b_prefix: None,
            b_dir: None,
        }
    }
}
