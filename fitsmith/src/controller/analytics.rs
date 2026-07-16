//! The Tools ▸ Analytics batch and chart: analyze every target file off the UI
//! thread, cache each frame's metrics, and turn the chosen metric's series into
//! the normalized geometry `chart.slint` draws.
//!
//! Every phase-1 metric is computed in one pixel read per file, so switching the
//! dropdown re-plots from the cache with no re-read ([`set_metric`] is pure
//! arithmetic). Zoom needs no controller code — the dialog's slider clamps
//! itself to [1, 4] and the chart reads the bound property directly.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fitz_core::analytics::{self, FileAnalysis, FileMetrics, Metric, Series, SkipReason};
use slint::{ComponentHandle, ModelRc, VecModel, Weak};

use crate::files::{display_name, is_fits_path};
use crate::view::format_stat;
use crate::{AppWindow, ChartPoint, ChartTick};

use super::{AppState, STATE, operation_targets, set_row_status};

/// Map a metric dropdown index to its [`Metric`]. The dialog's ComboBox model is
/// built from [`Metric::all`], so the index is just a position in that list;
/// anything out of range falls back to the default metric.
fn metric_for_index(index: i32) -> Metric {
    usize::try_from(index)
        .ok()
        .and_then(|i| Metric::all().get(i).copied())
        .unwrap_or(DEFAULT_METRIC)
}

/// The metric plotted when the dialog first opens.
const DEFAULT_METRIC: Metric = Metric::Mean;

/// Open the Analytics dialog: gather the target FITS files (the checked rows, or
/// all of them when none are checked) and start the batch. The dialog itself
/// only appears once the metrics are in, so it never shows an empty chart that
/// then fills in.
pub fn open_analytics_dialog(app: &AppWindow) {
    let targets = operation_targets(is_fits_path);
    app.set_analytics_metric_model(ModelRc::new(VecModel::from(
        Metric::all()
            .iter()
            .map(|m| m.label().into())
            .collect::<Vec<slint::SharedString>>(),
    )));
    app.set_analytics_metric(
        Metric::all()
            .iter()
            .position(|&m| m == DEFAULT_METRIC)
            .unwrap_or(0) as i32,
    );
    app.set_analytics_zoom(1.0);

    // Starting a batch supersedes any batch still running, and gets its own
    // cancel flag so stopping this one can't reach back and stop a later one.
    let (generation, cancel) = STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.analytics.clear();
        let generation = abort_batch(&mut st);
        st.analytics_cancel = Arc::new(AtomicBool::new(false));
        (generation, st.analytics_cancel.clone())
    });

    if targets.is_empty() {
        app.set_status_text("No FITS files to analyze".into());
        return;
    }
    spawn_analysis(app.as_weak(), targets, generation, cancel);
}

/// Signal the running worker to stop between files and bump the generation so a
/// result already in flight lands stale. Returns the new generation. Shared by
/// cancelling, closing the dialog, and starting a fresh batch.
fn abort_batch(st: &mut AppState) -> u64 {
    st.analytics_cancel.store(true, Ordering::Relaxed);
    st.analytics_generation += 1;
    st.analytics_generation
}

/// The tally of one analytics batch: the frames that produced metrics, plus the
/// per-reason counts of those that didn't.
#[derive(Default)]
struct Batch {
    metrics: Vec<FileMetrics>,
    no_date: usize,
    not_mono: usize,
    failed: usize,
}

