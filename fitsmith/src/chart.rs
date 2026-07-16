//! Turning an analytics [`Series`] into the geometry `chart.slint` draws: points
//! and axis ticks in screen-normalized 0..1, plus the SVG path for the series
//! line. Pure "data in → Slint props out", mirroring [`crate::view`] — the
//! controller owns the files and threading, this owns the arithmetic, and all of
//! it is unit-testable without a window.

use fitz_core::analytics::Series;

use crate::view::format_stat;
use crate::{ChartPoint, ChartTick};

/// A series rendered into the chart's coordinate space: points and ticks in
/// screen-normalized 0..1 (X from the left, Y from the top), plus the SVG path
/// for the series line in that same space.
#[derive(Default, PartialEq, Debug)]
pub struct Plot {
    pub points: Vec<ChartPoint>,
    pub x_ticks: Vec<ChartTick>,
    pub y_ticks: Vec<ChartTick>,
    pub line: String,
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
pub fn plot(series: &Series) -> Plot {
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
    use fitz_core::analytics::{Metric, SamplePoint};
    use std::path::PathBuf;

    fn series(samples: &[(f64, f64)]) -> Series {
        Series {
            metric: Metric::Mean,
            unavailable: 0,
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
