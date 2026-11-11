//! The plain-text session report: the layout `fitz summary` prints, shared with
//! FitSmith's "copy to clipboard" so both produce identical text. Pure
//! formatting of a [`Summary`] — scanning and grouping are
//! [`crate::session`]'s.

use crate::session::{
    FilterGroup, FrameType, Session, Summary, count_noun, format_duration, format_exposure,
};
use std::fmt::Write as _;

/// One printed line of the report: the light frames as a whole, or the frames
/// of one filter taken at one exposure time.
struct Row {
    /// Nesting depth below the session line: 0 for the lights, 1 for a filter.
    depth: usize,
    label: String,
    count: usize,
    /// What follows the count: `" frames"` for the lights, `"x300s"` (each
    /// frame's exposure) for a filter.
    unit: String,
    integration: f64,
}

/// The unit printed after the lights' frame count.
fn frames_unit(count: usize) -> &'static str {
    if count == 1 { " frame" } else { " frames" }
}

impl Row {
    /// One row per exposure time of `group`, longest first, each labelled
    /// with the filter.
    fn filter_rows(group: &FilterGroup) -> impl Iterator<Item = Self> + '_ {
        group.exposures.iter().map(|e| Self {
            depth: 1,
            label: group.label().to_string(),
            count: e.count,
            unit: format!("x{}", format_exposure(e.exposure)),
            integration: e.integration,
        })
    }
}

/// The light frames of `session` — one row for all of them, then one per filter
/// and exposure time. Calibration frames are only counted (see
/// [`frame_counts`]), not broken down.
fn session_rows(session: &Session) -> Vec<Row> {
    let Some(lights) = session.type_group(FrameType::Light) else {
        return Vec::new();
    };
    std::iter::once(Row {
        depth: 0,
        label: FrameType::Light.label().to_string(),
        count: lights.count,
        unit: frames_unit(lights.count).to_string(),
        integration: lights.integration,
    })
    .chain(lights.filters.iter().flat_map(Row::filter_rows))
    .collect()
}

