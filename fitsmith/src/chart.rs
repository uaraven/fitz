//! Turning an analytics [`Series`] into the geometry `chart.slint` draws: points
//! and axis ticks in screen-normalized 0..1, plus the SVG path for the series
//! line. Pure "data in → Slint props out", mirroring [`crate::view`] — the
//! controller owns the files and threading, this owns the arithmetic, and all of
//! it is unit-testable without a window.

use libfitz::analytics::Series;

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

/// Break a Unix-epoch timestamp (seconds, UTC) into civil fields
/// `(year, month, day, hour, minute, second)`. Uses Howard Hinnant's
/// `civil_from_days` — the exact inverse of the `days_from_civil` that
/// `parse_date_obs` builds the epoch with — so the chart needs no date crate.
/// `div_euclid`/`rem_euclid` keep the day split correct for pre-1970 (negative)
/// timestamps.
fn civil_from_epoch(epoch: f64) -> (i64, u32, u32, u32, u32, u32) {
    let secs = epoch.floor() as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let (year, month, day) = civil_from_days(days);
    let hour = (rem / 3600) as u32;
    let minute = (rem % 3600 / 60) as u32;
    let second = (rem % 60) as u32;
    (year, month, day, hour, minute, second)
}

/// Civil date for a count of days since the Unix epoch (1970-01-01), exact over
/// the whole proleptic Gregorian calendar. The inverse of `days_from_civil`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // March-based month, [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d)
}

/// The axis tick's lower line: the UTC time of day, `HH:MM`.
fn format_time(epoch: f64) -> String {
    let (_, _, _, h, mi, _) = civil_from_epoch(epoch);
    format!("{h:02}:{mi:02}")
}

/// The axis tick's upper line: the UTC date, `YYYY-MM-DD`.
fn format_date(epoch: f64) -> String {
    let (y, m, d, ..) = civil_from_epoch(epoch);
    format!("{y:04}-{m:02}-{d:02}")
}

/// A point tooltip's timestamp: the full UTC date and time to the second. The
/// tooltip is a single line with room to spare, so unlike the axis it always
/// carries the date.
fn format_stamp(epoch: f64) -> String {
    let (y, m, d, h, mi, s) = civil_from_epoch(epoch);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Map a time-ordered [`Series`] into the chart's 0..1 space: X spans the first
/// to the last timestamp, Y spans the value axis inverted (0 is the top). An
/// empty series plots nothing; a single point (or several sharing one timestamp)
/// centers on X, having no span to normalize against.
///
/// Times are labeled in UTC — the frame's acquisition time (`DATE-LOC`, else
/// `DATE-OBS`) exactly as the header spelled it, with no zone conversion.
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
            time_label: format_stamp(p.time).into(),
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

    // The date labels the first tick and then only the ticks that roll over to
    // a new day. Repeating one date under all six ticks of a single night is
    // noise; showing it on the change makes a midnight crossing visible instead.
    let mut prev_date = None;
    let x_ticks = time_ticks(t_lo, t_hi)
        .into_iter()
        .map(|t| {
            let date = format_date(t);
            let changed = prev_date.as_ref() != Some(&date);
            prev_date = Some(date.clone());
            ChartTick {
                pos: x_of(t),
                label: format_time(t).into(),
                date_label: if changed {
                    date.into()
                } else {
                    Default::default()
                },
            }
        })
        .collect();

    Plot {
        points,
        x_ticks,
        y_ticks: axis
            .ticks
            .into_iter()
            .map(|v| ChartTick {
                pos: y_of(v),
                label: format_stat(v).into(),
                // A value axis has no date line.
                date_label: Default::default(),
            })
            .collect(),
        line: line.trim_end().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfitz::analytics::{Metric, SamplePoint};
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
        let lo = libfitz::info::parse_date_obs("2026-06-22T22:00:00").unwrap();
        let ticks = time_ticks(lo, lo + 3.0 * 3600.0);
        let labels: Vec<String> = ticks.iter().map(|&t| format_time(t)).collect();
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
        assert!(ragged.iter().all(|&t| format_stamp(t).ends_with(":00")));

        // A 12-hour span steps up to hours rather than crowding the axis.
        let long = time_ticks(lo, lo + 12.0 * 3600.0);
        assert!(long.len() <= 7);
        assert!(long.iter().all(|&t| format_time(t).ends_with(":00")));

        // A single instant still yields one tick rather than an empty axis.
        assert_eq!(time_ticks(lo, lo), vec![lo]);
    }

    #[test]
    fn formatters_render_utc_wall_clock() {
        let t = libfitz::info::parse_date_obs("2026-05-31T04:57:09.004664").unwrap();
        // The timestamp renders exactly as the header spelled it — no zone shift.
        assert_eq!(format_time(t), "04:57");
        assert_eq!(format_date(t), "2026-05-31");
        assert_eq!(format_stamp(t), "2026-05-31 04:57:09");

        // A timestamp late in the day keeps its own date; no offset moves it.
        let evening = libfitz::info::parse_date_obs("2026-06-22T23:30:00").unwrap();
        assert_eq!(format_date(evening), "2026-06-22");
        assert_eq!(format_time(evening), "23:30");

        // The epoch itself, and an instant before it (exercising the pre-1970
        // negative-day path).
        assert_eq!(format_stamp(0.0), "1970-01-01 00:00:00");
        assert_eq!(format_stamp(-1.0), "1969-12-31 23:59:59");
    }

    #[test]
    fn x_ticks_carry_the_date_only_where_it_changes() {
        // A session running up to and through midnight UTC, so the date rolls
        // over partway across the axis.
        let lo = libfitz::info::parse_date_obs("2026-06-22T23:00:00").unwrap();
        let p = plot(&series(&[
            (lo, 100.0),
            (lo + 3600.0, 150.0),
            (lo + 7200.0, 200.0),
        ]));

        let labels: Vec<(&str, &str)> = p
            .x_ticks
            .iter()
            .map(|t| (t.date_label.as_str(), t.label.as_str()))
            .collect();
        // The first tick is dated; the rest stay bare until midnight rolls the
        // date over, and the day after that goes bare again.
        assert_eq!(
            labels,
            [
                ("2026-06-22", "23:00"),
                ("", "23:30"),
                ("2026-06-23", "00:00"),
                ("", "00:30"),
                ("", "01:00"),
            ]
        );

        // A value axis never carries a date line.
        assert!(p.y_ticks.iter().all(|t| t.date_label.is_empty()));
    }

    #[test]
    fn plot_normalizes_points_into_the_unit_square() {
        // Three frames an hour apart with a rising metric.
        let lo = libfitz::info::parse_date_obs("2026-06-22T22:00:00").unwrap();
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
        // The tooltip stamp is the full UTC date and time.
        assert_eq!(p.points[1].time_label, "2026-06-22 23:00:00");

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
