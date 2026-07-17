//! Turning an analytics [`Series`] into the geometry `chart.slint` draws: points
//! and axis ticks in screen-normalized 0..1, plus the SVG path for the series
//! line. Pure "data in → Slint props out", mirroring [`crate::view`] — the
//! controller owns the files and threading, this owns the arithmetic, and all of
//! it is unit-testable without a window.

use chrono::{DateTime, Local, TimeZone};
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

/// Civil (wall-clock) time in `tz` for an epoch timestamp, or `None` if it isn't
/// representable there — an out-of-range timestamp, or a local time that a DST
/// jump skipped or repeated.
fn civil<Tz: TimeZone>(epoch: f64, tz: &Tz) -> Option<DateTime<Tz>> {
    tz.timestamp_opt(epoch.floor() as i64, 0).single()
}

/// Render an epoch timestamp in `tz` with a strftime `fmt`. `DATE-OBS` is UTC by
/// FITS convention, but an observer reads their own wall clock, so the chart
/// converts: a session that ran 22:00–02:00 local should say so rather than
/// naming the UTC hours it happened to span.
///
/// A timestamp `tz` can't represent falls back to rendering it as UTC. That is
/// wrong by an offset, but it only arises for absurd dates or a sample landing
/// exactly in a DST gap, and a chart that draws a slightly-off label beats one
/// that refuses to draw.
fn format_in<Tz: TimeZone>(epoch: f64, tz: &Tz, fmt: &str) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match civil(epoch, tz) {
        Some(t) => t.format(fmt).to_string(),
        None => civil(epoch, &chrono::Utc)
            .map(|t| t.format(fmt).to_string())
            .unwrap_or_default(),
    }
}

/// The axis tick's lower line: the local time of day, `HH:MM`.
fn format_time<Tz: TimeZone>(epoch: f64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    format_in(epoch, tz, "%H:%M")
}

/// The axis tick's upper line: the local date, `YYYY-MM-DD`.
fn format_date<Tz: TimeZone>(epoch: f64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    format_in(epoch, tz, "%Y-%m-%d")
}

/// A point tooltip's timestamp: the full local date and time to the second. The
/// tooltip is a single line with room to spare, so unlike the axis it always
/// carries the date.
fn format_stamp<Tz: TimeZone>(epoch: f64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    format_in(epoch, tz, "%Y-%m-%d %H:%M:%S")
}

/// Map a time-ordered [`Series`] into the chart's 0..1 space, labeling times in
/// the machine's local zone. See [`plot_in`].
pub fn plot(series: &Series) -> Plot {
    plot_in(series, &Local)
}

