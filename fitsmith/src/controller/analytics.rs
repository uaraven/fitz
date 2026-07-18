//! The Tools ▸ Analytics and Tools ▸ Star metrics batches and chart: analyze
//! every target file off the UI thread, cache each frame's metrics, and turn the
//! chosen metric's series into the normalized geometry `chart.slint` draws.
//!
//! Two menu entries, one dialog. They differ in four things — the title, which
//! metrics the dropdown lists, whether the batch detects stars, and the export
//! file-name prefix — and share everything else: the chart, the zoom slider, the
//! progress overlay, the cancel path, both exports, the resizable card. So the
//! family travels as state ([`AppState::analytics_family`]) rather than as a
//! copy of the widget tree, and each entry point is a thin call into
//! [`open_chart_dialog`].
//!
//! Every metric of the open family is computed in one file read each, so
//! switching the dropdown re-plots from the cache with no re-read. Zoom needs no
//! controller code — the dialog's slider clamps itself to [1, 4] and the chart
//! reads the bound property directly.
//!
//! Those per-file results outlive the dialog, in [`AnalyticsCache`]: the numbers
//! depend on nothing but the file's bytes, so reopening over the same selection
//! reads no file at all and the dialog appears instantly, and adding one frame
//! to a 200-frame set analyzes one frame. [`plan_batch`] is the single place
//! that decides what gets read; everything else about the open path follows from
//! its answer.
//!
//! Both exports write the whole plotted series, not the part that happens to be
//! on screen: the SVG re-renders the same [`Plot`](crate::chart::Plot) geometry
//! the chart draws, the CSV writes the same series as rows. Neither reads the
//! zoom — it stretches the live chart's X axis only.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use anyhow::{Context, Result};
use libfitz::analytics::{
    self, AnalyzeOptions, FileAnalysis, FileMetrics, Metric, MetricFamily, Series, SkipReason,
};
use slint::{ComponentHandle, ModelRc, VecModel, Weak};

use crate::AppWindow;
use crate::chart::plot;
use crate::chart_svg::svg;
use crate::files::{display_name, is_fits_path};

use super::{AppState, STATE, operation_targets, set_row_status};

/// Map a metric dropdown index to its [`Metric`] within `family`. The dialog's
/// ComboBox model is built from [`Metric::of_family`], so the index is just a
/// position in that family's list; anything out of range falls back to the
/// family's default metric.
fn metric_for_index(family: MetricFamily, index: i32) -> Metric {
    usize::try_from(index)
        .ok()
        .and_then(|i| Metric::of_family(family).get(i).copied())
        .unwrap_or(default_metric(family))
}

/// The metric plotted when a family's dialog first opens.
fn default_metric(family: MetricFamily) -> Metric {
    match family {
        MetricFamily::Pixel => Metric::Mean,
        // The metric people actually cull subs on.
        MetricFamily::Star => Metric::Hfr,
    }
}

/// The dialog's title for a family.
fn family_title(family: MetricFamily) -> &'static str {
    match family {
        MetricFamily::Pixel => "Analytics",
        MetricFamily::Star => "Star metrics",
    }
}

/// Tools ▸ Analytics…: the pixel metrics. Never detects stars, so it stays
/// exactly as fast as it has always been.
pub fn open_analytics_dialog(app: &AppWindow) {
    open_chart_dialog(app, MetricFamily::Pixel);
}

/// Tools ▸ Star metrics…: star count, HFR, FWHM and eccentricity. Detects stars
/// because that is the entire point of opening it — the menu entry *is* the
/// opt-in, so there is no checkbox to forget.
pub fn open_star_metrics_dialog(app: &AppWindow) {
    open_chart_dialog(app, MetricFamily::Star);
}

