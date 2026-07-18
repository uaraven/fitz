//! Time-series analytics over a set of FITS frames: compute per-frame metrics,
//! key each frame by its acquisition time (`DATE-LOC`, else `DATE-OBS`), and
//! assemble a
//! time-ordered series for one chosen metric. Pure and `Send` — no GUI types, no
//! terminal I/O — so both the FitSmith dialogs and a future CLI subcommand can
//! drive it.
//!
//! Metrics come in two families (see [`MetricFamily`]): the pixel statistics
//! every batch reads, and the star metrics only a batch that opted into
//! [`AnalyzeOptions::detect_stars`] pays for.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fitskit::FitsFile;

use crate::fits_image::{detection_plane, find_image_hdu};
use crate::info::{PixelStats, parse_date_obs, pixel_stats};
use crate::stars::{StarDetectOptions, StarStats, detect_stars, plane_background};

/// Which dialog lists a metric — and, therefore, whether a batch has to detect
/// stars to answer it. The two families are disjoint: no star metric appears in
/// the Analytics dropdown, and no pixel metric in the Star metrics one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MetricFamily {
    /// Read off a frame's [`PixelStats`] — every batch computes these.
    Pixel,
    /// Measured from the frame's detected stars, which only a batch that opted
    /// into [`AnalyzeOptions::detect_stars`] has.
    Star,
}

/// A per-frame statistic that can be plotted over a session.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    Min,
    Max,
    Median,
    Mean,
    MaxPixelCount,
    MinPixelCount,
    Sigma,
    Mad,
    Mode,
    Saturated,
    StarCount,
    Hfr,
    Fwhm,
    Eccentricity,
}

impl Metric {
    /// Human-readable name, used for dropdown entries and axis labels.
    pub fn label(self) -> &'static str {
        match self {
            Metric::Min => "Min ADU",
            Metric::Max => "Max ADU",
            Metric::Median => "Median ADU",
            Metric::Mean => "Mean ADU",
            Metric::MaxPixelCount => "Max-ADU count",
            Metric::MinPixelCount => "Min-ADU count",
            Metric::Sigma => "Noise sigma",
            Metric::Mad => "Noise MAD",
            Metric::Mode => "Sky background",
            Metric::Saturated => "Saturated pixels",
            Metric::StarCount => "Star count",
            Metric::Hfr => "HFR",
            Metric::Fwhm => "FWHM",
            Metric::Eccentricity => "Eccentricity",
        }
    }

    /// Every metric, in the order a selection dropdown should list them. New
    /// metrics are appended, never inserted: a stored dropdown index has to keep
    /// meaning the same thing across versions. The rule now applies per family
    /// list — see [`Metric::of_family`], which is what a dropdown is actually
    /// built from.
    pub fn all() -> &'static [Metric] {
        &[
            Metric::Min,
            Metric::Max,
            Metric::Median,
            Metric::Mean,
            Metric::MaxPixelCount,
            Metric::MinPixelCount,
            Metric::Sigma,
            Metric::Mad,
            Metric::Mode,
            Metric::Saturated,
            Metric::StarCount,
            Metric::Hfr,
            Metric::Fwhm,
            Metric::Eccentricity,
        ]
    }

    /// Which dialog lists this metric.
    pub fn family(self) -> MetricFamily {
        match self {
            Metric::StarCount | Metric::Hfr | Metric::Fwhm | Metric::Eccentricity => {
                MetricFamily::Star
            }
            _ => MetricFamily::Pixel,
        }
    }

    /// The metrics of one family, in dropdown order — the source one dropdown is
    /// built from, so a position in this list is that dialog's stored index.
    pub fn of_family(family: MetricFamily) -> &'static [Metric] {
        match family {
            MetricFamily::Pixel => &Metric::all()[..10],
            MetricFamily::Star => &Metric::all()[10..],
        }
    }

    /// Extract this metric's value from a frame's metrics, or `None` when the
    /// frame has no value for it: every star metric is `None` for a batch that
    /// did not detect stars, and HFR/FWHM/eccentricity are also `None` for a
    /// frame where detection found none.
    pub fn value(self, m: &FileMetrics) -> Option<f64> {
        let stats = &m.stats;
        Some(match self {
            Metric::Min => stats.min,
            Metric::Max => stats.max,
            Metric::Median => stats.median,
            Metric::Mean => stats.mean,
            Metric::MaxPixelCount => stats.max_count as f64,
            Metric::MinPixelCount => stats.min_count as f64,
            Metric::Sigma => stats.sigma,
            Metric::Mad => stats.mad,
            Metric::Mode => stats.mode,
            Metric::Saturated => stats.saturated as f64,
            // A frame with no stars still has a star *count* — it is zero, and
            // that is a real, plottable measurement (a cloud indicator). The
            // shape metrics have nothing to report.
            Metric::StarCount => m.stars.as_ref()?.count as f64,
            Metric::Hfr => m.stars.as_ref()?.hfr?,
            Metric::Fwhm => m.stars.as_ref()?.fwhm?,
            Metric::Eccentricity => m.stars.as_ref()?.eccentricity?,
        })
    }
}

