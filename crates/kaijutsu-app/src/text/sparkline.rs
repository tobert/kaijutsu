//! Sparkline rendering — inline timeseries mini-charts as plain UI geometry.
//!
//! Sparklines are detected from fenced code blocks:
//! ````text
//! ```sparkline
//! 1, 3, 7, 2, 5
//! ```
//! ````
//!
//! `build_sparkline_geometry` is pure: it turns parsed values into a handful
//! of rectangles (a thin rotated rectangle per stroke segment, a small
//! square at each interior joint, and axis-aligned fill bars). The renderer
//! (`view::block_render`) spawns these as plain Bevy UI `Node` +
//! `BackgroundColor` children of the block cell — no vello, no custom
//! shader, just literal rectangle geometry that Bevy's own UI rasterizer
//! draws. `build_sparkline_paths`/`render_to_svg` remain for the golden SVG
//! regression tests below (a `kurbo::BezPath` is just a geometry container
//! here, not something rasterized via vello).

use bevy::prelude::Color;
#[cfg(test)]
use kurbo::{BezPath, Point};

/// Parsed sparkline data from a fenced code block.
#[derive(Clone, Debug)]
pub struct SparklineData {
    pub values: Vec<f64>,
    #[allow(dead_code)] // Phase 2: sparkline labels
    pub label: Option<String>,
}

/// Colors for sparkline rendering.
#[derive(Clone, Debug)]
pub struct SparklineColors {
    pub line: Color,
    pub fill: Option<Color>,
}

/// Mirrors the `sparkline_line_color`/`sparkline_fill_color` fields of
/// `Theme::default()` (`ui::theme::Theme`) — kept in sync by
/// `sparkline_colors_default_matches_theme_default` below. Production always
/// builds `SparklineColors` from the live theme (`view/block_render.rs`);
/// this default only serves tests and standalone use of this module.
impl Default for SparklineColors {
    fn default() -> Self {
        Self {
            line: Color::srgb(0.490, 0.812, 1.00), // #7dcfff Tokyo Night cyan
            fill: Some(Color::srgba(0.490, 0.812, 1.00, 0.15)), // cyan at 15% alpha
        }
    }
}

/// Stroke width for the sparkline line and its joint patches (px).
pub const SPARKLINE_STROKE_WIDTH: f32 = 2.0;

/// An axis-aligned rectangle in the block's local content space (px, origin
/// top-left, y down).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SparklineRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// A stroke segment: a thin rectangle `length` px long and
/// `SPARKLINE_STROKE_WIDTH` px thick, centered on `(cx, cy)` and rotated
/// `angle` radians about that same center.
///
/// `angle` is `dy.atan2(dx)` computed directly in UI (y-down) pixel
/// coordinates — that is exactly what `bevy_ui::UiTransform::from_rotation`
/// (`Rot2::radians`) expects: `UiTransform` rotates a node about its own
/// layout center (bevy_ui's `ui_layout_system` composes
/// `local_center + rotate*scale`, in that order), so centering this
/// rectangle's box at `(cx, cy)` and rotating it by `angle` reproduces the
/// segment from point 1 to point 2 exactly, no sign flip needed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SparklineSegment {
    pub cx: f32,
    pub cy: f32,
    pub length: f32,
    pub angle: f32,
}

/// Plain-geometry sparkline: every field here is spawned as a literal Bevy
/// UI rectangle child by `view::block_render` — no vello, no shader.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SparklineGeometry {
    /// Stroke: one rectangle per `(point[i], point[i+1])` pair.
    pub segments: Vec<SparklineSegment>,
    /// Small squares plugging the visible gap a straight-segment stroke
    /// leaves at each interior direction change (endpoints need none).
    pub joints: Vec<SparklineRect>,
    /// Area-under-curve fill, tiled left-to-right with no gaps or overlaps.
    /// Each bar's top is the midpoint height of the two points it spans — a
    /// cheap, honest approximation of the true linear-interpolation
    /// trapezoid that needs no rotated geometry (a real trapezoid would
    /// need triangles, which is exactly the vello-shaped tool we're
    /// dropping).
    pub fill_bars: Vec<SparklineRect>,
}

