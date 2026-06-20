# fitz

FITS file utility.

Supports following operations on FITS files:
 - compression using RICE_1 and GZIP algorithms
 - decompression using the same algorithms
 - debayering a mosaic image and saving it as a FITS  or TIFF file
 - Split FITS file into separate per-channel R,G,B files, debayering if needed. 

## Usage

```shell
fitz [options] COMMAND [command-options]
```

`options`:
 - `-v`, `--verbose` - print each file being processed

`COMMAND` - one of the following:
 - `compress` to compress the FITS file. Use `fitz compress --help` to see more options
 - `decompress` to decompress the compressed FITS file. Use `fitz decompress --help` to see more options
 - `debayer` to debayer a FITS mosaic image and save it as a FITS or TIFF file. Use `fitz debayer --help` to see more options
 - `split` to debayer a FITS mosaic image (or split an already-debayered RGB cube) and save each color channel as a separate FITS file. Use `fitz split --help` to see more options

### compress

```
Usage: fitz compress [OPTIONS] [FILES]...

Arguments:
  [FILES]...  FITS files to compress

Options:
  -k, --keep                   Keep original file after compression
  -f, --force                  Overwrite output file if it already exists
  -a, --algorithm <ALGORITHM>  Compression algorithm [default: rice1] [possible values: rice1, gzip1, gzip2]
  -o, --output <OUTPUT>        Write output to this file (only valid with a single input file)
  -v, --verbose                Print each file being processed
  -h, --help                   Print help
```

### decompress

```
Usage: fitz decompress [OPTIONS] [FILES]...

Arguments:
  [FILES]...  FITS files to decompress

Options:
  -k, --keep             Keep original file after decompression
  -f, --force             Overwrite output file if it already exists
  -o, --output <OUTPUT>  Write output to this file (only valid with a single input file)
  -v, --verbose          Print each file being processed
  -h, --help             Print help
```

### debayer

Debayers a FITS mosaic image using [libbayer](https://github.com/wangds/libbayer) and saves it as a FITS (3-plane RGB cube) or TIFF file. The Bayer pattern is taken from `--pattern` when given, and otherwise read from the FITS `BAYERPAT` header keyword. If the input has no `BAYERPAT` header but is already a 3-plane RGB cube (`NAXIS3=3`), the demosaic step is skipped and a notice is printed to stdout. Pass `--force-demosaic` if an input is actually a raw mosaic that happens to have 3 planes for some other reason, so it isn't silently treated as RGB; this requires a Bayer pattern from either `--pattern` or the header.

When `-o`/`--output` is not given, the output file is named `{input-stem}_debayer.{ext}` next to the input, where `ext` is `fits` or `tiff` depending on `--format`.

FITS output always preserves the source image's pixel format (its `BITPIX` and `BSCALE`/`BZERO` scaling), so `--bpp` only affects TIFF output, which is written as 8-, 16-, or 32-bit unsigned integers.

```
Usage: fitz debayer [OPTIONS] [FILES]...

Arguments:
  [FILES]...  FITS files to debayer

Options:
  -f, --force              Overwrite output file if it already exists
      --bpp <BPP>          Bits per pixel in the output image (TIFF only; FITS keeps the source format) [default: 16] (8, 16 or 32)
      --pattern <PATTERN>  Bayer pattern of the sensor; if omitted, read from the FITS BAYERPAT header [possible values: RGGB, GBRG, BGGR, GRBG]
      --force-demosaic     Always demosaic, even if the input looks like an already-debayered RGB cube
      --format <FORMAT>    Output file format [default: fits] (TIFF or FITS)
  -o, --output <OUTPUT>    Write output to this file, or to this folder if processing multiple files
  -v, --verbose            Print each file being processed
  -h, --help               Print help
```

### split

Debayers a FITS mosaic image and saves each color channel as a separate FITS file. If the input has no `BAYERPAT` header, it's assumed to already be a 3-plane RGB cube (`NAXIS3=3`) and the debayer step is skipped. Pass `--force-demosaic` (with `--pattern`, since there's no header to read it from) if an input is actually a raw mosaic that happens to have 3 planes for some other reason.

`--r-prefix`/`--r-dir` (and the `g`/`b` equivalents) are mutually exclusive. If none of the six prefix/dir options are given, all three channels are saved next to the input file using the default `R-`/`G-`/`B-` prefixes. If any are given, only the explicitly configured channels are saved. In directory mode the original filename is kept unchanged (use distinct directories per channel to avoid one channel overwriting another).

```
Usage: fitz split [OPTIONS] [FILES]...

Arguments:
  [FILES]...  FITS files to split into channels

Options:
  -f, --force                Overwrite output files if they already exist
      --format <FORMAT>      Pixel format of the resulting per-channel FITS files [default: i16] [possible values: i8, i16, i32, f32, f64]
      --pattern <PATTERN>    Bayer pattern of the sensor; if omitted, read from the FITS BAYERPAT header [possible values: RGGB, GBRG, BGGR, GRBG]
      --force-demosaic       Always demosaic, even if the input looks like an already-debayered RGB cube
      --r-prefix <R_PREFIX>  Prefix for the red channel file: {prefix}-{original-file-name}
      --r-dir <R_DIR>        Directory to save the red channel file into (original filename kept)
      --g-prefix <G_PREFIX>  Prefix for the green channel file: {prefix}-{original-file-name}
      --g-dir <G_DIR>        Directory to save the green channel file into (original filename kept)
      --b-prefix <B_PREFIX>  Prefix for the blue channel file: {prefix}-{original-file-name}
      --b-dir <B_DIR>        Directory to save the blue channel file into (original filename kept)
  -v, --verbose              Print each file being processed
  -h, --help                 Print help
```

## Warning

This tool was mostly vibe-coded. I reviewed the code and made some changes, but still most of the authorship goes to those anonymous heroes who write the code, on which anthropic trains their models.
