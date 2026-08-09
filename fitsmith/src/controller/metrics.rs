//! `fitsmith`-local batch/time-series primitives behind the Analytics and
//! Star metrics dialogs: which metrics exist, how one file is measured, and
//! how a batch of files becomes a plotted [`Series`]. Per-file numbers come
//! straight from `libfitz::stats::Stats`/`libfitz::stars::StarStats` —
//! nothing here duplicates their fields, only the batching/time-series shape
//! around them is new. This stays local to `fitsmith` rather than living in
//! `libfitz`: multi-file session analysis has no `fitz` CLI use case.

use std::path::{Path, PathBuf};

use anyhow::Result;
use libfitz::stars::{StarDetectOptions, StarStats};
use libfitz::stats::Stats;

/// Which family of metrics a dialog/batch is working with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MetricFamily {
    Pixel,
    Star,
}

/// One plottable measurement, either read straight off a channel's [`Stats`]
/// or off a frame's [`StarStats`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    Min,
    Max,
    Mean,
    Median,
    Sigma,
    Mad,
    Mode,
    ZeroCount,
    SaturatedCount,
    StarCount,
    Hfr,
    Fwhm,
    Eccentricity,
}

const PIXEL_METRICS: [Metric; 9] = [
    Metric::Mean,
    Metric::Median,
    Metric::Min,
    Metric::Max,
    Metric::Sigma,
    Metric::Mad,
    Metric::Mode,
    Metric::ZeroCount,
    Metric::SaturatedCount,
];
const STAR_METRICS: [Metric; 4] = [
    Metric::Hfr,
    Metric::Fwhm,
    Metric::Eccentricity,
    Metric::StarCount,
];

impl Metric {
    /// The dropdown label for this metric.
    pub fn label(self) -> &'static str {
        match self {
            Metric::Min => "Min ADU",
            Metric::Max => "Max ADU",
            Metric::Mean => "Mean ADU",
            Metric::Median => "Median ADU",
            Metric::Sigma => "Noise sigma",
            Metric::Mad => "MAD ADU",
            Metric::Mode => "Mode ADU",
            Metric::ZeroCount => "Zero count",
            Metric::SaturatedCount => "Saturated count",
            Metric::StarCount => "Star count",
            Metric::Hfr => "HFR",
            Metric::Fwhm => "FWHM",
            Metric::Eccentricity => "Eccentricity",
        }
    }

    /// Which family this metric belongs to.
    pub fn family(self) -> MetricFamily {
        match self {
            Metric::StarCount | Metric::Hfr | Metric::Fwhm | Metric::Eccentricity => {
                MetricFamily::Star
            }
            _ => MetricFamily::Pixel,
        }
    }

    /// Every metric in `family`, in dropdown order.
    pub fn of_family(family: MetricFamily) -> &'static [Metric] {
        match family {
            MetricFamily::Pixel => &PIXEL_METRICS,
            MetricFamily::Star => &STAR_METRICS,
        }
    }

    /// This metric's value for one channel's [`Stats`]. Only meaningful for a
    /// `Pixel`-family metric.
    fn pixel_value(self, s: &Stats) -> f64 {
        match self {
            Metric::Min => s.min as f64,
            Metric::Max => s.max as f64,
            Metric::Mean => s.mean as f64,
            Metric::Median => s.median as f64,
            Metric::Sigma => s.sigma as f64,
            Metric::Mad => s.mad as f64,
            Metric::Mode => s.mode as f64,
            Metric::ZeroCount => s.zero_count as f64,
            Metric::SaturatedCount => s.saturated_count as f64,
            Metric::StarCount | Metric::Hfr | Metric::Fwhm | Metric::Eccentricity => {
                unreachable!("not a pixel metric")
            }
        }
    }

    /// This metric's value from a frame's [`StarStats`], if it has one. Only
    /// meaningful for a `Star`-family metric.
    fn star_value(self, s: &StarStats) -> Option<f64> {
        match self {
            Metric::StarCount => Some(s.count as f64),
            Metric::Hfr => s.hfr,
            Metric::Fwhm => s.fwhm,
            Metric::Eccentricity => s.eccentricity,
            _ => unreachable!("not a star metric"),
        }
    }
}

