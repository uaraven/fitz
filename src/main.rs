mod compress;
mod decompress;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use compress::compress_file;
use decompress::decompress_file;

#[derive(Parser)]
#[command(
    name = "fitz",
    about = "Compress/decompress FITS files using RICE_1 tile compression",
    long_about = "Compress FITS files to .fz (tile-compressed) or decompress .fz back to FITS.\n\
                  Similar to gzip: compresses by default, -d to decompress.\n\
                  Output file replaces the input unless -k is given."
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

    /// FITS files to process
    files: Vec<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();

    if args.files.is_empty() {
        eprintln!("fitz: no files given");
        return ExitCode::FAILURE;
    }

    let mut had_error = false;

    for path in &args.files {
        let decompress = args.decompress
            || path.extension().map(|e| e == "fz").unwrap_or(false);

        let result = if decompress {
            decompress_file(path, args.keep, args.force, args.verbose)
        } else {
            compress_file(path, args.keep, args.force, args.verbose)
        };

        if let Err(e) = result {
            eprintln!("fitz: {}: {e:#}", path.display());
            had_error = true;
        }
    }

    if had_error { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
