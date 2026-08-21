//! Tools ▸ Detect Bad Frames…: flag the frames of a session that look worse
//! than the session itself — clouds (background floor up / star count down),
//! focus drift (FWHM up) and tracking failure (eccentricity up) — and turn the
//! verdict into the file list's checkbox selection so the user can act on it
//! (remove, export elsewhere, …).
//!
//! The dialog reuses the analytics batch machinery ([`super::analytics`]) with
//! the star family, so every measured frame lands in the same cache the chart
//! dialogs use: opening it after Star metrics… reads no file at all, and every
//! knob change re-evaluates purely in memory.

use std::collections::HashSet;
use std::path::PathBuf;
use rayon::prelude::*;
use slint::{ModelRc, VecModel};
use libfitz::stars::median_in_place;
use libfitz::stats::Stats;
use super::analytics::{self, Plan};
use super::metrics::FileMetrics;
use super::{STATE, set_checked_paths, update_checked_count, update_exposure};
use crate::files::display_name;
use crate::{AppWindow, BadFrameRow};

/// The dialog's knobs: which failure modes to look for, and how far from the
/// session baseline a frame must stray to be flagged. Each factor has its
/// own threshold; `star_count` shares `floor_sigma` since it shares the
/// dialog's "Transparency" checkbox with the floor factor.
pub struct BadFrameParams {
    /// Factor 1 — transparency: background floor rising.
    pub floor: bool,
    /// Factor 2 — focus: median star FWHM rising.
    pub fwhm: bool,
    /// Factor 3 — tracking: median star eccentricity rising.
    pub eccentricity: bool,
    /// Factor 4 — transparency: star count dropping. Shares the dialog's
    /// "Transparency" checkbox (and its threshold) with `floor`.
    pub star_count: bool,
    /// Transparency's rejection threshold, in robust sigmas (3 conservative
    /// … 1 aggressive). Governs both `floor` and `star_count`.
    pub floor_sigma: f32,
    /// Focus's rejection threshold, in robust sigmas.
    pub fwhm_sigma: f32,
    /// Tracking's rejection threshold, in robust sigmas.
    pub eccentricity_sigma: f32,
}

/// Decide which of the session's frames are bad under `params`.
///
/// Intended algorithm (see `.plan/bad-frame-detector.md`): for each enabled
/// factor's metric, compute the session median and MAD across `files`, then
/// each frame's robust z-score `z = (value − median) / (1.4826 × MAD)`,
/// one-tailed — only the direction that means "worse" counts (background,
/// FWHM and eccentricity rising; star count dropping). A frame is flagged
/// when any enabled factor's |z| crosses that factor's own sigma threshold
/// in its bad direction (OR across factors — the failure modes are
/// physically distinct). A zero MAD means the metric has no dispersion and
/// never trips; a frame missing a star metric is excluded from that
/// metric's baseline and can't trip it.
///
/// Returns the flagged frames in `files` order.
pub fn evaluate(files: &[FileMetrics], params: &BadFrameParams) -> Vec<PathBuf> {
    let mut bad_frames: HashSet<PathBuf> = HashSet::new();
    if params.floor {
        bad_frames.extend(estimate_frame_for_noise_floor(files, params.floor_sigma));
    }
    if params.star_count {
        let bads = estimate_frame_for_star_count(files, params.floor_sigma);
        bad_frames.extend(bads);
    }
    if params.fwhm {
        bad_frames.extend(estimate_frame_for_focus(files, params.fwhm_sigma));
    }
    if params.eccentricity {
        bad_frames.extend(estimate_frame_for_eccentricity(files, params.eccentricity_sigma));
    }
    // ensure the list is returned in the original order
    files
        .iter()
        .map(|m| &m.path)
        .filter(|p| bad_frames.contains(*p))
        .cloned()
        .collect()
}

fn estimate_frame_for_star_count(metrics: &[FileMetrics], sigma: f32) -> Vec<PathBuf> {
    let meta = calculate_meta_stats(metrics, |s| Some(s.stars.count as f64));
    if meta.mad == 0.0 {
        return vec![]
    }
    metrics.iter().flat_map(|m| {
        let stars = &m.stars;
        let z = (stars.count as f32 - meta.median) / meta.mad;
        if z <= -sigma {
            Some(m.path.clone())
        } else {
            None
        }
    }).collect()
}