/// Analyze `targets` in order, reporting each file to `progress` before reading
/// it and each read failure to `failed`. Checks `cancel` between files and
/// returns `None` the moment it is raised, so a long batch stops promptly
/// without finishing the queue. Holds no UI types, so it is testable directly.
fn analyze_batch(
    targets: &[PathBuf],
    cancel: &AtomicBool,
    mut progress: impl FnMut(usize, &Path),
    mut failed: impl FnMut(&Path, String),
) -> Option<Batch> {
    let mut batch = Batch::default();
    for (i, path) in targets.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        progress(i, path);
        match analytics::analyze_file(path) {
            Ok(FileAnalysis::Analyzed(m)) => batch.metrics.push(m),
            Ok(FileAnalysis::Skipped(SkipReason::NoDateObs)) => batch.no_date += 1,
            Ok(FileAnalysis::Skipped(SkipReason::NotMono)) => batch.not_mono += 1,
            Err(e) => {
                batch.failed += 1;
                failed(path, e.to_string());
            }
        }
    }
    // A cancel raised during the final file still counts.
    (!cancel.load(Ordering::Relaxed)).then_some(batch)
}

/// Run the batch on a worker thread behind the cancellable progress overlay, and
/// show the dialog with the results. A file that can't be read is badged with
/// its error but does not abort the batch. A cancelled or superseded batch drops
/// its results and opens no dialog.
fn spawn_analysis(
    weak: Weak<AppWindow>,
    targets: Vec<PathBuf>,
    generation: u64,
    cancel: Arc<AtomicBool>,
) {
    let _ = weak.upgrade_in_event_loop(|app| {
        app.set_analytics_in_progress(true);
        app.set_analytics_progress(0.0);
        app.set_analytics_progress_text("".into());
        app.set_busy(true);
        app.set_stage_text("".into());
    });
    std::thread::spawn(move || {
        let total = targets.len();
        let batch = analyze_batch(
            &targets,
            &cancel,
            |i, path| {
                let progress = i as f32 / total as f32;
                let text = format!("{} ({}/{})", display_name(path), i + 1, total);
                let _ = weak.clone().upgrade_in_event_loop(move |app| {
                    app.set_analytics_progress(progress);
                    app.set_analytics_progress_text(text.into());
                });
            },
            |path, msg| {
                let path = path.to_path_buf();
                let _ = weak.clone().upgrade_in_event_loop(move |_app| {
                    set_row_status(&path, "error", &msg);
                });
            },
        );
        // Cancelling already reset the overlay, so a stopped batch just leaves.
        let Some(batch) = batch else {
            return;
        };

        let _ = weak.upgrade_in_event_loop(move |app| {
            if STATE.with(|s| s.borrow().analytics_generation) != generation {
                return; // a newer batch (or a close) superseded this one
            }
            clear_progress(&app);
            app.set_analytics_progress(1.0);
            app.set_analytics_skipped_no_date(batch.no_date as i32);
            app.set_analytics_skipped_not_mono(batch.not_mono as i32);
            STATE.with(|s| s.borrow_mut().analytics = batch.metrics);
            replot(&app);
            app.set_show_analytics(true);
            if batch.failed > 0 {
                let failed = batch.failed;
                app.set_status_text(format!("Analytics: {failed} file(s) failed to read").into());
            }
        });
    });
}

/// Take down the progress overlay and the busy chrome it turned on.
fn clear_progress(app: &AppWindow) {
    app.set_analytics_in_progress(false);
    app.set_busy(false);
    app.set_stage_text("".into());
}

/// Cancel the running batch from the progress overlay's Cancel button: stop the
/// worker and take the overlay down without opening the dialog. Nothing to
/// un-cache — a batch only publishes its metrics once it finishes.
pub fn cancel_analytics(app: &AppWindow) {
    STATE.with(|s| abort_batch(&mut s.borrow_mut()));
    clear_progress(app);
    app.set_status_text("Analytics canceled".into());
}

/// Re-plot after the metric dropdown changed. Pure: [`Series`] comes from the
/// cached per-file metrics, so no file is read again.
pub fn analytics_metric_changed(app: &AppWindow, index: i32) {
    app.set_analytics_metric(index);
    replot(app);
}

/// Close the dialog: drop the cached metrics and stop any batch still running
/// for it.
pub fn close_analytics(app: &AppWindow) {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.analytics.clear();
        abort_batch(&mut st);
    });
    clear_progress(app);
}

