mod compress;
mod decompress;
mod options;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use fitskit::CompressionType;

use compress::compress_file;
use decompress::decompress_file;
use options::Options;

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

#[derive(Parser)]
#[command(
    name = "fitz",
    about = "Compress/decompress FITS files using tile compression",
    long_about = "Compress FITS files to .fz (tile-compressed) or decompress .fz back to FITS.\n\
                  Similar to gzip: compresses by default, -d to decompress.\n\
                  Output file replaces the input unless -k is given.\n\
                  Supported algorithms: rice1 (default), gzip1, gzip2."
)]
struct Args {
    /// Decompress (auto-detected when input ends with .fz)
    #[arg(short = 'd', long)]
    decompress: bool,

    /// Keep original file after compression/decompression
    #[arg(short = 'k', long)]
    keep: bool,

    /// Overwrite output file if it already exists
    #[arg(short = 'f', long)]
    force: bool,

    /// Print each file being processed
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Compression algorithm (ignored when decompressing)
    #[arg(short = 'a', long, default_value = "rice1")]
    algorithm: Algorithm,

    /// Write output to this file (only valid with a single input file)
    #[arg(short = 'o', long)]
    output: Option<PathBuf>,

    /// FITS files to process
    files: Vec<PathBuf>,
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
    let Args { decompress, keep, force, verbose, algorithm, output, files } = Args::parse();

    if files.is_empty() {
        eprintln!("fitz: no files given");
        return ExitCode::FAILURE;
    }

    if output.is_some() && files.len() != 1 {
        eprintln!("fitz: -o requires exactly one input file");
        return ExitCode::FAILURE;
    }

    let opts = Options { keep, force, verbose, output, 
        algorithm: algorithm.into(),
    multi_file: files.len() > 1 };

    let mut had_error = false;

    for path in &files {
        let do_decompress = decompress || path.extension().map(|e| e == "fz").unwrap_or(false);
        let output = output_path(path, &opts, do_decompress);

        let result = if do_decompress {
            decompress_file(path, &output, &opts)
        } else {
            compress_file(path, &output, &opts)
        };

        if let Err(e) = result {
            eprintln!("fitz: {}: {e:#}", path.display());
            had_error = true;
        }
    }

    if had_error { ExitCode::FAILURE } else { ExitCode::SUCCESS }
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
        let p = output_path(Path::new("/data/image.fit"), &opts(Some("/out/result.fz"), false), false);
        assert_eq!(p, PathBuf::from("/out/result.fz"));
    }

    #[test]
    fn compress_multi_file_joins_filename_into_dir() {
        let p = output_path(Path::new("/data/image.fit"), &opts(Some("/out"), true), false);
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
        let p = output_path(Path::new("/data/image.fits.fz"), &opts(Some("/out/result.fits"), false), true);
        assert_eq!(p, PathBuf::from("/out/result.fits"));
    }

    #[test]
    fn decompress_multi_file_strips_fz_into_dir() {
        let p = output_path(Path::new("/data/image.fits.fz"), &opts(Some("/out"), true), true);
        assert_eq!(p, PathBuf::from("/out/image.fits"));
    }

    #[test]
    fn decompress_multi_file_no_fz_ext_keeps_filename() {
        let p = output_path(Path::new("/data/image.fits"), &opts(Some("/out"), true), true);
        assert_eq!(p, PathBuf::from("/out/image.fits"));
    }
}
