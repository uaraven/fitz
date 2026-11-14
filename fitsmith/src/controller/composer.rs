//! Tools ▸ RGB Compositor…: combine the 3 or 4 checked images into a colour
//! preview with [`libfitz::composer::RgbComposer`]. The first three images feed
//! the red, green and blue channels through per-image sliders; an optional
//! fourth is the luminance. Dragging one image onto another swaps them.
//!
//! The slots, not the images, own the slider values: swapping two images keeps
//! each slot's amounts, which is what re-routes an image to another channel.
//!
//! The dialog composes only a preview: each input is loaded once, shrunk
//! [`PREVIEW_DOWNSCALE`]-fold and auto-stretched, then kept in memory while the
//! dialog is open so a slider move or a swap only re-runs the (cheap)
//! composition, on a worker thread. Moves arriving while one is running are
//! coalesced into a single follow-up run with the latest values.
//!
//! Save… re-loads the inputs at full size, runs them through the same stretch
//! and composition with the current sliders, and writes the result as FITS or
//! TIFF, so the file matches what the preview shows.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Result, bail};
use libfitz::composer::{new_channel_input, new_rgb_composer};
use libfitz::data::{Image as FitsImage, ImageType};
use libfitz::export::{ExportFormat, FitsOptions, TiffOptions};
use libfitz::loader::load_image_from_file;
use libfitz::preview::render_preview;
use libfitz::stretch::DEFAULT_BRIGHTNESS;
use rayon::prelude::*;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use super::{STATE, set_composer_state};
use crate::files::display_name;
use crate::image::preview_to_image;
use crate::{AppWindow, ComposerRow};

/// The preview shows each input at 1/4 of its size in both dimensions.
const PREVIEW_DOWNSCALE: usize = 2;

/// How many characters at the start of a file name, and at its end (the frame
/// number, the extension), a slot always shows; its middle is shown in whatever
/// room is left, see [`split_name`].
const NAME_END_CHARS: usize = 13;

/// Index of the slot that is the luminance image, when a fourth was picked.
const LUMINANCE_SLOT: usize = 3;

/// One slot's slider values: the amount of its image that goes into red, green
/// and blue, then (for the luminance slot) into luminance.
type Gains = [f32; 4];

/// Initial slider values: images 0, 1, 2 feed R, G, B at full strength and the
/// fourth is a full-strength luminance.
fn default_gains(slot: usize) -> Gains {
    let mut gains = [0.0; 4];
    gains[slot.min(LUMINANCE_SLOT)] = 1.0;
    gains
}

/// `name` cut into its first and last [`NAME_END_CHARS`] characters and the
/// middle between them. The slot elides only the middle, so the width it has
/// decides how much of the name shows. A name no longer than the two ends
/// together comes back whole as the first part.
fn split_name(name: &str) -> (&str, &str, &str) {
    let len = name.chars().count();
    if len <= 2 * NAME_END_CHARS {
        return (name, "", "");
    }
    let mut starts = name.char_indices().map(|(i, _)| i);
    let head_end = starts.nth(NAME_END_CHARS).unwrap_or(name.len());
    let tail_start = starts.nth(len - 2 * NAME_END_CHARS - 1).unwrap_or(name.len());
    (&name[..head_end], &name[head_end..tail_start], &name[tail_start..])
}

/// Fill in how a slot shows its image's file name.
fn set_names(row: &mut ComposerRow, path: &Path) {
    let full = display_name(path);
    let (head, mid, tail) = split_name(&full);
    row.name_head = head.into();
    row.name_mid = mid.into();
    row.name_tail = tail.into();
    row.full_name = full.as_str().into();
}

/// Everything behind the open dialog, held in [`super::AppState`].
#[derive(Default)]
pub(super) struct ComposerState {
    /// The slots' images, in slot order. Empty while closed.
    paths: Vec<PathBuf>,
    /// The prepared preview inputs, parallel to `paths`; empty until loaded.
    inputs: Vec<Arc<FitsImage>>,
    /// The sliders, parallel to `paths`.
    gains: Vec<Gains>,
    /// The `[ComposerRow]` model behind the slots.
    model: Rc<VecModel<ComposerRow>>,
    /// Bumped when the dialog opens or closes, so a load or composition
    /// started for an earlier opening lands as stale and is dropped.
    generation: u64,
    /// A composition is running on a worker thread.
    busy: bool,
    /// A full-size save is running on a worker thread.
    saving: bool,
    /// The inputs or sliders changed since the running composition started.
    dirty: bool,
}

