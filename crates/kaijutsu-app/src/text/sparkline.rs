//! Sparkline rendering — inline timeseries mini-charts via Vello vector paths.
//!
//! Sparklines are detected from fenced code blocks:
//! ````text
//! ```sparkline
//! 1, 3, 7, 2, 5
//! ```
//! ````
//!
//! Rendering is pure Vello `BezPath` — no text layout involved.

use bevy::prelude::Color;
use bevy_vello::vello;
use vello::kurbo::{BezPath, Cap, Join, Point, Stroke};
use vello::peniko::{Brush, Fill};

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

impl Default for SparklineColors {
    fn default() -> Self {
        Self {
            line: Color::srgb(0.490, 0.812, 1.00), // #7dcfff Tokyo Night cyan
            fill: Some(Color::srgba(0.490, 0.812, 1.00, 0.15)), // cyan at 15% alpha
        }
    }
}

/// Computed paths ready for rendering or SVG export.
#[derive(Clone, Debug)]
pub struct SparklinePaths {
    pub line: BezPath,
    pub fill: Option<BezPath>,
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

    let values: Vec<f64> = inner
        .split([',', ' ', '\n', '\t'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<f64>().ok())
        .collect();

    if values.is_empty() {
        return None;
    }

    Some(SparklineData {
        values,
        label: None,
    })
}

/// Build sparkline geometry from data.
///
/// Pure function — returns line path + optional fill path.
/// Coordinates: x evenly spaced with padding, y inverted (high values at top).
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

    let point_at = |i: usize| -> Point {
        let x = padding + i as f64 * x_step;
        let normalized = (data.values[i] - min_val) / range;
        let y = padding + draw_height * (1.0 - normalized); // invert: high values at top
        Point::new(x, y)
    };

    // Build line path
    let mut line = BezPath::new();
    if n == 1 {
        // Single value: draw a small horizontal dash
        let y = padding + draw_height * 0.5;
        let cx = width / 2.0;
        let dash = 4.0_f64.min(draw_width / 2.0);
        line.move_to(Point::new(cx - dash, y));
        line.line_to(Point::new(cx + dash, y));
    } else {
        let p0 = point_at(0);
        line.move_to(p0);
        for i in 1..n {
            line.line_to(point_at(i));
        }
    }

    // Build fill path (close along the bottom edge)
    let fill = if n >= 2 {
        let mut fill_path = line.clone();
        let last_x = padding + (n - 1) as f64 * x_step;
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

/// Render sparkline paths into a Vello scene.
pub fn render_sparkline_scene(
    scene: &mut vello::Scene,
    paths: &SparklinePaths,
    colors: &SparklineColors,
) {
    let line_brush = bevy_color_to_brush(colors.line);
    let stroke = Stroke {
        width: 2.0,
        join: Join::Round,
        start_cap: Cap::Round,
        end_cap: Cap::Round,
        ..Default::default()
    };

    // Fill area under the curve
    if let (Some(fill_path), Some(fill_color)) = (&paths.fill, &colors.fill) {
        let fill_brush = bevy_color_to_brush(*fill_color);
        scene.fill(
            Fill::NonZero,
            vello::kurbo::Affine::IDENTITY,
            &fill_brush,
            None,
            fill_path,
        );
    }

    // Stroke the line on top
    scene.stroke(
        &stroke,
        vello::kurbo::Affine::IDENTITY,
        &line_brush,
        None,
        &paths.line,
    );
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

fn bevy_color_to_brush(color: Color) -> Brush {
    let srgba = color.to_srgba();
    Brush::Solid(vello::peniko::Color::from_rgba8(
        (srgba.red * 255.0) as u8,
        (srgba.green * 255.0) as u8,
        (srgba.blue * 255.0) as u8,
        (srgba.alpha * 255.0) as u8,
    ))
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

    #[test]
    fn parse_sparkline_with_whitespace() {
        let input = "  ```sparkline\n  1, 3, 7  \n  ```  ";
        let data = try_parse_sparkline(input).expect("should parse");
        assert_eq!(data.values, vec![1.0, 3.0, 7.0]);
    }

    #[test]
    fn parse_sparkline_floats() {
        let input = "```sparkline\n1.5, 3.14, 7.0\n```";
        let data = try_parse_sparkline(input).expect("should parse");
        assert_eq!(data.values, vec![1.5, 3.14, 7.0]);
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

    // -- Geometry tests --

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
}
