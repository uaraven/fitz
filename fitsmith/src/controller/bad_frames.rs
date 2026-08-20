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
use libfitz::stats::Stats;
use super::analytics::{self, Plan};
use super::metrics::{FileMetrics, MetricFamily};
use super::{STATE, set_checked_paths, update_checked_count, update_exposure};
use crate::files::display_name;
use crate::{AppWindow, BadFrameRow};

/// The dialog's knobs: which failure modes to look for, and how far from the
/// session baseline a frame must stray to be flagged.
// The `expect` covers the fields only [`evaluate`]'s stub leaves unread; it
// starts warning (and gets removed) the moment the algorithm lands.
#[expect(dead_code)]
pub struct BadFrameParams {
    /// Factor 1 — transparency: background floor rising, or star count dropping.
    pub floor: bool,
    /// Factor 2 — focus: median star FWHM rising.
    pub fwhm: bool,
    /// Factor 3 — tracking: median star eccentricity rising.
    pub eccentricity: bool,
    /// The rejection threshold in robust sigmas (3 conservative … 1 aggressive).
    pub sigma: f32,
}

/// Map the aggressiveness dropdown index to its sigma threshold —
/// Conservative (3σ) / Moderate (2σ) / Aggressive (1σ). Out-of-range (and
/// Slint's -1 "no selection") falls back to conservative.
pub fn sigma_for_index(index: i32) -> f32 {
    match index {
        1 => 2.0,
        2 => 1.0,
        _ => 3.0,
    }
}

/// One flagged frame: its path and a short human-readable account of which
/// factor(s) tripped and by how much (e.g. `"FWHM z=2.4 · Ecc z=3.2"`).
pub struct BadFrame {
    pub path: PathBuf,
    pub reason: String,
}

/// Decide which of the session's frames are bad under `params`.
///
/// Intended algorithm (see `.plan/bad-frame-detector.md`): for each enabled
/// factor's metric, compute the session median and MAD across `files`, then
/// each frame's robust z-score `z = (value − median) / (1.4826 × MAD)`,
/// one-tailed — only the direction that means "worse" counts (background,
/// FWHM and eccentricity rising; star count dropping). A frame is flagged
/// when any enabled factor's |z| crosses `params.sigma` in its bad direction
/// (OR across factors — the failure modes are physically distinct). A zero
/// MAD means the metric has no dispersion and never trips; a frame missing a
/// star metric is excluded from that metric's baseline and can't trip it.
///
/// Returns the flagged frames in `files` order.
pub fn evaluate(files: &[FileMetrics], params: &BadFrameParams) -> Vec<BadFrame> {
    // TODO: detection algorithm — see the doc comment above.
    let _ = (files, params);
    Vec::new()
}

fn estimate_frame_for_star_count(metrics: &[FileMetrics], param: &BadFrameParams) -> Vec<BadFrame> {
    let meta = calculate_meta_stats(metrics, |s| s.stars.as_ref().map(|s| s.count as f32).unwrap_or(0f32));
    metrics.iter().flat_map(|m| {
        if let Some(stars) = &m.stars {
            let delta = meta.mean - stars.count as f32;
            if delta >= meta.sigma * param.sigma {
                Some(BadFrame{
                    path: m.path.clone(),
                    reason: format!("star count below {}σ", param.sigma),
                })
            } else {
                None
            }
        } else {
            None
        }
    }).collect()
}

struct MetaStats {
    mean: f32,
    sigma: f32,
    median: f32,
    mad: f32
}

fn stats_extractor(extractor: fn(Stats) -> f32) -> impl Fn(&FileMetrics) -> f32 + Sync {
    move |m: &FileMetrics| {
        if m.stats.len() == 1 { extractor(m.stats[0]) } else { extractor(m.stats[1]) }
    }
}
fn calculate_meta_stats(files: &[FileMetrics], extractor: impl Fn(&FileMetrics) -> f32 + Sync) -> MetaStats {
    let n = files.len() as f32;
    let mut data: Vec<f32> = files.par_iter().map(|m| extractor(m)).collect();
    data.sort_by(|a, b| a.total_cmp(b));

    let calc_median = |d: &[f32]| {
        let median = match data.len() % 2 == 0 {
            true => data[data.len() / 2 - 1] + data[data.len() / 2],
            false => data[data.len() / 2],
        };
        median
    };
    let median = calc_median(&data);
    let mut mad_data: Vec<f32> = data.iter().map(|d| (d - median).abs()).collect();
    mad_data.sort_by(|a, b| a.total_cmp(b));


    let mean: f32 = data.iter().sum::<f32>()/n;
    let std_dev: f32 = (data.iter().map(|m| (m - mean).powf(2.0)).sum::<f32>() / n).sqrt();
    let mad = calc_median(&mad_data) * 1.4826;
    MetaStats{
        mean,
        sigma: std_dev,
        median,
        mad,
    }
}


