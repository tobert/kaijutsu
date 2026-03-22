//! ABC notation → Vello scene rendering.
//!
//! Converts `Vec<EngravingElement>` from `kaijutsu-abc` into a `vello::Scene`.
//! Lives in kaijutsu-app (which has both bevy_vello and kaijutsu-abc deps)
//! so kaijutsu-abc stays a leaf crate.

use bevy::prelude::default;
use bevy_vello::prelude::*;
use bevy_vello::vello;
use vello::kurbo::{Affine, BezPath, Line, Stroke};
use vello::peniko::{Brush, Fill};

use kaijutsu_abc::engrave::font::font_cache;
use kaijutsu_abc::engrave::EngravingElement;

use super::TextMetrics;

/// Render engraving elements to a vello Scene.
///
/// Returns `(scene, width, height)` where width/height are the intrinsic
/// dimensions of the notation content (including margin).
pub fn render_engraving_to_scene(
    elements: &[EngravingElement],
    margin: f64,
    color: &Brush,
    font: Option<&VelloFont>,
    _text_metrics: &TextMetrics,
) -> (vello::Scene, f64, f64) {
    let fc = font_cache();
    let mut scene = vello::Scene::new();

    // Compute bounding box — mirrors logic from engrave/svg.rs
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);

    for elem in elements {
        match elem {
            EngravingElement::Glyph {
                x,
                y,
                scale,
                codepoint,
                ..
            } => {
                let advance = fc.glyph_advance(*codepoint).unwrap_or(500.0) * scale;
                let glyph_height = fc.upem() * scale;
                min_x = min_x.min(*x);
                min_y = min_y.min(y - glyph_height);
                max_x = max_x.max(x + advance);
                max_y = max_y.max(y + glyph_height * 0.5);
            }
            EngravingElement::Line { x1, y1, x2, y2, .. } => {
                min_x = min_x.min(*x1).min(*x2);
                min_y = min_y.min(*y1).min(*y2);
                max_x = max_x.max(*x1).max(*x2);
                max_y = max_y.max(*y1).max(*y2);
            }
            EngravingElement::Text {
                x, y, size, content, ..
            } => {
                min_x = min_x.min(*x);
                min_y = min_y.min(y - size);
                max_x = max_x.max(x + content.len() as f64 * size * 0.6);
                max_y = max_y.max(*y);
            }
            EngravingElement::Path { .. } => {}
        }
    }

    // Guard against empty
    if min_x > max_x {
        min_x = 0.0;
        max_x = 100.0;
        min_y = 0.0;
        max_y = 100.0;
    }

    let width = (max_x - min_x) + margin * 2.0;
    let height = (max_y - min_y) + margin * 2.0;
    let origin_x = min_x - margin;
    let origin_y = min_y - margin;

    // Render elements with origin offset so (origin_x, origin_y) maps to (0,0)
    let offset = Affine::translate((-origin_x, -origin_y));

    for elem in elements {
        match elem {
            EngravingElement::Glyph {
                codepoint,
                x,
                y,
                scale,
                ..
            } => {
                if let Some(bezpath) = fc.glyph_bezpath(*codepoint) {
                    let transform =
                        offset * Affine::translate((*x, *y)) * Affine::scale(*scale);
                    scene.fill(Fill::NonZero, transform, color, None, bezpath);
                }
            }
            EngravingElement::Line {
                x1,
                y1,
                x2,
                y2,
                width: line_width,
                ..
            } => {
                let line = Line::new((*x1, *y1), (*x2, *y2));
                scene.stroke(&Stroke::new(*line_width), offset, color, None, &line);
            }
            EngravingElement::Path { d, fill, .. } => {
                if let Ok(bezpath) = BezPath::from_svg(d) {
                    if *fill {
                        scene.fill(Fill::NonZero, offset, color, None, &bezpath);
                    } else {
                        scene.stroke(&Stroke::new(0.5), offset, color, None, &bezpath);
                    }
                }
            }
            EngravingElement::Text {
                content,
                x,
                y,
                size,
                ..
            } => {
                // Render text using Parley via VelloFont if available
                if let Some(vello_font) = font {
                    let style = VelloTextStyle {
                        font_size: *size as f32,
                        brush: color.clone(),
                        ..default()
                    };
                    let layout = vello_font.layout(
                        content,
                        &style,
                        VelloTextAlign::Left,
                        None,
                    );
                    // Parley baseline is at y=0 of the layout; ABC puts y at the baseline
                    let text_offset = (-origin_x + x, -origin_y + y - *size);
                    super::rich::render_layout_with_brushes(
                        &mut scene,
                        &layout,
                        &[],
                        color,
                        text_offset,
                    );
                }
                // If no font available, text is silently dropped (matches SVG path behavior)
            }
        }
    }

    (scene, width, height)
}
