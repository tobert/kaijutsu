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
    CARD_TEX_H, CARD_TEX_W, Card, HorizonLabel, LABEL_TEX_W, READING_TEX_H, READING_TEX_W,
    ReadingCard, TimeWellState, accent_vec4,
};
use crate::shaders::WellCardMaterial;
use crate::text::ShapingFonts;
use crate::text::components::bevy_color_to_brush;
use crate::text::msdf::{FontDataMap, MsdfAtlas, MsdfBlockGlyphs, PositionedGlyph, collect_msdf_glyphs};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};

/// Inner padding (logical px in the card-texture space).
const PAD: f32 = 14.0;

/// Font size (logical px in the label-texture space) for [`HorizonLabel`] text.
const LABEL_FONT_SIZE: f32 = 24.0;
/// Inner padding (logical px in the label-texture space).
const LABEL_PAD: f32 = 10.0;
/// Dim, desaturated LDR brush for label text — passive structural state, not
/// live action (the HDR-tiering rule reserves bloom for the latter).
fn label_brush() -> Brush {
    bevy_color_to_brush(Color::srgba(0.75, 0.85, 0.97, 0.75))
}

/// Font size for the card-face gist line, ~0.75× the smallest badge (fork
/// badge, 12.0 — see `card_text_glyphs`) so it reads smaller than every badge
/// above it, not just the biggest.
const GIST_FONT_SIZE: f32 = 9.0;
/// Heuristic characters-per-wrapped-line budget for the gist line — same
/// "approximate width, not measured" idiom as HUD south's `SOUTH_LINE_CHARS`
/// (`view::time_well::hud`). Deliberately conservative: parley's real glyph
/// widths drive the actual on-screen wrap ([`VelloFont::layout`]'s
/// `break_all_lines`), so `wrap_gist` just needs each pre-wrapped line to be
/// safely short enough that parley never wraps it a second time.
const GIST_LINE_CHARS: usize = 40;
/// Hard cap on wrapped gist lines; content beyond this is ellipsized.
const GIST_MAX_LINES: usize = 2;

/// Dim brush for the gist line — dimmer than every badge brush above it
/// (title ~1.0 alpha, model badge 0.85, fork badge 0.65): this is the least
/// important line on the card face, a hint rather than a spec.
fn gist_brush() -> Brush {
    bevy_color_to_brush(Color::srgba(0.80, 0.86, 0.97, 0.55))
}

/// Greedy word-wrap: split `text` into lines of at most `line_chars`
/// characters each, never breaking a word mid-way. A single word longer than
/// `line_chars` still gets its own (overlong) line rather than being split —
/// simplicity over exactness, since this only has to bound the *count* of
/// lines, not lay out pixel-perfect ones (parley does that at render time).
fn word_wrap(text: &str, line_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current = word.to_string();
        } else if current.chars().count() + 1 + word.chars().count() <= line_chars {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Wrap `text` to at most `max_lines` lines of at most `line_chars` characters
/// each, ellipsizing the final kept line when content overflows beyond that.
/// Pure and font-free — a heuristic pre-wrap, not a substitute for parley's
/// real per-glyph wrap, just a worst-case line-count bound. Returns `None` for
/// blank input so callers can skip the whole line box (no reserved blank
/// space) rather than render an empty line.
fn wrap_gist(text: &str, line_chars: usize, max_lines: usize) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || line_chars == 0 || max_lines == 0 {
        return None;
    }
    let mut lines = word_wrap(text, line_chars);
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            if last.chars().count() >= line_chars {
                *last = crate::text::truncate_chars(last, line_chars);
            } else {
                last.push('…');
            }
        }
    }
    if lines.is_empty() { None } else { Some(lines.join("\n")) }
}