/// Build the series for the currently selected metric from the cached metrics
/// and push its normalized geometry into the chart's properties.
fn replot(app: &AppWindow) {
    let metric = metric_for_index(app.get_analytics_metric());
    let series = STATE.with(|s| analytics::build_series(&s.borrow().analytics, metric));
    let plot = plot(&series);

    app.set_analytics_metric_label(metric.label().into());
    app.set_analytics_plotted_count(series.points.len() as i32);
    app.set_analytics_points(ModelRc::new(VecModel::from(plot.points)));
    app.set_analytics_x_ticks(ModelRc::new(VecModel::from(plot.x_ticks)));
    app.set_analytics_y_ticks(ModelRc::new(VecModel::from(plot.y_ticks)));
    app.set_analytics_line_commands(plot.line.into());
}

// --- plotting (pure) -----------------------------------------------------

/// A series rendered into the chart's coordinate space: points and ticks in
/// screen-normalized 0..1 (X from the left, Y from the top), plus the SVG path
/// for the series line in that same space.
#[derive(Default, PartialEq, Debug)]
struct Plot {
    points: Vec<ChartPoint>,
    x_ticks: Vec<ChartTick>,
    y_ticks: Vec<ChartTick>,
    line: String,
}

/// A value axis: the (nice, rounded-outward) bounds the plot maps onto and the
/// tick values inside them.
struct ValueAxis {
    lo: f64,
    hi: f64,
    ticks: Vec<f64>,
}

/// A "nice" axis step — 1, 2 or 5 times a power of ten — giving roughly `target`
/// intervals across `range`.
fn nice_step(range: f64, target: usize) -> f64 {
    // An empty, negative or NaN range has no meaningful step; 1.0 keeps the
    // caller's arithmetic finite.
    if range.is_nan() || range <= 0.0 {
        return 1.0;
    }
    let raw = range / target.max(1) as f64;
    let magnitude = 10f64.powf(raw.log10().floor());
    let normalized = raw / magnitude;
    let step = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    step * magnitude
}

/// Round `[min, max]` outward to whole multiples of a nice step and place a tick
/// at every step between. Plotting against these bounds (rather than the raw
/// min/max) puts the gridlines on round numbers and keeps the extreme points off
/// the frame edge. A flat series (min == max) gets an arbitrary ±1 range so its
/// line lands mid-plot instead of dividing by zero.
fn value_axis(min: f64, max: f64) -> ValueAxis {
    let (min, max) = if max > min {
        (min, max)
    } else {
        (min - 1.0, max + 1.0)
    };
    let step = nice_step(max - min, 4);
    let lo = (min / step).floor() * step;
    let hi = (max / step).ceil() * step;
    let count = ((hi - lo) / step).round().max(1.0) as usize;
    let ticks = (0..=count).map(|i| lo + i as f64 * step).collect();
    ValueAxis { lo, hi, ticks }
}

/// Tick steps for a time axis, in seconds: the human-readable divisions of a
/// minute, an hour and a day rather than the powers of ten [`nice_step`] gives.
const TIME_STEPS: [f64; 18] = [
    1.0, 2.0, 5.0, 10.0, 15.0, 30.0, // seconds
    60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, // minutes
    3600.0, 7200.0, 10800.0, 21600.0, 43200.0, 86400.0, // hours and a day
];

