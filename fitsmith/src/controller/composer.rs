//! Tools ▸ RGB Compositor…: combine the 3 or 4 checked images into a colour
//! preview with [`libfitz::composer::RgbComposer`]. The first three images feed
//! the red, green and blue channels through per-image sliders; an optional
//! fourth is the luminance. Dragging one image onto another swaps them.
//!
//! The slots, not the images, own the slider values: swapping two images keeps
//! each slot's amounts, which is what re-routes an image to another channel.
//!
//! Only a preview is composed: each input is loaded once, shrunk
//! [`PREVIEW_DOWNSCALE`]-fold and auto-stretched, then kept in memory while the
//! dialog is open so a slider move or a swap only re-runs the (cheap)
//! composition, on a worker thread. Moves arriving while one is running are
//! coalesced into a single follow-up run with the latest values.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Result, bail};
use libfitz::composer::{new_channel_input, new_rgb_composer};
use libfitz::data::{Image as FitsImage, ImageType};
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
const PREVIEW_DOWNSCALE: usize = 4;

/// The longest file name, in characters, that fits a slot's handle; longer ones
/// are elided in the middle so both the start and the end (the frame number,
/// the extension) stay readable. Slint can only elide the end of a text and
/// cannot measure one, so this is a budget for the pane's fixed width; the
/// handle's own end-elision remains as a backstop for wide glyphs.
const MAX_NAME_CHARS: usize = 26;

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

/// `name` shortened to at most `max` characters by replacing the middle with
/// `…`; names that already fit are returned untouched.
fn elide_middle(name: &str, max: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max || max < 2 {
        return name.to_string();
    }
    let tail = (max - 1) / 2;
    let head = max - 1 - tail;
    let mut elided: String = chars[..head].iter().collect();
    elided.push('…');
    elided.extend(&chars[chars.len() - tail..]);
    elided
}

/// How a slot shows its image's file name.
fn slot_name(path: &Path) -> slint::SharedString {
    elide_middle(&display_name(path), MAX_NAME_CHARS).into()
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
    /// The inputs or sliders changed since the running composition started.
    dirty: bool,
}

impl ComposerState {
    fn row(&self, slot: usize) -> ComposerRow {
        let gains = self.gains[slot];
        ComposerRow {
            name: slot_name(&self.paths[slot]),
            red: gains[0],
            green: gains[1],
            blue: gains[2],
            lum: gains[3],
        }
    }
}

/// Bind the slots' model to the window once, at startup.
pub fn init_composer(app: &AppWindow) {
    set_composer_state(|c| app.set_composer_model(ModelRc::from(c.model.clone())));
}

/// Load one input for the preview: a monochrome image, shrunk and stretched so
/// the linear data is visible.
fn load_preview_input(path: &Path) -> Result<FitsImage> {
    let image = load_image_from_file(path)?;
    if image.image_type != ImageType::Grayscale {
        bail!("{} is not a monochrome image", display_name(path));
    }
    Ok(image
        .downscale(PREVIEW_DOWNSCALE)
        .stretch(true, DEFAULT_BRIGHTNESS))
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

/// Compose the prepared `inputs` (slot order) with the slots' `gains` into an
/// RGB8 preview.
fn compose_preview(inputs: &[Arc<FitsImage>], gains: &[Gains]) -> Result<image::RgbImage> {
    let channels = inputs
        .iter()
        .zip(gains)
        .enumerate()
        .map(|(slot, (input, gains))| {
            let mut channel = new_channel_input(copy_of(input))?;
            if slot == LUMINANCE_SLOT {
                channel.lum = gains[3];
            } else {
                [channel.red, channel.green, channel.blue] = [gains[0], gains[1], gains[2]];
            }
            Ok(channel)
        })
        .collect::<Result<Vec<_>>>()?;
    let composed = new_rgb_composer(channels.iter().collect())?.compose()?;
    Ok(render_preview(&composed)?.to_rgb8())
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
            // Only the name moves: rewriting the sliders' initial values would
            // mean nothing to sliders that already own theirs.
            let mut row = c.model.row_data(slot).unwrap_or_default();
            row.name = slot_name(&c.paths[slot]);
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
    fn long_names_are_elided_in_the_middle_keeping_both_ends() {
        let name = "NGC7000_Light_300s_Ha_0042.fits";
        let short = elide_middle(name, 20);
        assert_eq!(short.chars().count(), 20);
        assert!(short.starts_with("NGC7000_Li"), "{short}");
        assert!(short.ends_with("0042.fits"), "{short}");
        assert_eq!(short.matches('…').count(), 1);
    }

    #[test]
    fn names_that_fit_are_left_alone() {
        assert_eq!(elide_middle("M31_001.fit", 26), "M31_001.fit");
        let exact = "a".repeat(26);
        assert_eq!(elide_middle(&exact, 26), exact);
    }

    #[test]
    fn eliding_counts_characters_not_bytes() {
        let short = elide_middle("ααααααααααββββββββββ", 9);
        assert_eq!(short.chars().count(), 9);
        assert_eq!(short, "αααα…ββββ");
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
}