// HDR gain for the focus card's bevel frame (the `border` ring band: the
// selection's accent lifted so the frame reads as a deliberate beveled edge,
// replacing the accidental cream ring the pre-mask-fix chatter/beat lanes
// used to paint; a touch under the HUD panels' `gain_hud_border` so the
// in-world card doesn't outshine its own HUD) moved onto
// `ScenePalette::gain_reading_border`.

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

    // ── Paused marker. ── Design-only state (see `CardData::paused`): visuals
    // only, no placement/behavior effect. Literal text, not the "⏸" glyph —
    // the MSDF font pipeline's glyph coverage for it is unverified, and the
    // literal reads correctly everywhere the rest of the card's text does.
    if data.paused {
        let brush = bevy_color_to_brush(Color::srgb(1.0, 0.78, 0.35));
        let marker = font.layout(
            "PAUSED",
            &VelloTextStyle { font_size: 13.0 * s, ..default() },
            VelloTextAlign::Left,
            max_advance,
        );
        let mh = marker.height();
        collect_field(&marker, (pad as f64, y as f64), &brush, atlas, font_data_map, &mut out);
        y += mh + 4.0 * s;
    }

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

    // ── Gist line(s). ── Sentence-level extractive summary (Tier 2,
    // `kaijutsu-kernel::runtime::synthesis::compute_gist`), riding the same
    // `CardData.preview` field the old 80-char block-head preview used — no
    // wire change, just richer content landing here (and in HUD south's
    // fallback) for free. Smaller and dimmer than the badges above it; skips
    // cleanly (no reserved space) when there's nothing to show.
    if let Some(preview) = data.preview.as_deref()
        && let Some(wrapped) = wrap_gist(preview, GIST_LINE_CHARS, GIST_MAX_LINES)
        && y < h - pad
    {
        let brush = gist_brush();
        let l = font.layout(
            &wrapped,
            &VelloTextStyle { font_size: GIST_FONT_SIZE * s, line_height: 1.15, ..default() },
            VelloTextAlign::Left,
            max_advance,
        );
        let lh = l.height();
        collect_field(&l, (pad as f64, y as f64), &brush, atlas, font_data_map, &mut out);
        y += lh + 4.0 * s;
    }

    // ── Keywords. ──
    let tail = data.keywords.join(" · ");
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
///
/// Dived-only (Slice C, `lovely-swimming-prism.md`): at room scale the card
/// text is unreadably small pixels, so this system doesn't even run while
/// ambient — no point rasterizing MSDF glyphs no one can read. That means a
/// card's data can change several times while ambient with this system never
/// observing it; `TimeWellState::card_text_dirty` (armed by `scene::arm_dive`
/// on every zoom-in) forces a full rebuild of every rim card once dived, not
/// just the ones `Changed<Card>` would happen to catch — belt-and-braces
/// alongside Bevy's own per-system change-tick tracking, which mirrors patch
/// bay's `text_dirty` idiom rather than leaning on a subtler assumption about
/// how long a gap in this system's own execution can safely be.
pub fn build_card_scenes(
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    mut materials: ResMut<Assets<WellCardMaterial>>,
    mut state: ResMut<TimeWellState>,
    mut query: Query<(
        Entity,
        &Card,
        &mut MsdfBlockGlyphs,
        &MeshMaterial3d<WellCardMaterial>,
    )>,
    changed: Query<Entity, Changed<Card>>,
) {
    let Some(font) = fonts.get(&font_handles.mono) else {
        return; // font still loading; retry next change, `card_text_dirty` untouched
    };
    let Some(atlas) = atlas.as_deref_mut() else {
        return; // atlas not ready yet; same retry contract
    };
    // Only clear the arm once glyphs are actually about to be committed for
    // every card (below) — same "don't eat the flag on a scheduling surprise"
    // discipline as `room::room_plate_text`'s `any` guard.
    let rebuild_all = state.card_text_dirty;
    let changed_set: std::collections::HashSet<Entity> = changed.iter().collect();
    for (entity, card, mut msdf, mat_node) in query.iter_mut() {
        if !rebuild_all && !changed_set.contains(&entity) {
            continue;
        }
        // Pure MSDF surface: the build size lives on the card's `UiRttTexture`
        // (set once at spawn); the MSDF pass clears and owns the texture. No
        // vello scene — the shader draws the body.
        let glyphs = card_text_glyphs(card, font, CARD_TEX_W, CARD_TEX_H, atlas, &mut font_data_map);
        commit_panel_glyphs(&mut msdf, glyphs);

        if let Some(mat) = materials.get_mut(&mat_node.0) {
            mat.accent = accent_vec4(&card.data.accent);
            mat.params = card_params(card);
        }
    }
    if rebuild_all {
        state.card_text_dirty = false;
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
    palette: Res<crate::view::scene_palette::ScenePalette>,
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
                let accent = accent_vec4(&card.data.accent);
                mat.accent = accent;
                // The focus card is the selection — no selection/lineage ring on it.
                mat.params = Vec4::ZERO;
                // Beveled frame: the accent lifted into HDR on the ring band.
                mat.border = (accent.truncate() * palette.gain_reading_border).extend(1.0);
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
                mat.border = Vec4::ZERO; // no frame around nothing
            }
            Vec::new()
        }
    };
    commit_panel_glyphs(&mut msdf, glyphs);
}