/// Try to parse a sparkline from a fenced code block.
///
/// Matches trimmed text of the form:
/// ````text
/// ```sparkline
/// 1, 3, 7, 2, 5
/// ```
/// ````
///
/// Inner data is comma/space/newline-separated f64 values.
pub fn try_parse_sparkline(text: &str) -> Option<SparklineData> {
    let trimmed = text.trim();

    // Match ```sparkline ... ``` fence
    let inner = trimmed.strip_prefix("```sparkline")?;
    let inner = inner.trim_start_matches([' ', '\t']);
    let inner = inner.strip_prefix('\n').unwrap_or(inner);
    let inner = inner.strip_suffix("```")?;
    let inner = inner.trim();

    if inner.is_empty() {
        return None;
    }

    // `is_finite` is load-bearing, not defensive noise: Rust's f64 parser
    // ACCEPTS "NaN", "inf" and "-inf", so a sparkline fence containing any of
    // them — which a model emitting a series with a missing sample will write
    // sooner or later — otherwise reaches the geometry math. It survives the
    // range guard too, because `f64::min`/`max` return the non-NaN operand, so
    // min/max come back finite and `(NaN - min) / range` is still NaN. That
    // NaN lands in a Bevy `Node`, and NaN in the layout phase is an
    // unrecoverable panic — a model could crash the UI by writing a datapoint.
    // Drop non-finite samples at the parse boundary, where "this is not a
    // number we can plot" is still a local, honest judgment.
    let values: Vec<f64> = inner
        .split([',', ' ', '\n', '\t'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .collect();

    if values.is_empty() {
        return None;
    }

    Some(SparklineData {
        values,
        label: None,
    })
}

/// Normalize `data.values` into local content-space `(x, y)` points for
/// `n >= 2` values — `x` evenly spaced with `padding` clearance on each
/// side, `y` inverted (high values map to a smaller `y` — "up" on screen).
///
/// Shared by `build_sparkline_geometry` (rendering) and `build_sparkline_paths`
/// (golden SVG tests) so the two never drift apart. Callers special-case
/// `n == 0` and `n == 1` themselves — see the doc comments on those
/// functions for why a single value can't reuse this normalization (a
/// degenerate `min == max` range).
fn sparkline_points(data: &SparklineData, width: f64, height: f64, padding: f64) -> Vec<(f64, f64)> {
    let n = data.values.len();
    let draw_width = (width - 2.0 * padding).max(1.0);
    let draw_height = (height - 2.0 * padding).max(1.0);

    let min_val = data.values.iter().copied().fold(f64::INFINITY, f64::min);
    let max_val = data
        .values
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    let range = (max_val - min_val).max(f64::EPSILON);

    let x_step = if n > 1 {
        draw_width / (n - 1) as f64
    } else {
        0.0
    };

    (0..n)
        .map(|i| {
            let x = padding + i as f64 * x_step;
            let normalized = (data.values[i] - min_val) / range;
            let y = padding + draw_height * (1.0 - normalized); // invert: high values at top
            (x, y)
        })
        .collect()
}

/// Build plain-rectangle sparkline geometry from data (local content space,
/// origin top-left, y down — matches the block's content box; callers add
/// the block's own `(pad_left, pad_top)` offset when spawning).
pub fn build_sparkline_geometry(
    data: &SparklineData,
    width: f32,
    height: f32,
    padding: f32,
) -> SparklineGeometry {
    let n = data.values.len();
    let mut geometry = SparklineGeometry::default();
    if n == 0 {
        return geometry;
    }

    if n == 1 {
        // Single value: nothing to normalize against (min == max), and
        // nothing to plot a slope for — draw a short flat dash centered in
        // the block, matching the old single-point Vello rendering rather
        // than a meaningless one-pixel mark.
        let draw_width = (width as f64 - 2.0 * padding as f64).max(1.0);
        let dash = 4.0_f64.min(draw_width / 2.0);
        geometry.segments.push(SparklineSegment {
            cx: width * 0.5,
            cy: padding + (height - 2.0 * padding).max(1.0) * 0.5,
            length: (dash * 2.0) as f32,
            angle: 0.0,
        });
        return geometry;
    }

    let points = sparkline_points(data, width as f64, height as f64, padding as f64);
    let bottom = padding as f64 + (height as f64 - 2.0 * padding as f64).max(1.0);

    for i in 0..n - 1 {
        let (x0, y0) = points[i];
        let (x1, y1) = points[i + 1];

        let dx = x1 - x0;
        let dy = y1 - y0;
        let length = (dx * dx + dy * dy).sqrt().max(0.01);
        geometry.segments.push(SparklineSegment {
            cx: ((x0 + x1) * 0.5) as f32,
            cy: ((y0 + y1) * 0.5) as f32,
            length: length as f32,
            angle: dy.atan2(dx) as f32,
        });

        let bar_top = (y0 + y1) * 0.5;
        geometry.fill_bars.push(SparklineRect {
            x: x0.min(x1) as f32,
            y: bar_top as f32,
            width: (x1 - x0).abs() as f32,
            height: (bottom - bar_top).max(0.0) as f32,
        });
    }

    for &(x, y) in &points[1..n - 1] {
        let half = SPARKLINE_STROKE_WIDTH as f64 * 0.5;
        geometry.joints.push(SparklineRect {
            x: (x - half) as f32,
            y: (y - half) as f32,
            width: SPARKLINE_STROKE_WIDTH,
            height: SPARKLINE_STROKE_WIDTH,
        });
    }

    geometry
}

/// Computed paths ready for SVG export (golden regression tests only — see
/// module docs; production rendering uses `build_sparkline_geometry`).
#[cfg(test)]
#[derive(Clone, Debug)]
pub struct SparklinePaths {
    pub line: BezPath,
    pub fill: Option<BezPath>,
}

/// Build a `kurbo::BezPath` pair (line + fill) from sparkline data, for the
/// golden SVG regression tests. Not used by the live renderer — see module
/// docs. Dock chrome (`ui::dock`) was the last production caller before it
/// moved onto `build_sparkline_geometry` (retiring vello, docs/issues.md,
/// 2026-08-12); test-only since.
#[cfg(test)]
pub fn build_sparkline_paths(
    data: &SparklineData,
    width: f64,
    height: f64,
    padding: f64,
) -> SparklinePaths {
    let n = data.values.len();

    if n == 0 {
        return SparklinePaths {
            line: BezPath::new(),
            fill: None,
        };
    }

    let draw_width = (width - 2.0 * padding).max(1.0);
    let draw_height = (height - 2.0 * padding).max(1.0);

    // Build line path
    let mut line = BezPath::new();
    if n == 1 {
        // Single value: draw a small horizontal dash (see the identical
        // special case in `build_sparkline_geometry`).
        let y = padding + draw_height * 0.5;
        let cx = width / 2.0;
        let dash = 4.0_f64.min(draw_width / 2.0);
        line.move_to(Point::new(cx - dash, y));
        line.line_to(Point::new(cx + dash, y));
    } else {
        let points = sparkline_points(data, width, height, padding);
        line.move_to(Point::new(points[0].0, points[0].1));
        for &(x, y) in &points[1..] {
            line.line_to(Point::new(x, y));
        }
    }

    // Build fill path (close along the bottom edge)
    let fill = if n >= 2 {
        let points = sparkline_points(data, width, height, padding);
        let mut fill_path = line.clone();
        let (last_x, _) = points[n - 1];
        let bottom = padding + draw_height;
        fill_path.line_to(Point::new(last_x, bottom));
        fill_path.line_to(Point::new(padding, bottom));
        fill_path.close_path();
        Some(fill_path)
    } else {
        None
    };

    SparklinePaths { line, fill }
}

/// Render sparkline paths to a minimal SVG document string.
///
/// Used for golden tests — produces a deterministic SVG.
#[cfg(test)]
pub fn render_to_svg(
    paths: &SparklinePaths,
    width: f64,
    height: f64,
    colors: &SparklineColors,
) -> String {
    let line_color = color_to_css(colors.line);
    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">"#,
    );
    svg.push('\n');

    // Fill area
    if let (Some(fill_path), Some(fill_color)) = (&paths.fill, &colors.fill) {
        let fill_css = color_to_css(*fill_color);
        let fill_alpha = fill_color.to_srgba().alpha;
        svg.push_str(&format!(
            r#"  <path d="{}" fill="{fill_css}" fill-opacity="{fill_alpha:.2}" stroke="none"/>"#,
            fill_path.to_svg()
        ));
        svg.push('\n');
    }

    // Stroke line
    svg.push_str(&format!(
        r#"  <path d="{}" fill="none" stroke="{line_color}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        paths.line.to_svg()
    ));
    svg.push('\n');

    svg.push_str("</svg>\n");
    svg
}

#[cfg(test)]
fn color_to_css(color: Color) -> String {
    let srgba = color.to_srgba();
    let r = (srgba.red * 255.0).round() as u8;
    let g = (srgba.green * 255.0).round() as u8;
    let b = (srgba.blue * 255.0).round() as u8;
    format!("#{r:02x}{g:02x}{b:02x}")
}

// =============================================================================
// GOLDEN TEST INFRASTRUCTURE
// =============================================================================

#[cfg(test)]
fn golden_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
}

