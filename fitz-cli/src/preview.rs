use std::fmt::Write;
// `flush()` lives on `std::io::Write`; bring it in anonymously so it doesn't
// clash with the `std::fmt::Write` used by the ANSI rendering path.
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fitz_core::fits_image::{find_image_hdu, high_byte, rgb16_to_rgb8};
use fitz_core::fitskit::FitsFile;
use fitz_core::preview::{PreviewSource, preview_rgb};
use fitz_core::resize::resize_to_fit;
use fitz_core::stretch::auto_stretch;

use crate::io_prompt::print_step;
use crate::kitty;
use crate::options::PreviewOptions;
use crate::terminal::{
    self, ColorMode, print_warning, supports_kitty_graphics, terminal_cell_pixels,
    terminal_color_mode,
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
/// otherwise probe for kitty support and fall back to the auto-detected ANSI
/// color mode. `supports_kitty_graphics` already requires a TTY on stdin/stdout,
/// so it is the single authority for that check.
fn choose_renderer(opts: &PreviewOptions) -> Renderer {
    if opts.fallback {
        Renderer::Ansi(ColorMode::HiColor)
    } else if opts.force_truecolor {
        Renderer::Ansi(ColorMode::TrueColor)
    } else if opts.force_kitty || supports_kitty_graphics() {
        Renderer::Kitty
    } else {
        Renderer::Ansi(terminal_color_mode())
    }
}

/// Render `input` to the terminal: load the image (debayering if needed),
/// auto-stretch it, downscale it to fit the terminal, and print it as ANSI
/// half-block "pixels" colored with either 256-color or true-color codes.
pub(crate) fn preview_file(input: &Path, opts: &PreviewOptions) -> Result<()> {
    let (width, height, stretched) = load_preview_pixels(input, opts)?;

    print_step(opts.verbose, "scaling");
    let (cols, rows) = terminal::terminal_dimensions();
    match choose_renderer(opts) {
        Renderer::Kitty => {
            // Fit the image into the terminal's pixel canvas: the cell grid times
            // each cell's pixel size. When the terminal doesn't report cell
            // pixels, fall back to an assumed cell size. One column is reserved
            // so the image never wraps to the next line.
            let (cw, ch) = terminal_cell_pixels().unwrap_or(FALLBACK_CELL);
            let max_w = (cols.saturating_sub(1)) as usize * cw as usize;
            let max_h = rows as usize * ch as usize;
            let (pw, ph, preview) = resize_to_fit(&stretched, width, height, max_w, max_h);
            let rgb8 = rgb16_to_rgb8(&preview);
            print!("{}", kitty::encode_image(&rgb8, pw, ph));
            println!();
            // Flush so the terminal has the whole image and can acknowledge it,
            // then swallow that acknowledgment before it leaks to the shell.
            let _ = std::io::stdout().flush();
        }
        Renderer::Ansi(ColorMode::BW) => {
            bail!("terminal does not support 216-color or true-color output");
        }
        Renderer::Ansi(mode) => {
            // Terminal cells are roughly twice as tall as they are wide; with
            // half-block rendering each cell stacks two pixels vertically, so the
            // usable pixel canvas is `cols` wide and `rows * 2` tall. Fit the
            // stretched image into that box, preserving its aspect ratio.
            let (pw, ph, preview) = resize_to_fit(
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

/// Load and stretch `input` for preview: normally debayers a raw mosaic like
/// the `stretch` command does. With `--no-debayer`, a raw (not-yet-debayered)
/// mosaic is instead shown as a stretched grayscale image using its raw sensor
/// values, skipping color interpolation entirely; an already-debayered image
/// has nothing to skip, so the flag is ignored with a warning.
fn load_preview_pixels(input: &Path, opts: &PreviewOptions) -> Result<(usize, usize, Vec<u16>)> {
    let fits =
        FitsFile::from_file(input).with_context(|| format!("cannot read {}", input.display()))?;
    let (header, img) = find_image_hdu(&fits, input)?;
    let img = img.as_ref();

    let pr = preview_rgb(
        header,
        img,
        !opts.no_debayer,
        opts.core.pattern,
        opts.core.force_demosaic,
    )?;

    // Surface how the preview was produced, matching the previous messages;
    // the plain demosaic path stays silent, as it did before.
    match pr.source {
        PreviewSource::RawMono => print_step(opts.verbose, "loading raw (no debayer)"),
        // `--no-debayer` on an image that's already debayered: it had no effect.
        PreviewSource::AlreadyDebayeredRgbCube | PreviewSource::AlreadyDebayeredMono
            if opts.no_debayer =>
        {
            print_warning(&format!(
                "{}: already debayered — ignoring --no-debayer",
                input.display()
            ));
        }
        _ => {}
    }

    // A raw-mono preview stretches its (grayscale) channels together, matching
    // the previous behavior; color previews honor the `--linked` option.
    let linked = opts.core.linked || pr.source == PreviewSource::RawMono;
    let pixels = auto_stretch(&pr.rgb, linked, opts.core.brightness);
    Ok((pr.width, pr.height, pixels))
}

/// Render an interleaved 16-bit RGB image as ANSI text. Each character cell
/// stacks two vertical pixels: the upper pixel is painted as the cell's
/// background and the lower as the foreground of a lower-half-block (`▄`).
fn convert_to_ansi(src: &[u16], width: usize, height: usize, mode: ColorMode) -> String {
    let cell_rows = height.div_ceil(2);
    // Each cell emits two color escapes (~20 bytes each) plus the block glyph,
    // and each row ends with a reset; preallocate so the buffer never reallocs.
    let mut result = String::with_capacity(cell_rows * (width * 44 + 5));
    for y in (0..height).step_by(2) {
        for x in 0..width {
            let top = (y * width + x) * 3;
            push_color_ansi(
                &mut result,
                true,
                src[top],
                src[top + 1],
                src[top + 2],
                mode,
            );
            // The lower half-block. A dangling final row (odd height) has no
            // pixel below it, so reuse the top color to fill the cell solidly.
            let bottom = if y + 1 < height {
                ((y + 1) * width + x) * 3
            } else {
                top
            };
            push_color_ansi(
                &mut result,
                false,
                src[bottom],
                src[bottom + 1],
                src[bottom + 2],
                mode,
            );
            result.push('▄');
        }
        // Reset colors at the end of each row so the last cell's background
        // doesn't bleed to the edge of the terminal.
        result.push_str("\x1b[0m\n");
    }
    result
}

/// The xterm 256-color cube's six per-channel intensity levels. The steps are
/// uneven — the 0->95 jump is far larger than the rest — so a channel must snap
/// to the *nearest* level rather than scale linearly; a linear `value / 13107`
/// pushes dim pixels up to level 1 (95/255), which on a standard cube renders as
/// a too-bright, saturated corner color. Konsole's palette hides this; iTerm2
/// and ghostty use the standard cube and expose it as speckled darks.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Index (0..=5) of the cube level nearest to an 8-bit channel value.
fn nearest_cube_level(v: u8) -> usize {
    (0..CUBE_LEVELS.len())
        .min_by_key(|&i| (CUBE_LEVELS[i] as i32 - v as i32).abs())
        .unwrap()
}

/// Map a 16-bit RGB color to the nearest xterm 256-color palette index,
/// choosing between the 6x6x6 color cube and the 24-step grayscale ramp by
/// whichever sits closer. Routing near-neutral darks to the gray ramp avoids the
/// saturated speckle the cube alone produces over the dark, slightly noisy
/// backgrounds typical of astronomical frames.
fn color_to_ansi256(r: u16, g: u16, b: u16) -> u8 {
    let (r, g, b) = (high_byte(r), high_byte(g), high_byte(b));

    // Nearest color from the 6x6x6 cube.
    let (ri, gi, bi) = (
        nearest_cube_level(r),
        nearest_cube_level(g),
        nearest_cube_level(b),
    );
    let cube_rgb = (CUBE_LEVELS[ri], CUBE_LEVELS[gi], CUBE_LEVELS[bi]);
    let cube_idx = (16 + 36 * ri + 6 * gi + bi) as u8;

    // Nearest gray from the 24-step ramp (values 8, 18, ..., 238; indices
    // 232..=255), which fills in the neutral tones the coarse cube can't.
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let gray_i = ((avg - 8 + 5) / 10).clamp(0, 23);
    let gray_val = (8 + gray_i * 10) as u8;
    let gray_idx = (232 + gray_i) as u8;

    let dist = |c: (u8, u8, u8)| {
        let dr = c.0 as i32 - r as i32;
        let dg = c.1 as i32 - g as i32;
        let db = c.2 as i32 - b as i32;
        dr * dr + dg * dg + db * db
    };

    if dist(cube_rgb) <= dist((gray_val, gray_val, gray_val)) {
        cube_idx
    } else {
        gray_idx
    }
}

/// Append the SGR color escape for one half-block straight into `out` (the
/// per-pixel hot path, so it avoids a throwaway `String` per call).
fn push_color_ansi(out: &mut String, is_bg: bool, r: u16, g: u16, b: u16, mode: ColorMode) {
    // Select-Graphic-Rendition parameter: 4x for background, 3x for foreground.
    let layer = if is_bg { '4' } else { '3' };
    // Writing into a `String` is infallible, so the `Result` can be discarded.
    match mode {
        ColorMode::TrueColor => {
            let _ = write!(
                out,
                "\x1b[{layer}8;2;{};{};{}m",
                high_byte(r),
                high_byte(g),
                high_byte(b)
            );
        }
        ColorMode::HiColor => {
            let _ = write!(out, "\x1b[{layer}8;5;{}m", color_to_ansi256(r, g, b));
        }
        // No color support: emit nothing rather than a bare escape introducer.
        ColorMode::BW => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fitz_core::stretch::{StretchOptions, load_and_stretch};

    use crate::test_support::test_data;

    /// Build a 16-bit channel value whose high byte is `v` (the byte the
    /// quantizer actually sees), mirroring `high_byte`.
    fn ch16(v: u8) -> u16 {
        ((v as u16) << 8) | v as u16
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
    fn ansi256_routes_dark_neutral_noise_to_gray_ramp() {
        // A dim, slightly color-imbalanced background pixel — the kind that used
        // to speckle into saturated low cube corners (blue/red/purple) — must
        // resolve to the neutral grayscale ramp (indices 232..=255) instead.
        let idx = color_to_ansi256(ch16(40), ch16(30), ch16(50));
        assert!(idx >= 232, "expected gray-ramp index, got {idx}");
    }

    #[test]
    fn ansi256_snaps_channels_to_nearest_cube_level() {
        // A value of 175 is exactly cube level 3; a saturated color must land on
        // the nearest cube levels, not a linearly-scaled (and too dark) bucket.
        // (175, 0, 0) -> r level 3, g/b level 0 -> 16 + 36*3.
        assert_eq!(color_to_ansi256(ch16(175), 0, 0), 16 + 36 * 3);
    }

    /// Render one half-block color escape into a fresh `String` so the
    /// per-pixel `push_color_ansi` can be asserted on in isolation.
    fn color_ansi(is_bg: bool, r: u16, g: u16, b: u16, mode: ColorMode) -> String {
        let mut s = String::new();
        push_color_ansi(&mut s, is_bg, r, g, b, mode);
        s
    }

    #[test]
    fn truecolor_emits_fg_and_bg_24bit_codes() {
        // Background uses 48;2, foreground 38;2, each with the high byte of the
        // 16-bit channel value.
        let bg = color_ansi(true, 0xFF00, 0x8000, 0x0100, ColorMode::TrueColor);
        assert_eq!(bg, "\x1b[48;2;255;128;1m");
        let fg = color_ansi(false, 0xFF00, 0x8000, 0x0100, ColorMode::TrueColor);
        assert_eq!(fg, "\x1b[38;2;255;128;1m");
    }

    #[test]
    fn hicolor_emits_256_palette_index() {
        let bg = color_ansi(true, 0, 0, 0, ColorMode::HiColor);
        assert_eq!(bg, "\x1b[48;5;16m");
        let fg = color_ansi(false, u16::MAX, u16::MAX, u16::MAX, ColorMode::HiColor);
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
        let stretched = load_and_stretch(&input, &StretchOptions::default()).unwrap();

        let (pw, ph, preview) = resize_to_fit(&stretched.pixels, stretched.width, stretched.height, 80, 48);
        let text = convert_to_ansi(&preview, pw, ph, ColorMode::TrueColor);
        assert!(text.contains('▄'));
    }

    #[test]
    fn scale_stretched_real_image_fits_box_and_keeps_aspect() {
        // Full pipeline on the bundled frame: load + stretch + scale to a small
        // terminal-sized box. The frame is square, so the preview must be too.
        let input = test_data("uncompressed.fit");
        let stretched = load_and_stretch(&input, &StretchOptions::default()).unwrap();

        let (pw, ph, preview) = resize_to_fit(&stretched.pixels, stretched.width, stretched.height, 80, 48);
        assert!(pw <= 80 && ph <= 48);
        assert_eq!(preview.len(), pw * ph * 3);
        assert_eq!(pw, ph, "square source should yield a square preview");
    }
}