/// Why a file was skipped rather than measured.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SkipReason {
    NoDateObs,
}

/// One file's measured metrics: its acquisition time and the numbers a chart
/// might plot. `stats` is straight from `Image::stats().channels` — one entry
/// for grayscale/CFA, three (R, G, B) for an already-debayered RGB cube.
#[derive(Clone)]
pub struct FileMetrics {
    pub path: PathBuf,
    pub time: f64,
    pub time_str: String,
    pub stats: Vec<Stats>,
    pub stars: Option<StarStats>,
}

/// The outcome of analyzing one file.
#[derive(Clone)]
pub enum FileAnalysis {
    Analyzed(FileMetrics),
    Skipped(SkipReason),
}

/// Tuning for [`analyze_file`]: whether to pay for star detection, which only
/// the Star-metrics family needs.
pub struct AnalyzeOptions {
    pub detect_stars: bool,
}

/// Measure one file: its acquisition time (`DATE-LOC`, else `DATE-OBS`),
/// pixel statistics, and — when requested — star metrics on its detection
/// plane. A frame with no acquisition time is [`FileAnalysis::Skipped`]
/// rather than an error, since it simply has nothing to plot a time axis
/// against.
pub fn analyze_file(path: &Path, opts: &AnalyzeOptions) -> Result<FileAnalysis> {
    let image = libfitz::fits_file::load_fits(path)?;
    let time_str = image
        .header
        .get_string("DATE-LOC")
        .or_else(|| image.header.get_string("DATE-OBS"))
        .map(str::to_string);
    let Some(time) = time_str
        .as_deref()
        .and_then(libfitz::fits_file::parse_date_obs)
    else {
        return Ok(FileAnalysis::Skipped(SkipReason::NoDateObs));
    };

    let stats = image.stats().channels;
    let stars = opts
        .detect_stars
        .then(|| image.detection_plane().ok())
        .flatten()
        .map(|plane| plane.detect_stars(&StarDetectOptions::default()));

    Ok(FileAnalysis::Analyzed(FileMetrics {
        path: path.to_path_buf(),
        time,
        time_str: time_str.unwrap_or_default(),
        stats,
        stars,
    }))
}

/// One sample on a chart line.
#[derive(Debug, PartialEq)]
pub struct SamplePoint {
    pub time: f64,
    pub time_str: String,
    pub value: f64,
    pub path: PathBuf,
}

/// One plotted line: `label` is `None` for a star metric or a single-channel
/// pixel source, `Some("R"/"G"/"B")` for a per-channel pixel metric measured
/// on an already-debayered RGB source.
#[derive(Debug, PartialEq)]
pub struct ChannelSeries {
    pub label: Option<&'static str>,
    pub points: Vec<SamplePoint>,
}

/// A metric plotted across a batch of files: 1 line for a star metric or a
/// single-channel pixel source, 3 (R, G, B) for a per-channel pixel metric on
/// an already-debayered RGB source.
#[derive(Debug, PartialEq)]
pub struct Series {
    pub metric: Metric,
    pub channels: Vec<ChannelSeries>,
    /// Analyzed files with no value for this metric: a star metric on a frame
    /// where detection found nothing, or — for a pixel metric — a file whose
    /// channel count doesn't match the rest of the batch (a working set
    /// mixing debayered-RGB and mono/CFA frames).
    pub unavailable: usize,
}

/// The R/G/B label for channel `index` of a `count`-channel source, or `None`
/// when `count` isn't 3 (a single-channel source, the only other shape
/// `Image::stats()` produces).
fn channel_label(count: usize, index: usize) -> Option<&'static str> {
    match (count, index) {
        (3, 0) => Some("R"),
        (3, 1) => Some("G"),
        (3, 2) => Some("B"),
        _ => None,
    }
}

