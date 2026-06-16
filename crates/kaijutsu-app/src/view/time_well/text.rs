//! Card text rendering: shape with the app's `VelloFont` and encode into a
//! `vello::Scene` that the shared `vello_ui_texture` RTT primitive rasterizes
//! onto each card quad (see `docs/viz-substrate.md`, "Rendering notes").
//!
//! The app's normal text path is MSDF (atlas + a bespoke pipeline tied to UI
//! cells), which doesn't fit a free-floating 3D quad. So cards take the other
//! documented route: a `vello::Scene` per card, rasterized to a per-card texture
//! sampled by the card material. This module owns the parley-`Layout` → `Scene`
//! glyph encoder and the per-card scene builder.
//!
//! NB: `VelloFont::layout` does **not** push the brush into the parley builder —
//! the MSDF path supplies color separately, so the layout's glyph runs carry
//! parley's default (black). We therefore pass the brush explicitly to
//! [`draw_layout`] rather than reading `glyph_run.style().brush`.

use bevy::prelude::*;
use vello::Scene;
use vello::kurbo::{Affine, RoundedRect, Stroke};
use vello::peniko::{Brush, Color as VColor, Fill};

use super::scene::{CARD_TEX_H, CARD_TEX_W, Card, accent_color};
use crate::text::ShapingFonts;
use crate::text::components::bevy_color_to_brush;
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};

/// Inner padding (logical px in the card-texture space).
const PAD: f32 = 14.0;

/// Encode a shaped parley layout into `scene`, translated to `origin`, painted
/// with `brush`.
///
/// The canonical parley→vello glyph walk: per `GlyphRun`, accumulate the pen x
/// from each glyph's advance and emit `vello::Glyph`s with the run's font, size,
/// and variable-font coords. The brush is supplied by the caller because the
/// app's shaper doesn't carry it on the layout (see module note).
pub fn draw_layout(
    scene: &mut Scene,
    layout: &parley::Layout<Brush>,
    origin: (f64, f64),
    brush: &Brush,
) {
    for line in layout.lines() {
        for item in line.items() {
            let parley::PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };
            let mut pen_x = glyph_run.offset();
            let pen_y = glyph_run.baseline();
            let run = glyph_run.run();
            let font = run.font();
            let font_size = run.font_size();
            let coords = run.normalized_coords();

            scene
                .draw_glyphs(font)
                .brush(brush)
                .font_size(font_size)
                .normalized_coords(coords)
                .transform(Affine::translate(origin))
                .draw(
                    Fill::NonZero,
                    glyph_run.glyphs().map(|g| {
                        let gx = pen_x + g.x;
                        let gy = pen_y - g.y;
                        pen_x += g.advance;
                        vello::Glyph {
                            id: g.id,
                            x: gx,
                            y: gy,
                        }
                    }),
                );
        }
    }
}

/// Status → glyph color. `None` (no status seen yet) draws no dot.
fn status_color(status: Option<kaijutsu_types::Status>) -> Option<VColor> {
    use kaijutsu_types::Status;
    Some(match status? {
        Status::Pending => VColor::new([0.62, 0.66, 0.72, 1.0]), // gray
        Status::Running => VColor::new([0.98, 0.78, 0.20, 1.0]), // amber
        Status::Done => VColor::new([0.36, 0.82, 0.45, 1.0]),    // green
        Status::Error => VColor::new([0.92, 0.34, 0.34, 1.0]),   // red
    })
}