/// Open the chart dialog for one metric family: gather the target FITS files
/// (the checked rows, or all of them when none are checked) and start the batch.
/// The dialog itself only appears once the metrics are in, so it never shows an
/// empty chart that then fills in.
fn open_chart_dialog(app: &AppWindow, family: MetricFamily) {
    let targets = operation_targets(is_fits_path);
    app.set_analytics_title(family_title(family).into());
    app.set_analytics_metric_model(ModelRc::new(VecModel::from(
        Metric::of_family(family)
            .iter()
            .map(|m| m.label().into())
            .collect::<Vec<slint::SharedString>>(),
    )));
    app.set_analytics_metric(
        Metric::of_family(family)
            .iter()
            .position(|&m| m == default_metric(family))
            .unwrap_or(0) as i32,
    );
    app.set_analytics_zoom(1.0);

    // Starting a batch supersedes any batch still running, and gets its own
    // cancel flag so stopping this one can't reach back and stop a later one.
    // The cache is not cleared — it is the whole point, and it outlives every
    // dialog session.
    let (generation, cancel, plan) = STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.analytics.clear();
        st.analytics_family = family;
        let generation = abort_batch(&mut st);
        st.analytics_cancel = Arc::new(AtomicBool::new(false));
        let plan = plan_batch(&targets, &st.analytics_cache, family);
        (generation, st.analytics_cancel.clone(), plan)
    });

    if targets.is_empty() {
        app.set_status_text("No FITS files to analyze".into());
        return;
    }

    // Reopened over a selection the cache already answers for: no worker, no
    // progress overlay, no file read at all — the dialog just appears.
    if plan.todo.is_empty() {
        show_results(app, plan, 0);
        return;
    }
    spawn_analysis(
        app.as_weak(),
        plan.todo,
        targets,
        family,
        generation,
        cancel,
    );
}

/// The family the open dialog is showing.
fn current_family() -> MetricFamily {
    STATE.with(|s| s.borrow().analytics_family)
}

/// Signal the running worker to stop between files and bump the generation so a
/// result already in flight lands stale. Returns the new generation. Shared by
/// cancelling, closing the dialog, and starting a fresh batch.
fn abort_batch(st: &mut AppState) -> u64 {
    st.analytics_cancel.store(true, Ordering::Relaxed);
    st.analytics_generation += 1;
    st.analytics_generation
}

// --- the analysis cache ---------------------------------------------------

/// Per-file analysis results, keyed by path. Lives in [`AppState`] and outlives
/// any one dialog session — see the field's comment for why it is unbounded.
pub(super) type AnalyticsCache = HashMap<PathBuf, CachedAnalysis>;

/// One cached file analysis and the identity of the bytes it was computed from.
pub(super) struct CachedAnalysis {
    /// The outcome, or the reason the frame was skipped. Caching a skip matters
    /// as much as caching a result: a frame with no DATE-OBS must not be re-read
    /// on every open just to be skipped again.
    outcome: FileAnalysis,
    stamp: FileStamp,
}

/// Cheap filesystem identity of a file's contents. The files under us do change
/// — `convert` rewrites them in place, and a capture program may still be
/// filling the folder — so every cache hit is revalidated against a fresh stamp.
#[derive(PartialEq, Clone, Copy, Debug)]
struct FileStamp {
    len: u64,
    mtime: Option<SystemTime>,
}

impl FileStamp {
    /// Stamp `path`, or `None` if its metadata can't be read — which is itself
    /// treated as a cache miss, never as a match.
    fn of(path: &Path) -> Option<FileStamp> {
        let md = std::fs::metadata(path).ok()?;
        Some(FileStamp {
            len: md.len(),
            mtime: md.modified().ok(),
        })
    }
}

/// Whether a cached outcome answers what `family` needs. The two families' work
/// is *nested*, not disjoint: a star analysis computes the pixel statistics too,
/// so it satisfies both, while a pixel analysis has no stars to report.
///
/// This is what makes Analytics-after-Star-metrics free, and Star-metrics-after
/// -Analytics an honest re-read: the second genuinely needs the detection pass.
fn satisfies(outcome: &FileAnalysis, family: MetricFamily) -> bool {
    match (outcome, family) {
        // A frame with no acquisition time is skipped by either family.
        (FileAnalysis::Skipped(_), _) => true,
        (FileAnalysis::Analyzed(_), MetricFamily::Pixel) => true,
        (FileAnalysis::Analyzed(m), MetricFamily::Star) => m.stars.is_some(),
    }
}

