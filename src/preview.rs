use std::io::{self, IsTerminal};
use std::path::Path;

use anyhow::{Context, Result};
use fitskit::FitsFile;

use crate::fits_image::{find_image_hdu, load_rgb, print_step};
use crate::kitty;
use crate::options::PreviewOptions;
use crate::stretch::auto_stretch;
use crate::terminal::{
    self, ColorMode, supports_kitty_graphics, terminal_cell_pixels, terminal_color_mode,
};

/// Assumed character-cell size in pixels when the terminal doesn't report one,
/// used only as a last-resort fallback for the kitty path. Roughly an 8x16 cell.
const FALLBACK_CELL: (u16, u16) = (8, 16);

/// Which renderer `preview_file` uses for a given run.
enum Renderer {
    /// Inline image via the kitty graphics protocol.
    Kitty,
    /// ANSI half-block pseudographics in the given color mode.
    Ansi(ColorMode),
}

/// Decide how to render: explicit `--truecolor`/`--kitty-graphics` flags win,
/// otherwise probe for kitty support (only when stdout is a TTY) and fall back
/// to the auto-detected ANSI color mode.
fn choose_renderer(opts: &PreviewOptions) -> Renderer {
    if opts.force_truecolor {
        Renderer::Ansi(ColorMode::TrueColor)
    } else if opts.force_kitty || (io::stdout().is_terminal() && supports_kitty_graphics()) {
        Renderer::Kitty
    } else {
        Renderer::Ansi(terminal_color_mode())
    }
}

/// Render `input` to the terminal: load the image (debayering if needed),
/// auto-stretch it, downscale it to fit the terminal, and print it as ANSI
/// half-block "pixels" colored with either 256-color or true-color codes.
pub(crate) fn preview_file(input: &Path, opts: &PreviewOptions) -> Result<()> {
    print_step(opts.verbose, "reading");
    let fits =
        FitsFile::from_file(input).with_context(|| format!("cannot read {}", input.display()))?;

    let (header, img) = find_image_hdu(&fits, input, opts.verbose)?;
    let img = img.as_ref();

    let (width, height, rgb) = load_rgb(
        header,
        img,
        input,
        opts.pattern,
        opts.force_demosaic,
        opts.verbose,
    )?;

    print_step(opts.verbose, "stretching");
    let stretched = auto_stretch(&rgb, opts.linked);

    print_step(opts.verbose, "scaling");
    let (cols, rows) = terminal::terminal_dimensions();
    match choose_renderer(opts) {
        Renderer::Kitty => {
            print_step(opts.verbose, "kitty");
            // Fit the image into the terminal's pixel canvas: the cell grid times
            // each cell's pixel size. When the terminal doesn't report cell
            // pixels, fall back to an assumed cell size. One column is reserved
            // so the image never wraps to the next line.
            let (cw, ch) = terminal_cell_pixels().unwrap_or(FALLBACK_CELL);
            let max_w = (cols.saturating_sub(1)) as usize * cw as usize;
            let max_h = rows as usize * ch as usize;
            let (pw, ph, preview) = scale_rgb_to_fit(&stretched, width, height, max_w, max_h);
            let rgb8 = kitty::rgb16_to_rgb8(&preview);
            print!("{}", kitty::encode_image(&rgb8, pw, ph));
            println!();
        }
        Renderer::Ansi(mode) => {
            // Terminal cells are roughly twice as tall as they are wide; with
            // half-block rendering each cell stacks two pixels vertically, so the
            // usable pixel canvas is `cols` wide and `rows * 2` tall. Fit the
            // stretched image into that box, preserving its aspect ratio.
            let (pw, ph, preview) = scale_rgb_to_fit(
                &stretched,
                width,
                height,
                (cols - 1) as usize,
                rows as usize * 2,
            );
            println!("{}", convert_to_ansi(&preview, pw, ph, mode));
        }
    }
    Ok(())
}

/// Render an interleaved 16-bit RGB image as ANSI text. Each character cell
/// stacks two vertical pixels: the upper pixel is painted as the cell's
/// background and the lower as the foreground of a lower-half-block (`▄`).
fn convert_to_ansi(src: &[u16], width: usize, height: usize, mode: ColorMode) -> String {
    if mode == ColorMode::BW {
        return String::from("Terminal doesn't support required color mode");
    }
    let mut result = String::new();
    for y in (0..height).step_by(2) {
        for x in 0..width {
            let top = (y * width + x) * 3;
            result.push_str(&color_to_ansi(
                true,
                src[top],
                src[top + 1],
                src[top + 2],
                mode,
            ));
            // The lower half-block. A dangling final row (odd height) has no
            // pixel below it, so reuse the top color to fill the cell solidly.
            let bottom = if y + 1 < height {
                ((y + 1) * width + x) * 3
            } else {
                top
            };
            result.push_str(&color_to_ansi(
                false,
                src[bottom],
                src[bottom + 1],
                src[bottom + 2],
                mode,
            ));
            result.push('▄');
        }
        // Reset colors at the end of each row so the last cell's background
        // doesn't bleed to the edge of the terminal.
        result.push_str("\x1b[0m\n");
    }
    result
}

