//! Fieldset/legend border drawing via Vello scenes.
//!
//! Pure functions that build `vello::Scene` objects for:
//! - **Fieldset borders**: Rounded rect with top label gap (kind, tool name)
//!   and optional bottom label (status).
//! - **Role group lines**: Horizontal divider with inset role label.
//! - **Animation overlays**: Chase, pulse, breathe effects.
//!
//! These replace the shader-based `BlockBorderMaterial` and text-based
//! `RoleHeader` with a single unified Vello vector path.

use bevy::prelude::*;
use bevy_vello::vello;
use vello::kurbo::{Affine, Arc, BezPath, Line, Point, RoundedRect, Shape, Stroke, Cap};
use vello::peniko::{Brush, Fill};

use super::block_border::{BlockBorderStyle, BorderAnimation, BorderKind};
use crate::text::components::bevy_color_to_brush;

// ============================================================================
// CONSTANTS
// ============================================================================

/// Horizontal inset from left edge where the label starts.
const LABEL_INSET: f64 = 12.0;

/// Horizontal padding around label text within the gap.
const LABEL_PAD: f64 = 6.0;

/// Font size for fieldset labels (tool name, status).
const FIELDSET_LABEL_FONT_SIZE: f32 = 11.0;

/// Font size for role group line labels.
#[allow(dead_code)] // Used by measure_role_label_width
const ROLE_LABEL_FONT_SIZE: f32 = 12.0;

// ============================================================================
// FIELDSET BORDER (per-block)
// ============================================================================

/// Build a fieldset border scene for a block.
///
/// Draws a rounded rect with gaps for top/bottom labels. The border is
/// constructed from individual path segments rather than a single stroked
/// RoundedRect, allowing precise label gap placement.
pub fn build_fieldset_border(
    scene: &mut vello::Scene,
    width: f64,
    height: f64,
    style: &BlockBorderStyle,
    top_label: Option<&str>,
    bottom_label: Option<&str>,
    font: Option<&bevy_vello::prelude::VelloFont>,
    time: f32,
) {
    let brush = bevy_color_to_brush(style.color);
    let alpha = animation_alpha(&style.animation, time);
    let brush = apply_alpha(&brush, alpha);

    let stroke = Stroke::new(style.thickness as f64)
        .with_caps(Cap::Butt);
    let r = style.corner_radius as f64;

    match style.kind {
        BorderKind::Full => {
            // Measure label widths for gap calculation
            let top_gap = top_label.map(|l| measure_label_width(l, font));
            let bottom_gap = bottom_label.map(|l| measure_label_width(l, font));

            draw_fieldset_rect(scene, width, height, r, &stroke, &brush, top_gap, bottom_gap);

            // Draw label text
            if let (Some(label), Some(font)) = (top_label, font) {
                draw_label_text(scene, label, LABEL_INSET + LABEL_PAD, 0.0, font, &brush);
            }
            if let (Some(label), Some(font)) = (bottom_label, font) {
                draw_label_text(scene, label, LABEL_INSET + LABEL_PAD, height, font, &brush);
            }
        }
        BorderKind::TopAccent => {
            // Just the top line with optional label gap
            let top_gap = top_label.map(|l| measure_label_width(l, font));
            draw_top_accent(scene, width, &stroke, &brush, top_gap);

            if let (Some(label), Some(font)) = (top_label, font) {
                draw_label_text(scene, label, LABEL_INSET + LABEL_PAD, 0.0, font, &brush);
            }
        }
        BorderKind::Dashed => {
            let dashed_stroke = Stroke::new(style.thickness as f64)
                .with_caps(Cap::Butt)
                .with_dashes(0.0, &[6.0, 4.0]);

            let rect = RoundedRect::new(
                style.thickness as f64 / 2.0,
                style.thickness as f64 / 2.0,
                width - style.thickness as f64 / 2.0,
                height - style.thickness as f64 / 2.0,
                r,
            );
            scene.stroke(&dashed_stroke, Affine::IDENTITY, &brush, None, &rect);
        }
    }

    // Animation overlays
    match style.animation {
        BorderAnimation::Chase => {
            chase_overlay(scene, width, height, r, style.thickness as f64, time, &style.color);
        }
        _ => {}
    }
}

// ============================================================================
// ROLE GROUP LINE
// ============================================================================

