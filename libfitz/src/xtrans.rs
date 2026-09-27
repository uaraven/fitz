//! The X-Trans sensor's 6x6 repeating colour-filter tile, and its string
//! encoding for the `BAYERPAT` header keyword.
//!
//! Unlike the four fixed 2x2 Bayer patterns (see [`crate::fits_bayer`]),
//! different X-Trans cameras (and even different firmware) lay out their 6x6
//! tile differently — LibRaw parses it per-file from the `XTransLayout` EXIF
//! tag — so the pattern has to travel with the [`crate::data::Image`] rather
//! than being inferred from a fixed table.

use crate::data::PixelBuffer;
use anyhow::{Result, bail};
use rayon::prelude::*;

/// One CFA colour sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XTransColor {
    Red,
    Green,
    Blue,
}

/// The X-Trans sensor's 6x6 repeating colour tile, row-major: `pattern[row][col]`.
/// Re-exported (see [`crate::data::ImageType::XTrans`]) since it's part of that
/// public enum's payload.
pub type XTransPattern = [[XTransColor; 6]; 6];

/// The colour at `(row, col)`, wrapping every 6 pixels — the tile repeats
/// across the whole sensor, exactly like a 2x2 Bayer pattern's cell.
pub(crate) fn color_at(pattern: &XTransPattern, row: usize, col: usize) -> XTransColor {
    pattern[row % 6][col % 6]
}

/// Resolve LibRaw's raw per-cell colour-index table (`idata.xtrans`) through
/// its colour-index-to-letter map (`idata.cdesc`, e.g. `b"RGBG"`) into a
/// canonical [`XTransPattern`].
pub(crate) fn pattern_from_libraw(table: [[i8; 6]; 6], cdesc: [i8; 5]) -> Result<XTransPattern> {
    let color = |idx: i8| -> Result<XTransColor> {
        match cdesc.get(idx as usize).copied() {
            Some(c) if c == b'R' as i8 => Ok(XTransColor::Red),
            Some(c) if c == b'G' as i8 => Ok(XTransColor::Green),
            Some(c) if c == b'B' as i8 => Ok(XTransColor::Blue),
            other => bail!("Unsupported X-Trans colour index: {other:?}"),
        }
    };

    let mut pattern = [[XTransColor::Green; 6]; 6];
    for (row, cells) in table.iter().enumerate() {
        for (col, &idx) in cells.iter().enumerate() {
            pattern[row][col] = color(idx)?;
        }
    }
    Ok(pattern)
}

/// Encode a pattern as the 36-character row-major `R`/`G`/`B` string Siril and
/// other astro tools already use for an X-Trans frame's `BAYERPAT`.
pub(crate) fn pattern_str(pattern: &XTransPattern) -> String {
    pattern
        .iter()
        .flat_map(|row| row.iter())
        .map(|c| match c {
            XTransColor::Red => 'R',
            XTransColor::Green => 'G',
            XTransColor::Blue => 'B',
        })
        .collect()
}

/// Parse a 36-character `R`/`G`/`B` string back into a pattern. `None` if the
/// length is wrong or a character isn't one of `R`/`G`/`B`.
pub(crate) fn parse_pattern(s: &str) -> Option<XTransPattern> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() != 36 {
        return None;
    }
    let mut pattern = [[XTransColor::Green; 6]; 6];
    for (i, c) in chars.into_iter().enumerate() {
        pattern[i / 6][i % 6] = match c {
            'R' => XTransColor::Red,
            'G' => XTransColor::Green,
            'B' => XTransColor::Blue,
            _ => return None,
        };
    }
    Some(pattern)
}

fn channel_index(c: XTransColor) -> usize {
    match c {
        XTransColor::Red => 0,
        XTransColor::Green => 1,
        XTransColor::Blue => 2,
    }
}

/// How far to look, in each axis, for same-colour neighbours when
/// interpolating a channel the sensor didn't sample at a given site. 5x5 is
/// generous: X-Trans's own design guarantees every 3x3 neighbourhood already
/// contains all three colours.
const SEARCH_RADIUS: i32 = 2;

/// Per (phase row, phase col, colour) same-coloured `(dy, dx)` offsets.
type OffsetTable = [[[Vec<(i32, i32)>; 3]; 6]; 6];

/// For every phase cell in the pattern's 6x6 tile and every colour, the
/// `(dy, dx)` offsets of same-coloured cells within [`SEARCH_RADIUS`]. Built
/// once per image and reused for every pixel of the same phase.
fn offset_table(pattern: &XTransPattern) -> OffsetTable {
    std::array::from_fn(|pr| {
        std::array::from_fn(|pc| {
            let mut lists: [Vec<(i32, i32)>; 3] = std::array::from_fn(|_| Vec::new());
            for dy in -SEARCH_RADIUS..=SEARCH_RADIUS {
                for dx in -SEARCH_RADIUS..=SEARCH_RADIUS {
                    let row = (pr as i32 + dy).rem_euclid(6) as usize;
                    let col = (pc as i32 + dx).rem_euclid(6) as usize;
                    lists[channel_index(pattern[row][col])].push((dy, dx));
                }
            }
            lists
        })
    })
}

