//! Vello scene builders for role group divider lines.
//!
//! Block borders are now shader-drawn via `BlockFxMaterial` (see `block_fx.wgsl`).
//! This module only handles role group lines — horizontal dividers with inset labels.

use bevy::prelude::*;
use bevy_vello::vello;
use vello::kurbo::{Affine, Cap, Line, Stroke};
use vello::peniko::{Brush, Fill};

use crate::text::components::bevy_color_to_brush;

// ============================================================================
// CONSTANTS
// ============================================================================

/// Horizontal inset from left edge where the label starts.
const LABEL_INSET: f64 = 12.0;

/// Horizontal padding around label text within the gap.
const LABEL_PAD: f64 = 6.0;

/// Font size for fieldset labels (block_render.rs uses this for MSDF label collection).
#[allow(dead_code)] // Referenced by hardcoded value in block_render.rs; Phase 5 promotes to theme
const FIELDSET_LABEL_FONT_SIZE: f32 = 11.0;

/// Font size for role group line labels.
const ROLE_LABEL_FONT_SIZE: f32 = 12.0;

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
// INTERNAL HELPERS
// ============================================================================

/// Draw label text at a given position using VelloFont.
///
/// The label is vertically centered around `y`.
fn draw_label_text(
    scene: &mut vello::Scene,
    text: &str,
    x: f64,
    y: f64,
    font: &bevy_vello::prelude::VelloFont,
    brush: &Brush,
) {
    let style = bevy_vello::prelude::VelloTextStyle {
        font_size: ROLE_LABEL_FONT_SIZE,
        ..default()
    };

    let layout = font.layout(
        text,
        &style,
        bevy_vello::prelude::VelloTextAlign::Left,
        None,
    );

    let line_height = layout.height() as f64;
    let text_y = y - line_height / 2.0;

    let transform = Affine::translate((x, text_y));

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

/// Measure label width using font layout or a heuristic fallback.
fn measure_label_width(label: &str, font: Option<&bevy_vello::prelude::VelloFont>) -> f64 {
    if let Some(font) = font {
        let style = bevy_vello::prelude::VelloTextStyle {
            font_size: ROLE_LABEL_FONT_SIZE,
            ..default()
        };
        let layout = font.layout(
            label,
            &style,
            bevy_vello::prelude::VelloTextAlign::Left,
            None,
        );
        layout.width() as f64
    } else {
        // Heuristic: monospace at 12px ≈ 7.2px per char
        label.len() as f64 * 7.2
    }
}
