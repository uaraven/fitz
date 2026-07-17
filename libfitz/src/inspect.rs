//! Geometry and cropping for the aberration inspector: from a rendered RGBA8
//! image, carve the nine fixed regions (four corners, four edge midpoints, the
//! center) a corner-to-corner focus/aberration check compares at a glance.
//!
//! Pure image math with no FITS or GUI dependency — it operates on the
//! interleaved RGBA8 buffer the preview pipeline already produces
//! ([`crate::preview::PreviewImage`]), so a GUI frontend crops the resident
//! preview with no re-read, and the geometry (the off-by-one-prone part) is
//! unit-testable with synthetic dimensions.

/// The tile side length as a fraction of the shorter frame axis: each region is
/// ~10% of the frame.
const TILE_FRACTION: f64 = 0.10;
/// Upper bound on the tile side, so a large sensor still yields tiles that fit
/// nine-up on screen at 1:1.
const TILE_MAX: usize = 256;

/// A square crop taken from a rendered RGBA8 image.
pub struct Tile {
    /// Side length in pixels.
    pub size: usize,
    /// Interleaved (R, G, B, A) bytes, `size * size * 4` long.
    pub rgba8: Vec<u8>,
}

/// The side length of each aberration tile for a `width × height` frame:
/// `min(round(0.10 * min(width, height)), 256)`, floored at 1. Using the
/// shorter axis keeps every tile square and the 10% rule meaningful on a
/// non-square sensor.
pub fn aberration_tile_size(width: usize, height: usize) -> usize {
    let short = width.min(height);
    let tenth = (TILE_FRACTION * short as f64).round() as usize;
    tenth.min(TILE_MAX).min(short).max(1)
}

/// The nine `sz × sz` region origins over a `width × height` frame, row-major:
/// TL, TC, TR, ML, C, MR, BL, BC, BR. Corners sit flush to each corner, edge
/// midpoints center along each side, and the center centers on the frame. Every
/// origin is clamped so its `sz × sz` rect stays inside the frame; if `sz`
/// exceeds an axis (pathological), that origin is 0 and tiles overlap.
pub fn aberration_regions(width: usize, height: usize, sz: usize) -> [(usize, usize); 9] {
    let xs = [0, (width.saturating_sub(sz)) / 2, width.saturating_sub(sz)];
    let ys = [
        0,
        (height.saturating_sub(sz)) / 2,
        height.saturating_sub(sz),
    ];
    let mut regions = [(0usize, 0usize); 9];
    for (row, &y) in ys.iter().enumerate() {
        for (col, &x) in xs.iter().enumerate() {
            regions[row * 3 + col] = (x, y);
        }
    }
    regions
}

/// Copy one `sz × sz` tile out of an interleaved RGBA8 buffer whose rows are
/// `width` pixels wide. The rect `(x, y)`..`(x + sz, y + sz)` must lie within
/// the `width × height` image (as [`aberration_regions`] guarantees); rows are
/// copied honoring the source stride.
pub fn crop_rgba8(src: &[u8], width: usize, height: usize, x: usize, y: usize, sz: usize) -> Tile {
    debug_assert_eq!(src.len(), width * height * 4);
    debug_assert!(x + sz <= width && y + sz <= height);
    let mut rgba8 = vec![0u8; sz * sz * 4];
    for row in 0..sz {
        let src_start = ((y + row) * width + x) * 4;
        let dst_start = row * sz * 4;
        rgba8[dst_start..dst_start + sz * 4].copy_from_slice(&src[src_start..src_start + sz * 4]);
    }
    Tile { size: sz, rgba8 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_size_applies_the_ten_percent_rule_on_the_shorter_axis() {
        // 10% of the shorter axis (2000), well under the cap.
        assert_eq!(aberration_tile_size(3000, 2000), 200);
        // Rounds to nearest: 0.10 * 1234 = 123.4 → 123.
        assert_eq!(aberration_tile_size(1234, 4000), 123);
    }

    #[test]
    fn tile_size_is_capped_at_256() {
        // 10% would be 600, but the cap holds it to 256.
        assert_eq!(aberration_tile_size(6000, 6000), 256);
    }

    #[test]
    fn tile_size_stays_within_a_tiny_frame() {
        // A frame smaller than one tile still yields a positive, in-frame size.
        assert_eq!(aberration_tile_size(4, 4), 1);
        assert_eq!(aberration_tile_size(30, 30), 3);
        assert!(aberration_tile_size(1, 1) >= 1);
    }

    #[test]
    fn regions_are_the_nine_positions_in_row_major_order() {
        // 100×80 frame, SZ=10: corners flush, midpoints centered, center centered.
        let r = aberration_regions(100, 80, 10);
        assert_eq!(
            r,
            [
                (0, 0),
                (45, 0),
                (90, 0), // top row
                (0, 35),
                (45, 35),
                (90, 35), // middle row
                (0, 70),
                (45, 70),
                (90, 70), // bottom row
            ]
        );
    }

    #[test]
    fn regions_stay_inside_the_frame() {
        // Odd dimensions and a nontrivial SZ: every origin keeps its rect in-frame.
        let (w, h, sz) = (101, 67, 13);
        for (x, y) in aberration_regions(w, h, sz) {
            assert!(x + sz <= w, "x={x} sz={sz} w={w}");
            assert!(y + sz <= h, "y={y} sz={sz} h={h}");
        }
    }

    #[test]
    fn regions_clamp_to_origin_when_the_tile_exceeds_the_frame() {
        // Pathological: SZ larger than the frame → every origin collapses to 0
        // (tiles overlap) instead of underflowing.
        assert_eq!(aberration_regions(5, 5, 8), [(0, 0); 9]);
    }

    #[test]
    fn crop_extracts_the_right_sub_rectangle() {
        // 4×4 RGBA8 image whose red channel encodes the pixel index (row*4+col);
        // green/blue/alpha are constant markers.
        let width = 4;
        let height = 4;
        let mut src = Vec::with_capacity(width * height * 4);
        for i in 0..(width * height) as u8 {
            src.extend_from_slice(&[i, 100, 200, 255]);
        }

        // A 2×2 crop at (1, 1) covers indices 5,6 (row 1) and 9,10 (row 2).
        let tile = crop_rgba8(&src, width, height, 1, 1, 2);
        assert_eq!(tile.size, 2);
        assert_eq!(tile.rgba8.len(), 2 * 2 * 4);
        let reds: Vec<u8> = tile.rgba8.chunks_exact(4).map(|p| p[0]).collect();
        assert_eq!(reds, [5, 6, 9, 10]);
        // The constant channels survive the copy.
        assert!(
            tile.rgba8
                .chunks_exact(4)
                .all(|p| p[1..] == [100, 200, 255])
        );
    }

    #[test]
    fn crop_of_a_corner_tile_reads_the_frame_edge() {
        let width = 4;
        let height = 4;
        let mut src = Vec::with_capacity(width * height * 4);
        for i in 0..(width * height) as u8 {
            src.extend_from_slice(&[i, 0, 0, 255]);
        }
        // Bottom-right 1×1 tile is the last pixel, index 15.
        let tile = crop_rgba8(&src, width, height, 3, 3, 1);
        assert_eq!(tile.rgba8, [15, 0, 0, 255]);
    }
}