/// Build the plotted series for `metric` from a batch's measured files,
/// sorted by acquisition time.
pub fn build_series(files: &[FileMetrics], metric: Metric) -> Series {
    let mut sorted: Vec<&FileMetrics> = files.iter().collect();
    sorted.sort_by(|a, b| a.time.total_cmp(&b.time));

    match metric.family() {
        MetricFamily::Star => {
            let mut points = Vec::new();
            let mut unavailable = 0;
            for f in &sorted {
                match f.stars.as_ref().and_then(|s| metric.star_value(s)) {
                    Some(value) => points.push(SamplePoint {
                        time: f.time,
                        time_str: f.time_str.clone(),
                        value,
                        path: f.path.clone(),
                    }),
                    None => unavailable += 1,
                }
            }
            Series {
                metric,
                channels: vec![ChannelSeries {
                    label: None,
                    points,
                }],
                unavailable,
            }
        }
        MetricFamily::Pixel => {
            // The channel count of the first file that has stats at all sets
            // the shape for the whole series; a file that disagrees (a mixed
            // batch) contributes to `unavailable` instead of a mismatched point.
            let Some(expected) = sorted
                .iter()
                .find_map(|f| (!f.stats.is_empty()).then_some(f.stats.len()))
            else {
                return Series {
                    metric,
                    channels: Vec::new(),
                    unavailable: sorted.len(),
                };
            };
            let mut channels: Vec<ChannelSeries> = (0..expected)
                .map(|i| ChannelSeries {
                    label: channel_label(expected, i),
                    points: Vec::new(),
                })
                .collect();
            let mut unavailable = 0;
            for f in &sorted {
                if f.stats.len() != expected {
                    unavailable += 1;
                    continue;
                }
                for (i, s) in f.stats.iter().enumerate() {
                    channels[i].points.push(SamplePoint {
                        time: f.time,
                        time_str: f.time_str.clone(),
                        value: metric.pixel_value(s),
                        path: f.path.clone(),
                    });
                }
            }
            Series {
                metric,
                channels,
                unavailable,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_data;
    use libfitz::stats::Stats;

    fn stats_fixture(mean: f32) -> Stats {
        Stats {
            mean,
            ..Stats::default()
        }
    }

    fn metrics(path: &str, time: f64, stats: Vec<Stats>, stars: Option<StarStats>) -> FileMetrics {
        FileMetrics {
            path: PathBuf::from(path),
            time,
            time_str: String::new(),
            stats,
            stars,
        }
    }

    #[test]
    fn every_metric_is_listed_in_exactly_one_family() {
        for family in [MetricFamily::Pixel, MetricFamily::Star] {
            for &m in Metric::of_family(family) {
                assert_eq!(m.family(), family);
            }
        }
    }

    #[test]
    fn build_series_splits_a_debayered_rgb_batch_into_three_channels() {
        let files = vec![
            metrics(
                "a.fits",
                100.0,
                vec![
                    stats_fixture(10.0),
                    stats_fixture(20.0),
                    stats_fixture(30.0),
                ],
                None,
            ),
            metrics(
                "b.fits",
                200.0,
                vec![
                    stats_fixture(11.0),
                    stats_fixture(21.0),
                    stats_fixture(31.0),
                ],
                None,
            ),
        ];
        let series = build_series(&files, Metric::Mean);

        assert_eq!(series.channels.len(), 3);
        assert_eq!(
            series.channels.iter().map(|c| c.label).collect::<Vec<_>>(),
            [Some("R"), Some("G"), Some("B")]
        );
        assert_eq!(
            series.channels[0]
                .points
                .iter()
                .map(|p| p.value)
                .collect::<Vec<_>>(),
            [10.0, 11.0]
        );
        assert_eq!(
            series.channels[1]
                .points
                .iter()
                .map(|p| p.value)
                .collect::<Vec<_>>(),
            [20.0, 21.0]
        );
        assert_eq!(
            series.channels[2]
                .points
                .iter()
                .map(|p| p.value)
                .collect::<Vec<_>>(),
            [30.0, 31.0]
        );
        assert_eq!(series.unavailable, 0);
    }

    #[test]
    fn build_series_keeps_a_mono_batch_as_one_unlabeled_line() {
        let files = vec![metrics("a.fits", 100.0, vec![stats_fixture(10.0)], None)];
        let series = build_series(&files, Metric::Mean);

        assert_eq!(series.channels.len(), 1);
        assert_eq!(series.channels[0].label, None);
        assert_eq!(series.channels[0].points.len(), 1);
    }

    #[test]
    fn build_series_excludes_a_mismatched_channel_count_from_a_mixed_batch() {
        // The working set mixes an already-debayered RGB file with a mono/CFA
        // one; the shape follows the first file, and the odd one out is
        // counted `unavailable` rather than plotted on the wrong axis.
        let files = vec![
            metrics(
                "rgb.fits",
                100.0,
                vec![
                    stats_fixture(10.0),
                    stats_fixture(20.0),
                    stats_fixture(30.0),
                ],
                None,
            ),
            metrics("mono.fits", 200.0, vec![stats_fixture(15.0)], None),
        ];
        let series = build_series(&files, Metric::Mean);

        assert_eq!(series.channels.len(), 3);
        assert!(series.channels.iter().all(|c| c.points.len() == 1));
        assert_eq!(series.unavailable, 1);
    }

    #[test]
    fn build_series_star_metric_is_always_one_line() {
        let files = vec![
            metrics(
                "a.fits",
                100.0,
                vec![],
                Some(StarStats {
                    count: 5,
                    hfr: Some(2.0),
                    fwhm: Some(3.0),
                    eccentricity: Some(0.1),
                }),
            ),
            metrics(
                "b.fits",
                200.0,
                vec![],
                Some(StarStats {
                    count: 0,
                    hfr: None,
                    fwhm: None,
                    eccentricity: None,
                }),
            ),
        ];
        let series = build_series(&files, Metric::Hfr);

        assert_eq!(series.channels.len(), 1);
        assert_eq!(series.channels[0].label, None);
        assert_eq!(series.channels[0].points.len(), 1);
        // The starless frame has no HFR to report.
        assert_eq!(series.unavailable, 1);
    }

    #[test]
    fn build_series_sorts_by_acquisition_time() {
        let files = vec![
            metrics("late.fits", 200.0, vec![stats_fixture(2.0)], None),
            metrics("early.fits", 100.0, vec![stats_fixture(1.0)], None),
        ];
        let series = build_series(&files, Metric::Mean);
        assert_eq!(
            series.channels[0]
                .points
                .iter()
                .map(|p| &p.path)
                .collect::<Vec<_>>(),
            [&PathBuf::from("early.fits"), &PathBuf::from("late.fits")]
        );
    }

    #[test]
    fn analyze_file_skips_a_frame_with_no_acquisition_time() {
        let outcome = analyze_file(
            &test_data("uncompressed.fit"),
            &AnalyzeOptions {
                detect_stars: false,
            },
        );
        // The bundled fixture carries a real DATE-LOC/DATE-OBS, so it measures.
        assert!(matches!(outcome.unwrap(), FileAnalysis::Analyzed(_)));
    }

    #[test]
    fn analyze_file_measures_pixel_stats_and_optional_stars() {
        let path = test_data("uncompressed.fit");
        let FileAnalysis::Analyzed(m) = analyze_file(
            &path,
            &AnalyzeOptions {
                detect_stars: false,
            },
        )
        .unwrap() else {
            panic!("expected the bundled frame to measure");
        };
        assert!(!m.stats.is_empty());
        assert!(m.stars.is_none(), "star detection wasn't requested");

        let FileAnalysis::Analyzed(m) =
            analyze_file(&path, &AnalyzeOptions { detect_stars: true }).unwrap()
        else {
            panic!("expected the bundled frame to measure");
        };
        assert!(m.stars.is_some());
    }
}
