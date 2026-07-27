# libfitz

The goal of the `libfitz` library is to handle common operations with FITS files perfromed by fitz and FitSmith:

 - read fits files:
   - read(path) -> (headers, pixels)
 - debayer(pixels: CFA) -> RGB pixel planes
 - CFA -> RGB without debayering - read pixels corresponding to each color and create a pixel plane for each. Green contains twice as many pixels as others, so it can be rescaled
 - autostretch(pixels: CFA|RGB) -> CFA|RGB
 - read/calculate additional file information:
   - focal length
   - pixel size
   - binning
   - calculated sampling arcsec/µm
 - calculate stats:
   - mean
   - median
   - sigma
   - avg deviation
   - MAD
   - min value (number of pixels with min value)
   - max value (number of pixels with max value)
 - star detection
 - star metrics:
   - star count
   - HFR
   - FWHM
   - eccentricity

Internal pixel format in memory:

 - u16
 - f32

Everything else is converted to these two.

u8, i16, u16 -> u16
i32, u32, f32, f63 -> f32

Each operation is performed on ONE image only. `libfitz` by itself doesn't do caching of images - it is responsibility of the user

## operations

### load_from_file

```rust
fn load_from_file(path: &str) -> Result<(Pixels, Headers), FitError>
```

loads and decompresses (if necessary) from the file. Returns Pixels (either grayscale, bayer or RGB) and FITS headers as a separate objects

### debayer

```rust
fn debayer(pixels: &Image) -> Result<Image, FitError>
```

Accepts bayer pattern pixels (1 channel, u16) and produces 3 channel, f32 pixels
