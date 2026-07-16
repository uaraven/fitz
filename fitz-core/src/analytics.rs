//! Time-series analytics over a set of FITS frames: compute per-frame pixel
//! metrics (min/max/median/mean ADU and the min/max pixel counts), key each
//! frame by its `DATE-OBS` acquisition time, and assemble a time-ordered
//! series for one chosen metric. Pure and `Send` — no GUI types, no terminal
//! I/O — so both the FitSmith analytics dialog and a future CLI subcommand can
//! drive it.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fitskit::FitsFile;

use crate::fits_image::find_image_hdu;
use crate::info::{PixelStats, parse_date_obs, pixel_stats};

/// A per-frame statistic that can be plotted over a session.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    Min,
    Max,
    Median,
    Mean,
    MaxPixelCount,
    MinPixelCount,
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
        }
    }

    /// Every metric, in the order a selection dropdown should list them.
    pub fn all() -> &'static [Metric] {
        &[
            Metric::Min,
            Metric::Max,
            Metric::Median,
            Metric::Mean,
            Metric::MaxPixelCount,
            Metric::MinPixelCount,
        ]
    }

    /// Extract this metric's value from a frame's pixel statistics.
    pub fn value(self, stats: &PixelStats) -> f64 {
        match self {
            Metric::Min => stats.min,
            Metric::Max => stats.max,
            Metric::Median => stats.median,
            Metric::Mean => stats.mean,
            Metric::MaxPixelCount => stats.max_count as f64,
            Metric::MinPixelCount => stats.min_count as f64,
        }
    }
}

/// Why a frame was left out of the series (reported, not an error: the batch
/// carries on and the dialog shows per-reason skip counts).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SkipReason {
    /// No `DATE-OBS` header, or one that doesn't parse — the frame has no
    /// place on the time axis.
    NoDateObs,
    /// An already-debayered RGB cube — ADU statistics are only meaningful on
    /// raw, non-debayered frames (mirrors `header_info_with_pixels`, which
    /// also declines to compute `PixelStats` for an RGB cube).
    NotMono,
}

/// The outcome of analyzing one file: either its metrics or the reason it was
/// skipped. Read failures are genuine `Err`s, not skips.
pub enum FileAnalysis {
    Analyzed(FileMetrics),
    Skipped(SkipReason),
}

/// All phase-1 metrics for one frame, computed in a single pixel read. Collect
/// these once per batch; switching the plotted metric is then just a
/// [`build_series`] call with no file re-read.
pub struct FileMetrics {
    pub path: PathBuf,
    /// `DATE-OBS` as seconds since the Unix epoch (see [`parse_date_obs`]).
    pub time: f64,
    /// The raw `DATE-OBS` string, for tooltips and CSV output.
    pub time_str: String,
    pub stats: PixelStats,
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
}

/// Analyze one FITS file: read its image (transparently decompressing `.fz`
/// inputs), key it by `DATE-OBS`, and compute its pixel statistics. Returns
/// [`FileAnalysis::Skipped`] for frames that can't participate in the series;
/// only actual read/decode failures are `Err`.
pub fn analyze_file(path: &Path) -> Result<FileAnalysis> {
    let fits =
        FitsFile::from_file(path).with_context(|| format!("cannot read {}", path.display()))?;
    let (header, img) = find_image_hdu(&fits, path)?;
    let img = img.as_ref();

    let time_str = header.get_string("DATE-OBS").map(str::trim);
    let Some((time_str, time)) = time_str.and_then(|s| Some((s, parse_date_obs(s)?))) else {
        return Ok(FileAnalysis::Skipped(SkipReason::NoDateObs));
    };
    if crate::fits_image::is_debayered_rgb_cube(header, img) {
        return Ok(FileAnalysis::Skipped(SkipReason::NotMono));
    }

    Ok(FileAnalysis::Analyzed(FileMetrics {
        path: path.to_path_buf(),
        time,
        time_str: time_str.to_string(),
        stats: pixel_stats(header, img),
    }))
}

/// Assemble the time-ordered series for one metric from already-computed
/// per-file metrics. Pure extraction + sort — no file I/O, so switching the
/// plotted metric is instant.
pub fn build_series(files: &[FileMetrics], metric: Metric) -> Series {
    let mut points: Vec<SamplePoint> = files
        .iter()
        .map(|f| SamplePoint {
            time: f.time,
            time_str: f.time_str.clone(),
            value: metric.value(&f.stats),
            path: f.path.clone(),
        })
        .collect();
    points.sort_by(|a, b| a.time.total_cmp(&b.time));
    Series { metric, points }
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
    use crate::test_support::{test_data, write_mosaic_fits, write_mosaic_fits_with_metadata};
    use fitskit::{HeaderValue, ImageData, PixelData};
    use tempfile::TempDir;

    fn metrics(path: &Path) -> FileMetrics {
        match analyze_file(path).unwrap() {
            FileAnalysis::Analyzed(m) => m,
            FileAnalysis::Skipped(reason) => panic!("unexpectedly skipped: {reason:?}"),
        }
    }

    fn skip_reason(path: &Path) -> SkipReason {
        match analyze_file(path).unwrap() {
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
    fn analyze_file_skips_rgb_cube_as_not_mono() {
        // A debayered RGB cube with a valid DATE-OBS: skipped for its shape,
        // not for its timestamp.
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("rgb.fits");
        let img = ImageData::new(vec![2, 2, 3], PixelData::I16(vec![0; 12]));
        let mut fits = FitsFile::with_primary_image(img);
        fits.primary_mut().header.set(
            "DATE-OBS",
            HeaderValue::String("2026-06-22T01:00:00".to_string()),
            None,
        );
        fits.to_file(&input).unwrap();
        assert_eq!(skip_reason(&input), SkipReason::NotMono);
    }

    #[test]
    fn analyze_file_reads_real_data_including_compressed() {
        // uncompressed.fit carries DATE-OBS 2026-05-31T04:57:09.004664 and the
        // .fz frame (a different exposure) 2026-05-28T02:42:57.2632740; the
        // compressed variant must decompress transparently and still analyze.
        let raw = metrics(&test_data("uncompressed.fit"));
        let fz = metrics(&test_data("compressed.fits.fz"));
        assert_eq!(raw.time, 1780203429.004664);
        assert_eq!(
            fz.time,
            parse_date_obs("2026-05-28T02:42:57.2632740").unwrap()
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

        assert_eq!(Metric::Min.value(&m.stats), 0.0);
        assert_eq!(Metric::Max.value(&m.stats), 15.0);
        assert_eq!(Metric::Median.value(&m.stats), 7.5);
        assert_eq!(Metric::Mean.value(&m.stats), 7.5);
        assert_eq!(Metric::MinPixelCount.value(&m.stats), 1.0);
        assert_eq!(Metric::MaxPixelCount.value(&m.stats), 1.0);
        assert_eq!(Metric::all().len(), 6);
    }

    #[test]
    fn write_csv_emits_header_and_time_ordered_rows() {
        let series = Series {
            metric: Metric::Mean,
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