/// Build a horizontal divider line with an inset role label.
///
/// Replaces the text-based "── USER ──────────" role headers with a
/// clean vector line + label.
pub fn build_role_group_line(
    scene: &mut vello::Scene,
    width: f64,
    height: f64,
    role_label: &str,
    color: Color,
    font: Option<&bevy_vello::prelude::VelloFont>,
) {
    let brush = bevy_color_to_brush(color);
    let stroke = Stroke::new(1.0).with_caps(Cap::Butt);

    // Line sits at vertical center of the allocated height
    let y = height / 2.0;

    let label_width = measure_label_width(role_label, font);

    // Left segment: from left edge to just before label
    let left_end = LABEL_INSET;
    if left_end > 0.0 {
        scene.stroke(
            &stroke,
            Affine::IDENTITY,
            &brush,
            None,
            &Line::new((0.0, y), (left_end, y)),
        );
    }

    // Right segment: from after label to right edge
    let right_start = LABEL_INSET + LABEL_PAD + label_width + LABEL_PAD;
    if right_start < width {
        scene.stroke(
            &stroke,
            Affine::IDENTITY,
            &brush,
            None,
            &Line::new((right_start, y), (width, y)),
        );
    }

    // Draw label text centered vertically on the line
    if let Some(font) = font {
        draw_label_text(scene, role_label, LABEL_INSET + LABEL_PAD, y, font, &brush);
    }
}

// ============================================================================
// INTERNAL DRAWING HELPERS
// ============================================================================

/// Draw a fieldset-style rounded rect with gaps for top/bottom labels.
///
/// The rect is drawn as individual segments to leave gaps:
/// - Top edge: left → gap → right
/// - Right edge: continuous with corner arcs
/// - Bottom edge: left → optional gap → right
/// - Left edge: continuous with corner arcs
fn draw_fieldset_rect(
    scene: &mut vello::Scene,
    width: f64,
    height: f64,
    radius: f64,
    stroke: &Stroke,
    brush: &Brush,
    top_gap_width: Option<f64>,
    bottom_gap_width: Option<f64>,
) {
    let half_t = stroke.width / 2.0;
    let r = radius.min(width / 2.0).min(height / 2.0);

    // Inset coordinates for stroke center
    let x0 = half_t;
    let y0 = half_t;
    let x1 = width - half_t;
    let y1 = height - half_t;

    // === Top edge ===
    match top_gap_width {
        Some(gap_w) => {
            let gap_start = LABEL_INSET;
            let gap_end = gap_start + LABEL_PAD + gap_w + LABEL_PAD;

            // Top-left corner arc + left segment
            let mut path = BezPath::new();
            // Start at bottom of left edge, go up to top-left corner
            arc_corner(&mut path, x0 + r, y0 + r, r, std::f64::consts::PI, true);
            // Top segment left of gap
            if gap_start > x0 + r {
                path.line_to((gap_start, y0));
            }
            scene.stroke(stroke, Affine::IDENTITY, brush, None, &path);

            // Top segment right of gap + top-right corner arc
            let mut path = BezPath::new();
            let clamped_end = gap_end.min(x1 - r);
            path.move_to((clamped_end, y0));
            path.line_to((x1 - r, y0));
            arc_corner(&mut path, x1 - r, y0 + r, r, -std::f64::consts::FRAC_PI_2, true);
            scene.stroke(stroke, Affine::IDENTITY, brush, None, &path);
        }
        None => {
            // Full top edge with both corners
            let mut path = BezPath::new();
            arc_corner(&mut path, x0 + r, y0 + r, r, std::f64::consts::PI, true);
            path.line_to((x1 - r, y0));
            arc_corner(&mut path, x1 - r, y0 + r, r, -std::f64::consts::FRAC_PI_2, true);
            scene.stroke(stroke, Affine::IDENTITY, brush, None, &path);
        }
    }

    // === Right edge ===
    {
        let mut path = BezPath::new();
        path.move_to((x1, y0 + r));
        path.line_to((x1, y1 - r));
        scene.stroke(stroke, Affine::IDENTITY, brush, None, &path);
    }

    // === Bottom edge ===
    match bottom_gap_width {
        Some(gap_w) => {
            let gap_start = LABEL_INSET;
            let gap_end = gap_start + LABEL_PAD + gap_w + LABEL_PAD;

            // Bottom-right corner arc + segment left of gap
            let mut path = BezPath::new();
            arc_corner(&mut path, x1 - r, y1 - r, r, 0.0, true);
            let clamped_end = gap_end.min(x1 - r);
            if clamped_end < x1 - r {
                // There's line before the gap ends, going right-to-left
            }
            // bottom segment from right corner to gap end
            path.line_to((clamped_end, y1));
            scene.stroke(stroke, Affine::IDENTITY, brush, None, &path);

            // Bottom segment from gap start to left corner
            let mut path = BezPath::new();
            path.move_to((gap_start, y1));
            path.line_to((x0 + r, y1));
            arc_corner(&mut path, x0 + r, y1 - r, r, std::f64::consts::FRAC_PI_2, true);
            scene.stroke(stroke, Affine::IDENTITY, brush, None, &path);
        }
        None => {
            // Full bottom edge with both corners
            let mut path = BezPath::new();
            arc_corner(&mut path, x1 - r, y1 - r, r, 0.0, true);
            path.line_to((x0 + r, y1));
            arc_corner(&mut path, x0 + r, y1 - r, r, std::f64::consts::FRAC_PI_2, true);
            scene.stroke(stroke, Affine::IDENTITY, brush, None, &path);
        }
    }

    // === Left edge ===
    {
        let mut path = BezPath::new();
        path.move_to((x0, y1 - r));
        path.line_to((x0, y0 + r));
        scene.stroke(stroke, Affine::IDENTITY, brush, None, &path);
    }
}

