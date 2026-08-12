//! Sparkline rendering — inline timeseries mini-charts as plain UI geometry.
//!
//! Sparklines are detected from fenced code blocks:
//! ````text
//! ```sparkline
//! 1, 3, 7, 2, 5
//! ```
//! ````
//!
//! `build_sparkline_vertices` is pure: it turns parsed values into
//! flat-colored triangles (fill trapezoids, stroke quads, joint squares) for
//! the MSDF pass's geometry lane (`MsdfBlockGeometry`) — both the block-cell
//! `Sparkline` arm (`view::block_render`) and the North dock's HUD
//! sparklines (`ui::dock`) draw them straight into the texture their surface
//! already renders. A UI-node-children rendering (rotated rectangle per
//! stroke segment) preceded it and is retired; `build_sparkline_paths`/
//! `render_to_svg` remain for the golden SVG regression tests below (a
//! `kurbo::BezPath` is just a geometry container here, not something
//! rasterized via vello).

use bevy::prelude::Color;
#[cfg(test)]
use kurbo::{BezPath, Point};

use crate::text::msdf::geometry::{GeometryVertex, rect_quad, stroke_line_quad};

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
/// Shared by `build_sparkline_vertices` (rendering) and `build_sparkline_paths`
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

/// Build flat-colored triangle-list vertices for a sparkline, for surfaces
/// that render through the MSDF pass's geometry lane (`MsdfBlockGeometry`)
/// instead of spawning UI-node children. The North dock draws its two HUD
/// sparklines this way, directly into the texture it already renders: the
/// child-respawn path despawned/respawned ~230 `Node` entities per data tick
/// *after* `UiSystems::Layout` had run, so every rebuild rendered one frame
/// of never-laid-out (zero-size) children — the 4Hz HUD flicker.
///
/// Same visual contract as the retired UI-node-children builder with one
/// deliberate upgrade: the fill under each sample pair is the true
/// linear-interpolation trapezoid (a convex quad — two triangles — trivial
/// in a triangle lane), not the bar-tiled midpoint approximation that
/// axis-aligned rectangle children forced.
///
/// Coordinates are block-local LOGICAL pixels (`GeometryVertex`'s contract —
/// the render-world builder converts to physical NDC); `offset` shifts the
/// whole sparkline. Colors are straight-alpha RGBA8 (`text::color_to_rgba8`),
/// premultiplied later in the geometry fragment shader.
///
/// Emission order is fill trapezoids, then stroke segments, then joint
/// patches — the geometry pass draws in order, so the stroke composites over
/// the fill.
pub fn build_sparkline_vertices(
    data: &SparklineData,
    width: f32,
    height: f32,
    padding: f32,
    offset: (f32, f32),
    line_color: [u8; 4],
    fill_color: Option<[u8; 4]>,
) -> Vec<GeometryVertex> {
    let n = data.values.len();
    let mut vertices = Vec::new();
    if n == 0 {
        return vertices;
    }

    let (ox, oy) = (offset.0 as f64, offset.1 as f64);
    let stroke = SPARKLINE_STROKE_WIDTH as f64;

    if n == 1 {
        // Single value: nothing to normalize against (min == max), and
        // nothing to plot a slope for — a short flat dash centered in the
        // block, matching the old single-point Vello rendering rather than
        // a meaningless one-pixel mark. No fill — one sample spans no area.
        let draw_width = (width as f64 - 2.0 * padding as f64).max(1.0);
        let dash = 4.0_f64.min(draw_width / 2.0);
        let cx = ox + width as f64 * 0.5;
        let cy = oy + padding as f64 + (height as f64 - 2.0 * padding as f64).max(1.0) * 0.5;
        vertices.extend(stroke_line_quad(cx - dash, cy, cx + dash, cy, stroke, line_color));
        return vertices;
    }

    let points = sparkline_points(data, width as f64, height as f64, padding as f64);
    let bottom = oy + padding as f64 + (height as f64 - 2.0 * padding as f64).max(1.0);

    if let Some(fill) = fill_color {
        for pair in points.windows(2) {
            let (x0, y0) = (ox + pair[0].0, oy + pair[0].1);
            let (x1, y1) = (ox + pair[1].0, oy + pair[1].1);
            let p0 = GeometryVertex { x: x0 as f32, y: y0 as f32, color: fill };
            let p1 = GeometryVertex { x: x1 as f32, y: y1 as f32, color: fill };
            let b1 = GeometryVertex { x: x1 as f32, y: bottom.max(y1) as f32, color: fill };
            let b0 = GeometryVertex { x: x0 as f32, y: bottom.max(y0) as f32, color: fill };
            vertices.extend([p0, p1, b1, p0, b1, b0]);
        }
    }

    for pair in points.windows(2) {
        let (x0, y0) = (ox + pair[0].0, oy + pair[0].1);
        let (x1, y1) = (ox + pair[1].0, oy + pair[1].1);
        vertices.extend(stroke_line_quad(x0, y0, x1, y1, stroke, line_color));
    }

    // Small squares plugging the notch butt-capped segments leave at each
    // interior direction change (endpoints need none).
    for &(x, y) in &points[1..n - 1] {
        let half = stroke * 0.5;
        vertices.extend(rect_quad(
            ox + x - half,
            oy + y - half,
            stroke,
            stroke,
            line_color,
        ));
    }

    vertices
}