/// What an [`analyze_file`] batch computes beyond the pixel statistics every
/// batch reads.
#[derive(Clone, Copy, Default, Debug)]
pub struct AnalyzeOptions {
    /// Detect stars and measure their shapes — the [`MetricFamily::Star`]
    /// metrics. Off by default: it costs a star-detection pass per frame, and
    /// the Analytics dialog never asks for it.
    pub detect_stars: bool,
}

/// Why a frame was left out of the series (reported, not an error: the batch
/// carries on and the dialog shows per-reason skip counts).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SkipReason {
    /// No usable acquisition time — neither `DATE-LOC` nor `DATE-OBS` is
    /// present and parseable, so the frame has no place on the time axis.
    NoDateObs,
}

/// The outcome of analyzing one file: either its metrics or the reason it was
/// skipped. Read failures are genuine `Err`s, not skips.
// The variants are lopsided (a `FileMetrics` against a one-byte reason), but
// boxing to even them out would buy an allocation and an indirection on every
// access to save moving 240 bytes once per file — a file whose statistics
// already own a 2 KB histogram on the heap.
// `Clone` so a caller can keep an outcome in a cache and still hand a copy to
// whatever plots it (FitSmith's analytics cache does exactly this).
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum FileAnalysis {
    Analyzed(FileMetrics),
    Skipped(SkipReason),
}

/// Every metric for one frame, computed in a single file read. Collect these
/// once per batch; switching the plotted metric is then just a [`build_series`]
/// call with no file re-read.
#[derive(Clone)]
pub struct FileMetrics {
    pub path: PathBuf,
    /// Acquisition time as seconds since the Unix epoch (see [`parse_date_obs`]),
    /// from `DATE-LOC` if present, else `DATE-OBS`.
    pub time: f64,
    /// The raw acquisition-time string (`DATE-LOC` or `DATE-OBS`), for tooltips
    /// and CSV output.
    pub time_str: String,
    pub stats: PixelStats,
    /// `None` when the batch did not ask for star detection — distinct from
    /// `Some(StarStats { count: 0, .. })`, which means it asked and the frame
    /// had none. The dialog's "no stars detected" note depends on telling those
    /// apart.
    pub stars: Option<StarStats>,
}

/// One plotted sample: a frame's acquisition time and its metric value.
pub struct SamplePoint {
    pub time: f64,
    pub time_str: String,
    pub value: f64,
    pub path: PathBuf,
}

/// A time-ordered series of one metric across a batch of frames.
pub struct Series {
    pub metric: Metric,
    pub points: Vec<SamplePoint>,
    /// Frames that analyzed fine but have no value for *this* metric — a
    /// starless frame's HFR, say. Distinct from a [`SkipReason`], which is about
    /// the frame rather than the metric, and worth reporting: a run of frames
    /// with no stars is a cloud indicator in its own right.
    pub unavailable: usize,
}

