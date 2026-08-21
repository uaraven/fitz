//! Split an image into three independent per-channel [`Image`]s, debayering a
//! raw mosaic first.

use std::str::FromStr;

use crate::data::{Image, ImageType, PixelBuffer};
use crate::debayer::{blue_sites, cfa_pixels, green_sites, red_sites};
use anyhow::{Result, bail};
use fitskit::{Bitpix, Header};

/// Pixel sample type used when writing each split-out channel to FITS.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelFormat {
    I8,
    I16,
    I32,
    F32,
    F64,
}

impl FromStr for ChannelFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "i8" => Ok(ChannelFormat::I8),
            "i16" => Ok(ChannelFormat::I16),
            "i32" => Ok(ChannelFormat::I32),
            "f32" => Ok(ChannelFormat::F32),
            "f64" => Ok(ChannelFormat::F64),
            _ => Err("format must be one of: i8, i16, i32, f32, f64".to_string()),
        }
    }
}

impl From<ChannelFormat> for Bitpix {
    fn from(format: ChannelFormat) -> Self {
        match format {
            ChannelFormat::I8 => Bitpix::U8,
            ChannelFormat::I16 => Bitpix::I16,
            ChannelFormat::I32 => Bitpix::I32,
            ChannelFormat::F32 => Bitpix::F32,
            ChannelFormat::F64 => Bitpix::F64,
        }
    }
}

impl Image {
    /// Split this image into its red, green and blue channels, each a
    /// standalone grayscale [`Image`] carrying the source header.
    ///
    /// A raw mosaic is demosaiced first; an already-debayered RGB cube is split
    /// as-is. A monochrome frame has no colour channels to separate, so it is an
    /// error rather than three copies of itself.
    pub fn split_channels(&self) -> Result<[Image; 3]> {
        let debayered;
        let rgb = match self.image_type {
            ImageType::RGB => self,
            ImageType::CFA(_) => {
                debayered = self
                    .debayer()
                    .expect("a CFA image always debayers into RGB")?;
                &debayered
            }
            ImageType::Grayscale => {
                bail!("no BAYERPAT header and not a 3-plane RGB cube — no colour channels to split")
            }
        };

        let plane = |c| {
            rgb.plane(c)
                .expect("an RGB image always has three colour planes")
        };
        Ok([plane(0), plane(1), plane(2)])
    }