impl ComposerState {
    fn row(&self, slot: usize) -> ComposerRow {
        let gains = self.gains[slot];
        let mut row = ComposerRow {
            red: gains[0],
            green: gains[1],
            blue: gains[2],
            lum: gains[3],
            ..Default::default()
        };
        set_names(&mut row, &self.paths[slot]);
        row
    }
}

/// Bind the slots' model to the window once, at startup.
pub fn init_composer(app: &AppWindow) {
    set_composer_state(|c| app.set_composer_model(ModelRc::from(c.model.clone())));
}

/// Load one composition input: a monochrome image shrunk `downscale`-fold
/// (1 keeps the full size) and stretched so the linear data is visible.
fn load_input(path: &Path, downscale: usize) -> Result<FitsImage> {
    let image = load_image_from_file(path)?;
    if image.image_type != ImageType::Grayscale {
        bail!("{} is not a monochrome image", display_name(path));
    }
    let image = if downscale > 1 {
        image.downscale(downscale)
    } else {
        image
    };
    Ok(image.stretch(true, DEFAULT_BRIGHTNESS))
}

/// Load one input for the preview, at [`PREVIEW_DOWNSCALE`] of its size.
fn load_preview_input(path: &Path) -> Result<FitsImage> {
    load_input(path, PREVIEW_DOWNSCALE)
}

/// An independent copy of `image` (which is not `Clone`, to keep full-size frames
/// from being duplicated by accident); the composer takes ownership of its
/// inputs, and a preview input is small.
fn copy_of(image: &FitsImage) -> FitsImage {
    FitsImage::new(
        image.image_type,
        image.header.clone(),
        image.width,
        image.height,
        image.pixels.clone(),
    )
}

/// Compose `images` (slot order, taken over) with the slots' `gains` into one
/// RGB image.
fn compose_images(images: Vec<FitsImage>, gains: &[Gains]) -> Result<FitsImage> {
    let channels = images
        .into_iter()
        .zip(gains)
        .enumerate()
        .map(|(slot, (image, gains))| {
            let mut channel = new_channel_input(image)?;
            if slot == LUMINANCE_SLOT {
                channel.lum = gains[3];
            } else {
                [channel.red, channel.green, channel.blue] = [gains[0], gains[1], gains[2]];
            }
            Ok(channel)
        })
        .collect::<Result<Vec<_>>>()?;
    new_rgb_composer(channels.iter().collect())?.compose()
}

/// Compose the prepared `inputs` (slot order) with the slots' `gains` into an
/// RGB8 preview.
fn compose_preview(inputs: &[Arc<FitsImage>], gains: &[Gains]) -> Result<image::RgbImage> {
    let composed = compose_images(inputs.iter().map(|i| copy_of(i)).collect(), gains)?;
    Ok(render_preview(&composed)?.to_rgb8())
}

/// The path to write and its format, from the extension the user chose: `.fits`
/// or `.fit` is FITS, `.tiff` or `.tif` is TIFF, both 16-bit and uncompressed
/// (the Export dialog's defaults). Any other name gets `.fits` appended.
fn save_target(path: PathBuf) -> (PathBuf, ExportFormat) {
    let fits = ExportFormat::Fits(FitsOptions {
        bpp: 16,
        compress: false,
    });
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    match extension.as_deref() {
        Some("tif" | "tiff") => (
            path,
            ExportFormat::Tiff(TiffOptions {
                bpp: 16,
                compress: false,
            }),
        ),
        Some("fits" | "fit") => (path, fits),
        _ => {
            let mut name = path.into_os_string();
            name.push(".fits");
            (PathBuf::from(name), fits)
        }
    }
}

/// Load the images at full size, compose them exactly as the preview does and
/// write the result to `target` as `format`.
fn save_full_size(
    paths: &[PathBuf],
    gains: &[Gains],
    target: &Path,
    format: ExportFormat,
) -> Result<()> {
    let inputs = paths
        .par_iter()
        .map(|path| load_input(path, 1))
        .collect::<Result<Vec<_>>>()?;
    compose_images(inputs, gains)?.export(target, format)
}

/// The file extensions the dialog's format ComboBox offers, in its order.
const SAVE_EXTENSIONS: [&str; 3] = ["fits", "fit", "tiff"];