/// Map a time-ordered [`Series`] into the chart's 0..1 space: X spans the first
/// to the last timestamp, Y spans the value axis inverted (0 is the top). An
/// empty series plots nothing; a single point (or several sharing one timestamp)
/// centers on X, having no span to normalize against.
///
/// Times are rendered in `tz`. Taking the zone as a parameter rather than
/// reaching for [`Local`] is what makes this testable: the tests pin exact
/// labels against a `FixedOffset` and pass wherever they run.
pub fn plot_in<Tz: TimeZone>(series: &Series, tz: &Tz) -> Plot
where
    Tz::Offset: std::fmt::Display,
{
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
            time_label: format_stamp(p.time, tz).into(),
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
            let date = format_date(t, tz);
            let changed = prev_date.as_ref() != Some(&date);
            prev_date = Some(date.clone());
            ChartTick {
                pos: x_of(t),
                label: format_time(t, tz).into(),
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
    use chrono::FixedOffset;
    use libfitz::analytics::{Metric, SamplePoint};
    use std::path::PathBuf;

    /// UTC+02:00 — a zone far enough east that a UTC-evening session lands on
    /// the next local day, which is exactly what the axis has to get right.
    fn tz() -> FixedOffset {
        FixedOffset::east_opt(2 * 3600).unwrap()
    }

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
        let labels: Vec<String> = ticks.iter().map(|&t| format_time(t, &tz())).collect();
        assert_eq!(
            labels,
            [
                "00:00", "00:30", "01:00", "01:30", "02:00", "02:30", "03:00"
            ]
        );

        // A ragged session (no end on a boundary) still ticks on round times,
        // and never outside its own range.
        let ragged = time_ticks(lo + 7.5, lo + 3.0 * 3600.0 - 128.0);
        assert!(ragged.iter().all(|&t| t > lo && t < lo + 3.0 * 3600.0));
        assert!(
            ragged
                .iter()
                .all(|&t| format_stamp(t, &tz()).ends_with(":00"))
        );

        // A 12-hour span steps up to hours rather than crowding the axis.
        let long = time_ticks(lo, lo + 12.0 * 3600.0);
        assert!(long.len() <= 7);
        assert!(long.iter().all(|&t| format_time(t, &tz()).ends_with(":00")));

        // A single instant still yields one tick rather than an empty axis.
        assert_eq!(time_ticks(lo, lo), vec![lo]);
    }

    #[test]
    fn formatters_render_the_given_zones_wall_clock() {
        let t = libfitz::info::parse_date_obs("2026-05-31T04:57:09.004664").unwrap();
        // UTC renders the timestamp as `DATE-OBS` spelled it.
        assert_eq!(format_time(t, &chrono::Utc), "04:57");
        assert_eq!(format_date(t, &chrono::Utc), "2026-05-31");
        assert_eq!(format_stamp(t, &chrono::Utc), "2026-05-31 04:57:09");

        // An eastward offset shifts the clock, here within the same day.
        assert_eq!(format_time(t, &tz()), "06:57");
        assert_eq!(format_stamp(t, &tz()), "2026-05-31 06:57:09");

        // The point of the exercise: an offset that carries the timestamp over
        // a date boundary must move the date with it, in both directions.
        let evening = libfitz::info::parse_date_obs("2026-06-22T23:30:00").unwrap();
        assert_eq!(format_date(evening, &tz()), "2026-06-23");
        assert_eq!(format_time(evening, &tz()), "01:30");

        let west = FixedOffset::west_opt(5 * 3600).unwrap();
        let morning = libfitz::info::parse_date_obs("2026-06-22T02:00:00").unwrap();
        assert_eq!(format_date(morning, &west), "2026-06-21");
        assert_eq!(format_time(morning, &west), "21:00");

        // The epoch itself, and an instant before it.
        assert_eq!(format_stamp(0.0, &chrono::Utc), "1970-01-01 00:00:00");
        assert_eq!(format_stamp(-1.0, &chrono::Utc), "1969-12-31 23:59:59");
    }

    #[test]
    fn x_ticks_carry_the_date_only_where_it_changes() {
        // A session running up to and through local midnight: 22:00 UTC is
        // 00:00 in `tz()`, so start an hour earlier to sit on the day before.
        let lo = libfitz::info::parse_date_obs("2026-06-22T21:00:00").unwrap();
        let p = plot_in(
            &series(&[(lo, 100.0), (lo + 3600.0, 150.0), (lo + 7200.0, 200.0)]),
            &tz(),
        );

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
        let p = plot_in(
            &series(&[(lo, 100.0), (lo + 3600.0, 150.0), (lo + 7200.0, 200.0)]),
            &tz(),
        );

        // X spans first..last; the middle sample sits halfway.
        assert_eq!(p.points[0].x, 0.0);
        assert_eq!(p.points[1].x, 0.5);
        assert_eq!(p.points[2].x, 1.0);
        // Y is inverted: the largest value plots nearest the top.
        assert!(p.points[0].y > p.points[1].y && p.points[1].y > p.points[2].y);
        assert!(p.points.iter().all(|q| (0.0..=1.0).contains(&q.y)));
        assert_eq!(p.points[1].value_label, "150");
        // The tooltip stamp is the full local date and time.
        assert_eq!(p.points[1].time_label, "2026-06-23 01:00:00");

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
        assert_eq!(plot_in(&series(&[]), &tz()), Plot::default());

        // A single frame has no time span to normalize against, so it centers.
        let one = plot_in(&series(&[(1000.0, 42.0)]), &tz());
        assert_eq!(one.points.len(), 1);
        assert_eq!(one.points[0].x, 0.5);
        assert!((0.0..=1.0).contains(&one.points[0].y));
        assert_eq!(one.x_ticks.len(), 1);

        // Several frames sharing one timestamp likewise collapse onto X 0.5
        // without producing NaNs.
        let same = plot_in(&series(&[(1000.0, 1.0), (1000.0, 2.0)]), &tz());
        assert!(same.points.iter().all(|p| p.x == 0.5 && p.y.is_finite()));
    }
}
