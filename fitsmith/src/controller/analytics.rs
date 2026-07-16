//! The Tools ▸ Analytics batch and chart: analyze every target file off the UI
//! thread, cache each frame's metrics, and turn the chosen metric's series into
//! the normalized geometry `chart.slint` draws.
//!
//! Every phase-1 metric is computed in one pixel read per file, so switching the
//! dropdown re-plots from the cache with no re-read ([`set_metric`] is pure
//! arithmetic). Zoom needs no controller code — the dialog's slider clamps
//! itself to [1, 4] and the chart reads the bound property directly.
//!
//! Both exports write what is currently plotted: the PNG is the live chart
//! cropped out of a window snapshot (one rendering path — no second plotter to
//! keep in sync), the CSV the same series as rows.

use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use fitz_core::analytics::{self, FileAnalysis, FileMetrics, Metric, Series, SkipReason};
use slint::{ComponentHandle, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel, Weak};

use crate::AppWindow;
use crate::chart::plot;
use crate::files::{display_name, is_fits_path};

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
    let series = current_series(app);
    let plot = plot(&series);

    app.set_analytics_metric_label(series.metric.label().into());
    app.set_analytics_plotted_count(series.points.len() as i32);
    app.set_analytics_points(ModelRc::new(VecModel::from(plot.points)));
    app.set_analytics_x_ticks(ModelRc::new(VecModel::from(plot.x_ticks)));
    app.set_analytics_y_ticks(ModelRc::new(VecModel::from(plot.y_ticks)));
    app.set_analytics_line_commands(plot.line.into());
}

/// The series currently on the chart, rebuilt from the cached metrics.
fn current_series(app: &AppWindow) -> Series {
    let metric = metric_for_index(app.get_analytics_metric());
    STATE.with(|s| analytics::build_series(&s.borrow().analytics, metric))
}

