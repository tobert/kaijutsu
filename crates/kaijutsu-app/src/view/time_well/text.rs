//! Card text + decor rendering.
//!
//! Card **text** goes through the app's MSDF pipeline (crisp at any zoom, shared
//! atlas — fixes the focus-dolly softening vello had): [`card_text_glyphs`] lays
//! out each field with parley and collects positioned MSDF glyphs into the
//! card's [`MsdfBlockGlyphs`], which the existing block MSDF render pass
//! composites into the card texture. Card **decor** (accent bg, top bar, status
//! dot, selection/lineage rings) is a small `vello::Scene` rasterized into the
//! same texture *underneath* the text (`BlockRenderMethod::Vello` → MSDF
//! composites on top — exactly how block borders + text coexist). The
//! `WellCardMaterial` samples the composited texture.
//!
//! Vello-for-decor is interim: the rings/bg can move to `well_card.wgsl` (SDF)
//! later to drop vello from the well entirely (see docs/viz-substrate.md).

use bevy::prelude::*;
use vello::Scene;
use vello::kurbo::{Affine, RoundedRect, Stroke};
use vello::peniko::{Brush, Color as VColor, Fill};

use kaijutsu_types::ContextId;

use super::scene::{
    CARD_TEX_H, CARD_TEX_W, Card, READING_TEX_H, READING_TEX_W, ReadingCard, TimeWellState,
    accent_color,
};
use crate::text::ShapingFonts;
use crate::text::components::bevy_color_to_brush;
use crate::text::msdf::{FontDataMap, MsdfAtlas, MsdfBlockGlyphs, PositionedGlyph, collect_msdf_glyphs};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};
use crate::view::vello_ui_texture::VelloUiScene;

/// Inner padding (logical px in the card-texture space).
const PAD: f32 = 14.0;

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

/// Build the card **decor** vello scene at a target `(w, h)`: accent bg, top
/// bar, status dot, and selection/lineage rings — everything *except* the text
/// (which is MSDF, composited on top). Metrics scale by `s = h / CARD_TEX_H` off
/// the rim-card reference so the same code serves rim cards and the focus card.
fn build_card_decor(card: &Card, w: f32, h: f32) -> Scene {
    let mut scene = Scene::new();
    let s = h / CARD_TEX_H;
    let data = &card.data;

    // ── Background: accent rounded rect. ──
    let bg = bevy_color_to_brush(accent_color(&data.accent).with_alpha(0.94));
    let rect = RoundedRect::new(
        s as f64,
        s as f64,
        (w - s) as f64,
        (h - s) as f64,
        (14.0 * s) as f64,
    );
    scene.fill(Fill::NonZero, Affine::IDENTITY, &bg, None, &rect);

    // A subtle top accent bar.
    let bar = vello::kurbo::Rect::new(
        (8.0 * s) as f64,
        (5.0 * s) as f64,
        (w - 8.0 * s) as f64,
        (9.0 * s) as f64,
    );
    let bar_brush = Brush::Solid(VColor::new([1.0, 1.0, 1.0, 0.16]));
    scene.fill(Fill::NonZero, Affine::IDENTITY, &bar_brush, None, &bar);

    // ── Live status glyph (top-right dot), data-tick driven. ──
    if let Some(color) = status_color(card.status) {
        let cx = (w - 18.0 * s) as f64;
        let cy = (18.0 * s) as f64;
        let dot = vello::kurbo::Circle::new((cx, cy), (7.0 * s) as f64);
        let halo = vello::kurbo::Circle::new((cx, cy), (9.0 * s) as f64);
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            &Brush::Solid(VColor::new([0.0, 0.0, 0.0, 0.45])),
            None,
            &halo,
        );
        scene.fill(Fill::NonZero, Affine::IDENTITY, &Brush::Solid(color), None, &dot);
    }

    // ── Lineage ring: amber edge marking a fork-ancestor of the selection. ──
    if card.in_lineage {
        let inset = (3.5 * s) as f64;
        let ring = RoundedRect::new(inset, inset, (w - 3.5 * s) as f64, (h - 3.5 * s) as f64, (13.0 * s) as f64);
        scene.stroke(
            &Stroke::new((5.0 * s) as f64),
            Affine::IDENTITY,
            &Brush::Solid(VColor::new([0.95, 0.70, 0.20, 1.0])),
            None,
            &ring,
        );
    }

    // ── Selection ring: saturated blue halo + bright inner edge. ──
    if card.selected {
        let inset = (3.5 * s) as f64;
        let ring = RoundedRect::new(inset, inset, (w - 3.5 * s) as f64, (h - 3.5 * s) as f64, (13.0 * s) as f64);
        scene.stroke(
            &Stroke::new((7.0 * s) as f64),
            Affine::IDENTITY,
            &Brush::Solid(VColor::new([0.20, 0.55, 0.95, 1.0])),
            None,
            &ring,
        );
        scene.stroke(
            &Stroke::new((2.5 * s) as f64),
            Affine::IDENTITY,
            &Brush::Solid(VColor::new([0.90, 0.97, 1.0, 1.0])),
            None,
            &ring,
        );
    }

    scene
}