/// Simple per-channel demosaic for a 6x6 X-Trans mosaic: a sensor site keeps
/// its own sampled colour, and the other two channels are the mean of nearby
/// same-coloured sites (see [`offset_table`]). Pragmatic rather than
/// high-quality (no directional/edge-aware interpolation like dcraw/LibRaw's
/// own Markesteijn algorithm) — the `bayer` crate can't help here at all,
/// since its `CFA` is hard-coded to a 2x2 pattern.
pub(crate) fn demosaic_to_rgb(
    pixels: &PixelBuffer,
    width: usize,
    height: usize,
    pattern: &XTransPattern,
) -> PixelBuffer {
    let values = pixels.as_u16ref();
    let offsets = offset_table(pattern);

    let interleaved: Vec<u16> = (0..height)
        .into_par_iter()
        .flat_map_iter(|y| {
            let values = &values;
            let offsets = &offsets;
            (0..width).flat_map(move |x| {
                let native = color_at(pattern, y, x);
                let (pr, pc) = (y % 6, x % 6);
                [XTransColor::Red, XTransColor::Green, XTransColor::Blue].map(|channel| {
                    if channel == native {
                        values[y * width + x]
                    } else {
                        let offs = &offsets[pr][pc][channel_index(channel)];
                        if offs.is_empty() {
                            // Degenerate pattern (shouldn't happen for a real
                            // X-Trans layout): fall back to the raw sample.
                            return values[y * width + x];
                        }
                        let sum: u32 = offs
                            .iter()
                            .map(|&(dy, dx)| {
                                let ny = (y as i32 + dy).clamp(0, height as i32 - 1) as usize;
                                let nx = (x as i32 + dx).clamp(0, width as i32 - 1) as usize;
                                values[ny * width + nx] as u32
                            })
                            .sum();
                        (sum / offs.len() as u32) as u16
                    }
                })
            })
        })
        .collect();

    PixelBuffer::U16(interleaved)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real Fuji X-Trans layout (X-T2), used across these tests.
    const SAMPLE: &str = "GGRGGBGGBGGRBRGRBGGGBGGRGGRGGBRBGBRG";

    #[test]
    fn pattern_str_round_trips_through_parse_pattern() {
        let pattern = parse_pattern(SAMPLE).unwrap();
        assert_eq!(pattern_str(&pattern), SAMPLE);
    }

    #[test]
    fn parse_pattern_rejects_wrong_length() {
        assert!(parse_pattern("RGGB").is_none());
        assert!(parse_pattern(&"G".repeat(35)).is_none());
        assert!(parse_pattern(&"G".repeat(37)).is_none());
    }

    #[test]
    fn parse_pattern_rejects_unknown_characters() {
        assert!(parse_pattern(&"X".repeat(36)).is_none());
    }

    #[test]
    fn color_at_wraps_every_six_pixels() {
        let pattern = parse_pattern(SAMPLE).unwrap();
        assert_eq!(color_at(&pattern, 0, 0), color_at(&pattern, 6, 0));
        assert_eq!(color_at(&pattern, 0, 0), color_at(&pattern, 0, 6));
        assert_eq!(color_at(&pattern, 2, 3), color_at(&pattern, 8, 9));
    }

    #[test]
    fn pattern_from_libraw_resolves_cdesc_indices() {
        // LibRaw's typical X-Trans cdesc: index 0 -> R, 1 -> G, 2 -> B.
        let cdesc = [b'R' as i8, b'G' as i8, b'B' as i8, b'G' as i8, 0];
        let table = [
            [1, 1, 0, 1, 1, 2],
            [1, 1, 2, 1, 1, 0],
            [2, 0, 1, 0, 2, 1],
            [1, 1, 2, 1, 1, 0],
            [1, 1, 0, 1, 1, 2],
            [0, 2, 1, 2, 0, 1],
        ];

        let pattern = pattern_from_libraw(table, cdesc).unwrap();
        assert_eq!(pattern_str(&pattern), SAMPLE);
    }

    #[test]
    fn pattern_from_libraw_rejects_an_unresolvable_index() {
        let cdesc = [b'R' as i8, b'G' as i8, b'B' as i8, b'G' as i8, 0];
        let mut table = [[1i8; 6]; 6];
        table[0][0] = 9; // out of range for a 5-entry cdesc
        assert!(pattern_from_libraw(table, cdesc).is_err());
    }
}
