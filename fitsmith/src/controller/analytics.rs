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
//! Both exports write the whole plotted series, not the part that happens to be
//! on screen: the SVG re-renders the same [`Plot`](crate::chart::Plot) geometry
//! the chart draws, the CSV writes the same series as rows. Neither reads the
//! zoom — it stretches the live chart's X axis only.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    let (generation, cancel) = STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.analytics.clear();
        st.analytics_family = family;
        let generation = abort_batch(&mut st);
        st.analytics_cancel = Arc::new(AtomicBool::new(false));
        (generation, st.analytics_cancel.clone())
    });

    if targets.is_empty() {
        app.set_status_text("No FITS files to analyze".into());
        return;
    }
    spawn_analysis(app.as_weak(), targets, family, generation, cancel);
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

/// The tally of one analytics batch: the frames that produced metrics, plus the
/// per-reason counts of those that didn't.
#[derive(Default)]
struct Batch {
    metrics: Vec<FileMetrics>,
    no_date: usize,
    failed: usize,
}

/// Analyze `targets` in order, reporting each file to `progress` before reading
/// it and each read failure to `failed`. Checks `cancel` between files and
/// returns `None` the moment it is raised, so a long batch stops promptly
/// without finishing the queue. Holds no UI types, so it is testable directly.
fn analyze_batch(
    targets: &[PathBuf],
    family: MetricFamily,
    cancel: &AtomicBool,
    mut progress: impl FnMut(usize, &Path),
    mut failed: impl FnMut(&Path, String),
) -> Option<Batch> {
    // Only the star family pays for detection: this is what keeps Analytics
    // from silently getting slower now that the machinery exists.
    let opts = AnalyzeOptions {
        detect_stars: family == MetricFamily::Star,
    };
    let mut batch = Batch::default();
    for (i, path) in targets.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        progress(i, path);
        match analytics::analyze_file(path, &opts) {
            Ok(FileAnalysis::Analyzed(m)) => batch.metrics.push(m),
            Ok(FileAnalysis::Skipped(SkipReason::NoDateObs)) => batch.no_date += 1,
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
        let total = targets.len();
        let batch = analyze_batch(
            &targets,
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
            MetricFamily::Pixel,
            &cancel,
            |i, path| order.push((i, path.to_path_buf())),
            |path, msg| failures.push((path.to_path_buf(), msg)),
        )
        .unwrap();

        // Both bundled frames are raw mosaics carrying an acquisition time
        // (DATE-LOC, else DATE-OBS; the second tile-compressed, decompressed
        // transparently), so both measure.
        assert_eq!(batch.metrics.len(), 2);
        assert_eq!(batch.no_date, 0);
        // The pixel family must not have paid for star detection.
        assert!(batch.metrics.iter().all(|m| m.stars.is_none()));
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
            MetricFamily::Pixel,
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
        let stopped = analyze_batch(
            &targets,
            MetricFamily::Pixel,
            &cancel,
            |_, _| count += 1,
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
        let batch =
            analyze_batch(&targets, MetricFamily::Star, &cancel, |_, _| {}, |_, _| {}).unwrap();

        let stars = batch.metrics[0].stars.as_ref().expect("stars measured");
        assert!(stars.count > 0);
        assert!(Metric::Hfr.value(&batch.metrics[0]).is_some());

        // Same file, pixel family: no detection, and so no star metric has an
        // answer. This is the test that Analytics did not silently start paying
        // for star detection.
        let batch =
            analyze_batch(&targets, MetricFamily::Pixel, &cancel, |_, _| {}, |_, _| {}).unwrap();
        assert!(batch.metrics[0].stars.is_none());
        assert_eq!(Metric::Hfr.value(&batch.metrics[0]), None);
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