/// Tick timestamps across `[lo, hi]`, on round wall-clock boundaries (e.g. every
/// 15 minutes on the quarter hour). Unlike the value axis the bounds are *not*
/// rounded outward — points sit at their true times, so a session's gaps stay
/// visible — and ticks simply fall where they fall inside the range. Since the
/// step only ever rounds up and the ends rarely land on a boundary, aim for six
/// intervals to still label a session a few times over.
fn time_ticks(lo: f64, hi: f64) -> Vec<f64> {
    // A single instant (or an unusable range) gets one tick, at that instant.
    if lo.is_nan() || hi.is_nan() || hi <= lo {
        return vec![lo];
    }
    let target = (hi - lo) / 6.0;
    let step = TIME_STEPS
        .iter()
        .copied()
        .find(|&s| s >= target)
        .unwrap_or(86400.0);
    let mut ticks = Vec::new();
    let mut t = (lo / step).ceil() * step;
    while t <= hi {
        ticks.push(t);
        t += step;
    }
    // A range shorter than the smallest step leaves nothing on a boundary.
    if ticks.is_empty() {
        ticks.push(lo);
    }
    ticks
}

/// UTC time of day for an epoch timestamp: `HH:MM`, or `HH:MM:SS` with
/// `seconds`. `DATE-OBS` is UTC by FITS convention, so no zone conversion is
/// applied; the date is dropped because a session is read as a single night.
fn format_time(epoch: f64, seconds: bool) -> String {
    let total = epoch.rem_euclid(86400.0).floor() as i64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if seconds {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}")
    }
}