/// What one dialog open has to do: what the cache can already answer, and what
/// still has to be read. The single place that decides whether a file is read,
/// so the "reopening changes nothing" guarantee is testable without an event
/// loop — assert on `todo`.
#[derive(Default)]
struct Plan {
    /// The frames to plot, from usable cache entries.
    metrics: Vec<FileMetrics>,
    /// Cached frames skipped for want of an acquisition time.
    no_date: usize,
    /// Targets with no usable entry, in target order.
    todo: Vec<PathBuf>,
}

/// Partition `targets` into what `cache` already answers for `family` and what
/// must be analyzed. An entry is usable only if it [`satisfies`] the family
/// *and* its stamp still matches the file on disk.
fn plan_batch(targets: &[PathBuf], cache: &AnalyticsCache, family: MetricFamily) -> Plan {
    let mut plan = Plan::default();
    for path in targets {
        match cache.get(path) {
            Some(entry)
                if satisfies(&entry.outcome, family)
                    && FileStamp::of(path) == Some(entry.stamp) =>
            {
                match &entry.outcome {
                    FileAnalysis::Analyzed(m) => plan.metrics.push(m.clone()),
                    FileAnalysis::Skipped(SkipReason::NoDateObs) => plan.no_date += 1,
                }
            }
            _ => plan.todo.push(path.clone()),
        }
    }
    plan
}

/// Record one file's outcome. Deliberately *not* generation-checked: the numbers
/// are a pure function of the file's bytes and are stamp-validated on the way
/// out, so a superseded batch's results are still correct and worth keeping.
/// Only the *plot* is generation-gated — which is what makes a cancelled batch
/// leave its finished work behind for the next open instead of throwing it away.
fn cache_outcome(path: PathBuf, outcome: FileAnalysis, stamp: FileStamp) {
    STATE.with(|s| {
        s.borrow_mut()
            .analytics_cache
            .insert(path, CachedAnalysis { outcome, stamp })
    });
}

/// Analyze `targets` in order, reporting each file to `progress` before reading
/// it, each outcome to `analyzed` as soon as it lands, and each read failure to
/// `failed`. Returns the number of read failures, or `None` if `cancel` was
/// raised — checked between files, so a long batch stops promptly without
/// finishing the queue. Holds no UI types, so it is testable directly.
///
/// Outcomes are reported one at a time rather than returned in a lump so a
/// cancelled batch's completed work can still be cached: stopping 190 files into
/// a 200-file batch should cost the last 10, not all 200.
///
/// Read failures are reported but not handed to `analyzed`, so they are never
/// cached: a file that failed to open may simply be mid-write by a capture
/// program, and retrying on the next open is the right behaviour.
fn analyze_batch(
    targets: &[PathBuf],
    family: MetricFamily,
    cancel: &AtomicBool,
    mut progress: impl FnMut(usize, &Path),
    mut analyzed: impl FnMut(&Path, FileAnalysis),
    mut failed: impl FnMut(&Path, String),
) -> Option<usize> {
    // Only the star family pays for detection: this is what keeps Analytics
    // from silently getting slower now that the machinery exists.
    let opts = AnalyzeOptions {
        detect_stars: family == MetricFamily::Star,
    };
    let mut failures = 0;
    for (i, path) in targets.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        progress(i, path);
        match analytics::analyze_file(path, &opts) {
            Ok(outcome) => analyzed(path, outcome),
            Err(e) => {
                failures += 1;
                failed(path, e.to_string());
            }
        }
    }
    // A cancel raised during the final file still counts.
    (!cancel.load(Ordering::Relaxed)).then_some(failures)
}