fn estimate_frame_for_noise_floor(metrics: &[FileMetrics], sigma: f32) -> Vec<PathBuf> {
    let meta = calculate_meta_stats(metrics, stats_extractor(|st| Some(st.median as f64)));
    if meta.mad == 0.0 {
        return vec![]
    }
    let data: Vec<f64> = metrics.iter().flat_map(stats_extractor(|st| Some(st.median as f64))).collect();
    data.iter().enumerate().flat_map(|(idx,d)| {
        let z = (*d as f32 - meta.median) / meta.mad;
        if z >= sigma {
            Some(metrics[idx].path.clone())
        } else {
            None
        }
    }).collect()
}


fn estimate_frame_for_focus(metrics: &[FileMetrics], sigma: f32) -> Vec<PathBuf> {
    let meta = calculate_meta_stats(metrics, |s| s.stars.fwhm);
    if meta.mad == 0.0 {
        return vec![]
    }
    metrics.iter().flat_map(|m| {
        let stars = &m.stars;
        let z = (stars.fwhm.unwrap_or(0.0) as f32 - meta.median) / meta.mad;
        if z >= sigma {
            Some(m.path.clone())
        } else {
            None
        }
    }).collect()
}


fn estimate_frame_for_eccentricity(metrics: &[FileMetrics], sigma: f32) -> Vec<PathBuf> {
    let meta = calculate_meta_stats(metrics, |s| s.stars.eccentricity);
    if meta.mad == 0.0 {
        return vec![]
    }
    metrics.iter().flat_map(|m| {
        let stars = &m.stars;
        let z = (stars.eccentricity.unwrap_or(0.0) as f32 - meta.median) / meta.mad;
        if z >= sigma {
            Some(m.path.clone())
        } else {
            None
        }
    }).collect()
}

struct MetaStats {
    median: f32,
    mad: f32
}

fn stats_extractor(extractor: fn(Stats) -> Option<f64>) -> impl Fn(&FileMetrics) -> Option<f64> + Sync {
    move |m: &FileMetrics| {
        if m.stats.len() == 1 { extractor(m.stats[0]) } else { extractor(m.stats[1]) }
    }
}
fn calculate_meta_stats(files: &[FileMetrics], extractor: impl Fn(&FileMetrics) -> Option<f64> + Sync) -> MetaStats {
    let mut data: Vec<f64> = files.par_iter().flat_map(|m| extractor(m)).collect();
    if data.is_empty() {
        return MetaStats{
            median: 0.0,
            mad: 0.0,
        }
    }
    let median = median_in_place(&mut data);
    let mut mad_data: Vec<f64> = data.iter().map(|d| (d - median).abs()).collect();
    mad_data.sort_by(|a, b| a.total_cmp(b));

    let mad = median_in_place(&mut mad_data) * 1.4826;
    MetaStats{
        median: median as f32,
        mad: mad as f32,
    }
}


/// Tools ▸ Detect Bad Frames…: measure the target files (the checked rows, or
/// all of them when none are checked) behind the cancellable progress overlay
/// — star detection included, exactly the Star metrics… batch — then evaluate
/// with the current knobs and show the dialog. Everything already in the
/// analysis cache is reused, so a reopen (or an open after Star metrics…)
/// reads no file.
pub fn open_bad_frames_dialog(app: &AppWindow) {
    analytics::start_batch(app, show_bad_frames);
}

/// Land the finished batch: keep the measured frames for live re-evaluation,
/// run the first evaluation, and show the dialog.
fn show_bad_frames(app: &AppWindow, plan: Plan, failures: usize) {
    STATE.with(|s| s.borrow_mut().bad_frames = plan.metrics);
    recompute_bad_frames(app);
    app.set_show_bad_frames(true);
    if failures > 0 {
        app.set_status_text(format!("Bad frames: {failures} file(s) failed to read").into());
    }
}