    /// Splits CFA bayer image into 3 channels: R, G, and B *without* debayering
    /// Resulting images are 1/2 width and height of original and contain only pixels of
    /// relevant colour
    /// The green channel is averaged from G1 and G2.
    pub fn split_cfa(&self) -> Result<[Image; 3]> {
        if let ImageType::CFA(pattern) = self.image_type {
            let (_, _, reds) = cfa_pixels(self, &red_sites(pattern));
            let (_, _, blues) = cfa_pixels(self, &blue_sites(pattern));
            let (_, _, greens) = cfa_pixels(self, &green_sites(pattern));
            let mut red_img = Image::new(
                ImageType::Grayscale,
                Header::new(),
                self.width / 2,
                self.height / 2,
                PixelBuffer::U16(reds),
            );
            red_img.merge_headers_from(self);
            let mut green_img = Image::new(
                ImageType::Grayscale,
                Header::new(),
                self.width / 2,
                self.height / 2,
                PixelBuffer::U16(greens),
            );
            green_img.merge_headers_from(self);
            let mut blue_img = Image::new(
                ImageType::Grayscale,
                Header::new(),
                self.width / 2,
                self.height / 2,
                PixelBuffer::U16(blues),
            );
            blue_img.merge_headers_from(self);
            Ok([red_img, green_img, blue_img])
        } else {
            bail!("Debayered images are not supported")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::PixelBuffer;
    use crate::fits_file::load_fits;
    use crate::test_support::{test_data, write_mosaic_fits, write_rgb_cube_fits};
    use bayer::CFA;
    use tempfile::TempDir;

    #[test]
    fn split_rgb_cube_recovers_each_plane() {
        // `write_rgb_cube_fits` lays down planar values 0..n, n..2n, 2n..3n, so
        // each split channel must come back as exactly its own run — the check
        // that the planar/interleaved conversion is right end to end.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("rgb.fits");
        write_rgb_cube_fits(&path, 4, 3);
        let n = 4 * 3;

        let channels = load_fits(&path).unwrap().split_channels().unwrap();

        for (c, channel) in channels.iter().enumerate() {
            assert_eq!(channel.image_type, ImageType::Grayscale);
            assert_eq!((channel.width, channel.height), (4, 3));
            let PixelBuffer::U16(values) = &channel.pixels else {
                panic!("an I16 source splits into u16 channels");
            };
            let expected: Vec<u16> = (0..n).map(|i| (c * n + i) as u16).collect();
            assert_eq!(values, &expected, "channel {c}");
        }
    }

    #[test]
    fn split_mosaic_debayers_first() {
        let loaded = load_fits(&test_data("cfa_orion.fits")).unwrap();
        let channels = loaded.split_channels().unwrap();

        // Each channel is the full frame, and they are genuinely different
        // colours rather than three copies of the mosaic.
        for channel in &channels {
            assert_eq!(
                (channel.width, channel.height),
                (loaded.width, loaded.height)
            );
            assert_eq!(channel.pixels.len(), loaded.width * loaded.height);
        }
        assert_ne!(channels[0].pixels, channels[2].pixels);

        // The green channel of the debayered cube, reached the other way round.
        let green = loaded.debayer().unwrap().unwrap().plane(1).unwrap();
        assert_eq!(channels[1].pixels, green.pixels);
    }

    #[test]
    fn split_rejects_a_monochrome_frame() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("mono.fits");
        write_mosaic_fits(&path, 4, 4, None);

        let err = load_fits(&path).unwrap().split_channels().unwrap_err();
        assert!(err.to_string().contains("3-plane RGB cube"));
    }

    #[test]
    fn split_cfa_extracts_half_size_channels_without_debayering() {
        // write_mosaic_fits lays sequential values 0..16 in row-major order
        // over a 4x4 RGGB mosaic:
        //   0  1  2  3
        //   4  5  6  7
        //   8  9 10 11
        //  12 13 14 15
        // RGGB sites within each 2x2 cell are R=(0,0), G=(1,0)+(0,1), B=(1,1),
        // so each output plane is computed by hand below.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("mosaic.fits");
        write_mosaic_fits(&path, 4, 4, Some("RGGB"));

        let img = load_fits(&path).unwrap();
        assert_eq!(img.image_type, ImageType::CFA(CFA::RGGB));

        let [red, green, blue] = img.split_cfa().unwrap();

        for channel in [&red, &green, &blue] {
            assert_eq!(channel.image_type, ImageType::Grayscale);
            assert_eq!((channel.width, channel.height), (2, 2));
        }

        let values = |channel: &Image| match &channel.pixels {
            PixelBuffer::U16(v) => v.clone(),
            _ => panic!("split_cfa always produces u16 channels"),
        };
        assert_eq!(values(&red), vec![0, 2, 8, 10]);
        assert_eq!(values(&green), vec![2, 4, 10, 12]);
        assert_eq!(values(&blue), vec![5, 7, 13, 15]);
    }

    #[test]
    fn split_cfa_averages_green_without_overflow() {
        // Both green sites of the one 2x2 cell are saturated (65535); the
        // averaged green output must be 65535, not a wrapped-around value from
        // summing in u16 before dividing.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("saturated.fits");
        write_mosaic_fits(&path, 2, 2, Some("RGGB"));
        {
            // write_mosaic_fits only synthesizes small sequential values, so
            // overwrite the green sites directly via the unsigned-16
            // convention (I16 sample + BZERO 32768) to get true 65535 pixels.
            let mut fits = fitskit::FitsFile::from_file(&path).unwrap();
            let hdu = fits.primary_mut();
            hdu.header.set(
                crate::keywords::BZERO,
                fitskit::HeaderValue::Float(32768.0),
                None,
            );
            let fitskit::HduData::Image(image_data) = &mut hdu.data else {
                panic!("write_mosaic_fits always writes image data");
            };
            let fitskit::PixelData::I16(pixels) = &mut image_data.pixels else {
                panic!("write_mosaic_fits always writes I16 data");
            };
            pixels[1] = i16::MAX; // (1, 0): 32767 + 32768 = 65535
            pixels[2] = i16::MAX; // (0, 1): 32767 + 32768 = 65535
            fits.to_file(&path).unwrap();
        }

        let img = load_fits(&path).unwrap();
        let [_, green, _] = img.split_cfa().unwrap();
        let PixelBuffer::U16(values) = &green.pixels else {
            panic!("split_cfa always produces u16 channels");
        };
        assert_eq!(values, &vec![65535]);
    }
}
