//! Rendering a [`Plot`] as a standalone SVG document: the analytics chart's
//! export format.
//!
//! Vector rather than raster because a chart *is* vector data — the export ends
//! up in a log, a forum post or a paper, all of which want it sharp at any size.
//! It also sidesteps the reason the old PNG export was wrong: that one cropped
//! the chart out of a window snapshot, so a zoomed-in chart exported only the
//! slice the Flickable happened to be showing. Here the geometry comes from
//! [`plot`](crate::chart::plot), which spans the whole series regardless of what
//! is on screen, so zoom simply doesn't enter into it.
//!
//! Pure "data in → string out", like the rest of [`crate::chart`]: no window, no
//! files, unit-testable on its own.

use crate::chart::Plot;

/// The exported canvas, in SVG user units. A 2.5:1 plot is wide enough for a
/// night's worth of subs without the marks colliding; being vector, the actual
/// display size is the viewer's business.
const WIDTH: f32 = 900.0;
const HEIGHT: f32 = 360.0;

// The gutters mirror `chart.slint`'s, so the export is laid out like the chart
// it came from. They are duplicated rather than shared because Slint owns the
// live values and Rust can't read them off a component that isn't rendering.
const Y_AXIS_W: f32 = 64.0;
const X_AXIS_H: f32 = 22.0;
const TITLE_H: f32 = 18.0;
const AREA_H: f32 = HEIGHT - X_AXIS_H - TITLE_H;
const PLOT_W: f32 = WIDTH - Y_AXIS_W;
const FONT_SIZE: f32 = 11.0;
/// Inset of the data from the frame, enough for a mark's radius plus the line's
/// stroke. Without it an extreme sample — the first frame, the night's lowest
/// value — is centered exactly on the frame and drawn half outside the canvas.
const PAD: f32 = 6.0;

// The light-theme colors from `chart.slint`. An export is a document that will
// be looked at anywhere, so it always renders light rather than following the
// app's current scheme.
const PAGE_BG: &str = "#ffffff";
const PLOT_BG: &str = "#fdfdfd";
const AXIS_COLOR: &str = "#999999";
const GRID_COLOR: &str = "#e4e4e4";
const LINE_COLOR: &str = "#0a5ea8";
const MARK_COLOR: &str = "#0a5ea8";
const LABEL_COLOR: &str = "#555555";

/// Escape the five XML metacharacters so a label can't break the document. Tick
/// labels are numbers and clock times, but a metric label is free text.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Plot-area X for a normalized 0..1 position. Both the data and the ticks map
/// through here, so [`PAD`] insets them together and a gridline still passes
/// through the sample it belongs to.
fn plot_x(pos: f32) -> f32 {
    Y_AXIS_W + PAD + pos * (PLOT_W - 2.0 * PAD)
}

/// Plot-area Y for a normalized 0..1 position.
fn plot_y(pos: f32) -> f32 {
    TITLE_H + PAD + pos * (AREA_H - 2.0 * PAD)
}