/// Register a layout's fonts and collect its MSDF glyphs at `offset`, colored by
/// `brush`, appending into `out`.
fn collect_field(
    layout: &parley::Layout<Brush>,
    offset: (f64, f64),
    brush: &Brush,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
    out: &mut Vec<PositionedGlyph>,
) {
    for line in layout.lines() {
        for item in line.items() {
            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                font_data_map.register(gr.run().font());
            }
        }
    }
    out.extend(collect_msdf_glyphs(layout, &[], brush, offset, atlas));
}

/// Lay out the card's text fields and collect MSDF glyphs (crisp text). Mirrors
/// the old vello field stacking — each field's glyphs land at the same `(pad, y)`
/// origin and color the vello text used, so the layout is unchanged, only the
/// rasterizer (MSDF, not vello).
fn card_text_glyphs(
    card: &Card,
    font: &VelloFont,
    w: f32,
    h: f32,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
) -> Vec<PositionedGlyph> {
    let s = h / CARD_TEX_H;
    let pad = PAD * s;
    let data = &card.data;
    let max_advance = Some(w - 2.0 * pad);
    let mut out = Vec::new();
    let mut y = pad + 6.0 * s;

    // ── Title. ──
    let title_brush = bevy_color_to_brush(Color::srgb(0.98, 0.99, 1.0));
    let title = font.layout(
        &data.title,
        &VelloTextStyle { font_size: 22.0 * s, line_height: 1.1, ..default() },
        VelloTextAlign::Left,
        max_advance,
    );
    let title_h = title.height();
    collect_field(&title, (pad as f64, y as f64), &title_brush, atlas, font_data_map, &mut out);
    y += title_h + 6.0 * s;

    // ── Model badge. ──
    if !data.model_badge.is_empty() {
        let brush = bevy_color_to_brush(Color::srgba(1.0, 1.0, 1.0, 0.85));
        let badge = font.layout(
            &data.model_badge,
            &VelloTextStyle { font_size: 14.0 * s, ..default() },
            VelloTextAlign::Left,
            max_advance,
        );
        let bh = badge.height();
        collect_field(&badge, (pad as f64, y as f64), &brush, atlas, font_data_map, &mut out);
        y += bh + 4.0 * s;
    }

    // ── Fork badge. ──
    if let Some(fork) = &data.fork_badge {
        let brush = bevy_color_to_brush(Color::srgba(1.0, 1.0, 1.0, 0.65));
        let label = format!("⑂ {fork}");
        let l = font.layout(
            &label,
            &VelloTextStyle { font_size: 12.0 * s, ..default() },
            VelloTextAlign::Left,
            max_advance,
        );
        let lh = l.height();
        collect_field(&l, (pad as f64, y as f64), &brush, atlas, font_data_map, &mut out);
        y += lh + 4.0 * s;
    }

    // ── Keywords or preview. ──
    let tail = if !data.keywords.is_empty() {
        data.keywords.join(" · ")
    } else {
        data.preview.clone().unwrap_or_default()
    };
    if !tail.is_empty() && y < h - pad {
        let brush = bevy_color_to_brush(Color::srgba(0.92, 0.95, 1.0, 0.72));
        let l = font.layout(
            &tail,
            &VelloTextStyle { font_size: 12.0 * s, line_height: 1.15, ..default() },
            VelloTextAlign::Left,
            max_advance,
        );
        collect_field(&l, (pad as f64, y as f64), &brush, atlas, font_data_map, &mut out);
    }

    // ── Cluster label (haystack only), bottom-anchored. ──
    if let Some(cluster) = &data.cluster_label {
        let brush = bevy_color_to_brush(Color::srgba(0.86, 0.93, 1.0, 0.95));
        let text = format!("◇ {cluster}");
        let l = font.layout(
            &text,
            &VelloTextStyle { font_size: 13.0 * s, ..default() },
            VelloTextAlign::Left,
            max_advance,
        );
        let lh = l.height();
        let fy = h - pad - lh;
        collect_field(&l, (pad as f64, fy as f64), &brush, atlas, font_data_map, &mut out);
    }

    out
}

