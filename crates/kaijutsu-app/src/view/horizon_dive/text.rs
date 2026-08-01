//! MSDF text for the dive: card faces and the query/reading HUD.
//!
//! Same pipeline as the well's card text (`time_well::text`) — parley lays a
//! field out, [`collect_msdf_glyphs`] positions its glyphs, and
//! [`commit_panel_glyphs`] stamps them onto the panel's
//! [`MsdfBlockGlyphs`](crate::text::msdf::MsdfBlockGlyphs) so the generic MSDF
//! pass re-rasterizes. Nothing here touches vello.
//!
//! **Landmine, inherited from the well** (`docs/timewell.md`, "Landmines"):
//! the brush must be handed to BOTH `layout` and `collect_msdf_glyphs`, or the
//! text renders black on a dark scene — i.e. invisible. [`collect_field`] is
//! the single place that pairs them, so no call site here can get it wrong.

use bevy::prelude::*;
use vello::peniko::Brush;

use crate::text::components::bevy_color_to_brush;
use crate::text::msdf::{FontDataMap, MsdfAtlas, PositionedGlyph, collect_msdf_glyphs};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};

use super::corpus::HorizonContext;

/// Card-face texture size (logical px). Small on purpose: 300 of these live
/// at once, and 160×100 × 4 bytes × 300 ≈ 19 MB of RTT — the budget line the
/// "one panel per card" decision had to clear (`docs/horizon-dive.md`, "What
/// 300 cards cost").
pub const CARD_TEX_W: f32 = 160.0;
pub const CARD_TEX_H: f32 = 100.0;

/// HUD panel texture size — the query line, the result count, and the
/// selection's reading line.
pub const HUD_TEX_W: f32 = 720.0;
pub const HUD_TEX_H: f32 = 200.0;

/// Inner padding in card-texture space.
const CARD_PAD: f32 = 9.0;
/// Inner padding in HUD-texture space.
const HUD_PAD: f32 = 18.0;

/// Register a layout's fonts and collect its glyphs at `offset`, coloured by
/// `brush`. The one place brush-to-`layout` and brush-to-`collect` are paired
/// — see the module doc's landmine note.
fn collect_field(
    text: &str,
    font: &VelloFont,
    style: &VelloTextStyle,
    max_advance: Option<f32>,
    offset: (f64, f64),
    brush: &Brush,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
    out: &mut Vec<PositionedGlyph>,
) -> f32 {
    let layout = font.layout(text, style, VelloTextAlign::Left, max_advance);
    for line in layout.lines() {
        for item in line.items() {
            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                font_data_map.register(gr.run().font());
            }
        }
    }
    out.extend(collect_msdf_glyphs(&layout, &[], brush, offset, atlas));
    layout.height()
}

/// The card face: title, then a dim `state · keywords` line.
///
/// Deliberately thin next to the well's card (no model badge, no gist, no
/// tail). At dive distances anything more is unreadable pixels, and the
/// reading line in the HUD carries the detail for whatever is selected —
/// principle 5 says *cards*, not that every card must be a full dossier.
pub fn card_glyphs(
    ctx: &HorizonContext,
    font: &VelloFont,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
) -> Vec<PositionedGlyph> {
    let max_advance = Some(CARD_TEX_W - 2.0 * CARD_PAD);
    let mut out = Vec::new();
    let mut y = CARD_PAD + 4.0;

    let title_brush = bevy_color_to_brush(Color::srgb(0.98, 0.99, 1.0));
    y += collect_field(
        &ctx.title,
        font,
        &VelloTextStyle { font_size: 15.0, line_height: 1.1, ..default() },
        max_advance,
        (CARD_PAD as f64, y as f64),
        &title_brush,
        atlas,
        font_data_map,
        &mut out,
    ) + 5.0;

    let sub_brush = bevy_color_to_brush(Color::srgba(0.84, 0.90, 1.0, 0.62));
    let keywords = ctx.keywords.join(" · ");
    let sub = if keywords.is_empty() {
        ctx.state.label().to_string()
    } else {
        format!("{} · {keywords}", ctx.state.label())
    };
    collect_field(
        &sub,
        font,
        &VelloTextStyle { font_size: 9.0, line_height: 1.15, ..default() },
        max_advance,
        (CARD_PAD as f64, y as f64),
        &sub_brush,
        atlas,
        font_data_map,
        &mut out,
    );

    out
}

/// What the HUD renders. A plain struct rather than a pile of arguments so
/// [`hud_text`] — the pure composition — can be unit-tested without a font.
pub struct HudModel<'a> {
    /// The current query line.
    pub query: &'a str,
    /// Whether the query line has the keyboard (drives the caret).
    pub typing: bool,
    /// How many contexts the query lit.
    pub lit: usize,
    /// How many contexts exist past the horizon in total.
    pub total: usize,
    /// The selection's reading line, if anything is selected.
    pub selected: Option<&'a HorizonContext>,
    /// How many edges the selection revealed.
    pub edges: usize,
    /// Transient feedback from the last verb (`open`, `surface`).
    pub status: &'a str,
    /// Whether the key legend is showing.
    pub legend: bool,
}

/// The dive's key legend — rendered from this literal rather than from
/// `InputMap` labels, which is a **known shortcut** for the spike: the well's
/// legend reads the real binding table so a rebind shows up in the legend for
/// free, and this one would have to before it shipped (see
/// `docs/horizon-dive.md`, "Prototype notes").
const LEGEND: &str = "hjkl / arrows  snap   ·  /  query   ·  Enter  open   ·  p  surface\n\
                      ?  legend            ·  Esc  leave the dive";