/// Re-evaluate with the knobs currently on the dialog and re-fill its list.
/// Called on every checkbox/slider change; pure in-memory work over the
/// frames measured at open, so it is instant.
pub fn recompute_bad_frames(app: &AppWindow) {
    let params = BadFrameParams {
        floor: app.get_bad_floor_enabled(),
        fwhm: app.get_bad_fwhm_enabled(),
        eccentricity: app.get_bad_ecc_enabled(),
        // The "Transparency (floor / star count)" checkbox drives both
        // transparency factors.
        star_count: app.get_bad_floor_enabled(),
        floor_sigma: app.get_bad_floor_sigma(),
        fwhm_sigma: app.get_bad_fwhm_sigma(),
        eccentricity_sigma: app.get_bad_ecc_sigma(),
    };
    let (rows, flagged, total) = STATE.with(|s| {
        let mut st = s.borrow_mut();
        let bad = evaluate(&st.bad_frames, &params);
        let rows: Vec<BadFrameRow> = bad
            .iter()
            .map(|b| BadFrameRow {   
                name: display_name(&b).into(),
            })
            .collect();
        let flagged: Vec<PathBuf> = bad.clone();
        let total = st.bad_frames.len();
        st.bad_frame_flagged = flagged.clone();
        (rows, flagged, total)
    });
    app.set_bad_frames_summary(format!("{} of {total} frames flagged", flagged.len()).into());
    app.set_bad_frames_model(ModelRc::new(VecModel::from(rows)));
}

/// The dialog's Select button: check exactly the flagged rows in the file
/// list, uncheck every other row, and close the dialog — the selection *is*
/// the result.
pub fn select_bad_frames(app: &AppWindow) {
    STATE.with(|s| {
        let st = s.borrow();
        let paths: HashSet<String> = st
            .bad_frame_flagged
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        set_checked_paths(&st.files_model, &paths);
    });
    update_checked_count(app);
    update_exposure(app);
    app.set_show_bad_frames(false);
    close_bad_frames(app);
}

