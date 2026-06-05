mod compress;
mod decompress;
mod options;

use std::path::PathBuf;
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

    let opts = Options { keep, force, verbose, output, algorithm: algorithm.into() };

    let mut had_error = false;

    for path in &files {
        let do_decompress = decompress || path.extension().map(|e| e == "fz").unwrap_or(false);

        let result = if do_decompress {
            decompress_file(path, &opts)
        } else {
            compress_file(path, &opts)
        };

        if let Err(e) = result {
            eprintln!("fitz: {}: {e:#}", path.display());
            had_error = true;
        }
    }

    if had_error { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