/// Analyze one FITS file: read its image (transparently decompressing `.fz`
/// inputs), key it by `DATE-LOC` (falling back to `DATE-OBS`), compute its pixel
/// statistics, and — when
/// `opts` asks — detect its stars. Returns [`FileAnalysis::Skipped`] for frames
/// that can't participate in the series; only actual read/decode failures are
/// `Err`.
pub fn analyze_file(path: &Path, opts: &AnalyzeOptions) -> Result<FileAnalysis> {
    let fits =
        FitsFile::from_file(path).with_context(|| format!("cannot read {}", path.display()))?;
    let (header, img) = find_image_hdu(&fits, path)?;
    let img = img.as_ref();

    // Prefer DATE-LOC (the observer's local wall clock) so the chart reads in
    // the time of night the session was actually shot; fall back to DATE-OBS
    // (UTC) when DATE-LOC is absent or unparseable. Both share the same
    // YYYY-MM-DDTHH:MM:SS[.sss] format, and the chart renders the epoch as-is
    // (no zone conversion), so a local string plots as local time.
    let Some((time_str, time)) = ["DATE-LOC", "DATE-OBS"].into_iter().find_map(|key| {
        let s = header.get_string(key).map(str::trim).unwrap_or("");
        parse_date_obs(s).map(|t| (s.to_string(), t))
    }) else {
        return Ok(FileAnalysis::Skipped(SkipReason::NoDateObs));
    };

    // Every image shape `detection_plane` and `pixel_stats` accept is analyzed:
    // a mono frame and a CFA mosaic on their own values, an already-debayered
    // RGB cube on its green channel (the plane with the most signal — see
    // `detection_plane`). Only a shape neither can reduce (e.g. a >3-plane cube)
    // surfaces as a read `Err`.
    let stars = if opts.detect_stars {
        let plane = detection_plane(header, img)?;
        let bg = plane_background(&plane);
        Some(detect_stars(&plane, &bg, &StarDetectOptions::default()))
    } else {
        None
    };

    Ok(FileAnalysis::Analyzed(FileMetrics {
        path: path.to_path_buf(),
        time,
        time_str: time_str.to_string(),
        stats: pixel_stats(header, img),
        stars,
    }))
}

/// Assemble the time-ordered series for one metric from already-computed
/// per-file metrics. Pure extraction + sort — no file I/O, so switching the
/// plotted metric is instant. Frames with no value for this metric are dropped
/// from the plot and counted in [`Series::unavailable`].
pub fn build_series(files: &[FileMetrics], metric: Metric) -> Series {
    let mut points: Vec<SamplePoint> = files
        .iter()
        .filter_map(|f| {
            Some(SamplePoint {
                time: f.time,
                time_str: f.time_str.clone(),
                value: metric.value(f)?,
                path: f.path.clone(),
            })
        })
        .collect();
    points.sort_by(|a, b| a.time.total_cmp(&b.time));
    Series {
        metric,
        unavailable: files.len() - points.len(),
        points,
    }
}