/// `"92 light frames, 30 dark frames, 20 flat frames"` — every frame type the
/// session has, in the usual type order.
fn frame_counts(session: &Session) -> String {
    session
        .types
        .iter()
        .map(|t| count_noun(t.count, &format!("{} frame", t.frame_type.noun())))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The one-line totals: `"2 sessions, 178 light frames, total integration
/// 13h 29m 00s"` — sessions, light frames and light integration over the whole
/// summary. The report's closing line, and the GUI's status line.
pub fn totals(summary: &Summary) -> String {
    format!(
        "{}, {}, total integration {}",
        count_noun(summary.sessions.len(), "session"),
        count_noun(summary.light_count(), "light frame"),
        format_duration(summary.integration)
    )
}

/// The report for one scanned directory: each session (a heading with its light
/// integration, the frame counts by type, then the light frames by filter with
/// columns aligned across the whole report) and a closing total over the light
/// frames.
pub fn format_summary(summary: &Summary) -> String {
    let mut out = String::new();

    let sessions: Vec<(&Session, Vec<Row>)> = summary
        .sessions
        .iter()
        .map(|s| (s, session_rows(s)))
        .collect();
    let rows = || sessions.iter().flat_map(|(_, rows)| rows);

    // Column widths shared by every row of the report. Widths are in characters
    // (what `{:<n}` pads to), not bytes: a filter name from a header may well be
    // non-ASCII ("Hα").
    let label_width = rows()
        .map(|r| 2 * r.depth + r.label.chars().count())
        .max()
        .unwrap_or(0);
    let count_width = rows().map(|r| r.count.to_string().len()).max().unwrap_or(1);
    // At least as wide as " frames", so the lights' row reads as it always has.
    let unit_width = rows()
        .map(|r| r.unit.chars().count())
        .max()
        .unwrap_or(0)
        .max(frames_unit(2).len());
    let time_width = rows()
        .map(|r| format_duration(r.integration).len())
        .max()
        .unwrap_or(0);

    for (session, rows) in &sessions {
        let _ = writeln!(
            out,
            "Session {} - {} light integration",
            session.label(),
            format_duration(session.integration)
        );
        let _ = writeln!(out, "    {}", frame_counts(session));
        for row in rows {
            let _ = writeln!(
                out,
                "  {:indent$}{:<label$}  {:>count$}{:<unit$}  {:>time$}",
                "",
                row.label,
                row.count,
                row.unit,
                format_duration(row.integration),
                indent = 2 * row.depth,
                label = label_width - 2 * row.depth,
                count = count_width,
                unit = unit_width,
                time = time_width,
            );
        }
    }

    let _ = writeln!(out, "Total: {}", totals(summary));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Frame, TypeGroup};
    use std::path::PathBuf;

    // The fixtures go through the real `FilterGroup`/`TypeGroup`/`Session`/
    // `Summary` constructors, so the totals and exposure groups the report
    // prints are computed by the same code as in a real scan.
    fn frames(count: usize, exposure: f64) -> impl Iterator<Item = Frame> {
        (0..count).map(move |i| Frame {
            path: PathBuf::from(format!("/data/M31/{exposure}s_{i:03}.fit")),
            exposure,
            observed: None,
        })
    }

    /// A filter's frames, `count` of them at each `exposure`.
    fn exposures(filter: Option<&str>, runs: &[(usize, f64)]) -> FilterGroup {
        let frames = runs
            .iter()
            .flat_map(|&(count, exposure)| frames(count, exposure))
            .collect();
        FilterGroup::new(filter.map(str::to_string), frames)
    }

    /// `count` frames through `filter`, all of one exposure.
    fn group(filter: Option<&str>, count: usize, exposure: f64) -> FilterGroup {
        exposures(filter, &[(count, exposure)])
    }

    fn type_group(frame_type: FrameType, filters: Vec<FilterGroup>) -> TypeGroup {
        TypeGroup::new(frame_type, filters)
    }

    fn night_session(day: &str, types: Vec<TypeGroup>) -> Session {
        let night = crate::session::night_of(
            crate::fits_file::parse_date_obs(&format!("{day}T22:00:00")).unwrap(),
        );
        session(Some(night), types)
    }

    fn session(night: Option<i64>, types: Vec<TypeGroup>) -> Session {
        Session::new(night, types)
    }

    fn summary_of(sessions: Vec<Session>) -> Summary {
        Summary::new(PathBuf::from("/data/M31"), sessions, Vec::new())
    }

    #[test]
    fn report_follows_the_session_template_with_aligned_columns() {
        let first = night_session(
            "2026-10-08",
            vec![
                type_group(
                    FrameType::Light,
                    vec![
                        group(Some("R"), 9, 120.0),
                        group(Some("G"), 9, 120.0),
                        group(Some("B"), 9, 120.0),
                        group(Some("Ha"), 18, 300.0),
                        group(Some("SII"), 22, 300.0),
                        group(Some("OIII"), 19, 300.0),
                    ],
                ),
                type_group(FrameType::Dark, vec![group(None, 20, 300.0)]),
                type_group(FrameType::Flat, vec![group(Some("Ha"), 30, 2.0)]),
            ],
        );
        let second = night_session(
            "2026-10-09",
            vec![type_group(
                FrameType::Light,
                vec![
                    group(Some("Ha"), 32, 300.0),
                    group(Some("SII"), 32, 300.0),
                    group(Some("OIII"), 28, 300.0),
                ],
            )],
        );

        let report = format_summary(&summary_of(vec![first, second]));

        let expected = "\
Session 2026-10-08 - 5h 49m 00s light integration
    86 light frames, 20 dark frames, 30 flat frames
  Lights  86 frames  5h 49m 00s
    R      9x120s    0h 18m 00s
    G      9x120s    0h 18m 00s
    B      9x120s    0h 18m 00s
    Ha    18x300s    1h 30m 00s
    SII   22x300s    1h 50m 00s
    OIII  19x300s    1h 35m 00s
Session 2026-10-09 - 7h 40m 00s light integration
    92 light frames
  Lights  92 frames  7h 40m 00s
    Ha    32x300s    2h 40m 00s
    SII   32x300s    2h 40m 00s
    OIII  28x300s    2h 20m 00s
Total: 2 sessions, 178 light frames, total integration 13h 29m 00s
";
        assert_eq!(report, expected);
    }

    #[test]
    fn singular_counts_unknown_dates_and_missing_filters_are_worded_plainly() {
        let unknown = session(
            None,
            vec![
                type_group(FrameType::Light, vec![group(None, 1, 30.0)]),
                type_group(FrameType::Bias, vec![group(None, 1, 0.0)]),
                type_group(FrameType::DarkFlat, vec![group(None, 2, 0.0)]),
            ],
        );

        let report = format_summary(&summary_of(vec![unknown]));

        let expected = "\
Session unknown date - 0h 00m 30s light integration
    1 light frame, 1 bias frame, 2 dark flat frames
  Lights         1 frame   0h 00m 30s
    (no filter)  1x30s     0h 00m 30s
Total: 1 session, 1 light frame, total integration 0h 00m 30s
";
        assert_eq!(report, expected);
    }

    #[test]
    fn a_calibration_only_night_has_counts_but_no_light_rows() {
        let darks = night_session(
            "2026-10-10",
            vec![type_group(FrameType::Dark, vec![group(None, 12, 300.0)])],
        );

        let report = format_summary(&summary_of(vec![darks]));

        let expected = "\
Session 2026-10-10 - 0h 00m 00s light integration
    12 dark frames
Total: 1 session, 0 light frames, total integration 0h 00m 00s
";
        assert_eq!(report, expected);
    }

    #[test]
    fn non_ascii_filter_names_keep_the_columns_aligned() {
        let night = night_session(
            "2026-10-08",
            vec![type_group(
                FrameType::Light,
                vec![
                    group(Some("Hα 7nm"), 2, 300.0),
                    group(Some("OIII"), 3, 300.0),
                ],
            )],
        );

        let report = format_summary(&summary_of(vec![night]));

        let expected = "\
Session 2026-10-08 - 0h 25m 00s light integration
    5 light frames
  Lights    5 frames  0h 25m 00s
    Hα 7nm  2x300s    0h 10m 00s
    OIII    3x300s    0h 15m 00s
Total: 1 session, 5 light frames, total integration 0h 25m 00s
";
        assert_eq!(report, expected);
    }

    #[test]
    fn each_exposure_time_of_a_filter_is_its_own_row_longest_first() {
        let night = night_session(
            "2026-10-08",
            vec![type_group(
                FrameType::Light,
                vec![
                    exposures(Some("Ha"), &[(30, 60.0), (12, 300.0)]),
                    exposures(Some("OIII"), &[(4, 0.5), (1, 120.0)]),
                ],
            )],
        );

        let report = format_summary(&summary_of(vec![night]));

        let expected = "\
Session 2026-10-08 - 1h 32m 02s light integration
    47 light frames
  Lights  47 frames  1h 32m 02s
    Ha    12x300s    1h 00m 00s
    Ha    30x60s     0h 30m 00s
    OIII   1x120s    0h 02m 00s
    OIII   4x0.5s    0h 00m 02s
Total: 1 session, 47 light frames, total integration 1h 32m 02s
";
        assert_eq!(report, expected);
    }

    #[test]
    fn an_empty_summary_reports_only_the_total() {
        assert_eq!(
            format_summary(&summary_of(Vec::new())),
            "Total: 0 sessions, 0 light frames, total integration 0h 00m 00s\n"
        );
    }
}
