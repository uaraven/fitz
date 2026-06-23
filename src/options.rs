use std::path::PathBuf;

use bayer::CFA;
use fitskit::CompressionType;

use crate::debayer::OutputFormat;
use crate::split_channel::ChannelFormat;

pub struct Options {
    pub keep: bool,
    pub force: bool,
    pub verbose: bool,
    pub output: Option<PathBuf>,
    pub algorithm: CompressionType,
    pub multi_file: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            keep: false,
            force: false,
            verbose: false,
            output: None,
            algorithm: CompressionType::Rice1,
            multi_file: false,
        }
    }
}

pub struct DebayerOptions {
    pub force: bool,
    pub verbose: bool,
    pub bpp: u32,
    /// Bayer pattern override; takes precedence over the FITS header's
    /// BAYERPAT keyword. Falls back to BAYERPAT when not given.
    pub pattern: Option<CFA>,
    /// Always demosaic, even if the input looks like an already-debayered
    /// RGB cube (no BAYERPAT header, 3-plane image). Use this when an input
    /// is a genuine raw mosaic that happens to have 3 planes for some other
    /// reason, so it isn't silently misread as RGB.
    pub force_demosaic: bool,
    pub format: OutputFormat,
    pub output: Option<PathBuf>,
    pub multi_file: bool,
}

impl Default for DebayerOptions {
    fn default() -> Self {
        DebayerOptions {
            force: false,
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
    pub force: bool,
    pub verbose: bool,
    /// Apply one shared set of stretch parameters to all channels instead of
    /// stretching each channel independently.
    pub linked: bool,
    /// Bayer pattern override; takes precedence over the FITS header's
    /// BAYERPAT keyword. Falls back to BAYERPAT when not given.
    pub pattern: Option<CFA>,
    /// Always demosaic, even if the input looks like an already-debayered
    /// RGB cube (no BAYERPAT header, 3-plane image). See `DebayerOptions::force_demosaic`.
    pub force_demosaic: bool,
    pub format: OutputFormat,
    pub output: Option<PathBuf>,
    pub multi_file: bool,
}

impl Default for StretchOptions {
    fn default() -> Self {
        StretchOptions {
            force: false,
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
    /// Bayer pattern override; takes precedence over the FITS header's
    /// BAYERPAT keyword. Falls back to BAYERPAT when not given.
    pub pattern: Option<CFA>,
    /// Always demosaic, even if the input looks like an already-debayered
    /// RGB cube (no BAYERPAT header, 3-plane image). See `DebayerOptions::force_demosaic`.
    pub force_demosaic: bool,
    /// Force kitty graphics protocol rendering, bypassing auto-detection.
    pub force_kitty: bool,
    /// Force true-color ANSI half-block rendering, bypassing auto-detection.
    pub force_truecolor: bool,
}

pub struct SplitChannelOptions {
    pub force: bool,
    pub verbose: bool,
    pub format: ChannelFormat,
    /// Bayer pattern override; takes precedence over the FITS header's
    /// BAYERPAT keyword. Falls back to BAYERPAT when not given.
    pub pattern: Option<CFA>,
    /// Always demosaic, even if the input looks like an already-debayered
    /// RGB cube (no BAYERPAT header, 3-plane image). See `DebayerOptions::force_demosaic`.
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
            force: false,
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