#[cfg(test)]
fn assert_golden(name: &str, actual: &str) {
    let path = golden_dir().join(format!("{name}.svg"));

    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all(golden_dir()).expect("create golden dir");
        std::fs::write(&path, actual).expect("write golden file");
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("Golden file not found: {path:?}\nRun with UPDATE_GOLDEN=1 to generate")
    });
    assert_eq!(
        actual, expected,
        "Golden mismatch for {name}. Run with UPDATE_GOLDEN=1 to update."
    );
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Parsing tests --

    #[test]
    fn parse_basic_sparkline() {
        let input = "```sparkline\n1, 3, 7, 2, 5\n```";
        let data = try_parse_sparkline(input).expect("should parse");
        assert_eq!(data.values, vec![1.0, 3.0, 7.0, 2.0, 5.0]);
        assert!(data.label.is_none());
    }

    #[test]
    fn parse_sparkline_spaces() {
        let input = "```sparkline\n1 3 7 2 5\n```";
        let data = try_parse_sparkline(input).expect("should parse");
        assert_eq!(data.values, vec![1.0, 3.0, 7.0, 2.0, 5.0]);
    }

    #[test]
    fn parse_sparkline_multiline() {
        let input = "```sparkline\n1, 3\n7, 2, 5\n```";
        let data = try_parse_sparkline(input).expect("should parse");
        assert_eq!(data.values, vec![1.0, 3.0, 7.0, 2.0, 5.0]);
    }

    /// Rust's f64 parser ACCEPTS "NaN"/"inf"/"-inf", so without an explicit
    /// finite filter these reach the geometry math — and they survive the
    /// range guard, because `f64::min`/`max` return the non-NaN operand, so
    /// min/max come back finite and `(NaN - min) / range` is still NaN. A NaN
    /// in a Bevy `Node` panics the layout phase, which means a model writing a
    /// missing sample as `NaN` could crash the UI. Drop them at parse.
    #[test]
    fn parse_sparkline_drops_non_finite_samples() {
        let input = "```sparkline\n1, NaN, 3, inf, -inf, 5\n```";
        let data = try_parse_sparkline(input).expect("should parse");
        assert_eq!(
            data.values,
            vec![1.0, 3.0, 5.0],
            "NaN/inf must never reach the geometry math"
        );
        assert!(
            data.values.iter().all(|v| v.is_finite()),
            "every retained sample must be finite"
        );
    }

    /// A fence of NOTHING but non-finite samples must parse as absent, not as
    /// an empty sparkline that then divides by a degenerate range.
    #[test]
    fn parse_sparkline_all_non_finite_is_none() {
        assert!(try_parse_sparkline("```sparkline\nNaN, inf\n```").is_none());
    }

    /// The geometry math itself must not produce non-finite coordinates for
    /// any finite input, including the degenerate all-equal case where the
    /// value range collapses to zero.
    #[test]
    fn sparkline_geometry_is_finite_for_degenerate_inputs() {
        for values in [vec![5.0, 5.0, 5.0], vec![0.0], vec![-3.0, -3.0]] {
            let data = SparklineData {
                values,
                label: None,
            };
            let geom = build_sparkline_geometry(&data, 120.0, 24.0, 4.0);
            for seg in &geom.segments {
                assert!(
                    seg.cx.is_finite()
                        && seg.cy.is_finite()
                        && seg.length.is_finite()
                        && seg.angle.is_finite(),
                    "degenerate input produced a non-finite segment: {seg:?}"
                );
            }
        }
    }

    #[test]
    fn parse_sparkline_with_whitespace() {
        let input = "  ```sparkline\n  1, 3, 7  \n  ```  ";
        let data = try_parse_sparkline(input).expect("should parse");
        assert_eq!(data.values, vec![1.0, 3.0, 7.0]);
    }

    #[test]
    fn parse_sparkline_floats() {
        let input = "```sparkline\n1.5, 3.5, 7.0\n```";
        let data = try_parse_sparkline(input).expect("should parse");
        assert_eq!(data.values, vec![1.5, 3.5, 7.0]);
    }

    #[test]
    fn parse_sparkline_single() {
        let input = "```sparkline\n42\n```";
        let data = try_parse_sparkline(input).expect("should parse");
        assert_eq!(data.values, vec![42.0]);
    }

    #[test]
    fn parse_not_sparkline() {
        assert!(try_parse_sparkline("```rust\nfn main() {}\n```").is_none());
        assert!(try_parse_sparkline("hello world").is_none());
        assert!(try_parse_sparkline("```sparkline\n```").is_none());
        assert!(try_parse_sparkline("```sparkline\nabc\n```").is_none());
    }

    // -- BezPath geometry tests (golden-test support) --

    #[test]
    fn build_paths_basic() {
        let data = SparklineData {
            values: vec![1.0, 3.0, 7.0, 2.0, 5.0],
            label: None,
        };
        let paths = build_sparkline_paths(&data, 200.0, 48.0, 4.0);
        assert!(!paths.line.elements().is_empty());
        assert!(paths.fill.is_some());
    }

    #[test]
    fn build_paths_single_value() {
        let data = SparklineData {
            values: vec![42.0],
            label: None,
        };
        let paths = build_sparkline_paths(&data, 200.0, 48.0, 4.0);
        assert!(!paths.line.elements().is_empty());
        assert!(paths.fill.is_none()); // single value has no fill area
    }

    #[test]
    fn build_paths_empty() {
        let data = SparklineData {
            values: vec![],
            label: None,
        };
        let paths = build_sparkline_paths(&data, 200.0, 48.0, 4.0);
        assert!(paths.line.elements().is_empty());
    }

    #[test]
    fn build_paths_flat_values() {
        let data = SparklineData {
            values: vec![5.0, 5.0, 5.0, 5.0],
            label: None,
        };
        let paths = build_sparkline_paths(&data, 200.0, 48.0, 4.0);
        assert!(!paths.line.elements().is_empty());
    }

    // -- Golden SVG tests --

    #[test]
    fn golden_basic_sparkline() {
        let data = SparklineData {
            values: vec![1.0, 3.0, 7.0, 2.0, 5.0],
            label: None,
        };
        let paths = build_sparkline_paths(&data, 200.0, 48.0, 4.0);
        let svg = render_to_svg(&paths, 200.0, 48.0, &SparklineColors::default());
        assert_golden("sparkline_basic", &svg);
    }

    #[test]
    fn golden_single_sparkline() {
        let data = SparklineData {
            values: vec![42.0],
            label: None,
        };
        let paths = build_sparkline_paths(&data, 200.0, 48.0, 4.0);
        let svg = render_to_svg(&paths, 200.0, 48.0, &SparklineColors::default());
        assert_golden("sparkline_single", &svg);
    }

    #[test]
    fn golden_flat_sparkline() {
        let data = SparklineData {
            values: vec![5.0, 5.0, 5.0, 5.0],
            label: None,
        };
        let paths = build_sparkline_paths(&data, 200.0, 48.0, 4.0);
        let svg = render_to_svg(&paths, 200.0, 48.0, &SparklineColors::default());
        assert_golden("sparkline_flat", &svg);
    }

    /// `SparklineColors::default()` mirrors `Theme::default()`'s sparkline_*
    /// fields (see `view/block_render.rs` for the production construction
    /// path). If this fails, one side drifted — update whichever is stale.
    #[test]
    fn sparkline_colors_default_matches_theme_default() {
        let colors = SparklineColors::default();
        let theme = crate::ui::theme::Theme::default();
        assert_eq!(colors.line, theme.sparkline_line_color);
        assert_eq!(colors.fill, theme.sparkline_fill_color);
    }

    // -- build_sparkline_geometry: point mapping / rectangle math --

    #[test]
    fn geometry_empty_data_yields_nothing() {
        let data = SparklineData { values: vec![], label: None };
        let geometry = build_sparkline_geometry(&data, 200.0, 48.0, 4.0);
        assert!(geometry.segments.is_empty());
        assert!(geometry.fill_bars.is_empty());
        assert!(geometry.joints.is_empty());
    }

    #[test]
    fn geometry_single_value_is_one_centered_dash_no_fill() {
        let data = SparklineData { values: vec![42.0], label: None };
        let geometry = build_sparkline_geometry(&data, 200.0, 48.0, 4.0);
        assert_eq!(geometry.segments.len(), 1);
        assert!(geometry.fill_bars.is_empty());
        assert!(geometry.joints.is_empty());
        let seg = geometry.segments[0];
        assert_eq!(seg.cx, 100.0); // width / 2
        assert_eq!(seg.angle, 0.0); // flat dash
    }

    #[test]
    fn geometry_segment_count_is_one_less_than_points() {
        let data = SparklineData {
            values: vec![1.0, 3.0, 7.0, 2.0, 5.0],
            label: None,
        };
        let geometry = build_sparkline_geometry(&data, 200.0, 48.0, 4.0);
        assert_eq!(geometry.segments.len(), data.values.len() - 1);
        assert_eq!(geometry.fill_bars.len(), data.values.len() - 1);
        // Interior joints only — first/last point excluded.
        assert_eq!(geometry.joints.len(), data.values.len() - 2);
    }

    #[test]
    fn geometry_rising_value_produces_upward_angle() {
        // y is inverted (high values map to small y), so a rising value
        // (data increasing) means the segment's y decreases left-to-right —
        // a negative angle in UI (y-down) atan2 convention.
        let data = SparklineData { values: vec![1.0, 10.0], label: None };
        let geometry = build_sparkline_geometry(&data, 200.0, 48.0, 4.0);
        assert_eq!(geometry.segments.len(), 1);
        assert!(geometry.segments[0].angle < 0.0);
    }

    #[test]
    fn geometry_falling_value_produces_downward_angle() {
        let data = SparklineData { values: vec![10.0, 1.0], label: None };
        let geometry = build_sparkline_geometry(&data, 200.0, 48.0, 4.0);
        assert_eq!(geometry.segments.len(), 1);
        assert!(geometry.segments[0].angle > 0.0);
    }

    #[test]
    fn geometry_flat_values_produce_zero_angle_segments() {
        let data = SparklineData {
            values: vec![5.0, 5.0, 5.0],
            label: None,
        };
        let geometry = build_sparkline_geometry(&data, 200.0, 48.0, 4.0);
        for seg in &geometry.segments {
            assert_eq!(seg.angle, 0.0);
        }
    }

    #[test]
    fn geometry_segment_endpoints_recover_via_rotation() {
        // The core placement contract `spawn_segment_child` relies on:
        // rotating a unit vector along local +X by `angle` and scaling by
        // `length / 2`, then adding the segment's center, must land exactly
        // on the original two data points. This is the same math
        // `bevy_ui::UiTransform`'s `Rot2` applies (see the doc comment on
        // `SparklineSegment::angle`) — verified directly against
        // `bevy_math::Rot2` here, without spinning up a Bevy App/World.
        use bevy::math::{Rot2, Vec2};

        let data = SparklineData {
            values: vec![2.0, 9.0, 3.0],
            label: None,
        };
        let width = 200.0_f32;
        let height = 48.0_f32;
        let padding = 4.0_f32;
        let geometry = build_sparkline_geometry(&data, width, height, padding);

        let points = sparkline_points(&data, width as f64, height as f64, padding as f64);

        for (i, seg) in geometry.segments.iter().enumerate() {
            let center = Vec2::new(seg.cx, seg.cy);
            let half = Rot2::radians(seg.angle) * Vec2::new(seg.length / 2.0, 0.0);

            let p0 = Vec2::new(points[i].0 as f32, points[i].1 as f32);
            let p1 = Vec2::new(points[i + 1].0 as f32, points[i + 1].1 as f32);

            assert!(
                (center - half - p0).length() < 1e-2,
                "segment {i}: center-half {:?} != p0 {:?}",
                center - half,
                p0
            );
            assert!(
                (center + half - p1).length() < 1e-2,
                "segment {i}: center+half {:?} != p1 {:?}",
                center + half,
                p1
            );
        }
    }

    #[test]
    fn geometry_fill_bars_tile_without_gaps_or_overlaps() {
        let data = SparklineData {
            values: vec![1.0, 3.0, 7.0, 2.0, 5.0],
            label: None,
        };
        let geometry = build_sparkline_geometry(&data, 200.0, 48.0, 4.0);
        for pair in geometry.fill_bars.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            assert!(
                (a.x + a.width - b.x).abs() < 1e-4,
                "gap/overlap between fill bars: {a:?} then {b:?}"
            );
        }
    }

    #[test]
    fn geometry_fill_bars_never_go_negative_height() {
        let data = SparklineData {
            values: vec![1.0, 100.0, 1.0],
            label: None,
        };
        let geometry = build_sparkline_geometry(&data, 200.0, 48.0, 4.0);
        for bar in &geometry.fill_bars {
            assert!(bar.height >= 0.0);
        }
    }
}