/// Rebuild a rim card's decor scene + MSDF text glyphs when its data changes.
/// The decor (vello) + glyphs (MSDF) composite into the one card texture.
pub fn build_card_scenes(
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    mut query: Query<(&Card, &mut VelloUiScene, &mut MsdfBlockGlyphs), Changed<Card>>,
) {
    let Some(font) = fonts.get(&font_handles.mono) else {
        return; // font still loading; retry next change
    };
    for (card, mut ui, mut msdf) in query.iter_mut() {
        ui.scene = build_card_decor(card, CARD_TEX_W, CARD_TEX_H);
        ui.built_width = CARD_TEX_W;
        ui.built_height = CARD_TEX_H;
        ui.version = ui.version.wrapping_add(1);

        if let Some(atlas) = atlas.as_deref_mut() {
            msdf.glyphs = card_text_glyphs(card, font, CARD_TEX_W, CARD_TEX_H, atlas, &mut font_data_map);
            msdf.version = msdf.version.wrapping_add(1);
        }
    }
}

/// Render the current selection into the center-bottom focus card (decor + MSDF
/// text) at the larger reading size. Rebuilds only when the selection changes.
pub fn update_reading_card(
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    state: Res<TimeWellState>,
    cards: Query<&Card>,
    mut reading: Query<(&mut VelloUiScene, &mut MsdfBlockGlyphs), With<ReadingCard>>,
    mut last: Local<Option<ContextId>>,
) {
    if state.selected == *last {
        return;
    }
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    let Ok((mut ui, mut msdf)) = reading.single_mut() else {
        return; // focus card not spawned yet
    };
    *last = state.selected;

    ui.built_width = READING_TEX_W;
    ui.built_height = READING_TEX_H;
    match state
        .selected
        .and_then(|sel| cards.iter().find(|c| c.context_id == sel))
    {
        Some(card) => {
            ui.scene = build_card_decor(card, READING_TEX_W, READING_TEX_H);
            if let Some(atlas) = atlas.as_deref_mut() {
                msdf.glyphs =
                    card_text_glyphs(card, font, READING_TEX_W, READING_TEX_H, atlas, &mut font_data_map);
            }
        }
        None => {
            ui.scene = Scene::new();
            msdf.glyphs.clear();
        }
    }
    ui.version = ui.version.wrapping_add(1);
    msdf.version = msdf.version.wrapping_add(1);
}
