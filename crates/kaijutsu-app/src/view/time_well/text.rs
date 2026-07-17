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
//!
//! **HUD-melt slice 3** (`docs/timewell.md`): [`reading_card_glyphs`] absorbs
//! the reference material the retired edge HUD's East (specs) and West
//! (ancestry) panels used to carry — [`specs_text`]/[`ancestry_text`] are
//! that content's pure composition, extracted from those panels' own bodies
//! so both the HUD panels (retired in slice 4) and the reading card rendered
//! byte-identical text from one place.

use bevy::prelude::*;
use vello::peniko::Brush;

use kaijutsu_client::ContextInfo;
use kaijutsu_types::ContextId;
use kaijutsu_viz::join::Join;
use kaijutsu_viz::layout::Band;

use super::panel::commit_panel_glyphs;
use super::scene::{
    CARD_TEX_H, CARD_TEX_W, Card, CardParams, HorizonLabel, LABEL_TEX_W, READING_TEX_H,
    READING_TEX_W, ReadingCard, TimeWellState, accent_vec4,
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
/// "approximate width, not measured" idiom the retired HUD South panel's
/// `SOUTH_LINE_CHARS` used. Deliberately conservative: parley's real glyph
/// widths drive the actual on-screen wrap ([`VelloFont::layout`]'s
/// `break_all_lines`), so `wrap_gist` just needs each pre-wrapped line to be
/// safely short enough that parley never wraps it a second time.
///
/// `pub(super)`: also the per-line budget the selected card's live-tail band
/// uses (`live::sync_selected_card_tail`) — same card width, same tuning.
pub(super) const GIST_LINE_CHARS: usize = 40;
/// Hard cap on wrapped gist lines; content beyond this is ellipsized.
const GIST_MAX_LINES: usize = 2;

/// Dim brush for the gist line — dimmer than every badge brush above it
/// (title ~1.0 alpha, model badge 0.85, fork badge 0.65): this is the least
/// important line on the card face, a hint rather than a spec.
fn gist_brush() -> Brush {
    bevy_color_to_brush(Color::srgba(0.80, 0.86, 0.97, 0.55))
}

/// Tail lines shown on the reading card's absorbed tail cut (HUD-melt slice
/// 3) — deeper than the rim card's live-tail band (`super::live::CARD_TAIL_LINES`
/// is 3) since the reading card is the surface Enter dollies to for an actual
/// look, not a passing glance; proportionate to its larger face
/// ([`READING_TEX_H`] is 2× [`CARD_TEX_H`]). **Amy-tunable.**
const READING_TAIL_LINES: usize = 6;

/// Font size (logical px, scaled by the card's own `s` like every other
/// field) for a reading-card block's section label ("SPECS"/"LINEAGE") — one
/// notch above the dim body so the label reads as a header, not more body
/// text.
///
/// Budget note (live-verified 2026-07-12): the reading card shares the rim
/// card's proportional scale (`s = h / CARD_TEX_H`), so these sizes spend the
/// same face-fraction budget as rim fields. At the original 11/10 the specs
/// block alone reached `y_limit` and the overflow guard silently dropped
/// ancestry + tail — the content slice 3 exists to show. 8/7 fits
/// title + specs + a short chain + the tail cut with slack. **Amy-tunable.**
const READING_BLOCK_HEADER_SIZE: f32 = 8.0;
/// Font size for a reading-card block's body lines — smaller than the gist
/// line above it (this is reference material, denser and further down the
/// hierarchy than the card's own summary).
const READING_BLOCK_BODY_SIZE: f32 = 7.0;

/// Dim, slightly-brighter-than-body brush for a reading-card block's section
/// label — one step up from [`gist_brush`]'s dimness, the same alpha-taper
/// idiom every other field on the card already uses for hierarchy.
fn reading_block_header_brush() -> Brush {
    bevy_color_to_brush(Color::srgba(0.86, 0.93, 1.0, 0.75))
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

// ============================================================================
// HUD-MELT SLICE 3: absorbed panel content (pure text, no Bevy)
// ============================================================================
//
// [`specs_text`] and [`ancestry_text`] are the retired edge HUD's East/West
// panels' own bodies, extracted so both those panels (retired in slice 4) and
// the reading card's absorbed content ([`reading_card_glyphs`]) composed from
// the same pure text, byte-for-byte — the HUD's East/West panels became thin
// wrappers over these before their own retirement, so their tests (and
// output) never moved.

/// Specs block: model, fork kind, band, keywords, cluster, and the live
/// transport line for the track this context rides (`docs/tracks.md`). The
/// longest line sorts to the top, tapering to the shortest, so the block
/// nests into its top corner instead of ragging unevenly.
///
/// The track transport line rides along here until timewell Stage 3 gives it
/// a real home on a track surface — see `docs/timewell.md`.
#[allow(dead_code)] // orphaned by the HUD-melt slice 4 retirement (its only
// caller was the retired HUD East panel) — kept as a tested pure primitive;
// `docs/issues.md` tracks the "give it a real home or delete it" follow-up.
pub fn specs_text(d: &super::card::CardData, track: Option<&kaijutsu_client::TrackInfo>) -> String {
    let keys = if d.keywords.is_empty() {
        "—".to_string()
    } else {
        d.keywords.join(", ")
    };
    let mut lines = vec![
        format!("model    {}", if d.model_badge.is_empty() { "—" } else { &d.model_badge }),
        format!("fork     {}", d.fork_badge.as_deref().unwrap_or("—")),
        format!("band     {}", band_label(d.band)),
        format!("keywords {keys}"),
    ];
    if let Some(c) = &d.cluster_label {
        lines.push(format!("cluster  ◇ {c}"));
    }
    if let Some(t) = track {
        let bpm = (60_000_000f64 / t.period_us.max(1) as f64).round() as u64;
        let transport = if t.playing {
            format!("▶ ♩{bpm} · tick {}", t.playhead_tick)
        } else {
            "■ stopped".to_string()
        };
        lines.push(format!("track    ♪ {} {transport}", t.id));
    }
    lines.sort_by_key(|l| std::cmp::Reverse(l.chars().count()));
    format!("SPECS\n{}", lines.join("\n"))
}

/// The reading card's specs block: [`specs_text`] minus the model and fork
/// lines — the card's own header (title, model badge, fork badge) sits
/// directly above the block on that face, and repeating them spent vertical
/// budget the ancestry chain + tail cut need (live-verified 2026-07-12: the
/// duplicated lines pushed both past `y_limit` on typical content).
/// [`specs_text`] itself stays the full block — the retired HUD East panel
/// had no header of its own to lean on.
pub fn reading_specs_text(
    d: &super::card::CardData,
    track: Option<&kaijutsu_client::TrackInfo>,
) -> String {
    let keys = if d.keywords.is_empty() {
        "—".to_string()
    } else {
        d.keywords.join(", ")
    };
    let mut lines = vec![
        format!("band     {}", band_label(d.band)),
        format!("keywords {keys}"),
    ];
    if let Some(c) = &d.cluster_label {
        lines.push(format!("cluster  ◇ {c}"));
    }
    if let Some(t) = track {
        let bpm = (60_000_000f64 / t.period_us.max(1) as f64).round() as u64;
        let transport = if t.playing {
            format!("▶ ♩{bpm} · tick {}", t.playhead_tick)
        } else {
            "■ stopped".to_string()
        };
        lines.push(format!("track    ♪ {} {transport}", t.id));
    }
    lines.sort_by_key(|l| std::cmp::Reverse(l.chars().count()));
    format!("SPECS\n{}", lines.join("\n"))
}

fn band_label(band: Band) -> &'static str {
    match band {
        Band::Active => "active",
        Band::Recent => "recent",
        Band::Bumped => "bumped",
        Band::Demoted => "demoted",
    }
}

/// Ancestry chain as text: walk the fork lineage upward (this ◂ parent ◂ …),
/// titles from `title_and_parent` — same shape as [`super::card::ancestors`]'s
/// own `parent_of` closure, paired with a title so the chain prints without a
/// second lookup.
///
/// Unlike the lineage drapes ([`super::drape::sync_lineage_drapes`], slice
/// 1), which only draw a ribbon to an ancestor with a live `Card` entity,
/// this has no such requirement: `title_and_parent` returning `None` — an id
/// with no join entry, i.e. an ancestor past the event horizon
/// (`sync_time_well` never joins those) — still prints that ancestor's short
/// id and stops the walk there for lack of further parent info, rather than
/// silently vanishing. That's the completeness the reading card's ancestry
/// block (HUD-melt slice 3) leans on: the drapes show topology, this chain
/// carries the paper trail one generation further.
///
/// Stops after 6 generations (guards a pathologically deep chain); an
/// immediate self-loop (`forked_from == self`) ends the walk on the spot,
/// and any longer cycle is still bounded by the depth cap, so a malformed
/// lineage can't hang the card.
///
/// `"(root)"` is appended only when the **selected** context itself has no
/// parent (a one-entry chain) — reaching an ancestor with no further parent
/// several generations up does not retroactively mark it. Pre-existing
/// behavior of the retired HUD West panel, kept as-is by this extraction
/// rather than "fixed".
pub fn ancestry_text(
    selected: ContextId,
    title_and_parent: impl Fn(ContextId) -> Option<(String, Option<ContextId>)>,
) -> String {
    let mut out = String::from("LINEAGE\n");
    let mut cur = Some(selected);
    let mut depth = 0;
    while let Some(id) = cur {
        let (title, parent) = match title_and_parent(id) {
            Some(tp) => tp,
            None => (id.short(), None),
        };
        if depth == 0 {
            out.push_str(&title);
        } else {
            out.push_str(&format!("\n◂ {title}"));
        }
        depth += 1;
        if depth >= 6 {
            out.push_str("\n◂ …");
            break;
        }
        cur = parent;
        if cur == Some(id) {
            break;
        }
    }
    if depth == 1 {
        out.push_str("\n(root)");
    }
    out
}

// HDR gain for the focus card's bevel frame (the `border` ring band: the
// selection's accent lifted so the frame reads as a deliberate beveled edge,
// replacing the accidental cream ring the pre-mask-fix chatter/beat lanes
// used to paint; a touch under the legend panel's `gain_hud_border` so the
// in-world card doesn't outshine the legend) moved onto
// `ScenePalette::gain_reading_border`.

/// `WellCardMaterial.params` for a card: `[selected, in_lineage, status, drifting]`.
/// `status` is a float code the shader switches on for the rim FX: pending/none →
/// 0, running → 1 (breathing pulse), done → 2 (no rim), error → 3 (steady red).
/// `drifting` (0/1) gates the drift sheen sweep. Time-based animation reads
/// `globals.time` in the shader.
fn card_params(params: &CardParams) -> Vec4 {
    use kaijutsu_types::Status;
    let status = match params.status {
        Some(Status::Running) => 1.0,
        Some(Status::Done) => 2.0,
        Some(Status::Error) => 3.0,
        _ => 0.0, // None or Pending
    };
    Vec4::new(
        if params.selected { 1.0 } else { 0.0 },
        if params.in_lineage { 1.0 } else { 0.0 },
        status,
        if params.drifting { 1.0 } else { 0.0 },
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

/// Lay out the shared "title/badges up top" header every card face shows —
/// title, the paused marker, model badge, fork badge — appending glyphs into
/// `out` and returning the y-cursor so the caller can keep flowing content
/// below it. Shared by the rim card ([`card_text_glyphs`]) and the reading
/// card ([`reading_card_glyphs`], HUD-melt slice 3): both faces start
/// identically and diverge only below this point (rim: gist/tail/keywords/
/// cluster; reading: the absorbed specs/ancestry/tail blocks), so this is the
/// seam between them, extracted so neither copy can drift from the other.
fn card_header_glyphs(
    card: &Card,
    font: &VelloFont,
    max_advance: Option<f32>,
    s: f32,
    pad: f32,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
    out: &mut Vec<PositionedGlyph>,
) -> f32 {
    let data = &card.data;
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
    collect_field(&title, (pad as f64, y as f64), &title_brush, atlas, font_data_map, out);
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
        collect_field(&marker, (pad as f64, y as f64), &brush, atlas, font_data_map, out);
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
        collect_field(&badge, (pad as f64, y as f64), &brush, atlas, font_data_map, out);
        y += bh + 4.0 * s;
    }

    // ── Fork badge. ──
    if let Some(fork) = &data.fork_badge {
        let brush = bevy_color_to_brush(Color::srgba(1.0, 1.0, 1.0, 0.65));
        // Plain word, not U+2442 ⑂ — the shaping font has no glyph for it
        // and renders tofu (live-verified 2026-07-12).
        let label = format!("fork {fork}");
        let l = font.layout(
            &label,
            &VelloTextStyle { font_size: 12.0 * s, ..default() },
            VelloTextAlign::Left,
            max_advance,
        );
        let lh = l.height();
        collect_field(&l, (pad as f64, y as f64), &brush, atlas, font_data_map, out);
        y += lh + 4.0 * s;
    }

    y
}

/// Lay out the card's text fields and collect MSDF glyphs (crisp at any zoom).
/// Each field lands at the same `(pad, y)` origin and color the card used
/// before. Rim card only (the sole caller is [`build_card_scenes`]) — the
/// reading/focus card has its own composition as of HUD-melt slice 3,
/// [`reading_card_glyphs`], which shares [`card_header_glyphs`] with this
/// function but diverges below it (specs/ancestry/tail-cut instead of
/// gist/keywords/cluster), so it's a sibling, not a caller, of this one.
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
    let mut y = card_header_glyphs(card, font, max_advance, s, pad, atlas, font_data_map, &mut out);

    // ── Gist line(s). ── Sentence-level extractive summary (Tier 2,
    // `kaijutsu-kernel::runtime::synthesis::compute_gist`), riding the same
    // `CardData.preview` field the old 80-char block-head preview used — no
    // wire change, just richer content landing here (and, formerly, in the
    // retired HUD South panel's fallback) for free. Smaller and dimmer than
    // the badges above it; skips cleanly (no reserved space) when there's
    // nothing to show.
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

    // ── Live-tail band (HUD-melt slice 2, replacing the South HUD panel's
    // job on the card face itself: `docs/timewell.md`, "Lineage drapes down
    // the bowl wall" 's sibling slice). `card.tail` is `None` for every
    // non-selected card and for a selected card with no live lines yet —
    // same style as the gist line above it (small, dim), one row lower. ──
    if let Some(tail_text) = card.tail.as_deref()
        && y < h - pad
    {
        let brush = gist_brush();
        let l = font.layout(
            tail_text,
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

/// Lay out one of the reading card's absorbed blocks (specs, ancestry, or the
/// deeper tail cut — HUD-melt slice 3) at `y`. `has_header` splits a leading
/// `"HEADER\nbody"` line (the shape [`specs_text`]/[`ancestry_text`] return)
/// into a brighter [`READING_BLOCK_HEADER_SIZE`] label and a dim
/// [`READING_BLOCK_BODY_SIZE`] body; `false` renders the whole string as one
/// dim run (the tail cut carries no such label, same as the rim card's own
/// live-tail band and HUD South before it). Skips (returns `y` unchanged)
/// once the flowing content has already reached `y_limit` — the same
/// overflow guard every block in [`card_text_glyphs`] uses, so a small
/// selection with a short chain never wastes the reading card's own bottom
/// margin. Returns the y-cursor after the block.
fn layout_reading_block(
    text: &str,
    font: &VelloFont,
    max_advance: Option<f32>,
    s: f32,
    pad: f32,
    y: f32,
    y_limit: f32,
    has_header: bool,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
    out: &mut Vec<PositionedGlyph>,
) -> f32 {
    if text.is_empty() || y >= y_limit {
        return y;
    }
    let mut y = y;
    let (header, body) = if has_header {
        match text.split_once('\n') {
            Some((h, b)) => (Some(h), b),
            None => (None, text),
        }
    } else {
        (None, text)
    };
    if let Some(header) = header {
        let l = font.layout(
            header,
            &VelloTextStyle { font_size: READING_BLOCK_HEADER_SIZE * s, ..default() },
            VelloTextAlign::Left,
            max_advance,
        );
        let lh = l.height();
        collect_field(&l, (pad as f64, y as f64), &reading_block_header_brush(), atlas, font_data_map, out);
        y += lh + 3.0 * s;
    }
    if !body.is_empty() {
        let l = font.layout(
            body,
            &VelloTextStyle { font_size: READING_BLOCK_BODY_SIZE * s, line_height: 1.3, ..default() },
            VelloTextAlign::Left,
            max_advance,
        );
        let lh = l.height();
        collect_field(&l, (pad as f64, y as f64), &gist_brush(), atlas, font_data_map, out);
        y += lh + 10.0 * s;
    }
    y
}

/// The reading (focus) card's own text composition (HUD-melt slice 3): the
/// shared title/badges header ([`card_header_glyphs`]), then the specs block
/// ([`specs_text`]), the ancestry chain ([`ancestry_text`]), and a deeper
/// tail cut ([`READING_TAIL_LINES`]) than the rim card's live-tail band — the
/// reference material the edge HUD's East/West/South panels carried before
/// the melt, retired in slice 4.
///
/// No gist/keywords/cluster repeat: the specs block already carries keywords
/// + cluster, and this card is reference material at reading distance, not a
/// bigger rim-card face.
///
/// `tail` is a **snapshot**, not a live window: [`update_reading_card`]'s own
/// change-guard only fires on a selection change (the same reasoning the
/// pre-slice-3 reading card drew from `card_text_glyphs`'s live-tail band —
/// see `docs/timewell.md`), so a tail that kept updating while the selection
/// stayed put would go stale here. Acceptable: the reading card is what you
/// get when you dolly in and look, not a live ticker.
fn reading_card_glyphs(
    card: &Card,
    join: &Join<ContextId, ContextInfo>,
    track: Option<&kaijutsu_client::TrackInfo>,
    tail: Option<&str>,
    font: &VelloFont,
    w: f32,
    h: f32,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
) -> Vec<PositionedGlyph> {
    let s = h / CARD_TEX_H;
    let pad = PAD * s;
    let max_advance = Some(w - 2.0 * pad);
    let y_limit = h - pad;
    let mut out = Vec::new();
    let mut y = card_header_glyphs(card, font, max_advance, s, pad, atlas, font_data_map, &mut out);

    let specs = reading_specs_text(&card.data, track);
    y = layout_reading_block(
        &specs, font, max_advance, s, pad, y, y_limit, true, atlas, font_data_map, &mut out,
    );

    let ancestry = ancestry_text(card.context_id, |id| {
        join.get(&id).map(|c| (c.id.display_or(Some(c.label.as_str())), c.forked_from))
    });
    y = layout_reading_block(
        &ancestry, font, max_advance, s, pad, y, y_limit, true, atlas, font_data_map, &mut out,
    );

    if let Some(tail_text) = tail {
        layout_reading_block(
            tail_text, font, max_advance, s, pad, y, y_limit, false, atlas, font_data_map, &mut out,
        );
    }

    out
}

/// Rebuild a rim card's MSDF text glyphs + sync its material's accent when its
/// data changes. No vello — the shader draws the body; MSDF owns the text
/// texture. Shader-param-only state (selection/lineage/drift/status) lives on
/// the sibling [`CardParams`] component and syncs separately, via
/// [`sync_card_material_params`] — split out (2026-07-17) so flipping those
/// flags doesn't also trip *this* system's `Changed<Card>` gate and force a
/// full glyph re-layout for text that never changed.
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
        }
    }
    if rebuild_all {
        state.card_text_dirty = false;
    }
}

/// Sync `WellCardMaterial.params` from [`CardParams`] — the shader-uniform-only
/// half of the material sync [`build_card_scenes`] used to do in one pass.
/// Split out (2026-07-17) so a selection/lineage/drift/status flip doesn't
/// route through `build_card_scenes`'s `Changed<Card>` gate and force a full
/// MSDF glyph re-layout for text that never changed — `card_params` is a few
/// enum matches, cheap enough to skip that gate entirely. `CardParams`'s own
/// write sites (`scene::highlight_selection`/`highlight_lineage`/
/// `highlight_drift`, `sync::sync_time_well`'s status refresh) already guard
/// their writes to fire only on a real flip, so gating on `Changed<CardParams>`
/// here is free — no extra staleness, just a narrower rebuild trigger.
///
/// Same dived-only cadence as `build_card_scenes` (see its own doc): the rim
/// card face isn't legible at room scale, so there is no ambient-tier variant
/// to keep in sync with here either.
pub fn sync_card_material_params(
    mut materials: ResMut<Assets<WellCardMaterial>>,
    query: Query<(&CardParams, &MeshMaterial3d<WellCardMaterial>), Changed<CardParams>>,
) {
    for (params, mat_node) in query.iter() {
        if let Some(mat) = materials.get_mut(&mat_node.0) {
            mat.params = card_params(params);
        }
    }
}

/// Render the current selection into the center-bottom focus card (MSDF text +
/// material body) at the larger reading size. Rebuilds only on selection
/// change.
///
/// HUD-melt slice 3: the reading card's content is [`reading_card_glyphs`],
/// not the rim card's [`card_text_glyphs`] — it absorbs the specs block, the
/// ancestry chain, and a deeper tail cut (the reference material the HUD's
/// East/West/South panels carried). `tracks`/`tails` are new params this
/// slice adds so that content has a track/tail to read at focus time; both
/// are read once per selection change (the existing `last` guard below), not
/// per frame — the tail is a snapshot, not a live-updating window (see
/// [`reading_card_glyphs`]'s own doc).
pub fn update_reading_card(
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    mut materials: ResMut<Assets<WellCardMaterial>>,
    state: Res<TimeWellState>,
    tracks: Res<super::rays::WellTracks>,
    tails: Res<super::live::ContextTails>,
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
                    let track = tracks.track_info_of(&card.context_id);
                    let tail = super::live::tail_lines(
                        &tails,
                        card.context_id,
                        READING_TAIL_LINES,
                        GIST_LINE_CHARS,
                    );
                    reading_card_glyphs(
                        card,
                        &state.join,
                        track,
                        tail.as_deref(),
                        font,
                        READING_TEX_W,
                        READING_TEX_H,
                        atlas,
                        &mut font_data_map,
                    )
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

    // ── specs_text (HUD-melt slice 3: formerly the East/West HUD panel's body; panels retired slice 4) ──

    fn card_data(band: Band) -> super::super::card::CardData {
        super::super::card::CardData {
            title: "alpha".into(),
            accent: "coder".into(),
            model_badge: "anthropic/claude-opus-4-8".into(),
            fork_badge: Some("full".into()),
            keywords: vec!["rings".into(), "pulse".into()],
            preview: None,
            band,
            forked_from: None,
            cluster_label: None,
            paused: false,
        }
    }

    #[test]
    fn specs_text_lists_fields_with_dash_fallbacks() {
        let mut d = card_data(Band::Recent);
        d.model_badge = String::new();
        d.fork_badge = None;
        d.keywords = vec![];
        let s = specs_text(&d, None);
        assert!(s.starts_with("SPECS\n"), "headed block: {s}");
        assert!(s.contains("model    —"), "empty model → dash: {s}");
        assert!(s.contains("fork     —"), "no fork → dash: {s}");
        assert!(s.contains("keywords —"), "no keywords → dash: {s}");
        assert!(s.contains("band     recent"), "band labeled: {s}");
        assert!(!s.contains("cluster"), "no cluster line when unclustered: {s}");
        assert!(!s.contains("track"), "no track line when unattached: {s}");
    }

    #[test]
    fn reading_specs_text_omits_the_header_duplicated_lines() {
        let mut d = card_data(Band::Recent);
        d.cluster_label = Some("storage".into());
        let s = reading_specs_text(&d, None);
        assert!(s.starts_with("SPECS\n"), "headed block: {s}");
        assert!(!s.contains("model"), "model lives in the card header: {s}");
        assert!(!s.contains("fork"), "fork lives in the card header: {s}");
        assert!(s.contains("band     recent"), "band kept: {s}");
        assert!(s.contains("keywords rings, pulse"), "keywords kept: {s}");
        assert!(s.contains("cluster  ◇ storage"), "cluster kept: {s}");
    }

    #[test]
    fn specs_text_shows_cluster_only_when_present() {
        let mut d = card_data(Band::Demoted);
        d.cluster_label = Some("storage".into());
        let s = specs_text(&d, None);
        assert!(s.contains("cluster  ◇ storage"), "cluster line present: {s}");
        assert!(s.contains("rings, pulse"), "keywords joined: {s}");
    }

    #[test]
    fn specs_text_shows_the_track_line_with_live_transport() {
        let mut t = kaijutsu_client::TrackInfo {
            id: "bass".into(),
            score_context_id: ContextId::from_bytes([9; 16]),
            playing: true,
            playhead_tick: 128,
            period_us: 500_000, // 120 BPM
            beats_per_phrase: 32,
            beat_count: 128,
            last_epoch_ns: 1,
            clock_kind: "system".into(),
            attached: vec![],
        };
        let s = specs_text(&card_data(Band::Active), Some(&t));
        assert!(s.contains("track    ♪ bass ▶ ♩120 · tick 128"), "playing: {s}");

        t.playing = false;
        let s = specs_text(&card_data(Band::Active), Some(&t));
        assert!(s.contains("track    ♪ bass ■ stopped"), "stopped: {s}");
    }

    // ── ancestry_text (HUD-melt slice 3: formerly the East/West HUD panel's body; panels retired slice 4) ──

    fn ctx(n: u8) -> ContextId {
        ContextId::from_bytes([n; 16])
    }

    #[test]
    fn ancestry_text_single_generation_marks_root() {
        let a = ctx(1);
        let out = ancestry_text(a, |_| Some(("alpha".into(), None)));
        assert_eq!(out, "LINEAGE\nalpha\n(root)");
    }

    #[test]
    fn ancestry_text_walks_the_chain_newest_first_no_marker_on_itself() {
        let (a, b, c) = (ctx(1), ctx(2), ctx(3));
        let titles: std::collections::HashMap<ContextId, (String, Option<ContextId>)> = [
            (a, ("child".to_string(), Some(b))),
            (b, ("parent".to_string(), Some(c))),
            (c, ("grandparent".to_string(), None)),
        ]
        .into_iter()
        .collect();
        let out = ancestry_text(a, |id| titles.get(&id).cloned());
        // `(root)` only fires when the SELECTED context itself has no parent
        // (see `ancestry_text_single_generation_marks_root`) — reaching an
        // ancestor with no further parent several generations up does not
        // retroactively mark it, a pre-existing quirk of the retired HUD West
        // panel this extraction keeps byte-identical rather than "fixing".
        assert_eq!(out, "LINEAGE\nchild\n◂ parent\n◂ grandparent");
    }

    #[test]
    fn ancestry_text_stops_at_an_ancestor_past_the_event_horizon() {
        // `b` has no join entry (a horizon ancestor — `sync_time_well` never
        // joins those): the chain still prints its short id, but can't
        // recurse further without parent info.
        let (a, b) = (ctx(1), ctx(2));
        let titles: std::collections::HashMap<ContextId, (String, Option<ContextId>)> =
            [(a, ("child".to_string(), Some(b)))].into_iter().collect();
        let out = ancestry_text(a, |id| titles.get(&id).cloned());
        assert_eq!(out, format!("LINEAGE\nchild\n◂ {}", b.short()));
        assert!(!out.contains("(root)"), "stopping short of a parent isn't the same as being one: {out}");
    }

    #[test]
    fn ancestry_text_caps_depth_at_six_generations() {
        let ids: Vec<ContextId> = (1..=8).map(ctx).collect();
        let titles: std::collections::HashMap<ContextId, (String, Option<ContextId>)> = (0..8)
            .map(|i| {
                let parent = ids.get(i + 1).copied();
                (ids[i], (format!("gen{i}"), parent))
            })
            .collect();
        let out = ancestry_text(ids[0], |id| titles.get(&id).cloned());
        assert!(out.ends_with("\n◂ …"), "depth cap trails off: {out}");
        // Header + 6 generations (gen0..gen5) + the ellipsis trailer.
        assert_eq!(out.lines().count(), 8, "header + 6 generations + ellipsis: {out:?}");
    }

    #[test]
    fn ancestry_text_self_parent_breaks_the_walk_without_hanging() {
        // A corrupted lineage (a context listing itself as its own
        // `forked_from`) must not spin forever: the immediate-self-loop
        // guard breaks the walk right after the first entry, same as a
        // genuine root (depth stays 1) — a malformed lineage reads as "no
        // known ancestor" rather than hanging the card.
        let a = ctx(1);
        let out = ancestry_text(a, move |_id| Some(("a".to_string(), Some(a))));
        assert_eq!(out, "LINEAGE\na\n(root)");
    }

    #[test]
    fn ancestry_text_longer_cycle_still_terminates_via_the_depth_cap() {
        // A 2-cycle (a's parent is b, b's parent is a) has no immediate
        // self-loop to catch, so the depth cap is what stops it — same
        // bound as `ancestry_text_caps_depth_at_six_generations`.
        let (a, b) = (ctx(1), ctx(2));
        let out = ancestry_text(a, move |id| {
            if id == a {
                Some(("a".to_string(), Some(b)))
            } else {
                Some(("b".to_string(), Some(a)))
            }
        });
        assert_eq!(out, "LINEAGE\na\n◂ b\n◂ a\n◂ b\n◂ a\n◂ b\n◂ …");
    }
}