/// Lay out a single line of dim, LDR label text for [`build_horizon_label`].
/// **Landmine avoided**: the brush is passed
/// explicitly to both `layout` (registers the brush per glyph run) and
/// `collect_msdf_glyphs` below, or the text renders black
/// (`docs/timewell.md`, "Landmines"). Empty `text` lays out no glyphs (still a
/// valid, cleared-to-transparent commit — see the callers).
fn layout_label_text(
    text: &str,
    font: &VelloFont,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
) -> Vec<PositionedGlyph> {
    if text.is_empty() {
        return Vec::new();
    }
    let brush = label_brush();
    let layout = font.layout(
        text,
        &VelloTextStyle { font_size: LABEL_FONT_SIZE, line_height: 1.1, ..default() },
        VelloTextAlign::Middle,
        Some(LABEL_TEX_W - 2.0 * LABEL_PAD),
    );
    for line in layout.lines() {
        for item in line.items() {
            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                font_data_map.register(gr.run().font());
            }
        }
    }
    collect_msdf_glyphs(&layout, &[], &brush, (LABEL_PAD as f64, LABEL_PAD as f64), atlas)
}

/// Refresh the [`HorizonLabel`]'s "+N" text only when
/// [`TimeWellState::horizon_count`] actually changes (zero shows a blank
/// panel, not "+0" — nothing has spilled past the horizon). Same font-asset
/// gate as the other label/card builders.
pub fn build_horizon_label(
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    state: Res<TimeWellState>,
    mut query: Query<&mut MsdfBlockGlyphs, With<HorizonLabel>>,
    mut last: Local<Option<usize>>,
) {
    if *last == Some(state.horizon_count) {
        return;
    }
    let Some(font) = fonts.get(&font_handles.mono) else {
        return; // font still loading; retry next tick (leaves `last` unset)
    };
    let Ok(mut msdf) = query.single_mut() else {
        return; // label not spawned yet
    };
    *last = Some(state.horizon_count);

    let text = if state.horizon_count > 0 {
        format!("+{}", state.horizon_count)
    } else {
        String::new()
    };
    let glyphs = match atlas.as_deref_mut() {
        Some(atlas) => layout_label_text(&text, font, atlas, &mut font_data_map),
        None => Vec::new(),
    };
    commit_panel_glyphs(&mut msdf, glyphs);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_gist_none_for_blank_input() {
        assert_eq!(wrap_gist("", 20, 2), None);
        assert_eq!(wrap_gist("   ", 20, 2), None);
    }

    #[test]
    fn wrap_gist_short_text_passes_through_on_one_line() {
        let w = wrap_gist("a short gist", 40, 2).unwrap();
        assert_eq!(w, "a short gist");
        assert!(!w.contains('…'), "fits on one line, no ellipsis: {w}");
    }

    #[test]
    fn wrap_gist_wraps_to_two_lines_within_budget() {
        // "one two three four" (18 chars) / "five six seven eight" (20 chars)
        // — exactly two lines at line_chars=20, no overflow.
        let text = "one two three four five six seven eight";
        let w = wrap_gist(text, 20, 2).unwrap();
        let lines: Vec<&str> = w.lines().collect();
        assert_eq!(lines.len(), 2, "wraps to exactly two lines: {w:?}");
        for line in &lines {
            assert!(line.chars().count() <= 20, "line over budget: {line:?}");
        }
        assert!(!w.contains('…'), "content fit in two lines, no ellipsis: {w}");
    }

    #[test]
    fn wrap_gist_ellipsizes_when_content_overflows_two_lines() {
        let text = "one two three four five six seven eight nine ten eleven twelve";
        let w = wrap_gist(text, 15, 2).unwrap();
        let lines: Vec<&str> = w.lines().collect();
        assert_eq!(lines.len(), 2, "never exceeds max_lines: {w:?}");
        assert!(lines[1].ends_with('…'), "overflow past line 2 is elided: {w:?}");
    }

    #[test]
    fn wrap_gist_respects_max_lines_of_one() {
        let text = "one two three four five six";
        let w = wrap_gist(text, 15, 1).unwrap();
        assert_eq!(w.lines().count(), 1);
        assert!(w.ends_with('…'), "overflow past line 1 is elided: {w:?}");
    }
}