fn scale_to_u8(input: u16) -> u32 {
    (input >> 8) as u32
}

fn color_to_ansi256(r: u16, g: u16, b: u16) -> u8 {
    let rm = r / 13107;
    let gm = g / 13107;
    let bm = b / 13107;
    (16 + 36 * rm + 6 * gm + bm) as u8
}

fn color_to_ansi(is_bg: bool, r: u16, g: u16, b: u16, mode: terminal::ColorMode) -> String {
    // Select-Graphic-Rendition parameter: 4x for background, 3x for foreground.
    let layer = if is_bg { '4' } else { '3' };
    match mode {
        ColorMode::TrueColor => format!(
            "\x1b[{layer}8;2;{};{};{}m",
            scale_to_u8(r),
            scale_to_u8(g),
            scale_to_u8(b)
        ),
        ColorMode::HiColor => format!("\x1b[{layer}8;5;{}m", color_to_ansi256(r, g, b)),
        // No color support: emit nothing rather than a bare escape introducer.
        ColorMode::BW => String::new(),
    }
}

/// Scale an interleaved 16-bit RGB image so it fits within `max_w` x `max_h`
/// while preserving its aspect ratio, returning the new `(width, height,
/// samples)`. Uses box (area-average) sampling, the right filter for the large
/// down-scales a terminal preview needs.
pub(crate) fn scale_rgb_to_fit(
    src: &[u16],
    src_w: usize,
    src_h: usize,
    max_w: usize,
    max_h: usize,
) -> (usize, usize, Vec<u16>) {
    let (dst_w, dst_h) = fit_dimensions(src_w, src_h, max_w, max_h);
    let scaled = resize_rgb(src, src_w, src_h, dst_w, dst_h);
    (dst_w, dst_h, scaled)
}

/// The largest `(width, height)` that fits inside `max_w` x `max_h` with the
/// same aspect ratio as `src_w` x `src_h`. Both dimensions are kept at least 1
/// for any non-empty source; an empty source or box maps to `(0, 0)`.
fn fit_dimensions(src_w: usize, src_h: usize, max_w: usize, max_h: usize) -> (usize, usize) {
    if src_w == 0 || src_h == 0 || max_w == 0 || max_h == 0 {
        return (0, 0);
    }
    let scale = (max_w as f64 / src_w as f64).min(max_h as f64 / src_h as f64);
    let w = ((src_w as f64 * scale).round() as usize).max(1);
    let h = ((src_h as f64 * scale).round() as usize).max(1);
    (w, h)
}