/// A default export file name for the plotted metric, e.g. `mean-adu.png`.
fn export_file_name(metric: Metric, extension: &str) -> String {
    let slug: String = metric
        .label()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("analytics-{}.{extension}", slug.to_ascii_lowercase())
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

/// A snapshot crop, in physical pixels.
#[derive(PartialEq, Debug)]
struct Crop {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// Convert the chart's logical-pixel rectangle within the window into physical
/// pixels of a `img_w` x `img_h` snapshot, clamped to the image. HiDPI displays
/// render more pixels than the layout's logical units, hence `scale`. Returns
/// `None` if the rectangle lands outside the snapshot or has no area, so a
/// degenerate export fails cleanly instead of writing an empty PNG.
fn crop_rect(rect: (f32, f32, f32, f32), scale: f32, img_w: u32, img_h: u32) -> Option<Crop> {
    let (x, y, w, h) = rect;
    let phys = |v: f32, max: u32| (v * scale).round().clamp(0.0, max as f32) as u32;
    let (x0, y0) = (phys(x, img_w), phys(y, img_h));
    let (x1, y1) = (phys(x + w, img_w), phys(y + h, img_h));
    (x1 > x0 && y1 > y0).then(|| Crop {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

/// Copy `crop` out of an RGBA snapshot as a tightly packed RGB8 buffer (the
/// form [`fitz_core::export::write_png`] takes). The chart is fully opaque, so
/// dropping alpha loses nothing.
fn crop_to_rgb8(snapshot: &SharedPixelBuffer<Rgba8Pixel>, crop: &Crop) -> Vec<u8> {
    let (stride, pixels) = (snapshot.width() as usize, snapshot.as_slice());
    let mut rgb = Vec::with_capacity(crop.w as usize * crop.h as usize * 3);
    for row in 0..crop.h as usize {
        let start = (crop.y as usize + row) * stride + crop.x as usize;
        for px in &pixels[start..start + crop.w as usize] {
            rgb.extend_from_slice(&[px.r, px.g, px.b]);
        }
    }
    rgb
}

/// Export the chart as a PNG. The dialog passes the chart's position and size
/// within the window; the snapshot is taken *before* the save dialog opens, so
/// the native file picker can't end up rendered into the image.
pub fn analytics_export_png(app: &AppWindow, x: f32, y: f32, w: f32, h: f32) {
    report_export(app, "PNG", export_png(app, (x, y, w, h)));
}

/// Snapshot, crop and write the chart; `Ok(None)` if the user cancelled.
fn export_png(app: &AppWindow, rect: (f32, f32, f32, f32)) -> Result<Option<PathBuf>> {
    let window = app.window();
    let snapshot = window
        .take_snapshot()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("cannot capture the window")?;
    let crop = crop_rect(
        rect,
        window.scale_factor(),
        snapshot.width(),
        snapshot.height(),
    )
    .context("the chart is not visible")?;
    let rgb = crop_to_rgb8(&snapshot, &crop);

    let metric = metric_for_index(app.get_analytics_metric());
    let Some(path) = prompt_export_path(metric, "PNG image", "png") else {
        return Ok(None);
    };
    fitz_core::export::write_png(&path, crop.w as usize, crop.h as usize, &rgb)?;
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

    /// A `w` x `h` snapshot whose every pixel encodes its own coordinates, so a
    /// crop's contents identify exactly which region was taken.
    fn snapshot(w: u32, h: u32) -> SharedPixelBuffer<Rgba8Pixel> {
        let mut buf = SharedPixelBuffer::new(w, h);
        for (i, px) in buf.make_mut_slice().iter_mut().enumerate() {
            let (x, y) = (i as u32 % w, i as u32 / w);
            *px = Rgba8Pixel {
                r: x as u8,
                g: y as u8,
                b: 7,
                a: 255,
            };
        }
        buf
    }

    #[test]
    fn crop_rect_scales_to_physical_pixels_and_clamps() {
        // Non-HiDPI: the logical rectangle is the physical one.
        assert_eq!(
            crop_rect((10.0, 20.0, 30.0, 40.0), 1.0, 200, 200),
            Some(Crop {
                x: 10,
                y: 20,
                w: 30,
                h: 40
            })
        );
        // HiDPI: every edge scales, so the crop keeps covering the same chart.
        assert_eq!(
            crop_rect((10.0, 20.0, 30.0, 40.0), 2.0, 400, 400),
            Some(Crop {
                x: 20,
                y: 40,
                w: 60,
                h: 80
            })
        );
        // A rectangle running past the snapshot is clamped to what exists.
        assert_eq!(
            crop_rect((90.0, 90.0, 50.0, 50.0), 1.0, 100, 100),
            Some(Crop {
                x: 90,
                y: 90,
                w: 10,
                h: 10
            })
        );
        // Nothing to crop: zero-sized, or entirely off the snapshot.
        assert_eq!(crop_rect((10.0, 10.0, 0.0, 40.0), 1.0, 100, 100), None);
        assert_eq!(crop_rect((150.0, 10.0, 40.0, 40.0), 1.0, 100, 100), None);
    }

    #[test]
    fn crop_to_rgb8_copies_just_the_cropped_region() {
        let snap = snapshot(8, 6);
        let crop = Crop {
            x: 2,
            y: 1,
            w: 3,
            h: 2,
        };
        let rgb = crop_to_rgb8(&snap, &crop);

        // Tightly packed RGB8: three bytes per pixel, alpha dropped.
        assert_eq!(rgb.len(), 3 * 3 * 2);
        // Each pixel carries its source coordinates, so the first and last
        // pixels pin the region down to the corner.
        assert_eq!(&rgb[..3], &[2, 1, 7]);
        assert_eq!(&rgb[rgb.len() - 3..], &[4, 2, 7]);

        // A full-frame crop reproduces the whole snapshot, row by row.
        let all = crop_to_rgb8(
            &snap,
            &Crop {
                x: 0,
                y: 0,
                w: 8,
                h: 6,
            },
        );
        assert_eq!(all.len(), 8 * 6 * 3);
        assert_eq!(&all[..3], &[0, 0, 7]);
        assert_eq!(&all[8 * 3..8 * 3 + 3], &[0, 1, 7], "row 1 starts at x=0");
    }

    #[test]
    fn export_file_name_slugifies_the_metric() {
        assert_eq!(
            export_file_name(Metric::Mean, "png"),
            "analytics-mean-adu.png"
        );
        assert_eq!(
            export_file_name(Metric::MaxPixelCount, "csv"),
            "analytics-max-adu-count.csv"
        );
        // The slugifier replaces every non-ASCII-alphanumeric char with '-', so
        // a "Noise σ" label would slug to a bare "analytics-noise-.png". The
        // label spells sigma out instead — the fix belongs in the label, not in
        // a special case here.
        assert_eq!(
            export_file_name(Metric::Sigma, "png"),
            "analytics-noise-sigma.png"
        );
    }
}