/// Draw a top-accent line with optional label gap.
fn draw_top_accent(
    scene: &mut vello::Scene,
    width: f64,
    stroke: &Stroke,
    brush: &Brush,
    gap_width: Option<f64>,
) {
    let y = stroke.width / 2.0;

    match gap_width {
        Some(gap_w) => {
            let gap_start = LABEL_INSET;
            let gap_end = gap_start + LABEL_PAD + gap_w + LABEL_PAD;

            // Left segment
            if gap_start > 0.0 {
                scene.stroke(stroke, Affine::IDENTITY, brush, None,
                    &Line::new((0.0, y), (gap_start, y)));
            }
            // Right segment
            if gap_end < width {
                scene.stroke(stroke, Affine::IDENTITY, brush, None,
                    &Line::new((gap_end, y), (width, y)));
            }
        }
        None => {
            scene.stroke(stroke, Affine::IDENTITY, brush, None,
                &Line::new((0.0, y), (width, y)));
        }
    }
}

/// Append a 90-degree corner arc to a BezPath.
///
/// The arc starts at `start_angle` and sweeps 90 degrees counterclockwise
/// (positive sweep). For clockwise corners, the path construction handles
/// the direction by choosing the right start angle.
fn arc_corner(path: &mut BezPath, cx: f64, cy: f64, radius: f64, start_angle: f64, _ccw: bool) {
    let arc = Arc::new(
        Point::new(cx, cy),
        (radius, radius),
        start_angle,
        std::f64::consts::FRAC_PI_2,
        0.0,
    );
    // Convert arc to bezier path elements
    let arc_path = arc.to_path(0.1);
    let mut first = true;
    for el in arc_path.elements() {
        match el {
            vello::kurbo::PathEl::MoveTo(p) => {
                if first {
                    // If the path already has content, line_to the arc start.
                    // If empty, move_to it.
                    if path.elements().len() > 0 {
                        path.line_to(*p);
                    } else {
                        path.move_to(*p);
                    }
                    first = false;
                }
            }
            vello::kurbo::PathEl::LineTo(p) => path.line_to(*p),
            vello::kurbo::PathEl::QuadTo(p1, p2) => path.quad_to(*p1, *p2),
            vello::kurbo::PathEl::CurveTo(p1, p2, p3) => path.curve_to(*p1, *p2, *p3),
            vello::kurbo::PathEl::ClosePath => {} // Don't close — we're building segments
        }
    }
}

/// Draw label text at a given position using VelloFont.
///
/// The label is vertically centered around `y` (baseline offset accounts
/// for font metrics).
fn draw_label_text(
    scene: &mut vello::Scene,
    text: &str,
    x: f64,
    y: f64,
    font: &bevy_vello::prelude::VelloFont,
    brush: &Brush,
) {
    let style = bevy_vello::prelude::VelloTextStyle {
        font_size: FIELDSET_LABEL_FONT_SIZE,
        ..default()
    };

    let layout = font.layout(text, &style, bevy_vello::prelude::VelloTextAlign::Left, None);

    // Offset y so the text baseline sits at the border line.
    // For top labels: text sits just below the line (y ≈ 0).
    // For bottom labels: text sits just above the line (y ≈ height).
    // We shift by half the line height for visual centering.
    let line_height = layout.height() as f64;
    let text_y = y - line_height / 2.0;

    let transform = Affine::translate((x, text_y));

    // Render each glyph run
    for line in layout.lines() {
        for item in line.items() {
            let bevy_vello::parley::PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };
            let mut gx = glyph_run.offset();
            let gy = glyph_run.baseline();
            let run = glyph_run.run();
            let run_font = run.font();
            let font_size = run.font_size();

            scene
                .draw_glyphs(run_font)
                .brush(brush)
                .hint(true)
                .transform(transform)
                .font_size(font_size)
                .normalized_coords(run.normalized_coords())
                .draw(
                    Fill::NonZero,
                    glyph_run.glyphs().map(|glyph| {
                        let px = gx + glyph.x;
                        let py = gy - glyph.y;
                        gx += glyph.advance;
                        vello::Glyph {
                            id: glyph.id as _,
                            x: px,
                            y: py,
                        }
                    }),
                );
        }
    }
}