/// Map a time-ordered [`Series`] into the chart's 0..1 space: X spans the first
/// to the last timestamp, Y spans the value axis inverted (0 is the top). An
/// empty series plots nothing; a single point (or several sharing one timestamp)
/// centers on X, having no span to normalize against.
fn plot(series: &Series) -> Plot {
    let (Some(first), Some(last)) = (series.points.first(), series.points.last()) else {
        return Plot::default();
    };
    let (t_lo, t_hi) = (first.time, last.time);
    let axis = series
        .points
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
            (lo.min(p.value), hi.max(p.value))
        });
    let axis = value_axis(axis.0, axis.1);

    let x_of = |t: f64| {
        if t_hi > t_lo {
            ((t - t_lo) / (t_hi - t_lo)) as f32
        } else {
            0.5
        }
    };
    let y_of = |v: f64| {
        if axis.hi > axis.lo {
            (1.0 - (v - axis.lo) / (axis.hi - axis.lo)) as f32
        } else {
            0.5
        }
    };

    let points: Vec<ChartPoint> = series
        .points
        .iter()
        .map(|p| ChartPoint {
            x: x_of(p.time),
            y: y_of(p.value),
            time_label: format_time(p.time, true).into(),
            value_label: format_stat(p.value).into(),
        })
        .collect();

    // Slint can't repeat a Path with `for`, so the whole polyline arrives as one
    // pre-built SVG command string in the same 0..1 space as the points.
    let mut line = String::new();
    for (i, p) in points.iter().enumerate() {
        let verb = if i == 0 { 'M' } else { 'L' };
        line.push_str(&format!("{verb} {:.5} {:.5} ", p.x, p.y));
    }

    Plot {
        points,
        x_ticks: time_ticks(t_lo, t_hi)
            .into_iter()
            .map(|t| ChartTick {
                pos: x_of(t),
                label: format_time(t, false).into(),
            })
            .collect(),
        y_ticks: axis
            .ticks
            .into_iter()
            .map(|v| ChartTick {
                pos: y_of(v),
                label: format_stat(v).into(),
            })
            .collect(),
        line: line.trim_end().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_data;
    use fitz_core::analytics::SamplePoint;

    fn series(samples: &[(f64, f64)]) -> Series {
        Series {
            metric: Metric::Mean,
            points: samples
                .iter()
                .map(|&(time, value)| SamplePoint {
                    time,
                    time_str: String::new(),
                    value,
                    path: PathBuf::from("f.fits"),
                })
                .collect(),
        }
    }

    #[test]
    fn analyze_batch_measures_real_frames_and_records_failures() {
        let missing = PathBuf::from("/nonexistent/nope.fits");
        let targets = vec![
            test_data("uncompressed.fit"),
            test_data("compressed.fits.fz"),
            missing.clone(),
        ];
        let cancel = AtomicBool::new(false);
        let (mut order, mut failures) = (Vec::new(), Vec::new());
        let batch = analyze_batch(
            &targets,
            &cancel,
            |i, path| order.push((i, path.to_path_buf())),
            |path, msg| failures.push((path.to_path_buf(), msg)),
        )
        .unwrap();

        // Both bundled frames are raw mosaics carrying a DATE-OBS (the second
        // tile-compressed, decompressed transparently), so both measure.
        assert_eq!(batch.metrics.len(), 2);
        assert_eq!((batch.no_date, batch.not_mono), (0, 0));
        // The unreadable path is reported once and doesn't abort the batch.
        assert_eq!(batch.failed, 1);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, missing);
        // Progress fires once per file, in order, before that file is read.
        assert_eq!(order.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(order[0].1, targets[0]);
    }

    #[test]
    fn analyze_batch_stops_as_soon_as_cancel_is_raised() {
        let targets = vec![
            test_data("uncompressed.fit"),
            test_data("compressed.fits.fz"),
        ];

        // Cancel raised while the first file is in flight: the batch abandons
        // its results rather than finishing the queue.
        let cancel = AtomicBool::new(false);
        let mut seen = Vec::new();
        let stopped = analyze_batch(
            &targets,
            &cancel,
            |_, path| {
                seen.push(path.to_path_buf());
                cancel.store(true, Ordering::Relaxed);
            },
            |_, _| {},
        );
        assert!(stopped.is_none());
        assert_eq!(seen.len(), 1, "the second file must never be read");

        // Already cancelled before the first file: nothing is read at all.
        let cancel = AtomicBool::new(true);
        let mut count = 0;
        let stopped = analyze_batch(&targets, &cancel, |_, _| count += 1, |_, _| {});
        assert!(stopped.is_none());
        assert_eq!(count, 0);
    }

    #[test]
    fn metric_index_maps_to_the_dropdown_order() {
        // The ComboBox model is built from Metric::all(), so index == position.
        for (i, &m) in Metric::all().iter().enumerate() {
            assert_eq!(metric_for_index(i as i32), m);
        }
        // Out-of-range (and Slint's -1 "no selection") fall back to the default.
        assert_eq!(metric_for_index(-1), DEFAULT_METRIC);
        assert_eq!(metric_for_index(99), DEFAULT_METRIC);
    }

    #[test]
    fn nice_step_picks_1_2_5_decades() {
        assert_eq!(nice_step(4.0, 4), 1.0);
        assert_eq!(nice_step(8.0, 4), 2.0);
        assert_eq!(nice_step(20.0, 4), 5.0);
        assert_eq!(nice_step(400.0, 4), 100.0);
        assert_eq!(nice_step(0.4, 4), 0.1);
        // A degenerate range still yields a usable step.
        assert_eq!(nice_step(0.0, 4), 1.0);
    }

    #[test]
    fn value_axis_rounds_outward_to_round_ticks() {
        let axis = value_axis(1103.0, 1748.0);
        assert!(axis.lo <= 1103.0 && axis.hi >= 1748.0);
        // Every tick sits on a whole multiple of the step, spanning lo..hi.
        assert_eq!(*axis.ticks.first().unwrap(), axis.lo);
        assert_eq!(*axis.ticks.last().unwrap(), axis.hi);
        assert!(axis.ticks.len() >= 3);

        // A flat series gets a real range instead of dividing by zero.
        let flat = value_axis(500.0, 500.0);
        assert!(flat.hi > flat.lo);
    }

    #[test]
    fn time_ticks_land_on_wall_clock_boundaries() {
        // A 3-hour session ticks every half hour, on the half hour.
        let lo = fitz_core::info::parse_date_obs("2026-06-22T22:00:00").unwrap();
        let ticks = time_ticks(lo, lo + 3.0 * 3600.0);
        let labels: Vec<String> = ticks.iter().map(|&t| format_time(t, false)).collect();
        assert_eq!(
            labels,
            [
                "22:00", "22:30", "23:00", "23:30", "00:00", "00:30", "01:00"
            ]
        );

        // A ragged session (no end on a boundary) still ticks on round times,
        // and never outside its own range.
        let ragged = time_ticks(lo + 7.5, lo + 3.0 * 3600.0 - 128.0);
        assert!(ragged.iter().all(|&t| t > lo && t < lo + 3.0 * 3600.0));
        assert!(
            ragged
                .iter()
                .all(|&t| format_time(t, true).ends_with(":00"))
        );

        // A 12-hour span steps up to hours rather than crowding the axis.
        let long = time_ticks(lo, lo + 12.0 * 3600.0);
        assert!(long.len() <= 7);
        assert!(long.iter().all(|&t| format_time(t, false).ends_with(":00")));

        // A single instant still yields one tick rather than an empty axis.
        assert_eq!(time_ticks(lo, lo), vec![lo]);
    }

    #[test]
    fn format_time_renders_utc_time_of_day() {
        let t = fitz_core::info::parse_date_obs("2026-05-31T04:57:09.004664").unwrap();
        assert_eq!(format_time(t, false), "04:57");
        assert_eq!(format_time(t, true), "04:57:09");
        // Midnight, and an epoch before 1970 (rem_euclid keeps the day positive).
        assert_eq!(format_time(0.0, true), "00:00:00");
        assert_eq!(format_time(-1.0, true), "23:59:59");
    }

    #[test]
    fn plot_normalizes_points_into_the_unit_square() {
        // Three frames an hour apart with a rising metric.
        let lo = fitz_core::info::parse_date_obs("2026-06-22T22:00:00").unwrap();
        let p = plot(&series(&[
            (lo, 100.0),
            (lo + 3600.0, 150.0),
            (lo + 7200.0, 200.0),
        ]));

        // X spans first..last; the middle sample sits halfway.
        assert_eq!(p.points[0].x, 0.0);
        assert_eq!(p.points[1].x, 0.5);
        assert_eq!(p.points[2].x, 1.0);
        // Y is inverted: the largest value plots nearest the top.
        assert!(p.points[0].y > p.points[1].y && p.points[1].y > p.points[2].y);
        assert!(p.points.iter().all(|q| (0.0..=1.0).contains(&q.y)));
        assert_eq!(p.points[1].value_label, "150");
        assert_eq!(p.points[1].time_label, "23:00:00");

        // The line is one move followed by a lineto per remaining point, in the
        // same coordinates as the marks.
        assert!(p.line.starts_with("M 0.00000 "));
        assert_eq!(p.line.matches('L').count(), 2);

        // Ticks stay inside the plot and are labeled.
        assert!(p.x_ticks.iter().all(|t| (0.0..=1.0).contains(&t.pos)));
        assert!(p.y_ticks.iter().all(|t| (0.0..=1.0).contains(&t.pos)));
        assert!(!p.y_ticks.is_empty());
    }

    #[test]
    fn plot_handles_empty_and_degenerate_series() {
        // Nothing to plot: no points, no line, no ticks.
        assert_eq!(plot(&series(&[])), Plot::default());

        // A single frame has no time span to normalize against, so it centers.
        let one = plot(&series(&[(1000.0, 42.0)]));
        assert_eq!(one.points.len(), 1);
        assert_eq!(one.points[0].x, 0.5);
        assert!((0.0..=1.0).contains(&one.points[0].y));
        assert_eq!(one.x_ticks.len(), 1);

        // Several frames sharing one timestamp likewise collapse onto X 0.5
        // without producing NaNs.
        let same = plot(&series(&[(1000.0, 1.0), (1000.0, 2.0)]));
        assert!(same.points.iter().all(|p| p.x == 0.5 && p.y.is_finite()));
    }
}