/// Write a series as CSV: a `time_iso,epoch_seconds,value` header line
/// followed by one row per point, in plot (time) order.
pub fn write_csv(series: &Series, mut w: impl Write) -> io::Result<()> {
    writeln!(w, "time_iso,epoch_seconds,value")?;
    for p in &series.points {
        writeln!(w, "{},{},{}", p.time_str, p.time, p.value)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        test_data, write_mosaic_fits, write_mosaic_fits_with_metadata, write_star_field_fits,
    };
    use fitskit::{HeaderValue, ImageData, PixelData};
    use tempfile::TempDir;

    /// Analyze a frame the way the Analytics dialog does: pixels only.
    fn metrics(path: &Path) -> FileMetrics {
        metrics_with(path, &AnalyzeOptions::default())
    }

    fn metrics_with(path: &Path, opts: &AnalyzeOptions) -> FileMetrics {
        match analyze_file(path, opts).unwrap() {
            FileAnalysis::Analyzed(m) => m,
            FileAnalysis::Skipped(reason) => panic!("unexpectedly skipped: {reason:?}"),
        }
    }

    fn skip_reason(path: &Path) -> SkipReason {
        match analyze_file(path, &AnalyzeOptions::default()).unwrap() {
            FileAnalysis::Skipped(reason) => reason,
            FileAnalysis::Analyzed(_) => panic!("expected a skip"),
        }
    }

    #[test]
    fn analyze_file_computes_time_and_metrics_for_mono_frame() {
        // write_mosaic_fits_with_metadata stamps DATE-OBS 2026-06-22T00:00:00
        // and stores sequential values 0..(w*h).
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("frame.fits");
        write_mosaic_fits_with_metadata(&input, 4, 4, Some("RGGB"));

        let m = metrics(&input);
        assert_eq!(m.time_str, "2026-06-22T00:00:00");
        assert_eq!(m.time, parse_date_obs("2026-06-22T00:00:00").unwrap());
        assert_eq!(m.path, input);
        assert_eq!(m.stats.min, 0.0);
        assert_eq!(m.stats.max, 15.0);
        assert_eq!(m.stats.mean, 7.5);
        assert_eq!(m.stats.min_count, 1);
        assert_eq!(m.stats.max_count, 1);
    }

    #[test]
    fn analyze_file_skips_frame_without_date_obs() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("no_date.fits");
        write_mosaic_fits(&input, 4, 4, Some("RGGB"));
        assert_eq!(skip_reason(&input), SkipReason::NoDateObs);
    }

    #[test]
    fn analyze_file_prefers_date_loc_over_date_obs() {
        // A frame carrying both keys: DATE-LOC (local wall clock) wins, so the
        // series reads in the observer's time rather than the UTC DATE-OBS.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("both.fits");
        let img = ImageData::new(vec![2, 2], PixelData::I16(vec![0; 4]));
        let mut fits = FitsFile::with_primary_image(img);
        let header = &mut fits.primary_mut().header;
        header.set(
            "DATE-OBS",
            HeaderValue::String("2026-06-22T00:00:00".to_string()),
            None,
        );
        header.set(
            "DATE-LOC",
            HeaderValue::String("2026-06-21T20:00:00".to_string()),
            None,
        );
        fits.to_file(&input).unwrap();

        let m = metrics(&input);
        assert_eq!(m.time_str, "2026-06-21T20:00:00");
        assert_eq!(m.time, parse_date_obs("2026-06-21T20:00:00").unwrap());
    }

    #[test]
    fn analyze_file_falls_back_to_date_obs_when_date_loc_blank() {
        // A present-but-blank DATE-LOC must not win; fall back to DATE-OBS.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("blank_loc.fits");
        let img = ImageData::new(vec![2, 2], PixelData::I16(vec![0; 4]));
        let mut fits = FitsFile::with_primary_image(img);
        let header = &mut fits.primary_mut().header;
        header.set("DATE-LOC", HeaderValue::String("   ".to_string()), None);
        header.set(
            "DATE-OBS",
            HeaderValue::String("2026-06-22T00:00:00".to_string()),
            None,
        );
        fits.to_file(&input).unwrap();

        let m = metrics(&input);
        assert_eq!(m.time_str, "2026-06-22T00:00:00");
    }

    #[test]
    fn analyze_file_measures_an_rgb_cube_on_its_green_channel() {
        // A debayered RGB cube is no longer skipped: its statistics are measured
        // on the green channel. The fixture's planes are sequential, so for a
        // 2x2 frame (n=4) the green plane holds 4..=7 — min=4, max=7, over just
        // the one plane's four samples, not all twelve.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        let pixels: Vec<i16> = (0..12).collect();
        let img = ImageData::new(vec![2, 2, 3], PixelData::I16(pixels));
        let mut fits = FitsFile::with_primary_image(img);
        fits.primary_mut().header.set(
            "DATE-OBS",
            HeaderValue::String("2026-06-22T01:00:00".to_string()),
            None,
        );
        fits.to_file(&input).unwrap();

        let m = metrics(&input);
        assert_eq!((m.stats.min, m.stats.max), (4.0, 7.0));
        assert_eq!(m.stats.count, 4);
    }

    #[test]
    fn analyze_file_detects_stars_on_an_rgb_cube() {
        // The star family also handles a cube now: detection runs on the green
        // channel at full resolution rather than erroring.
        let m = metrics_with(
            &test_data("uncompressed_debayer.fits"),
            &AnalyzeOptions { detect_stars: true },
        );
        assert!(m.stars.as_ref().expect("stars measured").count > 0);
    }

    #[test]
    fn analyze_file_reads_real_data_including_compressed() {
        // Both frames carry DATE-LOC, which wins over DATE-OBS: uncompressed.fit
        // has DATE-LOC 2026-05-31T00:57:09.0046645 (DATE-OBS is the UTC
        // 04:57:09) and the .fz frame (a different exposure) has DATE-LOC
        // 2026-05-27T22:42:57.2632740. The compressed variant must decompress
        // transparently — DATE-LOC lives on the tile-compressed HDU's header —
        // and still analyze.
        let raw = metrics(&test_data("uncompressed.fit"));
        let fz = metrics(&test_data("compressed.fits.fz"));
        assert_eq!(
            raw.time,
            parse_date_obs("2026-05-31T00:57:09.0046645").unwrap()
        );
        assert_eq!(
            fz.time,
            parse_date_obs("2026-05-27T22:42:57.2632740").unwrap()
        );
        assert!(fz.stats.max >= fz.stats.median && fz.stats.median >= fz.stats.min);
    }

    #[test]
    fn build_series_sorts_points_by_time() {
        let tmp = TempDir::new().unwrap();
        // Same pixel content, three distinct timestamps written out of order.
        let times = [
            "2026-06-22T03:00:00",
            "2026-06-22T01:00:00",
            "2026-06-22T02:00:00",
        ];
        let files: Vec<FileMetrics> = times
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let path = tmp.path().join(format!("f{i}.fits"));
                let img = ImageData::new(vec![2, 2], PixelData::I16(vec![i as i16; 4]));
                let mut fits = FitsFile::with_primary_image(img);
                fits.primary_mut()
                    .header
                    .set("DATE-OBS", HeaderValue::String(t.to_string()), None);
                fits.to_file(&path).unwrap();
                metrics(&path)
            })
            .collect();

        let series = build_series(&files, Metric::Mean);
        assert_eq!(series.metric, Metric::Mean);
        assert_eq!(series.points.len(), 3);
        let labels: Vec<&str> = series.points.iter().map(|p| p.time_str.as_str()).collect();
        assert_eq!(
            labels,
            [
                "2026-06-22T01:00:00",
                "2026-06-22T02:00:00",
                "2026-06-22T03:00:00"
            ]
        );
        // Values follow their frames through the sort (frames 1, 2, 0 are
        // constant images of 1.0, 2.0, 0.0).
        let values: Vec<f64> = series.points.iter().map(|p| p.value).collect();
        assert_eq!(values, [1.0, 2.0, 0.0]);
        assert!(series.points.windows(2).all(|w| w[0].time < w[1].time));
    }

    #[test]
    fn metric_values_read_the_matching_stat() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("frame.fits");
        write_mosaic_fits_with_metadata(&input, 4, 4, None);
        let m = metrics(&input);

        assert_eq!(Metric::Min.value(&m), Some(0.0));
        assert_eq!(Metric::Max.value(&m), Some(15.0));
        assert_eq!(Metric::Median.value(&m), Some(7.5));
        assert_eq!(Metric::Mean.value(&m), Some(7.5));
        assert_eq!(Metric::MinPixelCount.value(&m), Some(1.0));
        assert_eq!(Metric::MaxPixelCount.value(&m), Some(1.0));

        // The 4x4 fixture stores sequential values 0..15, one pixel each: every
        // value ties for most common, so the mode is the lowest of them, and
        // nothing reaches the I16 ceiling (a physical 32767 with no BZERO).
        assert_eq!(Metric::Mode.value(&m), Some(0.0));
        assert_eq!(Metric::Saturated.value(&m), Some(0.0));
        assert_eq!(Metric::Sigma.value(&m), Some(m.stats.sigma));
        assert_eq!(Metric::Mad.value(&m), Some(m.stats.mad));
        // A uniform 0..15 spread is nothing like Gaussian, so the scaled MAD
        // (1.4826 * 4 = 5.9304) overshoots the true sigma here — the metrics
        // read their stat, they don't reinterpret it.
        assert_eq!(Metric::Mad.value(&m), Some(1.4826 * 4.0));

        // This batch didn't ask for stars, so no star metric has an answer —
        // including the count, which is not zero but unknown.
        for &metric in Metric::of_family(MetricFamily::Star) {
            assert_eq!(metric.value(&m), None, "{metric:?}");
        }

        assert_eq!(Metric::all().len(), 14);
    }

    #[test]
    fn star_metrics_read_the_detected_stars() {
        // A real mosaic, analyzed the way the Star metrics dialog does.
        let m = metrics_with(
            &test_data("uncompressed.fit"),
            &AnalyzeOptions { detect_stars: true },
        );
        let stars = m.stars.as_ref().unwrap();

        assert_eq!(Metric::StarCount.value(&m), Some(stars.count as f64));
        assert_eq!(Metric::Hfr.value(&m), stars.hfr);
        assert_eq!(Metric::Fwhm.value(&m), stars.fwhm);
        assert_eq!(Metric::Eccentricity.value(&m), stars.eccentricity);

        // Detecting stars doesn't cost the pixel metrics their answers.
        for &metric in Metric::of_family(MetricFamily::Pixel) {
            assert!(metric.value(&m).is_some(), "{metric:?}");
        }
    }

    #[test]
    fn of_family_partitions_every_metric_exactly_once() {
        // The two dropdowns between them must list every metric, once: a metric
        // in neither list is unreachable, and one in both would be plotted by a
        // dialog whose batch may not have measured it.
        let listed: Vec<Metric> = [MetricFamily::Pixel, MetricFamily::Star]
            .iter()
            .flat_map(|&f| Metric::of_family(f).iter().copied())
            .collect();
        assert_eq!(listed, Metric::all());

        for &m in Metric::of_family(MetricFamily::Pixel) {
            assert_eq!(m.family(), MetricFamily::Pixel, "{m:?}");
        }
        for &m in Metric::of_family(MetricFamily::Star) {
            assert_eq!(m.family(), MetricFamily::Star, "{m:?}");
        }
    }

    #[test]
    fn build_series_drops_frames_with_no_value_for_the_metric() {
        let tmp = TempDir::new().unwrap();
        // Two frames the batch measured stars on — one with a star, one bare —
        // and one it never asked about.
        let starry = tmp.path().join("starry.fits");
        write_star_field_fits(&starry, 60, 60, 1000.0, &[(30.0, 30.0, 2.0, 2.0, 5000.0)]);
        let empty = tmp.path().join("empty.fits");
        write_star_field_fits(&empty, 60, 60, 1000.0, &[]);
        let unasked = tmp.path().join("unasked.fits");
        write_star_field_fits(&unasked, 60, 60, 1000.0, &[(30.0, 30.0, 2.0, 2.0, 5000.0)]);

        let stamp = |path: &Path, opts: &AnalyzeOptions, time: f64| {
            let mut m = metrics_with(path, opts);
            m.time = time;
            m
        };
        let detect = AnalyzeOptions { detect_stars: true };
        let files = vec![
            stamp(&starry, &detect, 1.0),
            stamp(&empty, &detect, 2.0),
            stamp(&unasked, &AnalyzeOptions::default(), 3.0),
        ];

        // HFR: only the frame with a star has one. The starless frame and the
        // one nobody asked about are counted, not plotted.
        let hfr = build_series(&files, Metric::Hfr);
        assert_eq!(hfr.points.len(), 1);
        assert_eq!(hfr.unavailable, 2);
        assert_eq!(hfr.points[0].time, 1.0);

        // Star count: a starless frame counts zero, which is a real measurement.
        let count = build_series(&files, Metric::StarCount);
        assert_eq!(count.points.len(), 2);
        assert_eq!(count.unavailable, 1);
        assert_eq!(count.points[1].value, 0.0);

        // A pixel metric is available for every frame regardless.
        let mean = build_series(&files, Metric::Mean);
        assert_eq!(mean.points.len(), 3);
        assert_eq!(mean.unavailable, 0);
    }

    #[test]
    fn write_csv_emits_header_and_time_ordered_rows() {
        let series = Series {
            metric: Metric::Mean,
            unavailable: 0,
            points: vec![
                SamplePoint {
                    time: 100.0,
                    time_str: "1970-01-01T00:01:40".to_string(),
                    value: 7.5,
                    path: PathBuf::from("a.fits"),
                },
                SamplePoint {
                    time: 160.25,
                    time_str: "1970-01-01T00:02:40.25".to_string(),
                    value: 8.0,
                    path: PathBuf::from("b.fits"),
                },
            ],
        };

        let mut out = Vec::new();
        write_csv(&series, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "time_iso,epoch_seconds,value\n\
             1970-01-01T00:01:40,100,7.5\n\
             1970-01-01T00:02:40.25,160.25,8\n"
        );
    }
}