/// Build the vello scene for one card: an accent rounded-rect background with the
/// title, model badge, fork badge, and a keyword/preview line stacked inside.
fn build_card_scene(card: &Card, font: &VelloFont) -> Scene {
    let mut scene = Scene::new();
    let w = CARD_TEX_W;
    let h = CARD_TEX_H;
    let data = &card.data;

    // ── Background: accent rounded rect. ──
    let bg = bevy_color_to_brush(accent_color(&data.accent).with_alpha(0.94));
    let rect = RoundedRect::new(1.0, 1.0, (w - 1.0) as f64, (h - 1.0) as f64, 14.0);
    scene.fill(Fill::NonZero, Affine::IDENTITY, &bg, None, &rect);

    // A subtle top accent bar so cards read as cards even before text loads.
    let bar = vello::kurbo::Rect::new(8.0, 5.0, (w - 8.0) as f64, 9.0);
    let bar_brush = Brush::Solid(VColor::new([1.0, 1.0, 1.0, 0.16]));
    scene.fill(Fill::NonZero, Affine::IDENTITY, &bar_brush, None, &bar);

    // ── Live status glyph (top-right dot), data-tick driven. ──
    if let Some(color) = status_color(card.status) {
        let dot = vello::kurbo::Circle::new(((w - 18.0) as f64, 18.0), 7.0);
        // Dark halo so the dot reads on any accent.
        let halo = vello::kurbo::Circle::new(((w - 18.0) as f64, 18.0), 9.0);
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            &Brush::Solid(VColor::new([0.0, 0.0, 0.0, 0.45])),
            None,
            &halo,
        );
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            &Brush::Solid(color),
            None,
            &dot,
        );
    }

    let max_advance = Some(w - 2.0 * PAD);
    let mut y = PAD + 6.0;

    // ── Title. ──
    let title_brush = bevy_color_to_brush(Color::srgb(0.98, 0.99, 1.0));
    let title_style = VelloTextStyle {
        font_size: 22.0,
        line_height: 1.1,
        ..default()
    };
    let title = font.layout(&data.title, &title_style, VelloTextAlign::Left, max_advance);
    let title_h = title.height();
    draw_layout(&mut scene, &title, (PAD as f64, y as f64), &title_brush);
    y += title_h + 6.0;

    // ── Model badge. ──
    if !data.model_badge.is_empty() {
        let brush = bevy_color_to_brush(Color::srgba(1.0, 1.0, 1.0, 0.85));
        let style = VelloTextStyle {
            font_size: 14.0,
            ..default()
        };
        let badge = font.layout(&data.model_badge, &style, VelloTextAlign::Left, max_advance);
        let bh = badge.height();
        draw_layout(&mut scene, &badge, (PAD as f64, y as f64), &brush);
        y += bh + 4.0;
    }

    // ── Fork badge (small, dim). ──
    if let Some(fork) = &data.fork_badge {
        let brush = bevy_color_to_brush(Color::srgba(1.0, 1.0, 1.0, 0.65));
        let style = VelloTextStyle {
            font_size: 12.0,
            ..default()
        };
        let label = format!("⑂ {fork}");
        let l = font.layout(&label, &style, VelloTextAlign::Left, max_advance);
        let lh = l.height();
        draw_layout(&mut scene, &l, (PAD as f64, y as f64), &brush);
        y += lh + 4.0;
    }

    // ── Keywords or preview, whichever is present, filling remaining space. ──
    let tail = if !data.keywords.is_empty() {
        data.keywords.join(" · ")
    } else {
        data.preview.clone().unwrap_or_default()
    };
    if !tail.is_empty() && y < h - PAD {
        let brush = bevy_color_to_brush(Color::srgba(0.92, 0.95, 1.0, 0.72));
        let style = VelloTextStyle {
            font_size: 12.0,
            line_height: 1.15,
            ..default()
        };
        let l = font.layout(&tail, &style, VelloTextAlign::Left, max_advance);
        draw_layout(&mut scene, &l, (PAD as f64, y as f64), &brush);
    }

    // ── Selection ring (drawn last, on top): a saturated outer halo stroke under
    // a bright inner edge, reading as a glow around the card. Both strokes are
    // opaque on purpose — the card material is `AlphaMode::Mask(0.5)`, which clamps
    // alpha to binary, so a translucent glow would simply be discarded. We fake the
    // falloff with width + color instead. Only present when selected; the
    // `selected` flag flips on select/deselect, rebuilding this scene. ──
    if card.selected {
        // Outer halo: wide, saturated blue, just inside the texture bounds.
        let ring = RoundedRect::new(3.5, 3.5, (w - 3.5) as f64, (h - 3.5) as f64, 13.0);
        scene.stroke(
            &Stroke::new(7.0),
            Affine::IDENTITY,
            &Brush::Solid(VColor::new([0.20, 0.55, 0.95, 1.0])),
            None,
            &ring,
        );
        // Bright inner edge over the halo.
        scene.stroke(
            &Stroke::new(2.5),
            Affine::IDENTITY,
            &Brush::Solid(VColor::new([0.90, 0.97, 1.0, 1.0])),
            None,
            &ring,
        );
    }

    scene
}

/// Rebuild the vello scene for any card whose data changed (or that was just
/// spawned, where `VelloUiScene::version == 0`). Bumps `version` so the shared
/// extract/render path re-rasterizes the card texture.
pub fn build_card_scenes(
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut query: Query<(&Card, &mut crate::view::vello_ui_texture::VelloUiScene), Changed<Card>>,
) {
    let Some(font) = fonts.get(&font_handles.mono) else {
        return; // font still loading; retry next change
    };
    for (card, mut ui) in query.iter_mut() {
        ui.scene = build_card_scene(card, font);
        ui.built_width = CARD_TEX_W;
        ui.built_height = CARD_TEX_H;
        ui.version = ui.version.wrapping_add(1);
    }
}