/// Render `plot` as a complete SVG document titled with `metric_label`. An empty
/// plot still yields a valid document — axes, frame and a "No data to plot"
/// note, matching what the chart shows.
pub fn svg(plot: &Plot, metric_label: &str) -> String {
    let mut s = String::with_capacity(1024 + plot.points.len() * 96);
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{WIDTH}\" height=\"{HEIGHT}\" \
         viewBox=\"0 0 {WIDTH} {HEIGHT}\" font-family=\"sans-serif\">\n"
    ));
    s.push_str(&format!(
        "<rect width=\"{WIDTH}\" height=\"{HEIGHT}\" fill=\"{PAGE_BG}\"/>\n"
    ));

    // Plot background, then the gridlines on top of it, then the frame last so
    // the axes stay crisp over the grid.
    s.push_str(&format!(
        "<rect x=\"{Y_AXIS_W}\" y=\"{TITLE_H}\" width=\"{PLOT_W}\" height=\"{AREA_H}\" \
         fill=\"{PLOT_BG}\"/>\n"
    ));
    for tick in &plot.y_ticks {
        let y = plot_y(tick.pos);
        s.push_str(&format!(
            "<line x1=\"{Y_AXIS_W}\" y1=\"{y:.2}\" x2=\"{:.2}\" y2=\"{y:.2}\" \
             stroke=\"{GRID_COLOR}\"/>\n",
            Y_AXIS_W + PLOT_W
        ));
    }
    for tick in &plot.x_ticks {
        let x = plot_x(tick.pos);
        s.push_str(&format!(
            "<line x1=\"{x:.2}\" y1=\"{TITLE_H}\" x2=\"{x:.2}\" y2=\"{:.2}\" \
             stroke=\"{GRID_COLOR}\"/>\n",
            TITLE_H + AREA_H
        ));
    }
    s.push_str(&format!(
        "<rect x=\"{Y_AXIS_W}\" y=\"{TITLE_H}\" width=\"{PLOT_W}\" height=\"{AREA_H}\" \
         fill=\"none\" stroke=\"{AXIS_COLOR}\"/>\n"
    ));

    // The Y-axis title, above the tick labels.
    s.push_str(&format!(
        "<text x=\"0\" y=\"{:.2}\" font-size=\"{FONT_SIZE}\" font-weight=\"700\" \
         fill=\"{LABEL_COLOR}\">{}</text>\n",
        TITLE_H - 5.0,
        escape(metric_label)
    ));

    // Y tick labels, right-aligned against the plot's left edge.
    for tick in &plot.y_ticks {
        s.push_str(&format!(
            "<text x=\"{:.2}\" y=\"{:.2}\" font-size=\"{FONT_SIZE}\" text-anchor=\"end\" \
             dominant-baseline=\"central\" fill=\"{LABEL_COLOR}\">{}</text>\n",
            Y_AXIS_W - 8.0,
            plot_y(tick.pos),
            escape(&tick.label)
        ));
    }

    // X tick labels, centered under their gridlines but pulled inside the
    // canvas at the ends, the same way the chart clamps them.
    for tick in &plot.x_ticks {
        let x = plot_x(tick.pos).clamp(Y_AXIS_W + 18.0, WIDTH - 18.0);
        s.push_str(&format!(
            "<text x=\"{x:.2}\" y=\"{:.2}\" font-size=\"{FONT_SIZE}\" text-anchor=\"middle\" \
             fill=\"{LABEL_COLOR}\">{}</text>\n",
            TITLE_H + AREA_H + 5.0 + FONT_SIZE,
            escape(&tick.label)
        ));
    }

    if plot.points.is_empty() {
        s.push_str(&format!(
            "<text x=\"{:.2}\" y=\"{:.2}\" font-size=\"{FONT_SIZE}\" text-anchor=\"middle\" \
             fill=\"{LABEL_COLOR}\">No data to plot</text>\n",
            Y_AXIS_W + PLOT_W / 2.0,
            TITLE_H + AREA_H / 2.0
        ));
        s.push_str("</svg>\n");
        return s;
    }

    // The series line. Built from the points rather than from `plot.line`: that
    // string is normalized for Slint's stretch-to-fit viewbox, which would scale
    // the stroke unevenly here.
    s.push_str(&format!(
        "<polyline fill=\"none\" stroke=\"{LINE_COLOR}\" stroke-width=\"2\" \
         stroke-linecap=\"round\" stroke-linejoin=\"round\" points=\""
    ));
    for (i, p) in plot.points.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{:.2},{:.2}", plot_x(p.x), plot_y(p.y)));
    }
    s.push_str("\"/>\n");

    // Point marks. No tooltips — an SVG has no hover — so each mark carries its
    // reading as a <title>, which viewers surface as a tooltip anyway.
    for p in &plot.points {
        s.push_str(&format!(
            "<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"4.5\" fill=\"{MARK_COLOR}\">\
             <title>{} — {}</title></circle>\n",
            plot_x(p.x),
            plot_y(p.y),
            escape(&p.time_label),
            escape(&p.value_label)
        ));
    }

    s.push_str("</svg>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::plot;
    use fitz_core::analytics::{Metric, SamplePoint, Series};
    use std::path::PathBuf;

    /// A three-frame session an hour apart, with a rising metric.
    fn sample_plot() -> Plot {
        let lo = fitz_core::info::parse_date_obs("2026-06-22T22:00:00").unwrap();
        let series = Series {
            metric: Metric::Mean,
            unavailable: 0,
            points: [(lo, 100.0), (lo + 3600.0, 150.0), (lo + 7200.0, 200.0)]
                .into_iter()
                .map(|(time, value)| SamplePoint {
                    time,
                    time_str: String::new(),
                    value,
                    path: PathBuf::from("f.fits"),
                })
                .collect(),
        };
        plot(&series)
    }

    #[test]
    fn svg_plots_every_point_at_full_extent() {
        let p = sample_plot();
        let doc = svg(&p, "Mean (ADU)");

        // A well-formed document at the declared canvas size.
        assert!(doc.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(doc.contains("viewBox=\"0 0 900 360\""));
        assert!(doc.trim_end().ends_with("</svg>"));
        assert_eq!(doc.matches("<svg").count(), 1);

        // Every sample is drawn, and the polyline joins all three.
        assert_eq!(doc.matches("<circle").count(), 3);
        assert_eq!(doc.matches("<polyline").count(), 1);

        // The whole series spans the plot width: the first and last marks sit at
        // its left and right extremes. This is what the old snapshot export got
        // wrong — at zoom > 1 it could only ever reach the visible slice.
        assert!(doc.contains(&format!("cx=\"{:.2}\"", Y_AXIS_W + PAD)));
        assert!(doc.contains(&format!("cx=\"{:.2}\"", WIDTH - PAD)));

        // Each mark carries its reading, and the metric titles the Y axis.
        assert!(doc.contains("<title>23:00:00 — 150</title>"));
        assert!(doc.contains(">Mean (ADU)</text>"));
    }

    #[test]
    fn svg_keeps_all_marks_and_labels_inside_the_canvas() {
        let p = sample_plot();
        let doc = svg(&p, "Mean (ADU)");

        // Every mark is drawn *whole* inside the frame — its center no closer to
        // an edge than its own radius. The extremes (the first frame, the lowest
        // value) sit exactly on the axes, so without an inset they render as
        // half-circles hanging off the canvas.
        const R: f32 = 4.5;
        for (attr, lo, hi) in [
            ("cx=\"", Y_AXIS_W + R, WIDTH - R),
            ("cy=\"", TITLE_H + R, TITLE_H + AREA_H - R),
        ] {
            let coords: Vec<f32> = doc
                .match_indices(attr)
                .map(|(i, _)| {
                    let rest = &doc[i + attr.len()..];
                    rest[..rest.find('"').unwrap()].parse().unwrap()
                })
                .collect();
            assert_eq!(coords.len(), 3);
            assert!(
                coords.iter().all(|&v| (lo..=hi).contains(&v)),
                "{attr} out of {lo}..{hi}: {coords:?}"
            );
        }

        // Edge tick labels are pulled in rather than hanging off the canvas.
        let xs: Vec<f32> = doc
            .match_indices("<text x=\"")
            .map(|(i, _)| {
                let rest = &doc[i + 9..];
                rest[..rest.find('"').unwrap()].parse().unwrap()
            })
            .collect();
        assert!(xs.iter().all(|&x| (0.0..=WIDTH).contains(&x)));
    }

    #[test]
    fn svg_renders_an_empty_plot_as_a_valid_document() {
        // No frames plotted: still a real SVG with axes and the chart's own
        // wording, not a truncated file.
        let doc = svg(&Plot::default(), "Mean (ADU)");
        assert!(doc.starts_with("<svg"));
        assert!(doc.trim_end().ends_with("</svg>"));
        assert!(doc.contains("No data to plot"));
        assert!(!doc.contains("<circle"));
        assert!(!doc.contains("<polyline"));
    }

    #[test]
    fn svg_escapes_xml_metacharacters_in_labels() {
        let doc = svg(&Plot::default(), "A & B <\"'>");
        assert!(doc.contains(">A &amp; B &lt;&quot;&apos;&gt;</text>"));
        assert!(!doc.contains("A & B <\""));
    }
}