/// Compose the HUD's three lines. Pure — unit-tested below.
pub fn hud_text(m: &HudModel<'_>) -> String {
    let caret = if m.typing { "▏" } else { "" };
    let query_line = if m.query.is_empty() && !m.typing {
        "/ search the horizon".to_string()
    } else {
        format!("/ {}{caret}", m.query)
    };

    let count_line = format!("{} lit · {} past the horizon", m.lit, m.total);

    let reading = match m.selected {
        Some(c) => {
            let keywords = if c.keywords.is_empty() {
                String::new()
            } else {
                format!(" · {}", c.keywords.join(" "))
            };
            format!("▸ {} · {}{keywords} · {} links", c.title, c.state.label(), m.edges)
        }
        None => "▸ nothing selected".to_string(),
    };

    let mut out = format!("{query_line}\n{count_line}\n{reading}");
    if !m.status.is_empty() {
        out.push_str(&format!("\n{}", m.status));
    }
    if m.legend {
        out.push('\n');
        out.push_str(LEGEND);
    }
    out
}

/// Lay the HUD out: the query line big and bright, everything under it small
/// and dim (the same alpha-taper hierarchy the well's card face uses).
pub fn hud_glyphs(
    m: &HudModel<'_>,
    font: &VelloFont,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
) -> Vec<PositionedGlyph> {
    let text = hud_text(m);
    let mut lines = text.lines();
    let max_advance = Some(HUD_TEX_W - 2.0 * HUD_PAD);
    let mut out = Vec::new();
    let mut y = HUD_PAD;

    if let Some(query_line) = lines.next() {
        // The query line is the primary input; it gets the only bright brush.
        let brush = bevy_color_to_brush(if m.typing {
            Color::srgb(1.0, 0.98, 0.90)
        } else {
            Color::srgba(0.90, 0.94, 1.0, 0.80)
        });
        y += collect_field(
            query_line,
            font,
            &VelloTextStyle { font_size: 26.0, line_height: 1.1, ..default() },
            max_advance,
            (HUD_PAD as f64, y as f64),
            &brush,
            atlas,
            font_data_map,
            &mut out,
        ) + 6.0;
    }

    let dim = bevy_color_to_brush(Color::srgba(0.84, 0.90, 1.0, 0.66));
    let rest: Vec<&str> = lines.collect();
    if !rest.is_empty() {
        collect_field(
            &rest.join("\n"),
            font,
            &VelloTextStyle { font_size: 15.0, line_height: 1.35, ..default() },
            max_advance,
            (HUD_PAD as f64, y as f64),
            &dim,
            atlas,
            font_data_map,
            &mut out,
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::super::corpus::{HorizonState, synth_corpus};
    use super::*;

    fn model<'a>(query: &'a str, typing: bool, selected: Option<&'a HorizonContext>) -> HudModel<'a> {
        HudModel {
            query,
            typing,
            lit: 12,
            total: 300,
            selected,
            edges: 3,
            status: "",
            legend: false,
        }
    }

    #[test]
    fn an_empty_idle_query_line_shows_the_prompt_not_a_bare_slash() {
        let text = hud_text(&model("", false, None));
        assert!(text.starts_with("/ search the horizon"), "{text}");
        assert!(!text.contains('▏'), "no caret when the query line isn't focused: {text}");
    }

    #[test]
    fn typing_shows_a_caret_even_with_an_empty_query() {
        let text = hud_text(&model("", true, None));
        assert!(text.starts_with("/ ▏"), "{text}");
    }

    #[test]
    fn the_count_line_reports_lit_out_of_total() {
        let text = hud_text(&model("wire", false, None));
        assert!(text.contains("12 lit · 300 past the horizon"), "{text}");
    }

    #[test]
    fn the_reading_line_carries_title_state_keywords_and_link_count() {
        let corpus = synth_corpus(20, 1, 1_800_000_000_000);
        let mut c = corpus[0].clone();
        c.title = "wire audit 0".into();
        c.keywords = vec!["capnp".into()];
        c.state = HorizonState::Archived;
        let text = hud_text(&model("", false, Some(&c)));
        assert!(text.contains("▸ wire audit 0 · archived · capnp · 3 links"), "{text}");
    }

    #[test]
    fn no_selection_says_so_rather_than_rendering_a_blank_row() {
        assert!(hud_text(&model("", false, None)).contains("▸ nothing selected"));
    }

    #[test]
    fn the_status_line_only_appears_when_there_is_one() {
        let mut m = model("", false, None);
        assert_eq!(hud_text(&m).lines().count(), 3);
        m.status = "surfaced → wire audit 0";
        let text = hud_text(&m);
        assert_eq!(text.lines().count(), 4);
        assert!(text.ends_with("surfaced → wire audit 0"), "{text}");
    }

    #[test]
    fn the_legend_appends_below_everything_else() {
        let mut m = model("", false, None);
        m.legend = true;
        let text = hud_text(&m);
        assert!(text.contains("snap"), "legend rows present: {text}");
        assert!(text.contains("Esc  leave the dive"), "{text}");
        let plain = hud_text(&model("", false, None));
        assert!(text.starts_with(&plain), "the legend must append, never reorder: {text}");
    }
}