/// Dialog dismissed: drop the measured frames and the verdict. The analysis
/// cache is left alone — surviving the close is exactly what it is for.
pub fn close_bad_frames(app: &AppWindow) {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.bad_frames.clear();
        st.bad_frame_flagged.clear();
    });
    app.set_bad_frames_model(ModelRc::new(VecModel::<BadFrameRow>::default()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::metrics::analyze_file;
    use crate::controller::test_data;
    use libfitz::stars::StarStats;
    use libfitz::stats::Stats;

    fn all_factors(sigma: f32) -> BadFrameParams {
        BadFrameParams {
            floor: true,
            fwhm: true,
            eccentricity: true,
            star_count: true,
            floor_sigma: sigma,
            fwhm_sigma: sigma,
            eccentricity_sigma: sigma,
        }
    }

    fn frame(path: &str, median: f32, stars: StarStats) -> FileMetrics {
        FileMetrics {
            path: PathBuf::from(path),
            time: None,
            time_str: String::new(),
            stats: vec![Stats {
                median,
                ..Stats::default()
            }],
            stars,
        }
    }

    #[test]
    fn evaluate_on_empty_input_returns_empty() {
        assert!(evaluate(&[], &all_factors(3.0)).is_empty());
    }

    #[test]
    fn frames_missing_every_star_metric_do_not_panic_and_are_not_flagged() {
        // Regression test: when every frame lacks fwhm/eccentricity, that
        // metric's baseline population is empty. calculate_meta_stats must
        // fall back to a zero MAD (never trips) instead of calling
        // median_in_place on an empty slice, which panics.
        let no_stars = StarStats {
            count: 0,
            hfr: None,
            fwhm: None,
            eccentricity: None,
        };
        let files = vec![
            frame("a.fits", 100.0, no_stars.clone()),
            frame("b.fits", 30000.0, no_stars),
        ];
        assert!(evaluate(&files, &all_factors(1.0)).is_empty());
    }

    #[test]
    fn calculate_meta_stats_excludes_missing_metric_frames_from_the_baseline() {
        // Regression test: a frame with no detected stars (eccentricity
        // None) must not be folded into the baseline as a substituted 0.0 —
        // that pulls the median/MAD toward zero and can mask a genuinely bad
        // frame's z-score (this previously caused bad_eccentricity.fits to
        // slip under threshold in the fixture test below).
        let files = vec![
            frame(
                "a.fits",
                0.0,
                StarStats { count: 1, hfr: None, fwhm: None, eccentricity: Some(1.0) },
            ),
            frame(
                "b.fits",
                0.0,
                StarStats { count: 1, hfr: None, fwhm: None, eccentricity: Some(3.0) },
            ),
            frame(
                "c.fits",
                0.0,
                StarStats { count: 0, hfr: None, fwhm: None, eccentricity: None },
            ),
        ];
        let meta = calculate_meta_stats(&files, |m| m.stars.eccentricity);
        // Median of {1.0, 3.0} is 2.0; including the None frame as 0.0 would
        // pull it down to 1.0.
        assert_eq!(meta.median, 2.0);
    }

    /// Measure the eleven `test-data/bad-image/` fixtures — six good frames
    /// and five whose file names say what is wrong with them — with star
    /// detection, the exact input the dialog evaluates.
    fn measure_bad_image_fixtures() -> Vec<FileMetrics> {
        let names = [
            "good1.fits",
            "good2.fits",
            "good3.fits",
            "good4.fits",
            "good5.fits",
            "good6.fits",
            "bad_no_stars1.fits",
            "bad_no_stars2.fits",
            "bad_no_stars3.fits",
            "bad_tracking1.fits",
            "bad_eccentricity.fits",
        ];
        names
            .iter()
            .map(|name| {
                analyze_file(&test_data(&format!("bad-image/{name}"))).unwrap()
            })
            .collect()
    }

    fn flagged_names(bad: &[PathBuf]) -> Vec<String> {
        bad.iter()
            .map(|b| b.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    // The acceptance tests for the detection algorithm, against real frames
    // whose file names describe their defect.

    #[test]
    fn all_factors_flag_exactly_the_bad_fixtures() {
        let files = measure_bad_image_fixtures();
        let flagged = flagged_names(&evaluate(&files, &all_factors(2.0)));
        for good in ["good1", "good2", "good3", "good4", "good5", "good6"] {
            assert!(
                !flagged.iter().any(|n| n.starts_with(good)),
                "{good} must not be flagged, got {flagged:?}"
            );
        }
        for bad in [
            "bad_no_stars1.fits",
            "bad_no_stars2.fits",
            "bad_no_stars3.fits",
            "bad_tracking1.fits",
            "bad_eccentricity.fits",
        ] {
            assert!(
                flagged.iter().any(|n| n == bad),
                "{bad} must be flagged, got {flagged:?}"
            );
        }
    }

    #[test]
    fn the_floor_factor_alone_catches_the_starless_frames() {
        let files = measure_bad_image_fixtures();
        let params = BadFrameParams {
            floor: true,
            fwhm: false,
            eccentricity: false,
            star_count: true,
            floor_sigma: 2.0,
            fwhm_sigma: 2.0,
            eccentricity_sigma: 2.0,
        };
        let flagged = flagged_names(&evaluate(&files, &params));
        for bad in [
            "bad_no_stars1.fits",
            "bad_no_stars2.fits",
            "bad_no_stars3.fits",
        ] {
            assert!(
                flagged.iter().any(|n| n == bad),
                "{bad} must be flagged by the floor factor, got {flagged:?}"
            );
        }
        assert!(
            !flagged.iter().any(|n| n.starts_with("good")),
            "no good frame may trip the floor factor, got {flagged:?}"
        );
    }

    #[test]
    fn the_eccentricity_factor_alone_catches_the_trailed_frames() {
        let files = measure_bad_image_fixtures();
        let params = BadFrameParams {
            floor: false,
            fwhm: false,
            eccentricity: true,
            star_count: false,
            floor_sigma: 2.0,
            fwhm_sigma: 2.0,
            eccentricity_sigma: 2.0,
        };
        let flagged = flagged_names(&evaluate(&files, &params));
        for bad in ["bad_tracking1.fits", "bad_eccentricity.fits"] {
            assert!(
                flagged.iter().any(|n| n == bad),
                "{bad} must be flagged by the eccentricity factor, got {flagged:?}"
            );
        }
        assert!(
            !flagged.iter().any(|n| n.starts_with("good")),
            "no good frame may trip the eccentricity factor, got {flagged:?}"
        );
    }
}