/// Computed paths ready for SVG export (golden regression tests only — see
/// module docs; production rendering uses `build_sparkline_vertices`).
#[cfg(test)]
#[derive(Clone, Debug)]
pub struct SparklinePaths {
    pub line: BezPath,
    pub fill: Option<BezPath>,
}

/// Build a `kurbo::BezPath` pair (line + fill) from sparkline data, for the
/// golden SVG regression tests. Not used by the live renderer — see module
/// docs; test-only since the vello retirement (2026-08-12).
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
        // special case in `build_sparkline_vertices`).
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

    // -- build_sparkline_vertices: triangle-lane sparkline --

    const LINE: [u8; 4] = [10, 20, 30, 255];
    const FILL: [u8; 4] = [10, 20, 30, 38];

    #[test]
    fn vertices_empty_data_yields_nothing() {
        let data = SparklineData { values: vec![], label: None };
        let v = build_sparkline_vertices(&data, 80.0, 20.0, 2.0, (0.0, 0.0), LINE, Some(FILL));
        assert!(v.is_empty());
    }

    #[test]
    fn vertices_single_value_is_one_dash_quad_no_fill() {
        let data = SparklineData { values: vec![42.0], label: None };
        let v = build_sparkline_vertices(&data, 80.0, 20.0, 2.0, (0.0, 0.0), LINE, Some(FILL));
        assert_eq!(v.len(), 6, "one quad = two triangles");
        assert!(v.iter().all(|vx| vx.color == LINE), "a lone sample has no fill area");
        // Dash is horizontal and centered: endpoints recover cx ± dash.
        let cx = (v[0].x + v[4].x) / 2.0;
        assert!((cx - 40.0).abs() < 1e-3, "dash centered at width/2, got {cx}");
    }

    /// n samples: (n-1) fill trapezoids + (n-1) stroke quads + (n-2) joint
    /// squares, 6 vertices each; dropping the fill color drops exactly the
    /// fill lane.
    #[test]
    fn vertices_counts_per_lane() {
        let data = SparklineData {
            values: vec![1.0, 3.0, 7.0, 2.0, 5.0],
            label: None,
        };
        let with_fill =
            build_sparkline_vertices(&data, 80.0, 20.0, 2.0, (0.0, 0.0), LINE, Some(FILL));
        assert_eq!(with_fill.len(), 6 * (4 + 4 + 3));
        assert_eq!(with_fill.iter().filter(|v| v.color == FILL).count(), 6 * 4);
        assert_eq!(with_fill.iter().filter(|v| v.color == LINE).count(), 6 * (4 + 3));

        let no_fill = build_sparkline_vertices(&data, 80.0, 20.0, 2.0, (0.0, 0.0), LINE, None);
        assert_eq!(no_fill.len(), 6 * (4 + 3));
        assert!(no_fill.iter().all(|v| v.color == LINE));
    }

    /// The stroke lane must land exactly on the shared normalized points:
    /// `stroke_line_quad` emits `[a, b, d, b, c, d]` where `(a+d)/2` is the
    /// segment start and `(b+c)/2` its end.
    #[test]
    fn vertices_stroke_quads_recover_data_points() {
        let data = SparklineData {
            values: vec![2.0, 9.0, 3.0],
            label: None,
        };
        let (w, h, pad) = (80.0_f32, 20.0_f32, 2.0_f32);
        let v = build_sparkline_vertices(&data, w, h, pad, (0.0, 0.0), LINE, None);
        let points = sparkline_points(&data, w as f64, h as f64, pad as f64);

        for i in 0..data.values.len() - 1 {
            let quad = &v[i * 6..i * 6 + 6];
            let start = (
                (quad[0].x + quad[2].x) / 2.0,
                (quad[0].y + quad[2].y) / 2.0,
            );
            let end = (
                (quad[1].x + quad[4].x) / 2.0,
                (quad[1].y + quad[4].y) / 2.0,
            );
            assert!((start.0 - points[i].0 as f32).abs() < 1e-2, "segment {i} start x");
            assert!((start.1 - points[i].1 as f32).abs() < 1e-2, "segment {i} start y");
            assert!((end.0 - points[i + 1].0 as f32).abs() < 1e-2, "segment {i} end x");
            assert!((end.1 - points[i + 1].1 as f32).abs() < 1e-2, "segment {i} end y");
        }
    }

    /// Fill trapezoids hang the true interpolated top edge (the data points
    /// themselves, not the bar-tiled midpoint) down to the baseline, and tile
    /// left-to-right without gaps.
    #[test]
    fn vertices_fill_trapezoids_span_points_to_baseline() {
        let data = SparklineData {
            values: vec![1.0, 3.0, 7.0, 2.0, 5.0],
            label: None,
        };
        let (w, h, pad) = (80.0_f32, 20.0_f32, 2.0_f32);
        let v = build_sparkline_vertices(&data, w, h, pad, (0.0, 0.0), LINE, Some(FILL));
        let points = sparkline_points(&data, w as f64, h as f64, pad as f64);
        let bottom = pad + (h - 2.0 * pad).max(1.0);

        for i in 0..data.values.len() - 1 {
            // Emission order per trapezoid: [p0, p1, b1, p0, b1, b0].
            let quad = &v[i * 6..i * 6 + 6];
            assert!((quad[0].x - points[i].0 as f32).abs() < 1e-3);
            assert!((quad[0].y - points[i].1 as f32).abs() < 1e-3, "top edge is the data point");
            assert!((quad[1].x - points[i + 1].0 as f32).abs() < 1e-3);
            assert!((quad[1].y - points[i + 1].1 as f32).abs() < 1e-3);
            assert!((quad[2].y - bottom).abs() < 1e-3, "hangs to the baseline");
            assert!((quad[5].y - bottom).abs() < 1e-3);
            assert!((quad[5].x - quad[0].x).abs() < 1e-3, "b0 under p0");
        }
    }

    #[test]
    fn vertices_offset_shifts_everything() {
        let data = SparklineData {
            values: vec![1.0, 3.0, 7.0, 2.0],
            label: None,
        };
        let base = build_sparkline_vertices(&data, 80.0, 20.0, 2.0, (0.0, 0.0), LINE, Some(FILL));
        let moved =
            build_sparkline_vertices(&data, 80.0, 20.0, 2.0, (100.0, 8.0), LINE, Some(FILL));
        assert_eq!(base.len(), moved.len());
        for (b, m) in base.iter().zip(&moved) {
            assert!((m.x - b.x - 100.0).abs() < 1e-3);
            assert!((m.y - b.y - 8.0).abs() < 1e-3);
            assert_eq!(b.color, m.color);
        }
    }

    /// Degenerate inputs (flat series, single sample, negative flats) must
    /// produce finite coordinates — mirrors
    /// `sparkline_geometry_is_finite_for_degenerate_inputs` for this lane.
    #[test]
    fn vertices_finite_for_degenerate_inputs() {
        for values in [vec![5.0, 5.0, 5.0], vec![0.0], vec![-3.0, -3.0]] {
            let data = SparklineData { values, label: None };
            let v = build_sparkline_vertices(&data, 80.0, 20.0, 2.0, (0.0, 0.0), LINE, Some(FILL));
            for vx in &v {
                assert!(
                    vx.x.is_finite() && vx.y.is_finite(),
                    "non-finite vertex from degenerate input: {vx:?}"
                );
            }
        }
    }

}