/// Approximate label width in pixels using character count heuristic.
///
/// We use monospace fonts, so width ≈ char_count * char_width.
/// Falls back to this heuristic when no font is available (first frame).
fn measure_label_width(label: &str, font: Option<&bevy_vello::prelude::VelloFont>) -> f64 {
    if let Some(font) = font {
        let style = bevy_vello::prelude::VelloTextStyle {
            font_size: FIELDSET_LABEL_FONT_SIZE,
            ..default()
        };
        let layout = font.layout(label, &style, bevy_vello::prelude::VelloTextAlign::Left, None);
        layout.width() as f64
    } else {
        // Heuristic: monospace at 11px ≈ 6.6px per char
        label.len() as f64 * 6.6
    }
}

/// Measure label width for role group lines (larger font).
#[allow(dead_code)] // Available for external use
pub fn measure_role_label_width(label: &str, font: Option<&bevy_vello::prelude::VelloFont>) -> f64 {
    if let Some(font) = font {
        let style = bevy_vello::prelude::VelloTextStyle {
            font_size: ROLE_LABEL_FONT_SIZE,
            ..default()
        };
        let layout = font.layout(label, &style, bevy_vello::prelude::VelloTextAlign::Left, None);
        layout.width() as f64
    } else {
        label.len() as f64 * 7.2
    }
}

// ============================================================================
// ANIMATION HELPERS
// ============================================================================

/// Chase animation: a bright segment traveling along the border perimeter.
fn chase_overlay(
    scene: &mut vello::Scene,
    width: f64,
    height: f64,
    radius: f64,
    thickness: f64,
    time: f32,
    color: &Color,
) {
    // Build the full border path for dash calculation
    let half_t = thickness / 2.0;
    let rect = RoundedRect::new(half_t, half_t, width - half_t, height - half_t, radius);
    let perimeter = rect.perimeter(0.1);

    // Chase segment: ~15% of perimeter, traveling at effect_chase_speed
    let chase_len = perimeter * 0.15;
    let position = (time as f64 * 2.0) % perimeter; // speed factor

    // Bright version of the border color
    let srgba = color.to_srgba();
    let bright = Color::srgba(
        (srgba.red * 1.5).min(1.0),
        (srgba.green * 1.5).min(1.0),
        (srgba.blue * 1.5).min(1.0),
        (srgba.alpha * 1.8).min(1.0),
    );
    let brush = bevy_color_to_brush(bright);

    // Use dashed stroke to create a single bright segment
    let stroke = Stroke::new(thickness * 1.5)
        .with_caps(Cap::Round)
        .with_dashes(position, &[chase_len, perimeter - chase_len]);

    scene.stroke(&stroke, Affine::IDENTITY, &brush, None, &rect);
}

/// Compute animation alpha multiplier.
fn animation_alpha(animation: &BorderAnimation, time: f32) -> f32 {
    match animation {
        BorderAnimation::None => 1.0,
        BorderAnimation::Chase => 1.0, // chase uses overlay, base alpha stays 1.0
        BorderAnimation::Pulse => {
            let base = 0.6;
            let amplitude = 0.4;
            base + amplitude * (time * 2.0).sin()
        }
        BorderAnimation::Breathe => {
            let base = 0.7;
            let amplitude = 0.3;
            base + amplitude * (time * 1.0).sin()
        }
    }
}

/// Apply alpha to a brush (creates a new brush with modified alpha).
fn apply_alpha(brush: &Brush, alpha: f32) -> Brush {
    if (alpha - 1.0).abs() < 0.01 {
        return brush.clone();
    }
    match brush {
        Brush::Solid(color) => {
            let rgba = color.to_rgba8();
            let new_a = ((rgba.a as f32) * alpha) as u8;
            Brush::Solid(vello::peniko::Color::from_rgba8(rgba.r, rgba.g, rgba.b, new_a))
        }
        _ => brush.clone(), // Gradients: leave as-is
    }
}
