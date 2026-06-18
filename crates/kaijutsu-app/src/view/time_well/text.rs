//! Card text — fully GPU, vello-free.
//!
//! Card **text** goes through the app's MSDF pipeline: [`card_text_glyphs`] lays
//! out each field with parley and collects positioned glyphs into the card's
//! [`MsdfBlockGlyphs`]; the existing (generic) block MSDF render pass clears the
//! card texture and renders the glyphs text-on-transparent
//! (`BlockRenderMethod::Msdf`). The card **body** — accent rounded-rect,
//! selection/lineage rings — is drawn by `well_card.wgsl` (SDF), driven by the
//! `WellCardMaterial` uniforms this module keeps in sync from `Card`. So vello no
//! longer touches card textures at all (it stays for SVG/ABC elsewhere).

use bevy::prelude::*;
use vello::peniko::Brush;

use kaijutsu_types::ContextId;

use super::panel::commit_panel_glyphs;
use super::scene::{
    CARD_TEX_H, CARD_TEX_W, Card, READING_TEX_H, READING_TEX_W, ReadingCard, TimeWellState,
    accent_vec4,
};
use crate::shaders::WellCardMaterial;
use crate::text::ShapingFonts;
use crate::text::components::bevy_color_to_brush;
use crate::text::msdf::{FontDataMap, MsdfAtlas, MsdfBlockGlyphs, PositionedGlyph, collect_msdf_glyphs};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};

/// Inner padding (logical px in the card-texture space).
const PAD: f32 = 14.0;

/// `WellCardMaterial.params` for a card: `[selected, in_lineage, status, drifting]`.
/// `status` is a float code the shader switches on for the rim FX: pending/none →
/// 0, running → 1 (breathing pulse), done → 2 (no rim), error → 3 (steady red).
/// `drifting` (0/1) gates the drift sheen sweep. Time-based animation reads
/// `globals.time` in the shader.
fn card_params(card: &Card) -> Vec4 {
    use kaijutsu_types::Status;
    let status = match card.status {
        Some(Status::Running) => 1.0,
        Some(Status::Done) => 2.0,
        Some(Status::Error) => 3.0,
        _ => 0.0, // None or Pending
    };
    Vec4::new(
        if card.selected { 1.0 } else { 0.0 },
        if card.in_lineage { 1.0 } else { 0.0 },
        status,
        if card.drifting { 1.0 } else { 0.0 },
    )
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

/// Lay out the card's text fields and collect MSDF glyphs (crisp at any zoom).
/// Each field lands at the same `(pad, y)` origin and color the card used before.
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

/// Rebuild a rim card's MSDF text glyphs + sync its material when its data
/// changes. No vello — the shader draws the body; MSDF owns the text texture.
pub fn build_card_scenes(
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    mut materials: ResMut<Assets<WellCardMaterial>>,
    mut query: Query<
        (
            &Card,
            &mut MsdfBlockGlyphs,
            &MeshMaterial3d<WellCardMaterial>,
        ),
        Changed<Card>,
    >,
) {
    let Some(font) = fonts.get(&font_handles.mono) else {
        return; // font still loading; retry next change
    };
    for (card, mut msdf, mat_node) in query.iter_mut() {
        // Pure MSDF surface: the build size lives on the card's `UiRttTexture`
        // (set once at spawn); the MSDF pass clears and owns the texture. No
        // vello scene — the shader draws the body.
        if let Some(atlas) = atlas.as_deref_mut() {
            let glyphs = card_text_glyphs(card, font, CARD_TEX_W, CARD_TEX_H, atlas, &mut font_data_map);
            commit_panel_glyphs(&mut msdf, glyphs);
        }

        if let Some(mat) = materials.get_mut(&mat_node.0) {
            mat.accent = accent_vec4(&card.data.accent);
            mat.params = card_params(card);
        }
    }
}

/// Render the current selection into the center-bottom focus card (MSDF text +
/// material body) at the larger reading size. Rebuilds only on selection change.
pub fn update_reading_card(
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    mut materials: ResMut<Assets<WellCardMaterial>>,
    state: Res<TimeWellState>,
    cards: Query<&Card>,
    mut reading: Query<
        (&mut MsdfBlockGlyphs, &MeshMaterial3d<WellCardMaterial>),
        With<ReadingCard>,
    >,
    mut last: Local<Option<ContextId>>,
) {
    if state.selected == *last {
        return;
    }
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    let Ok((mut msdf, mat_node)) = reading.single_mut() else {
        return; // focus card not spawned yet
    };
    *last = state.selected;

    // Build size lives on the focus card's `UiRttTexture` (set at spawn).
    let glyphs = match state
        .selected
        .and_then(|sel| cards.iter().find(|c| c.context_id == sel))
    {
        Some(card) => {
            if let Some(mat) = materials.get_mut(&mat_node.0) {
                mat.accent = accent_vec4(&card.data.accent);
                // The focus card is the selection — no selection/lineage ring on it.
                mat.params = Vec4::ZERO;
            }
            match atlas.as_deref_mut() {
                Some(atlas) => {
                    card_text_glyphs(card, font, READING_TEX_W, READING_TEX_H, atlas, &mut font_data_map)
                }
                None => Vec::new(),
            }
        }
        None => {
            if let Some(mat) = materials.get_mut(&mat_node.0) {
                mat.accent = Vec4::ZERO; // blank plate
            }
            Vec::new()
        }
    };
    commit_panel_glyphs(&mut msdf, glyphs);
}