/// Run the batch on a worker thread behind the cancellable progress overlay, and
/// show the dialog with the results. A file that can't be read is badged with
/// its error but does not abort the batch. A cancelled or superseded batch opens
/// no dialog and plots nothing — but its completed analyses stay cached, so the
/// next open picks up where it left off.
///
/// `todo` is only the files that need reading; `all` is the full target list the
/// dialog plots, re-planned from the cache once the batch lands. Progress counts
/// `todo`, so the overlay reads `3/7` rather than `3/200` — it reports the work
/// actually being done.
fn spawn_analysis(
    weak: Weak<AppWindow>,
    todo: Vec<PathBuf>,
    all: Vec<PathBuf>,
    family: MetricFamily,
    generation: u64,
    cancel: Arc<AtomicBool>,
) {
    let _ = weak.upgrade_in_event_loop(|app| {
        app.set_analytics_in_progress(true);
        app.set_analytics_progress(0.0);
        app.set_analytics_progress_text("".into());
        app.set_analytics_progress_detail("".into());
        app.set_busy(true);
        app.set_stage_text("".into());
    });
    std::thread::spawn(move || {
        let total = todo.len();
        let failures = analyze_batch(
            &todo,
            family,
            &cancel,
            |i, path| {
                let progress = i as f32 / total as f32;
                let text = display_name(path);
                let detail = format!("{}/{}", i + 1, total);
                let _ = weak.clone().upgrade_in_event_loop(move |app| {
                    app.set_analytics_progress(progress);
                    app.set_analytics_progress_text(text.into());
                    app.set_analytics_progress_detail(detail.into());
                });
            },
            |path, outcome| {
                // Stamp on the worker, right after the read, so the entry
                // records the bytes it was actually computed from. A file whose
                // metadata vanished between the two is simply not cached.
                let Some(stamp) = FileStamp::of(path) else {
                    return;
                };
                let path = path.to_path_buf();
                let _ = weak
                    .clone()
                    .upgrade_in_event_loop(move |_app| cache_outcome(path, outcome, stamp));
            },
            |path, msg| {
                let path = path.to_path_buf();
                let _ = weak.clone().upgrade_in_event_loop(move |_app| {
                    set_row_status(&path, "error", &msg);
                });
            },
        );
        // Cancelling already reset the overlay, so a stopped batch just leaves —
        // its cached outcomes are already in, queued ahead of this point.
        let Some(failures) = failures else {
            return;
        };

        let _ = weak.upgrade_in_event_loop(move |app| {
            if STATE.with(|s| s.borrow().analytics_generation) != generation {
                return; // a newer batch (or a close) superseded this one
            }
            clear_progress(&app);
            app.set_analytics_progress(1.0);
            // Every `analyzed` callback above was queued on this same event
            // loop before this closure, so the cache now answers for the whole
            // target list bar the files that failed to read.
            let plan = STATE.with(|s| plan_batch(&all, &s.borrow().analytics_cache, family));
            show_results(&app, plan, failures);
        });
    });
}

/// Fill the dialog from a plan whose `todo` is done with, and show it.
fn show_results(app: &AppWindow, plan: Plan, failures: usize) {
    app.set_analytics_skipped_no_date(plan.no_date as i32);
    STATE.with(|s| s.borrow_mut().analytics = plan.metrics);
    replot(app);
    app.set_show_analytics(true);
    if failures > 0 {
        app.set_status_text(format!("Analytics: {failures} file(s) failed to read").into());
    }
}

/// Take down the progress overlay and the busy chrome it turned on.
fn clear_progress(app: &AppWindow) {
    app.set_analytics_in_progress(false);
    app.set_busy(false);
    app.set_stage_text("".into());
}

/// Cancel the running batch from the progress overlay's Cancel button: stop the
/// worker and take the overlay down without opening the dialog. The frames it
/// did finish stay in the analysis cache — they are correct regardless of who
/// asked for them — so reopening resumes rather than starting over.
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

/// Close the dialog: drop the plotted series and stop any batch still running
/// for it. Leaves `analytics_cache` alone — surviving the close is exactly what
/// it is for.
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
    let series = current_series(app);
    let plot = plot(&series);

    app.set_analytics_metric_label(series.metric.label().into());
    app.set_analytics_plotted_count(series.points.len() as i32);
    app.set_analytics_unavailable_note(unavailable_note(&series).into());
    app.set_analytics_points(ModelRc::new(VecModel::from(plot.points)));
    app.set_analytics_x_ticks(ModelRc::new(VecModel::from(plot.x_ticks)));
    app.set_analytics_y_ticks(ModelRc::new(VecModel::from(plot.y_ticks)));
    app.set_analytics_line_commands(plot.line.into());
}

/// How to word the frames that analyzed fine but have no value for the plotted
/// metric. Family-specific, hence a controller-built string: in the star dialog
/// the reason is always that detection found nothing — itself worth reading, a
/// run of starless frames is a cloud indicator. Empty when there are none.
fn unavailable_note(series: &Series) -> String {
    match (series.unavailable, series.metric.family()) {
        (0, _) => String::new(),
        (n, MetricFamily::Star) => format!("{n} with no stars detected"),
        (n, MetricFamily::Pixel) => format!("{n} without a value"),
    }
}

