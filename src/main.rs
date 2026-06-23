mod compress;
mod debayer;
mod decompress;
mod fits_image;
mod info;
mod kitty;
mod options;
mod preview;
mod split_channel;
mod stretch;
mod terminal;

#[cfg(test)]
mod test_support;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Result, anyhow};
use bayer::CFA;
use clap::{Parser, Subcommand, ValueEnum};
use fitskit::CompressionType;
use rayon::prelude::*;

use compress::compress_file;
use debayer::{OutputFormat, debayer_file, parse_output_format};
use decompress::decompress_file;
use info::info_file;
use options::{
    DebayerOptions, InfoOptions, Options, PreviewOptions, SplitChannelOptions, StretchOptions,
};
use preview::preview_file;
use split_channel::{ChannelFormat, parse_channel_format, split_channel_file};
use stretch::stretch_file;

#[derive(Clone, Copy, ValueEnum)]
enum Algorithm {
    #[value(name = "rice1")]
    Rice1,
    #[value(name = "gzip1")]
    Gzip1,
    #[value(name = "gzip2")]
    Gzip2,
}

impl From<Algorithm> for CompressionType {
    fn from(a: Algorithm) -> Self {
        match a {
            Algorithm::Rice1 => CompressionType::Rice1,
            Algorithm::Gzip1 => CompressionType::Gzip1,
            Algorithm::Gzip2 => CompressionType::Gzip2,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum BayerPattern {
    #[value(name = "RGGB")]
    Rggb,
    #[value(name = "GBRG")]
    Gbrg,
    #[value(name = "BGGR")]
    Bggr,
    #[value(name = "GRBG")]
    Grbg,
}

impl From<BayerPattern> for CFA {
    fn from(p: BayerPattern) -> Self {
        match p {
            BayerPattern::Rggb => CFA::RGGB,
            BayerPattern::Gbrg => CFA::GBRG,
            BayerPattern::Bggr => CFA::BGGR,
            BayerPattern::Grbg => CFA::GRBG,
        }
    }
}

fn parse_bpp(s: &str) -> Result<u32, String> {
    match s.parse::<u32>() {
        Ok(v) if v == 8 || v == 16 || v == 32 => Ok(v),
        _ => Err("bpp must be one of: 8, 16, 32".to_string()),
    }
}

#[derive(Parser)]
#[command(
    name = "fitz",
    version,
    about = "Compress/decompress FITS files using tile compression",
    long_about = "Compress FITS files to .fz (tile-compressed) or decompress .fz back to FITS.\n\
                  Output file replaces the input unless -k is given.\n\
                  Supported algorithms: rice1 (default), gzip1, gzip2."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Print each file being processed
    #[arg(short = 'v', long, global = true)]
    verbose: bool,

    /// Number of files to process in parallel (default: number of CPU cores)
    #[arg(short = 'j', long, global = true, default_value_t = default_jobs())]
    jobs: usize,
}

/// The default `--jobs` value: the number of available CPU cores, or 1 if that
/// can't be determined.
fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[derive(Subcommand)]
enum Command {
    /// Compress FITS files
    Compress(CompressArgs),
    /// Decompress FITS files
    Decompress(DecompressArgs),
    /// Debayer a FITS mosaic image and save it as a FITS or TIFF file
    Debayer(DebayerArgs),
    /// Auto-stretch a FITS image (debayering it first if needed) and save it as a FITS or TIFF file
    Stretch(StretchArgs),
    /// Debayer a FITS mosaic image and save each color channel as a separate FITS file
    #[command(name = "split")]
    SplitChannel(SplitChannelArgs),
    /// Print information about FITS files (resolution, bit depth, channels, coordinates, pixel stats)
    Info(InfoArgs),
    /// Render a FITS image to the terminal as colored ANSI text (auto-stretched, debayered if needed)
    Preview(PreviewArgs),
}

#[derive(clap::Args)]
struct CompressArgs {
    /// Keep original file after compression
    #[arg(short = 'k', long)]
    keep: bool,

    /// Overwrite output file if it already exists
    #[arg(short = 'f', long)]
    force: bool,

    /// Compression algorithm
    #[arg(short = 'a', long, default_value = "rice1")]
    algorithm: Algorithm,

    /// Write output to this file (only valid with a single input file)
    #[arg(short = 'o', long)]
    output: Option<PathBuf>,

    /// FITS files to compress
    files: Vec<PathBuf>,
}

#[derive(clap::Args)]
struct DecompressArgs {
    /// Keep original file after decompression
    #[arg(short = 'k', long)]
    keep: bool,

    /// Overwrite output file if it already exists
    #[arg(short = 'f', long)]
    force: bool,

    /// Write output to this file (only valid with a single input file)
    #[arg(short = 'o', long)]
    output: Option<PathBuf>,

    /// FITS files to decompress
    files: Vec<PathBuf>,
}

#[derive(clap::Args)]
struct DebayerArgs {
    /// Overwrite output file if it already exists
    #[arg(short = 'f', long)]
    force: bool,

    /// Bits per pixel in the output image (TIFF only; FITS output keeps the
    /// source image's pixel format)
    #[arg(long, default_value = "16", value_parser = parse_bpp)]
    bpp: u32,

    /// Bayer pattern of the sensor; if omitted, read from the FITS BAYERPAT header
    #[arg(long)]
    pattern: Option<BayerPattern>,

    /// Always demosaic, even if the input has no BAYERPAT header but looks
    /// like an already-debayered RGB cube (a 3-plane image). Use this for a
    /// raw mosaic that happens to have 3 planes for some other reason.
    #[arg(long)]
    force_demosaic: bool,

    /// Output file format
    #[arg(long, default_value = "fits", value_parser = parse_output_format)]
    format: OutputFormat,

    /// Write output to this file, or to this folder if processing multiple files
    #[arg(short = 'o', long)]
    output: Option<PathBuf>,

    /// FITS files to debayer
    files: Vec<PathBuf>,
}

#[derive(clap::Args)]
struct StretchArgs {
    /// Overwrite output file if it already exists
    #[arg(short = 'f', long)]
    force: bool,

    /// Apply one shared stretch to all channels instead of stretching each
    /// channel independently (which also neutralizes the background)
    #[arg(long)]
    linked_channel: bool,

    /// Bayer pattern of the sensor; if omitted, read from the FITS BAYERPAT header
    #[arg(long)]
    pattern: Option<BayerPattern>,

    /// Always demosaic, even if the input has no BAYERPAT header but looks
    /// like an already-debayered RGB cube (a 3-plane image). Use this for a
    /// raw mosaic that happens to have 3 planes for some other reason.
    #[arg(long)]
    force_demosaic: bool,

    /// Output file format
    #[arg(long, default_value = "fits", value_parser = parse_output_format)]
    format: OutputFormat,

    /// Write output to this file, or to this folder if processing multiple files
    #[arg(short = 'o', long)]
    output: Option<PathBuf>,

    /// FITS files to stretch
    files: Vec<PathBuf>,
}

#[derive(clap::Args)]
struct SplitChannelArgs {
    /// Overwrite output files if they already exist
    #[arg(short = 'f', long)]
    force: bool,

    /// Per-channel pixel format of the resulting FITS files
    #[arg(long, default_value = "i16", value_parser = parse_channel_format)]
    format: ChannelFormat,

    /// Bayer pattern of the sensor; if omitted, read from the FITS BAYERPAT header
    #[arg(long)]
    pattern: Option<BayerPattern>,

    /// Always demosaic, even if the input has no BAYERPAT header but looks
    /// like an already-debayered RGB cube (a 3-plane image). Use this for a
    /// raw mosaic that happens to have 3 planes for some other reason
    /// (requires --pattern if there's no BAYERPAT header).
    #[arg(long)]
    force_demosaic: bool,

    /// Prefix for the red channel file: {prefix}-{original-file-name}
    #[arg(long, conflicts_with = "r_dir")]
    r_prefix: Option<String>,
    /// Directory to save the red channel file into (original filename kept)
    #[arg(long)]
    r_dir: Option<PathBuf>,

    /// Prefix for the green channel file: {prefix}-{original-file-name}
    #[arg(long, conflicts_with = "g_dir")]
    g_prefix: Option<String>,
    /// Directory to save the green channel file into (original filename kept)
    #[arg(long)]
    g_dir: Option<PathBuf>,

    /// Prefix for the blue channel file: {prefix}-{original-file-name}
    #[arg(long, conflicts_with = "b_dir")]
    b_prefix: Option<String>,
    /// Directory to save the blue channel file into (original filename kept)
    #[arg(long)]
    b_dir: Option<PathBuf>,

    /// FITS files to split into channels
    files: Vec<PathBuf>,
}

#[derive(clap::Args)]
struct InfoArgs {
    /// Read the pixel data (decompressing first if needed) and report pixel
    /// statistics (min/max/mean/median and the count of zero-valued pixels).
    /// Not supported for debayered RGB images.
    #[arg(long)]
    pixel: bool,
    /// FITS files to inspect
    files: Vec<PathBuf>,
}

#[derive(clap::Args)]
struct PreviewArgs {
    /// Apply one shared stretch to all channels instead of stretching each
    /// channel independently (which also neutralizes the background)
    #[arg(long)]
    linked_channel: bool,

    /// Bayer pattern of the sensor; if omitted, read from the FITS BAYERPAT header
    #[arg(long)]
    pattern: Option<BayerPattern>,

    /// Always demosaic, even if the input has no BAYERPAT header but looks
    /// like an already-debayered RGB cube (a 3-plane image). Use this for a
    /// raw mosaic that happens to have 3 planes for some other reason.
    #[arg(long)]
    force_demosaic: bool,

    /// Force kitty graphics protocol rendering, skipping auto-detection
    #[arg(long, conflicts_with = "truecolor")]
    graphics: bool,

    /// Force true-color ANSI half-block rendering, skipping auto-detection
    #[arg(long)]
    truecolor: bool,

    /// FITS file to preview (only a single file is accepted)
    file: PathBuf,
}

/// Derive an output path: explicit `--output` is used as-is (or joined with the
/// input's stem when batching into a directory); otherwise the input's stem gets
/// `suffix` and `.ext` appended, placed beside the input.
fn derive_output_path(
    input: &Path,
    output: Option<&Path>,
    multi_file: bool,
    ext: &str,
    suffix: &str,
) -> Result<PathBuf> {
    let path = match output {
        Some(dir) if multi_file => {
            let stem = file_stem(input)?;
            let mut name: OsString = stem.to_owned();
            name.push(format!(".{ext}"));
            PathBuf::from(dir).join(name)
        }
        Some(p) => p.to_path_buf(),
        None => {
            let stem = file_stem(input)?;
            let mut name: OsString = stem.to_owned();
            name.push(format!("{suffix}.{ext}"));
            place_beside(input, name)
        }
    };
    Ok(path)
}

fn debayer_output_path(input: &Path, opts: &DebayerOptions) -> Result<PathBuf> {
    derive_output_path(
        input,
        opts.output.as_deref(),
        opts.multi_file,
        opts.format.extension(),
        "_debayer",
    )
}

fn stretch_output_path(input: &Path, opts: &StretchOptions) -> Result<PathBuf> {
    derive_output_path(
        input,
        opts.output.as_deref(),
        opts.multi_file,
        opts.format.extension(),
        "_stretch",
    )
}

fn file_stem(input: &Path) -> Result<&std::ffi::OsStr> {
    input
        .file_stem()
        .ok_or_else(|| anyhow!("{}: path has no file name", input.display()))
}

/// Place `name` in the same directory as `input`, falling back to a bare
/// relative path when `input` has no parent directory.
pub(crate) fn place_beside(input: &Path, name: OsString) -> PathBuf {
    match input.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

fn output_path(input: &Path, opts: &Options, is_decompress: bool) -> PathBuf {
    match opts.output.as_deref() {
        Some(dir) if opts.multi_file => {
            let filename = input.file_name().expect("input has no filename");
            let name: OsString = if is_decompress {
                let p = Path::new(filename);
                if p.extension().map(|e| e == "fz").unwrap_or(false) {
                    p.with_extension("").into_os_string()
                } else {
                    filename.to_owned()
                }
            } else {
                let mut s = filename.to_owned();
                s.push(".fz");
                s
            };
            PathBuf::from(dir).join(name)
        }
        Some(p) => p.to_path_buf(),
        None if is_decompress => {
            if input.extension().map(|e| e == "fz").unwrap_or(false) {
                input.with_extension("")
            } else {
                input.to_path_buf()
            }
        }
        None => {
            let mut s: OsString = input.as_os_str().to_owned();
            s.push(".fz");
            PathBuf::from(s)
        }
    }
}

fn main() -> ExitCode {
    let Cli {
        command,
        verbose,
        jobs,
    } = Cli::parse();

    // Size the global rayon pool that `process_files` uses to run files in
    // parallel. 0 leaves rayon's own default (core count); any other value caps
    // the worker threads. `build_global` only fails if called twice, so the
    // error is irrelevant here.
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build_global();

    match command {
        Command::Compress(a) => run_compress_decompress(
            false,
            a.keep,
            a.force,
            a.algorithm.into(),
            a.output,
            a.files,
            verbose,
        ),
        Command::Decompress(a) => run_compress_decompress(
            true,
            a.keep,
            a.force,
            CompressionType::Rice1,
            a.output,
            a.files,
            verbose,
        ),
        Command::Debayer(a) => run_debayer(a, verbose),
        Command::Stretch(a) => run_stretch(a, verbose),
        Command::SplitChannel(a) => run_split_channel(a, verbose),
        Command::Info(a) => run_info(a, verbose),
        Command::Preview(a) => run_preview(a, verbose),
    }
}

/// Run `process` over every input file in parallel (one rayon task per file,
/// each file being fully independent), printing per-file errors and mapping the
/// overall outcome to an exit code. Errors don't abort the batch. A single
/// input runs inline on the current thread, avoiding any thread-pool overhead.
fn process_files(files: &[PathBuf], process: impl Fn(&Path) -> Result<()> + Sync) -> ExitCode {
    if files.is_empty() {
        eprintln!("fitz: no files given");
        return ExitCode::FAILURE;
    }

    let run = |path: &Path| {
        if let Err(e) = process(path) {
            eprintln!("fitz: {}: {e:#}", path.display());
            true
        } else {
            false
        }
    };

    let had_error = if files.len() == 1 {
        run(&files[0])
    } else {
        files
            .par_iter()
            .map(|p| run(p))
            .reduce(|| false, |a, b| a || b)
    };

    if had_error {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn run_compress_decompress(
    is_decompress: bool,
    keep: bool,
    force: bool,
    algorithm: CompressionType,
    output: Option<PathBuf>,
    files: Vec<PathBuf>,
    verbose: bool,
) -> ExitCode {
    if !files.is_empty() && output.is_some() && files.len() != 1 {
        eprintln!("fitz: -o requires exactly one input file");
        return ExitCode::FAILURE;
    }

    let opts = Options {
        keep,
        force,
        verbose,
        output,
        algorithm,
        multi_file: files.len() > 1,
    };

    process_files(&files, |path| {
        let output = output_path(path, &opts, is_decompress);
        if is_decompress {
            decompress_file(path, &output, &opts)
        } else {
            compress_file(path, &output, &opts)
        }
    })
}

fn run_debayer(args: DebayerArgs, verbose: bool) -> ExitCode {
    let DebayerArgs {
        force,
        bpp,
        pattern,
        force_demosaic,
        format,
        output,
        files,
    } = args;

    let opts = DebayerOptions {
        force,
        verbose,
        bpp,
        pattern: pattern.map(Into::into),
        force_demosaic,
        format,
        output,
        multi_file: files.len() > 1,
    };

    process_files(&files, |path| {
        let output = debayer_output_path(path, &opts)?;
        debayer_file(path, &output, &opts)
    })
}

fn run_stretch(args: StretchArgs, verbose: bool) -> ExitCode {
    let StretchArgs {
        force,
        linked_channel,
        pattern,
        force_demosaic,
        format,
        output,
        files,
    } = args;

    let opts = StretchOptions {
        force,
        verbose,
        linked: linked_channel,
        pattern: pattern.map(Into::into),
        force_demosaic,
        format,
        output,
        multi_file: files.len() > 1,
    };

    process_files(&files, |path| {
        let output = stretch_output_path(path, &opts)?;
        stretch_file(path, &output, &opts)
    })
}

fn run_split_channel(args: SplitChannelArgs, verbose: bool) -> ExitCode {
    let SplitChannelArgs {
        force,
        format,
        pattern,
        force_demosaic,
        r_prefix,
        r_dir,
        g_prefix,
        g_dir,
        b_prefix,
        b_dir,
        files,
    } = args;

    let opts = SplitChannelOptions {
        force,
        verbose,
        format,
        pattern: pattern.map(Into::into),
        force_demosaic,
        r_prefix,
        r_dir,
        g_prefix,
        g_dir,
        b_prefix,
        b_dir,
    };

    process_files(&files, |path| split_channel_file(path, &opts))
}

fn run_info(args: InfoArgs, verbose: bool) -> ExitCode {
    let InfoArgs { pixel, files } = args;
    let opts = InfoOptions { verbose, pixel };
    process_files(&files, |path| info_file(path, &opts))
}

/// Unlike the other commands, `preview` renders to the terminal and so accepts
/// exactly one file (enforced by clap's single `PathBuf` argument).
fn run_preview(args: PreviewArgs, verbose: bool) -> ExitCode {
    let PreviewArgs {
        linked_channel,
        pattern,
        force_demosaic,
        graphics,
        truecolor,
        file,
    } = args;

    let opts = PreviewOptions {
        verbose,
        linked: linked_channel,
        pattern: pattern.map(Into::into),
        force_demosaic,
        force_kitty: graphics,
        force_truecolor: truecolor,
    };

    if let Err(e) = preview_file(&file, &opts) {
        eprintln!("fitz: {}: {e:#}", file.display());
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(output: Option<&str>, multi_file: bool) -> Options {
        Options {
            output: output.map(PathBuf::from),
            multi_file,
            ..Options::default()
        }
    }

    #[test]
    fn compress_default_appends_fz() {
        let p = output_path(Path::new("/data/image.fit"), &opts(None, false), false);
        assert_eq!(p, PathBuf::from("/data/image.fit.fz"));
    }

    #[test]
    fn compress_explicit_output_used_as_is() {
        let p = output_path(
            Path::new("/data/image.fit"),
            &opts(Some("/out/result.fz"), false),
            false,
        );
        assert_eq!(p, PathBuf::from("/out/result.fz"));
    }

    #[test]
    fn compress_multi_file_joins_filename_into_dir() {
        let p = output_path(
            Path::new("/data/image.fit"),
            &opts(Some("/out"), true),
            false,
        );
        assert_eq!(p, PathBuf::from("/out/image.fit.fz"));
    }

    #[test]
    fn decompress_strips_fz_extension() {
        let p = output_path(Path::new("/data/image.fits.fz"), &opts(None, false), true);
        assert_eq!(p, PathBuf::from("/data/image.fits"));
    }

    #[test]
    fn decompress_no_fz_extension_returns_input_for_inplace() {
        let p = output_path(Path::new("/data/image.fits"), &opts(None, false), true);
        assert_eq!(p, PathBuf::from("/data/image.fits"));
    }

    #[test]
    fn decompress_explicit_output_used_as_is() {
        let p = output_path(
            Path::new("/data/image.fits.fz"),
            &opts(Some("/out/result.fits"), false),
            true,
        );
        assert_eq!(p, PathBuf::from("/out/result.fits"));
    }

    #[test]
    fn decompress_multi_file_strips_fz_into_dir() {
        let p = output_path(
            Path::new("/data/image.fits.fz"),
            &opts(Some("/out"), true),
            true,
        );
        assert_eq!(p, PathBuf::from("/out/image.fits"));
    }

    #[test]
    fn decompress_multi_file_no_fz_ext_keeps_filename() {
        let p = output_path(
            Path::new("/data/image.fits"),
            &opts(Some("/out"), true),
            true,
        );
        assert_eq!(p, PathBuf::from("/out/image.fits"));
    }

    fn debayer_opts(
        format: OutputFormat,
        output: Option<&str>,
        multi_file: bool,
    ) -> DebayerOptions {
        DebayerOptions {
            format,
            output: output.map(PathBuf::from),
            multi_file,
            ..DebayerOptions::default()
        }
    }

    #[test]
    fn debayer_default_appends_debayer_suffix_with_fits_extension() {
        let p = debayer_output_path(
            Path::new("/data/image.fit"),
            &debayer_opts(OutputFormat::Fits, None, false),
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/data/image_debayer.fits"));
    }

    #[test]
    fn debayer_default_appends_debayer_suffix_with_tiff_extension() {
        let p = debayer_output_path(
            Path::new("/data/image.fit"),
            &debayer_opts(OutputFormat::Tiff, None, false),
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/data/image_debayer.tiff"));
    }

    #[test]
    fn debayer_explicit_output_used_as_is() {
        let p = debayer_output_path(
            Path::new("/data/image.fit"),
            &debayer_opts(OutputFormat::Fits, Some("/out/result.fits"), false),
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/out/result.fits"));
    }

    #[test]
    fn debayer_multi_file_joins_stem_with_format_extension_into_dir() {
        let p = debayer_output_path(
            Path::new("/data/image.fit"),
            &debayer_opts(OutputFormat::Fits, Some("/out"), true),
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/out/image.fits"));
    }

    #[test]
    fn debayer_output_path_errors_when_input_has_no_filename() {
        let err = debayer_output_path(
            Path::new("/"),
            &debayer_opts(OutputFormat::Fits, None, false),
        )
        .unwrap_err();
        assert!(err.to_string().contains("no file name"));
    }
}