/// The extension for a format ComboBox index; an out-of-range index falls back
/// to `fits`.
fn save_extension(index: i32) -> &'static str {
    usize::try_from(index)
        .ok()
        .and_then(|i| SAVE_EXTENSIONS.get(i))
        .unwrap_or(&SAVE_EXTENSIONS[0])
}

/// Ask where to save the composition as a `.{extension}` file, starting beside
/// the first image. `None` if the user cancelled. The file dialog is limited to
/// that one type because macOS's merges every filter into a single list and
/// offers no format choice; the choice is made in the compositor instead.
fn prompt_save_path(first_input: &Path, extension: &str) -> Option<PathBuf> {
    let kind = if extension == "tiff" {
        "TIFF image"
    } else {
        "FITS image"
    };
    let mut dialog = rfd::FileDialog::new()
        .add_filter(kind, &[extension])
        .set_file_name(format!("composite.{extension}"));
    if let Some(dir) = first_input.parent() {
        dialog = dialog.set_directory(dir);
    }
    dialog.save_file()
}

/// The Save button: pick a file name, then close the dialog and compose and
/// write the full-size image on a worker thread. Cancelling the file dialog
/// leaves the compositor open. Progress and outcome show in the status bar.
pub fn save_composition(app: &AppWindow) {
    let job = STATE.with(|s| {
        let st = s.borrow();
        let c = &st.composer;
        (!c.saving && !c.inputs.is_empty()).then(|| (c.paths.clone(), c.gains.clone()))
    });
    let Some((paths, gains)) = job else {
        return;
    };
    let extension = save_extension(app.get_composer_save_format());
    let Some(chosen) = prompt_save_path(&paths[0], extension) else {
        return;
    };
    let (target, format) = save_target(chosen);
    // The job owns its paths and gains, so the dialog can drop its own state.
    close_composer(app);
    set_composer_state(|c| c.saving = true);
    app.set_composer_saving(true);
    app.set_status_text(format!("Saving {}…", display_name(&target)).into());

    let weak = app.as_weak();
    std::thread::spawn(move || {
        let result = save_full_size(&paths, &gains, &target, format);
        let _ = weak.upgrade_in_event_loop(move |app| finish_save(&app, &target, result));
    });
}

/// Report a finished save in the status bar.
fn finish_save(app: &AppWindow, target: &Path, result: Result<()>) {
    set_composer_state(|c| c.saving = false);
    app.set_composer_saving(false);
    let status = match result {
        Ok(()) => format!("Composite saved to {}", target.display()),
        Err(e) => format!("Composite save failed: {e:#}"),
    };
    app.set_status_text(status.into());
}

/// Open the compositor on the checked images, in list order, and load them on a
/// worker thread. A no-op unless exactly 3 or 4 rows are checked (the menu item
/// is disabled otherwise).
pub fn open_composer_dialog(app: &AppWindow) {
    let (generation, paths) = STATE.with(|s| {
        let mut st = s.borrow_mut();
        let paths: Vec<PathBuf> = st
            .paths
            .iter()
            .zip(st.files_model.iter())
            .filter(|(_, row)| row.checked)
            .map(|(path, _)| path.clone())
            .collect();
        if !(3..=4).contains(&paths.len()) {
            return (None, paths);
        }
        let c = &mut st.composer;
        c.generation += 1;
        c.gains = (0..paths.len()).map(default_gains).collect();
        c.paths = paths.clone();
        c.inputs.clear();
        c.dirty = false;
        let rows: Vec<ComposerRow> = (0..paths.len()).map(|slot| c.row(slot)).collect();
        c.model.set_vec(rows);
        (Some(c.generation), paths)
    });
    let Some(generation) = generation else {
        return;
    };

    app.set_composer_preview(Default::default());
    app.set_composer_notice("Loading…".into());
    app.set_show_composer(true);

    let weak = app.as_weak();
    std::thread::spawn(move || {
        let loaded: Result<Vec<Arc<FitsImage>>> = paths
            .par_iter()
            .map(|path| load_preview_input(path).map(Arc::new))
            .collect();
        let _ = weak.upgrade_in_event_loop(move |app| apply_inputs(&app, generation, loaded));
    });
}

/// Keep the loaded inputs and compose the first preview — unless the dialog was
/// closed or reopened meanwhile.
fn apply_inputs(app: &AppWindow, generation: u64, loaded: Result<Vec<Arc<FitsImage>>>) {
    if STATE.with(|s| s.borrow().composer.generation) != generation {
        return;
    }
    match loaded {
        Ok(inputs) => {
            set_composer_state(|c| c.inputs = inputs);
            request_compose(app);
        }
        Err(e) => app.set_composer_notice(e.to_string().into()),
    }
}

