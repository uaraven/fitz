//! The plain-text session report: the layout `fitz summary` prints, shared with
//! FitSmith's "copy to clipboard" so both produce identical text. Pure
//! formatting of a [`Summary`] — scanning and grouping are
//! [`crate::session`]'s.

use crate::session::{FilterGroup, FrameType, Session, Summary, format_duration};
use std::fmt::Write as _;

/// One printed line of the report: the light frames as a whole, or one of their
/// filters.
struct Row {
    /// Nesting depth below the session line: 0 for the lights, 1 for a filter.
    depth: usize,
    label: String,
    count: usize,
    integration: f64,
}

impl Row {
    fn filter_row(group: &FilterGroup) -> Self {
        Self {
            depth: 1,
            label: group.label().to_string(),
            count: group.count,
            integration: group.integration,
        }
    }
}

/// The light frames of `session` — one row for all of them, then one per filter.
/// Calibration frames are only counted (see [`frame_counts`]), not broken down.
fn session_rows(session: &Session) -> Vec<Row> {
    let Some(lights) = session.type_group(FrameType::Light) else {
        return Vec::new();
    };
    std::iter::once(Row {
        depth: 0,
        label: FrameType::Light.label().to_string(),
        count: lights.count,
        integration: lights.integration,
    })
    .chain(lights.filters.iter().map(Row::filter_row))
    .collect()
}

fn frames(count: usize, noun: &str) -> String {
    format!("{count} {noun} frame{}", if count == 1 { "" } else { "s" })
}

/// `"92 light frames, 30 dark frames, 20 flat frames"` — every frame type the
/// session has, in the usual type order.
fn frame_counts(session: &Session) -> String {
    session
        .types
        .iter()
        .map(|t| frames(t.count, t.frame_type.noun()))
        .collect::<Vec<_>>()
        .join(", ")
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

    // Column widths shared by every row of the report.
    let label_width = rows()
        .map(|r| 2 * r.depth + r.label.len())
        .max()
        .unwrap_or(0);
    let count_width = rows().map(|r| r.count.to_string().len()).max().unwrap_or(1);
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
                "  {:indent$}{:<label$}  {:>count$} {:<6}  {:>time$}",
                "",
                row.label,
                row.count,
                if row.count == 1 { "frame" } else { "frames" },
                format_duration(row.integration),
                indent = 2 * row.depth,
                label = label_width - 2 * row.depth,
                count = count_width,
                time = time_width,
            );
        }
    }

    let _ = writeln!(
        out,
        "Total: {}, {}, total integration {}",
        if summary.sessions.len() == 1 {
            "1 session".to_string()
        } else {
            format!("{} sessions", summary.sessions.len())
        },
        frames(summary.light_count(), "light"),
        format_duration(summary.integration)
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Frame, TypeGroup};
    use std::path::PathBuf;

    fn group(filter: Option<&str>, count: usize, integration: f64) -> FilterGroup {
        FilterGroup {
            filter: filter.map(str::to_string),
            frames: Vec::<Frame>::new(),
            count,
            integration,
        }
    }

    fn type_group(frame_type: FrameType, filters: Vec<FilterGroup>) -> TypeGroup {
        TypeGroup {
            frame_type,
            count: filters.iter().map(|g| g.count).sum(),
            integration: filters.iter().map(|g| g.integration).sum(),
            filters,
        }
    }

    fn night_session(day: &str, types: Vec<TypeGroup>) -> Session {
        let night = crate::session::night_of(
            crate::fits_file::parse_date_obs(&format!("{day}T22:00:00")).unwrap(),
        );
        session(Some(night), types)
    }

    fn session(night: Option<i64>, types: Vec<TypeGroup>) -> Session {
        let light = types.iter().find(|t| t.frame_type == FrameType::Light);
        Session {
            night,
            count: types.iter().map(|t| t.count).sum(),
            integration: light.map_or(0.0, |t| t.integration),
            start: None,
            end: None,
            types,
        }
    }

    fn summary_of(sessions: Vec<Session>) -> Summary {
        Summary {
            root: PathBuf::from("/data/M31"),
            count: sessions.iter().map(|s| s.count).sum(),
            integration: sessions.iter().map(|s| s.integration).sum(),
            sessions,
            errors: Vec::new(),
        }
    }

    #[test]
    fn report_follows_the_session_template_with_aligned_columns() {
        let first = night_session(
            "2026-10-08",
            vec![
                type_group(
                    FrameType::Light,
                    vec![
                        group(Some("R"), 9, 1_080.0),
                        group(Some("G"), 9, 1_080.0),
                        group(Some("B"), 9, 1_080.0),
                        group(Some("Ha"), 18, 5_400.0),
                        group(Some("SII"), 22, 6_600.0),
                        group(Some("OIII"), 19, 5_700.0),
                    ],
                ),
                type_group(FrameType::Dark, vec![group(None, 20, 6_000.0)]),
                type_group(FrameType::Flat, vec![group(Some("Ha"), 30, 60.0)]),
            ],
        );
        let second = night_session(
            "2026-10-09",
            vec![type_group(
                FrameType::Light,
                vec![
                    group(Some("Ha"), 32, 9_600.0),
                    group(Some("SII"), 32, 9_600.0),
                    group(Some("OIII"), 28, 8_400.0),
                ],
            )],
        );

        let report = format_summary(&summary_of(vec![first, second]));

        let expected = "\
Session 2026-10-08 - 5h 49m 00s light integration
    86 light frames, 20 dark frames, 30 flat frames
  Lights  86 frames  5h 49m 00s
    R      9 frames  0h 18m 00s
    G      9 frames  0h 18m 00s
    B      9 frames  0h 18m 00s
    Ha    18 frames  1h 30m 00s
    SII   22 frames  1h 50m 00s
    OIII  19 frames  1h 35m 00s
Session 2026-10-09 - 7h 40m 00s light integration
    92 light frames
  Lights  92 frames  7h 40m 00s
    Ha    32 frames  2h 40m 00s
    SII   32 frames  2h 40m 00s
    OIII  28 frames  2h 20m 00s
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
    (no filter)  1 frame   0h 00m 30s
Total: 1 session, 1 light frame, total integration 0h 00m 30s
";
        assert_eq!(report, expected);
    }

    #[test]
    fn a_calibration_only_night_has_counts_but_no_light_rows() {
        let darks = night_session(
            "2026-10-10",
            vec![type_group(FrameType::Dark, vec![group(None, 12, 3_600.0)])],
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
    fn an_empty_summary_reports_only_the_total() {
        assert_eq!(
            format_summary(&summary_of(Vec::new())),
            "Total: 0 sessions, 0 light frames, total integration 0h 00m 00s\n"
        );
    }
}
