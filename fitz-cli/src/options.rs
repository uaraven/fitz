use std::path::PathBuf;

use libfitz::fitskit::CompressionType;

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
    pub core: libfitz::debayer::DebayerOptions,
    pub yes: bool,
    pub verbose: bool,
    pub output: Option<PathBuf>,
    pub multi_file: bool,
}

/// The stretch knobs the `Image`-based pipeline still supports: unlike the old
/// pre-refactor `libfitz::stretch::StretchOptions`, there is no `pattern`/
/// `force_demosaic` override — `Image::debayer` always reads the Bayer pattern
/// from the FITS header.
#[derive(Clone, Copy, Debug)]
pub struct StretchCore {
    pub linked: bool,
    pub brightness: f32,
}

impl Default for StretchCore {
    fn default() -> Self {
        StretchCore {
            linked: false,
            brightness: libfitz::stretch::DEFAULT_BRIGHTNESS,
        }
    }
}

pub struct StretchOptions {
    pub core: StretchCore,
    pub yes: bool,
    pub verbose: bool,
    pub format: libfitz::debayer::OutputFormat,
    pub output: Option<PathBuf>,
    pub multi_file: bool,
}

impl Default for StretchOptions {
    fn default() -> Self {
        StretchOptions {
            core: StretchCore::default(),
            yes: false,
            verbose: false,
            format: libfitz::debayer::OutputFormat::Fits,
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
    /// Detect the frame's stars and report their count and median shape.
    /// Independent of `pixel`: star detection builds its threshold from the
    /// detection plane's own background, never from the frame's `PixelStats`.
    pub stars: bool,
    /// Use a logarithmic vertical axis for the pixel histogram. Only meaningful
    /// together with `pixel`, which is what triggers the histogram.
    pub log: bool,
    /// Print the raw FITS header cards of the image HDU instead of the formatted
    /// summary.
    pub headers: bool,
}

pub struct PreviewOptions {
    pub verbose: bool,
    pub core: StretchCore,
    /// Force kitty graphics protocol rendering, bypassing auto-detection.
    pub force_kitty: bool,
    /// Force true-color ASCII rendering, bypassing auto-detection.
    pub force_truecolor: bool,
    /// Fallback to most compatible ASCII rendering mode, with only 216 colours.
    pub fallback: bool,
    /// Skip debayering a raw mosaic, showing it as a stretched grayscale
    /// image instead; ignored (with a warning) if already debayered.
    pub no_debayer: bool,
}

pub struct SplitChannelOptions {
    pub core: libfitz::split_channel::SplitChannelOptions,
    pub yes: bool,
    pub verbose: bool,
    pub format: libfitz::split_channel::ChannelFormat,
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
            core: libfitz::split_channel::SplitChannelOptions::default(),
            yes: false,
            verbose: false,
            format: libfitz::split_channel::ChannelFormat::I16,
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