/// Resample an interleaved RGB image from `src_w` x `src_h` to `dst_w` x
/// `dst_h`. Each destination pixel is the average of the block of source pixels
/// it maps to (a box filter); the integer source spans partition the image
/// exactly when down-scaling, with no gaps or overlap. Returns an empty buffer
/// for a zero-sized target.
fn resize_rgb(src: &[u16], src_w: usize, src_h: usize, dst_w: usize, dst_h: usize) -> Vec<u16> {
    if dst_w == 0 || dst_h == 0 {
        return Vec::new();
    }
    let mut out = vec![0u16; dst_w * dst_h * 3];
    for dy in 0..dst_h {
        let sy0 = dy * src_h / dst_h;
        let sy1 = (((dy + 1) * src_h) / dst_h).max(sy0 + 1);
        for dx in 0..dst_w {
            let sx0 = dx * src_w / dst_w;
            let sx1 = (((dx + 1) * src_w) / dst_w).max(sx0 + 1);

            let (mut r, mut g, mut b, mut count) = (0u64, 0u64, 0u64, 0u64);
            for sy in sy0..sy1 {
                let row = sy * src_w;
                for sx in sx0..sx1 {
                    let i = (row + sx) * 3;
                    r += src[i] as u64;
                    g += src[i + 1] as u64;
                    b += src[i + 2] as u64;
                    count += 1;
                }
            }

            let o = (dy * dst_w + dx) * 3;
            out[o] = (r / count) as u16;
            out[o + 1] = (g / count) as u16;
            out[o + 2] = (b / count) as u16;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_data;

    #[test]
    fn fit_dimensions_is_width_limited_for_landscape() {
        // 3:2 image into a square box: width fills, height scales to match.
        assert_eq!(fit_dimensions(300, 200, 60, 60), (60, 40));
    }

    #[test]
    fn fit_dimensions_is_height_limited_for_portrait() {
        // tall image into a wide box: height fills, width scales to match.
        assert_eq!(fit_dimensions(200, 400, 100, 50), (25, 50));
    }

    #[test]
    fn fit_dimensions_keeps_at_least_one_pixel() {
        // an extreme aspect ratio must not round a dimension down to zero.
        let (w, h) = fit_dimensions(3008, 4, 80, 48);
        assert!(w >= 1 && h >= 1);
    }

    #[test]
    fn fit_dimensions_handles_empty_source() {
        assert_eq!(fit_dimensions(0, 0, 80, 24), (0, 0));
    }

    #[test]
    fn resize_rgb_averages_block_to_single_pixel() {
        // 2x2 distinct pixels averaged into one: per-channel arithmetic mean.
        let src: Vec<u16> = vec![
            0, 1, 2, 10, 11, 12, //
            20, 21, 22, 30, 31, 32,
        ];
        assert_eq!(resize_rgb(&src, 2, 2, 1, 1), vec![15, 16, 17]);
    }

    #[test]
    fn resize_rgb_preserves_solid_color() {
        let src: Vec<u16> = std::iter::repeat([7u16, 8, 9]).take(16).flatten().collect();
        let out = resize_rgb(&src, 4, 4, 2, 3);
        assert_eq!(out.len(), 2 * 3 * 3);
        assert!(out.chunks_exact(3).all(|c| c == [7, 8, 9]));
    }

    #[test]
    fn resize_rgb_upscales_without_panicking() {
        // 1x1 source replicated across a larger target.
        let out = resize_rgb(&[1, 2, 3], 1, 1, 3, 2);
        assert_eq!(out.len(), 3 * 2 * 3);
        assert!(out.chunks_exact(3).all(|c| c == [1, 2, 3]));
    }

    #[test]
    fn ansi256_maps_primaries_to_cube_corners() {
        // Black and white sit at opposite corners of the 6x6x6 color cube
        // (indices 16 and 231); pure red is the +36-per-level red axis tip.
        assert_eq!(color_to_ansi256(0, 0, 0), 16);
        assert_eq!(color_to_ansi256(u16::MAX, u16::MAX, u16::MAX), 231);
        assert_eq!(color_to_ansi256(u16::MAX, 0, 0), 16 + 36 * 5);
    }

    #[test]
    fn truecolor_emits_fg_and_bg_24bit_codes() {
        // Background uses 48;2, foreground 38;2, each with the high byte of the
        // 16-bit channel value.
        let bg = color_to_ansi(true, 0xFF00, 0x8000, 0x0100, ColorMode::TrueColor);
        assert_eq!(bg, "\x1b[48;2;255;128;1m");
        let fg = color_to_ansi(false, 0xFF00, 0x8000, 0x0100, ColorMode::TrueColor);
        assert_eq!(fg, "\x1b[38;2;255;128;1m");
    }

    #[test]
    fn hicolor_emits_256_palette_index() {
        let bg = color_to_ansi(true, 0, 0, 0, ColorMode::HiColor);
        assert_eq!(bg, "\x1b[48;5;16m");
        let fg = color_to_ansi(false, u16::MAX, u16::MAX, u16::MAX, ColorMode::HiColor);
        assert_eq!(fg, "\x1b[38;5;231m");
    }

    #[test]
    fn convert_to_ansi_handles_odd_height_without_panicking() {
        // Three rows (odd) means the final cell row has no bottom pixel; the
        // dangling-row branch must reuse the top pixel instead of indexing out
        // of bounds. One 1-wide column keeps the buffer minimal.
        let src: Vec<u16> = vec![
            1, 2, 3, // row 0
            4, 5, 6, // row 1
            7, 8, 9, // row 2
        ];
        let text = convert_to_ansi(&src, 1, 3, ColorMode::TrueColor);
        // Two cell rows (y = 0 and y = 2), each ending with a reset + newline.
        assert_eq!(text.matches("\x1b[0m\n").count(), 2);
        assert!(text.contains('▄'));
    }

    #[test]
    fn preview_real_image_runs_and_renders_cells() {
        // Full pipeline on the bundled frame: it must complete and emit at
        // least one half-block cell.
        let input = test_data("uncompressed.fit");
        let fits = FitsFile::from_file(&input).unwrap();
        let (header, img) = find_image_hdu(&fits, &input, false).unwrap();
        let img = img.as_ref();
        let (w, h, rgb) = load_rgb(header, img, &input, None, false, false).unwrap();
        let stretched = auto_stretch(&rgb, false);

        let (pw, ph, preview) = scale_rgb_to_fit(&stretched, w, h, 80, 48);
        let text = convert_to_ansi(&preview, pw, ph, ColorMode::TrueColor);
        assert!(text.contains('▄'));
    }

    #[test]
    fn scale_stretched_real_image_fits_box_and_keeps_aspect() {
        // Full pipeline on the bundled frame: load + stretch + scale to a small
        // terminal-sized box. The frame is square, so the preview must be too.
        let input = test_data("uncompressed.fit");
        let fits = FitsFile::from_file(&input).unwrap();
        let (header, img) = find_image_hdu(&fits, &input, false).unwrap();
        let img = img.as_ref();
        let (w, h, rgb) = load_rgb(header, img, &input, None, false, false).unwrap();
        let stretched = auto_stretch(&rgb, false);

        let (pw, ph, preview) = scale_rgb_to_fit(&stretched, w, h, 80, 48);
        assert!(pw <= 80 && ph <= 48);
        assert_eq!(preview.len(), pw * ph * 3);
        assert_eq!(pw, ph, "square source should yield a square preview");
    }
}