/// Tools ▸ Detect Bad Frames…: measure the target files (the checked rows, or
/// all of them when none are checked) behind the cancellable progress overlay
/// — star detection included, exactly the Star metrics… batch — then evaluate
/// with the current knobs and show the dialog. Everything already in the
/// analysis cache is reused, so a reopen (or an open after Star metrics…)
/// reads no file.
pub fn open_bad_frames_dialog(app: &AppWindow) {
    analytics::start_batch(app, MetricFamily::Star, show_bad_frames);
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
/// Called on every checkbox/dropdown change; pure in-memory work over the
/// frames measured at open, so it is instant.
pub fn recompute_bad_frames(app: &AppWindow) {
    let params = BadFrameParams {
        floor: app.get_bad_floor_enabled(),
        fwhm: app.get_bad_fwhm_enabled(),
        eccentricity: app.get_bad_ecc_enabled(),
        sigma: sigma_for_index(app.get_bad_sigma_index()),
    };
    let (rows, flagged, total) = STATE.with(|s| {
        let mut st = s.borrow_mut();
        let bad = evaluate(&st.bad_frames, &params);
        let rows: Vec<BadFrameRow> = bad
            .iter()
            .map(|b| BadFrameRow {
                name: display_name(&b.path).into(),
                reason: b.reason.clone().into(),
            })
            .collect();
        let flagged: Vec<PathBuf> = bad.into_iter().map(|b| b.path).collect();
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
    use crate::controller::metrics::{AnalyzeOptions, analyze_file};
    use crate::controller::test_data;
    use libfitz::stars::StarStats;
    use libfitz::stats::Stats;

    fn all_factors(sigma: f32) -> BadFrameParams {
        BadFrameParams {
            floor: true,
            fwhm: true,
            eccentricity: true,
            sigma,
        }
    }

    fn frame(path: &str, median: f32, stars: Option<StarStats>) -> FileMetrics {
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
    fn sigma_index_maps_to_the_dropdown_order() {
        assert_eq!(sigma_for_index(0), 3.0);
        assert_eq!(sigma_for_index(1), 2.0);
        assert_eq!(sigma_for_index(2), 1.0);
        // Out-of-range (and Slint's -1 "no selection") stays conservative.
        assert_eq!(sigma_for_index(-1), 3.0);
        assert_eq!(sigma_for_index(99), 3.0);
    }

    #[test]
    fn evaluate_stub_flags_nothing() {
        // Locks the scaffold behavior until the detection algorithm lands: no
        // parameter combination flags a frame.
        let files = vec![frame("a.fits", 100.0, None), frame("b.fits", 30000.0, None)];
        assert!(evaluate(&files, &all_factors(1.0)).is_empty());
        assert!(evaluate(&[], &all_factors(3.0)).is_empty());
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
                analyze_file(
                    &test_data(&format!("bad-image/{name}")),
                    &AnalyzeOptions { detect_stars: true },
                )
                .unwrap()
            })
            .collect()
    }

    fn flagged_names(bad: &[BadFrame]) -> Vec<String> {
        bad.iter()
            .map(|b| b.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    // The acceptance tests for the detection algorithm, against real frames
    // whose file names describe their defect. Un-ignore when `evaluate` is
    // implemented.

    #[test]
    #[ignore = "detection not implemented yet"]
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
    #[ignore = "detection not implemented yet"]
    fn the_floor_factor_alone_catches_the_starless_frames() {
        let files = measure_bad_image_fixtures();
        let params = BadFrameParams {
            floor: true,
            fwhm: false,
            eccentricity: false,
            sigma: 2.0,
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
    #[ignore = "detection not implemented yet"]
    fn the_eccentricity_factor_alone_catches_the_trailed_frames() {
        let files = measure_bad_image_fixtures();
        let params = BadFrameParams {
            floor: false,
            fwhm: false,
            eccentricity: true,
            sigma: 2.0,
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