/// The series currently on the chart, rebuilt from the cached metrics.
fn current_series(app: &AppWindow) -> Series {
    let metric = metric_for_index(current_family(), app.get_analytics_metric());
    STATE.with(|s| analytics::build_series(&s.borrow().analytics, metric))
}

/// A default export file name for the plotted metric, e.g. `analytics-mean-adu.svg`
/// or `star-hfr.svg` — the prefix says which dialog it came out of.
fn export_file_name(metric: Metric, extension: &str) -> String {
    let slug: String = metric
        .label()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let prefix = match metric.family() {
        MetricFamily::Pixel => "analytics",
        MetricFamily::Star => "star",
    };
    format!("{prefix}-{}.{extension}", slug.to_ascii_lowercase())
}

/// Prompt for an export path, offering a name derived from the plotted metric.
/// `None` if the user cancelled.
fn prompt_export_path(metric: Metric, filter: &str, extension: &str) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .add_filter(filter, &[extension])
        .set_file_name(export_file_name(metric, extension))
        .save_file()
}

/// Report an export's outcome in the status bar. A cancelled save is the user's
/// own doing, so it says nothing.
fn report_export(app: &AppWindow, kind: &str, result: Result<Option<PathBuf>>) {
    match result {
        Ok(Some(path)) => app.set_status_text(format!("{kind} saved to {}", path.display()).into()),
        Ok(None) => {}
        Err(e) => app.set_status_text(format!("{kind} export failed: {e:#}").into()),
    }
}

/// Export the chart as an SVG. Re-rendered from the plotted series rather than
/// captured off the screen, so the file always holds the entire chart — a
/// zoomed-in dialog is showing a slice of it, and used to export just that.
pub fn analytics_export_svg(app: &AppWindow) {
    report_export(app, "SVG", export_svg(app));
}

/// Render and write the chart as SVG; `Ok(None)` if the user cancelled.
fn export_svg(app: &AppWindow) -> Result<Option<PathBuf>> {
    let series = current_series(app);
    let doc = svg(&plot(&series), series.metric.label());
    let Some(path) = prompt_export_path(series.metric, "SVG image", "svg") else {
        return Ok(None);
    };
    let write =
        || -> io::Result<()> { BufWriter::new(File::create(&path)?).write_all(doc.as_bytes()) };
    write().with_context(|| format!("cannot write {}", path.display()))?;
    Ok(Some(path))
}

/// Export the plotted series as CSV.
pub fn analytics_export_csv(app: &AppWindow) {
    report_export(app, "CSV", export_csv(app));
}