/// Recompose with the latest inputs and sliders: now, or right after the
/// composition already running finishes.
fn request_compose(app: &AppWindow) {
    set_composer_state(|c| c.dirty = true);
    start_compose(app);
}

/// Start a composition on a worker thread, if none is running and there is
/// something new to compose.
fn start_compose(app: &AppWindow) {
    let job = STATE.with(|s| {
        let mut st = s.borrow_mut();
        let c = &mut st.composer;
        if c.busy || !c.dirty {
            return None;
        }
        c.dirty = false;
        if c.inputs.is_empty() {
            return None;
        }
        c.busy = true;
        Some((c.generation, c.inputs.clone(), c.gains.clone()))
    });
    let Some((generation, inputs, gains)) = job else {
        return;
    };
    let weak = app.as_weak();
    std::thread::spawn(move || {
        let preview = compose_preview(&inputs, &gains);
        let _ = weak.upgrade_in_event_loop(move |app| apply_preview(&app, generation, preview));
    });
}

/// Show a finished composition (unless it is stale), then run the follow-up the
/// sliders asked for in the meantime, if any.
fn apply_preview(app: &AppWindow, generation: u64, preview: Result<image::RgbImage>) {
    let current = STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.composer.busy = false;
        st.composer.generation == generation
    });
    if current {
        match preview {
            Ok(rgb) => {
                app.set_composer_preview(preview_to_image(&rgb));
                app.set_composer_notice("".into());
            }
            Err(e) => app.set_composer_notice(e.to_string().into()),
        }
    }
    start_compose(app);
}

/// A slider moved: `channel` is 0, 1, 2 for red, green, blue and 3 for
/// luminance.
pub fn composer_gain_changed(app: &AppWindow, slot: i32, channel: i32, value: f32) {
    let changed = set_composer_state(|c| {
        let gain = c.gains.get_mut(slot as usize)?.get_mut(channel as usize)?;
        *gain = value;
        Some(())
    });
    if changed.is_some() {
        request_compose(app);
    }
}

/// Swap the images in two slots; each slot keeps its sliders.
pub fn swap_composer_slots(app: &AppWindow, a: i32, b: i32) {
    let (a, b) = (a as usize, b as usize);
    let swapped = set_composer_state(|c| {
        if a == b || a >= c.paths.len() || b >= c.paths.len() {
            return false;
        }
        c.paths.swap(a, b);
        if !c.inputs.is_empty() {
            c.inputs.swap(a, b);
        }
        for slot in [a, b] {
            // Only the names move: rewriting the sliders' initial values would
            // mean nothing to sliders that already own theirs.
            let mut row = c.model.row_data(slot).unwrap_or_default();
            set_names(&mut row, &c.paths[slot]);
            c.model.set_row_data(slot, row);
        }
        true
    });
    if swapped {
        request_compose(app);
    }
}

/// Close the dialog and drop its images.
pub fn close_composer(app: &AppWindow) {
    app.set_show_composer(false);
    app.set_composer_preview(Default::default());
    set_composer_state(|c| {
        c.generation += 1;
        c.paths.clear();
        c.inputs.clear();
        c.gains.clear();
        c.dirty = false;
        c.model.set_vec(Vec::new());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfitz::fits_file::{SaveOptions, load_fits, save_fits};
    use tempfile::TempDir;

    use crate::controller::test_data;

    /// The three planes of the bundled RGB cube, each saved as its own
    /// monochrome FITS file — three real, same-sized mono frames.
    fn mono_frames(dir: &TempDir) -> Vec<PathBuf> {
        let rgb = load_fits(&test_data("rgb.fits")).unwrap();
        (0..3)
            .map(|channel| {
                let path = dir.path().join(format!("mono_{channel}.fits"));
                let plane = rgb.plane(channel).unwrap();
                save_fits(&path, &plane, SaveOptions::default()).unwrap();
                path
            })
            .collect()
    }

    fn prepared(paths: &[PathBuf]) -> Vec<Arc<FitsImage>> {
        paths
            .iter()
            .map(|p| Arc::new(load_preview_input(p).unwrap()))
            .collect()
    }

    #[test]
    fn long_names_are_split_into_both_ends_and_a_middle() {
        let name = "NGC7000_Light_300s_Ha_0042.fits";
        let (head, mid, tail) = split_name(name);
        assert_eq!(head, "NGC7000_Light");
        assert_eq!(tail, "_Ha_0042.fits");
        assert_eq!(mid, "_300s");
        assert_eq!(format!("{head}{mid}{tail}"), name);
    }

    #[test]
    fn names_that_fit_both_ends_are_left_whole() {
        assert_eq!(split_name("M31_001.fit"), ("M31_001.fit", "", ""));
        let exact = "a".repeat(2 * NAME_END_CHARS);
        assert_eq!(split_name(&exact), (exact.as_str(), "", ""));
        // One more character gives the middle something to hold.
        let longer = "a".repeat(2 * NAME_END_CHARS + 1);
        let (head, mid, tail) = split_name(&longer);
        assert_eq!((head.len(), mid.len(), tail.len()), (NAME_END_CHARS, 1, NAME_END_CHARS));
    }

    #[test]
    fn splitting_counts_characters_not_bytes() {
        let name = format!("{}{}{}", "α".repeat(NAME_END_CHARS), "γγ", "β".repeat(NAME_END_CHARS));
        let (head, mid, tail) = split_name(&name);
        assert_eq!(head, "α".repeat(NAME_END_CHARS));
        assert_eq!(mid, "γγ");
        assert_eq!(tail, "β".repeat(NAME_END_CHARS));
    }

    #[test]
    fn slot_rows_carry_the_split_and_full_names() {
        let path = Path::new("/data/NGC7000_Light_300s_Ha_0042.fits");
        let mut row = ComposerRow::default();
        set_names(&mut row, path);
        assert_eq!(row.full_name, "NGC7000_Light_300s_Ha_0042.fits");
        assert_eq!(format!("{}{}{}", row.name_head, row.name_mid, row.name_tail), row.full_name.as_str());
    }

    #[test]
    fn default_gains_route_images_to_r_g_b_then_luminance() {
        assert_eq!(default_gains(0), [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(default_gains(1), [0.0, 1.0, 0.0, 0.0]);
        assert_eq!(default_gains(2), [0.0, 0.0, 1.0, 0.0]);
        assert_eq!(default_gains(3), [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn preview_input_is_a_quarter_size_stretched_mono_frame() {
        let dir = TempDir::new().unwrap();
        let paths = mono_frames(&dir);
        let full = load_fits(&paths[0]).unwrap();

        let input = load_preview_input(&paths[0]).unwrap();
        assert_eq!(input.image_type, ImageType::Grayscale);
        assert_eq!(input.width, full.width / PREVIEW_DOWNSCALE);
        assert_eq!(input.height, full.height / PREVIEW_DOWNSCALE);
    }

    #[test]
    fn a_colour_image_is_not_a_valid_input() {
        let err = load_preview_input(&test_data("rgb.fits")).unwrap_err();
        assert!(err.to_string().contains("not a monochrome image"), "{err}");
    }

    #[test]
    fn composing_three_planes_gives_a_colour_preview_and_swapping_changes_it() {
        let dir = TempDir::new().unwrap();
        let inputs = prepared(&mono_frames(&dir));
        let gains: Vec<Gains> = (0..3).map(default_gains).collect();

        let preview = compose_preview(&inputs, &gains).unwrap();
        assert_eq!(
            (preview.width() as usize, preview.height() as usize),
            (inputs[0].width, inputs[0].height)
        );

        // Swapping images 0 and 1 under the same gains routes them to the
        // other channels.
        let swapped_inputs = vec![inputs[1].clone(), inputs[0].clone(), inputs[2].clone()];
        let swapped = compose_preview(&swapped_inputs, &gains).unwrap();
        assert_ne!(preview.as_raw(), swapped.as_raw());

        let first = preview.pixels().next().unwrap().0;
        let first_swapped = swapped.pixels().next().unwrap().0;
        assert_eq!(first[0], first_swapped[1]);
        assert_eq!(first[1], first_swapped[0]);
        assert_eq!(first[2], first_swapped[2]);
    }

    #[test]
    fn a_fourth_image_is_blended_in_as_luminance() {
        let dir = TempDir::new().unwrap();
        let mut inputs = prepared(&mono_frames(&dir));
        inputs.push(inputs[0].clone());
        let mut gains: Vec<Gains> = (0..4).map(default_gains).collect();

        gains[3][3] = 0.0;
        let without = compose_preview(&inputs, &gains).unwrap();
        gains[3][3] = 1.0;
        let with = compose_preview(&inputs, &gains).unwrap();

        assert_eq!(without.dimensions(), with.dimensions());
        assert_ne!(without.as_raw(), with.as_raw());
    }

    #[test]
    fn images_of_different_sizes_are_rejected() {
        let dir = TempDir::new().unwrap();
        let mut inputs = prepared(&mono_frames(&dir));
        inputs[2] = Arc::new(inputs[2].downscale(2));
        let gains: Vec<Gains> = (0..3).map(default_gains).collect();

        let err = compose_preview(&inputs, &gains).unwrap_err();
        assert!(err.to_string().contains("same size"), "{err}");
    }

    #[test]
    fn save_target_picks_the_format_from_the_extension() {
        for (name, fits) in [
            ("a.fits", true),
            ("a.FIT", true),
            ("a.tiff", false),
            ("a.Tif", false),
        ] {
            let (path, format) = save_target(PathBuf::from(name));
            assert_eq!(path, PathBuf::from(name), "{name} keeps its name");
            assert_eq!(matches!(format, ExportFormat::Fits(_)), fits, "{name}");
        }
    }

    #[test]
    fn save_extension_follows_the_format_combo_box() {
        assert_eq!(save_extension(0), "fits");
        assert_eq!(save_extension(1), "fit");
        assert_eq!(save_extension(2), "tiff");
        assert_eq!(save_extension(-1), "fits");
        assert_eq!(save_extension(9), "fits");
        // Each offered extension maps to the format its name suggests.
        for (ext, fits) in [("fits", true), ("fit", true), ("tiff", false)] {
            let (_, format) = save_target(PathBuf::from(format!("x.{ext}")));
            assert_eq!(matches!(format, ExportFormat::Fits(_)), fits, "{ext}");
        }
    }

    #[test]
    fn save_target_appends_fits_to_other_names() {
        for (name, expected) in [("m42", "m42.fits"), ("m42.final", "m42.final.fits")] {
            let (path, format) = save_target(PathBuf::from(name));
            assert_eq!(path, PathBuf::from(expected));
            assert!(matches!(format, ExportFormat::Fits(_)));
        }
    }

    #[test]
    fn saving_writes_the_full_size_composition_as_fits() {
        let dir = TempDir::new().unwrap();
        let paths = mono_frames(&dir);
        // Sliders as the user might leave them: green dimmed, blue mixed into red.
        let mut gains: Vec<Gains> = (0..3).map(default_gains).collect();
        gains[1][1] = 0.5;
        gains[2][0] = 0.25;
        let target = dir.path().join("out.fits");
        let (target, format) = save_target(target);
        save_full_size(&paths, &gains, &target, format).unwrap();

        let saved = load_fits(&target).unwrap();
        let original = load_fits(&test_data("rgb.fits")).unwrap();
        assert_eq!(saved.image_type, ImageType::RGB);
        // Full size, not the preview's reduced one.
        assert_eq!((saved.width, saved.height), (original.width, original.height));

        let inputs: Vec<FitsImage> = paths.iter().map(|p| load_input(p, 1).unwrap()).collect();
        let expected = compose_images(inputs, &gains).unwrap();
        assert_eq!(saved.pixels.as_u16(), expected.pixels.as_u16());
    }

    #[test]
    fn saving_writes_a_tiff_file() {
        let dir = TempDir::new().unwrap();
        let paths = mono_frames(&dir);
        let gains: Vec<Gains> = (0..3).map(default_gains).collect();
        let (target, format) = save_target(dir.path().join("out.tiff"));
        save_full_size(&paths, &gains, &target, format).unwrap();

        let bytes = std::fs::read(&target).unwrap();
        let original = load_fits(&test_data("rgb.fits")).unwrap();
        // A TIFF header (either byte order), then at least the 16-bit RGB samples.
        assert!(bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*"));
        assert!(bytes.len() >= original.width * original.height * 3 * 2);
    }

    #[test]
    fn saving_a_non_monochrome_input_fails_without_writing() {
        let dir = TempDir::new().unwrap();
        let mut paths = mono_frames(&dir);
        paths[1] = test_data("rgb.fits");
        let gains: Vec<Gains> = (0..3).map(default_gains).collect();
        let target = dir.path().join("out.fits");
        let err = save_full_size(&paths, &gains, &target, save_target(target.clone()).1).unwrap_err();
        assert!(err.to_string().contains("not a monochrome"), "{err}");
        assert!(!target.exists());
    }
}