/// Write the current series as CSV; `Ok(None)` if the user cancelled.
fn export_csv(app: &AppWindow) -> Result<Option<PathBuf>> {
    let series = current_series(app);
    let Some(path) = prompt_export_path(series.metric, "CSV file", "csv") else {
        return Ok(None);
    };
    let write = || -> io::Result<()> {
        analytics::write_csv(&series, BufWriter::new(File::create(&path)?))
    };
    write().with_context(|| format!("cannot write {}", path.display()))?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_data;

    /// What a run of [`analyze_batch`] produced, tallied the way the cache and
    /// the dialog between them would consume it.
    #[derive(Default)]
    struct Tally {
        metrics: Vec<FileMetrics>,
        no_date: usize,
        failed: usize,
    }

    /// Run a batch with no progress or failure reporting, tallying its outcomes.
    /// `None` when it was cancelled.
    fn tally(targets: &[PathBuf], family: MetricFamily, cancel: &AtomicBool) -> Option<Tally> {
        let mut t = Tally::default();
        let failed = analyze_batch(
            targets,
            family,
            cancel,
            |_, _| {},
            |_, outcome| match outcome {
                FileAnalysis::Analyzed(m) => t.metrics.push(m),
                FileAnalysis::Skipped(SkipReason::NoDateObs) => t.no_date += 1,
            },
            |_, _| {},
        )?;
        t.failed = failed;
        Some(t)
    }

    /// Populate a cache the way a completed batch does — the setup every
    /// planning test starts from.
    fn analyzed_cache(targets: &[PathBuf], family: MetricFamily) -> AnalyticsCache {
        let mut cache = AnalyticsCache::new();
        analyze_batch(
            targets,
            family,
            &AtomicBool::new(false),
            |_, _| {},
            |path, outcome| {
                let stamp = FileStamp::of(path).unwrap();
                cache.insert(path.to_path_buf(), CachedAnalysis { outcome, stamp });
            },
            |_, _| {},
        );
        cache
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
        let (mut order, mut failures, mut analyzed) = (Vec::new(), Vec::new(), Vec::new());
        let failed = analyze_batch(
            &targets,
            MetricFamily::Pixel,
            &cancel,
            |i, path| order.push((i, path.to_path_buf())),
            |path, _| analyzed.push(path.to_path_buf()),
            |path, msg| failures.push((path.to_path_buf(), msg)),
        )
        .unwrap();

        // Both bundled frames are raw mosaics carrying an acquisition time
        // (DATE-LOC, else DATE-OBS; the second tile-compressed, decompressed
        // transparently), so both measure — and each is reported for caching as
        // it lands, rather than in a lump at the end.
        assert_eq!(analyzed, targets[..2]);
        // The unreadable path is reported once and doesn't abort the batch. It
        // is *not* handed to `analyzed`, so nothing caches the failure.
        assert_eq!(failed, 1);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, missing);
        // Progress fires once per file, in order, before that file is read.
        assert_eq!(order.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(order[0].1, targets[0]);

        // The same run, tallied: two measured frames, neither paying for star
        // detection the pixel family didn't ask for.
        let t = tally(&targets, MetricFamily::Pixel, &cancel).unwrap();
        assert_eq!(t.metrics.len(), 2);
        assert_eq!((t.no_date, t.failed), (0, 1));
        assert!(t.metrics.iter().all(|m| m.stars.is_none()));
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
        let (mut seen, mut finished) = (Vec::new(), 0);
        let stopped = analyze_batch(
            &targets,
            MetricFamily::Pixel,
            &cancel,
            |_, path| {
                seen.push(path.to_path_buf());
                cancel.store(true, Ordering::Relaxed);
            },
            |_, _| finished += 1,
            |_, _| {},
        );
        assert!(stopped.is_none());
        assert_eq!(seen.len(), 1, "the second file must never be read");
        // The file that did finish was still reported, so a cancelled batch
        // leaves its completed work in the cache instead of throwing it away.
        assert_eq!(finished, 1);

        // Already cancelled before the first file: nothing is read at all.
        let cancel = AtomicBool::new(true);
        let mut count = 0;
        let stopped = analyze_batch(
            &targets,
            MetricFamily::Pixel,
            &cancel,
            |_, _| count += 1,
            |_, _| {},
            |_, _| {},
        );
        assert!(stopped.is_none());
        assert_eq!(count, 0);
    }

    #[test]
    fn analyze_batch_detects_stars_only_for_the_star_family() {
        // The star family's whole reason for existing: its batch measures
        // stars on a real mosaic.
        let targets = vec![test_data("uncompressed.fit")];
        let cancel = AtomicBool::new(false);
        let t = tally(&targets, MetricFamily::Star, &cancel).unwrap();

        let stars = t.metrics[0].stars.as_ref().expect("stars measured");
        assert!(stars.count > 0);
        assert!(Metric::Hfr.value(&t.metrics[0]).is_some());

        // Same file, pixel family: no detection, and so no star metric has an
        // answer. This is the test that Analytics did not silently start paying
        // for star detection.
        let t = tally(&targets, MetricFamily::Pixel, &cancel).unwrap();
        assert!(t.metrics[0].stars.is_none());
        assert_eq!(Metric::Hfr.value(&t.metrics[0]), None);
    }

    /// Write a minimal frame with no acquisition time — the input `analyze_file`
    /// answers with [`SkipReason::NoDateObs`].
    fn write_undated_frame(path: &Path) {
        use libfitz::fitskit::{FitsFile, ImageData, PixelData};
        let img = ImageData::new(vec![2, 2], PixelData::I16(vec![7; 4]));
        FitsFile::with_primary_image(img).to_file(path).unwrap();
    }

    #[test]
    fn satisfies_follows_the_nesting_of_the_two_families() {
        let starry = tally(
            &[test_data("uncompressed.fit")],
            MetricFamily::Star,
            &AtomicBool::new(false),
        )
        .unwrap();
        let pixel = tally(
            &[test_data("uncompressed.fit")],
            MetricFamily::Pixel,
            &AtomicBool::new(false),
        )
        .unwrap();

        // A star analysis computed the pixel statistics on its way, so it
        // answers for both dialogs.
        let star_entry = FileAnalysis::Analyzed(starry.metrics[0].clone());
        assert!(satisfies(&star_entry, MetricFamily::Star));
        assert!(satisfies(&star_entry, MetricFamily::Pixel));

        // A pixel analysis has no stars to report, so the star dialog must read
        // the frame again.
        let pixel_entry = FileAnalysis::Analyzed(pixel.metrics[0].clone());
        assert!(satisfies(&pixel_entry, MetricFamily::Pixel));
        assert!(!satisfies(&pixel_entry, MetricFamily::Star));

        // A frame with no acquisition time is skipped by either family — and
        // caching that is the point: it must not be re-read just to be skipped
        // again.
        let skipped = FileAnalysis::Skipped(SkipReason::NoDateObs);
        assert!(satisfies(&skipped, MetricFamily::Pixel));
        assert!(satisfies(&skipped, MetricFamily::Star));
    }

    #[test]
    fn plan_batch_reads_every_target_with_an_empty_cache() {
        let targets = vec![
            test_data("uncompressed.fit"),
            test_data("compressed.fits.fz"),
        ];
        let plan = plan_batch(&targets, &AnalyticsCache::new(), MetricFamily::Pixel);
        assert_eq!(plan.todo, targets);
        assert!(plan.metrics.is_empty());
        assert_eq!(plan.no_date, 0);
    }

    #[test]
    fn reopening_over_an_unchanged_selection_reads_nothing() {
        // The headline requirement: analyze once, reopen, read no file.
        let tmp = tempfile::tempdir().unwrap();
        let undated = tmp.path().join("undated.fits");
        write_undated_frame(&undated);
        let targets = vec![
            test_data("uncompressed.fit"),
            test_data("compressed.fits.fz"),
            undated.clone(),
        ];

        let cache = analyzed_cache(&targets, MetricFamily::Pixel);
        let plan = plan_batch(&targets, &cache, MetricFamily::Pixel);
        assert!(plan.todo.is_empty(), "a reopen must read no file");
        assert_eq!(plan.metrics.len(), 2);
        // The skipped frame is still counted for the dialog's readout — cached
        // as a skip rather than re-read to rediscover it.
        assert_eq!(plan.no_date, 1);

        // Checking one more file analyzes exactly that one, not the whole set.
        let mut grown = targets.clone();
        grown.push(test_data("uncompressed_debayer.fits"));
        let plan = plan_batch(&grown, &cache, MetricFamily::Pixel);
        assert_eq!(plan.todo, [test_data("uncompressed_debayer.fits")]);
        assert_eq!(plan.metrics.len(), 2);

        // Deselecting needs no invalidation: the surviving targets still hit.
        let plan = plan_batch(&targets[..1], &cache, MetricFamily::Pixel);
        assert!(plan.todo.is_empty());
        assert_eq!(plan.metrics.len(), 1);
    }

    #[test]
    fn star_metrics_re_reads_after_analytics_but_not_the_reverse() {
        let targets = vec![test_data("uncompressed.fit")];

        // Analytics then Star metrics: the detection pass is new information,
        // so the frame is genuinely read again.
        let pixel_cache = analyzed_cache(&targets, MetricFamily::Pixel);
        assert_eq!(
            plan_batch(&targets, &pixel_cache, MetricFamily::Star).todo,
            targets
        );

        // Star metrics then Analytics: the richer entry already holds the pixel
        // statistics, so nothing is read.
        let star_cache = analyzed_cache(&targets, MetricFamily::Star);
        let plan = plan_batch(&targets, &star_cache, MetricFamily::Pixel);
        assert!(plan.todo.is_empty());
        assert!(plan.metrics[0].stars.is_some());
        // And reopening the star dialog is free too.
        assert!(
            plan_batch(&targets, &star_cache, MetricFamily::Star)
                .todo
                .is_empty()
        );
    }

    #[test]
    fn a_rewritten_file_is_a_cache_miss() {
        // The files under us do change — `convert` rewrites them in place, and a
        // capture program may still be filling the folder — so a stale entry
        // must not be plotted.
        let tmp = tempfile::tempdir().unwrap();
        let frame = tmp.path().join("frame.fit");
        std::fs::copy(test_data("uncompressed.fit"), &frame).unwrap();
        let targets = vec![frame.clone()];

        let cache = analyzed_cache(&targets, MetricFamily::Pixel);
        let before = FileStamp::of(&frame).unwrap();
        assert!(
            plan_batch(&targets, &cache, MetricFamily::Pixel)
                .todo
                .is_empty()
        );

        // Rewrite it with different content: the stamp moves and the entry is
        // no longer usable.
        write_undated_frame(&frame);
        let after = FileStamp::of(&frame).unwrap();
        assert_ne!(before, after);
        assert_eq!(
            plan_batch(&targets, &cache, MetricFamily::Pixel).todo,
            targets
        );

        // A file that has gone missing entirely is a miss too, never a hit on
        // the stale entry.
        std::fs::remove_file(&frame).unwrap();
        assert_eq!(FileStamp::of(&frame), None);
        assert_eq!(
            plan_batch(&targets, &cache, MetricFamily::Pixel).todo,
            targets
        );
    }

    #[test]
    fn metric_index_maps_to_the_dropdown_order() {
        // Each dialog's ComboBox model is built from its own family's list, so
        // an index is a position within that family — this is what keeps a
        // stored index meaning what it meant.
        for family in [MetricFamily::Pixel, MetricFamily::Star] {
            for (i, &m) in Metric::of_family(family).iter().enumerate() {
                assert_eq!(metric_for_index(family, i as i32), m);
            }
            // Out-of-range (and Slint's -1 "no selection") fall back to the
            // family's default, never to the other family's metric.
            assert_eq!(metric_for_index(family, -1), default_metric(family));
            assert_eq!(metric_for_index(family, 99), default_metric(family));
            assert_eq!(default_metric(family).family(), family);
        }
        // The star list is short: an index valid in the pixel dialog must not
        // silently address something in the star one.
        assert_eq!(metric_for_index(MetricFamily::Star, 9), Metric::Hfr);
    }

    #[test]
    fn unavailable_note_words_itself_per_family() {
        let note = |metric, unavailable| {
            unavailable_note(&Series {
                metric,
                points: Vec::new(),
                unavailable,
            })
        };
        // Nothing missing: no note at all, not "0 with no stars".
        assert_eq!(note(Metric::Hfr, 0), "");
        assert_eq!(note(Metric::Hfr, 2), "2 with no stars detected");
        assert_eq!(note(Metric::Mean, 2), "2 without a value");
    }

    #[test]
    fn export_file_name_slugifies_the_metric() {
        assert_eq!(
            export_file_name(Metric::Mean, "svg"),
            "analytics-mean-adu.svg"
        );
        assert_eq!(
            export_file_name(Metric::MaxPixelCount, "csv"),
            "analytics-max-adu-count.csv"
        );
        // The slugifier replaces every non-ASCII-alphanumeric char with '-', so
        // a "Noise σ" label would slug to a bare "analytics-noise-.svg". The
        // label spells sigma out instead — the fix belongs in the label, not in
        // a special case here.
        assert_eq!(
            export_file_name(Metric::Sigma, "svg"),
            "analytics-noise-sigma.svg"
        );

        // A star metric exports under its own prefix, so the two dialogs' files
        // don't collide in a download folder. All four star labels are ASCII,
        // so the slugifier needs no help this time.
        assert_eq!(export_file_name(Metric::Hfr, "svg"), "star-hfr.svg");
        assert_eq!(
            export_file_name(Metric::Eccentricity, "csv"),
            "star-eccentricity.csv"
        );
        assert_eq!(
            export_file_name(Metric::StarCount, "svg"),
            "star-star-count.svg"
        );
    }
}
