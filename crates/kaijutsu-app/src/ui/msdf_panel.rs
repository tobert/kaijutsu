//! The centered MSDF text panel the approval surfaces are drawn on.
//!
//! `ui::ask_sheet` and `ui::ledger_ribbon` are the same object with
//! different rows: one Bevy UI node carrying
//! [`msdf_surface_bundle`](crate::view::block_render::msdf_surface_bundle),
//! glyphs rebuilt in `PostUpdate` from a pure `Vec<`[`PanelLine`]`>`, a
//! `GlobalZIndex` from `constants::ZLayer`. That is `ui::quick_context`'s
//! shape exactly, and the machinery it shares with the docks
//! (`collect_dock_text_glyphs`, `measure_text`) is reused here rather than
//! copied — this module adds only what a *centered, width-driven* panel
//! needs that the quick-context peek does not:
//!
//! - a width taken from the window instead of from the content, so the panel
//!   is a fixed share of the frame and the rows lay out to fit it;
//! - the character column count that share buys, so wrapping and truncation
//!   are decided in characters (the chrome font is monospace) rather than by
//!   re-measuring every candidate line;
//! - [`wrap`] and [`truncate`], the two row policies the surfaces need: the
//!   sheet wraps so nothing is lost, the ribbon truncates so rows stay rows.
//!
//! **Rows in, glyphs out.** [`collect_panel_glyphs`] takes laid-out rows and
//! returns positioned glyphs plus the height they occupy. It reads no
//! resource and knows nothing about asks, so presenting the same glyph
//! texture on a camera-anchored quad in the room later is a matter of
//! pointing a different surface at it (`docs/scenes/`).

use bevy::prelude::*;

use crate::text::msdf::{FontDataMap, MsdfAtlas, PositionedGlyph};
use crate::text::shaping::VelloFont;
use crate::text::{bevy_color_to_brush, ShapingFonts};
use crate::ui::dock::{collect_dock_text_glyphs, measure_text as measure_dock_text};
use crate::ui::quick_context::{tone_color, LineTone, PanelLine};
use crate::ui::theme::Theme;

/// Body-row font size — the dock's small-widget size, which both approval
/// surfaces inherit so their rows line up with the chrome around them.
pub const ROW_FONT_SIZE: f32 = 13.0;
/// Section-heading font size.
pub const HEAD_FONT_SIZE: f32 = 12.0;
/// Baseline-to-baseline spacing, logical px.
pub const LINE_HEIGHT: f64 = 18.0;
/// Inner padding, logical px.
pub const PAD: f64 = 12.0;

/// Share of the window width a centered approval panel occupies (Amy,
/// 2026-09-12: "the center ~65% of the frame width").
pub const PANEL_WIDTH_FRACTION: f32 = 0.65;

/// Top edge of a centered panel, logical px — clear of the 40px north dock,
/// the same gap `ui::quick_context` leaves.
pub const PANEL_TOP: f32 = 52.0;

/// Fraction of the window height a panel may grow to before its rows start
/// scrolling instead. Leaves the south dock and a margin visible, so the
/// player can still see the `!n` marker and the hints line the panel is an
/// answer to.
pub const PANEL_MAX_HEIGHT_FRACTION: f32 = 0.72;

/// Panel background alpha. Fully opaque, unlike the quick-context peek: this
/// is a surface you make a decision from, and a statement you are about to
/// allow must not be read through a transcript behind it.
pub const PANEL_BG_ALPHA: f32 = 1.0;

/// The ellipsis both row policies mark an elision with. A truncation that
/// did not say so would pass a fragment off as the whole statement.
pub const ELLIPSIS: char = '\u{2026}';

// ============================================================================
// GEOMETRY (pure)
// ============================================================================

/// A centered panel's pixel width for a window this wide, clamped so it stays
/// readable on a narrow window and does not sprawl on a wide one.
pub fn panel_width(window_width: f32) -> f32 {
    (window_width * PANEL_WIDTH_FRACTION).clamp(360.0, 1100.0)
}

/// Left inset that centers a panel of [`panel_width`] in the window.
pub fn panel_left(window_width: f32) -> f32 {
    ((window_width - panel_width(window_width)) * 0.5).max(0.0)
}

/// How many characters of the chrome font fit across a panel's text area.
///
/// `char_width` is one monospace advance, measured once per rebuild
/// ([`measure_char_width`]). A zero or negative advance — a font that has
/// not finished loading — yields a floor of one column rather than a divide
/// by zero, and the rows it produces are honest (narrow), not absent.
pub fn columns(width: f64, char_width: f64) -> usize {
    if char_width <= 0.0 {
        return 1;
    }
    (((width - PAD * 2.0) / char_width).floor() as isize).max(1) as usize
}

/// One monospace advance in the chrome font at [`ROW_FONT_SIZE`].
pub fn measure_char_width(font: &VelloFont) -> f64 {
    measure_dock_text("0", ROW_FONT_SIZE, font)
}

/// How many rows fit in a panel of this pixel height.
pub fn rows_for_height(height: f64) -> usize {
    (((height - PAD * 2.0) / LINE_HEIGHT).floor() as isize).max(1) as usize
}

// ============================================================================
// ROW POLICIES (pure)
// ============================================================================

/// Wrap `text` to `cols` characters, indenting every continuation row by
/// `indent` spaces so a wrapped statement still reads as one statement.
///
/// Breaks on whitespace where it can and mid-word where it cannot (a long
/// path has no spaces, and must still fit). Control characters become spaces
/// — a ledger statement is kernel-supplied text, and a stray newline in it
/// must not tear the panel's row grid apart. Always returns at least one row,
/// so a caller never has to special-case empty text.
pub fn wrap(text: &str, cols: usize, indent: usize) -> Vec<String> {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cols == 0 {
        return vec![cleaned];
    }

    let mut out: Vec<String> = Vec::new();
    let mut rest: Vec<char> = cleaned.chars().collect();
    loop {
        let pad = if out.is_empty() { 0 } else { indent.min(cols.saturating_sub(1)) };
        let room = cols.saturating_sub(pad).max(1);
        if rest.len() <= room {
            out.push(format!("{}{}", " ".repeat(pad), rest.iter().collect::<String>()));
            break;
        }
        // Prefer the last space that fits; fall back to a hard cut.
        let cut = rest[..=room]
            .iter()
            .rposition(|c| *c == ' ')
            .filter(|at| *at > 0)
            .unwrap_or(room);
        let head: String = rest[..cut].iter().collect();
        out.push(format!("{}{}", " ".repeat(pad), head.trim_end()));
        rest = rest[cut..].to_vec();
        while rest.first() == Some(&' ') {
            rest.remove(0);
        }
        if rest.is_empty() {
            break;
        }
    }
    out
}

/// Shorten `text` to `cols` characters, marking the cut with [`ELLIPSIS`].
/// Control characters become spaces, for the reason [`wrap`] gives.
pub fn truncate(text: &str, cols: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.chars().count() <= cols {
        return cleaned;
    }
    if cols == 0 {
        return String::new();
    }
    let head: String = cleaned.chars().take(cols.saturating_sub(1)).collect();
    format!("{head}{ELLIPSIS}")
}

/// One row of a scrolling region, plus the marker that says how much of it is
/// out of sight.
///
/// Nothing scrolls away silently: when `rows` cannot hold `body`, the window
/// ends with a dim `\u{2191}n \u{2193}m` row naming what is above and below
/// it. `scroll` is clamped, so a stale offset shows the end of the body
/// rather than nothing at all.
pub fn scroll_window(body: &[PanelLine], scroll: usize, rows: usize) -> Vec<PanelLine> {
    if rows == 0 {
        return Vec::new();
    }
    if body.len() <= rows {
        return body.to_vec();
    }
    // One row of the window goes to the marker.
    let shown = rows - 1;
    let max_scroll = body.len() - shown;
    let at = scroll.min(max_scroll);
    let mut out = body[at..at + shown].to_vec();
    out.push(PanelLine {
        text: format!("\u{2191}{} \u{2193}{}", at, body.len() - at - shown),
        tone: LineTone::Dim,
    });
    out
}

/// The largest `scroll` offset that still shows content, for `j` to stop at.
pub fn max_scroll(body_len: usize, rows: usize) -> usize {
    if rows <= 1 || body_len <= rows {
        return 0;
    }
    body_len - (rows - 1)
}

// ============================================================================
// GLYPHS
// ============================================================================

/// Turn laid-out rows into positioned glyphs, and return the panel height
/// they occupy.
///
/// [`LineTone::Head`] rows are drawn at [`HEAD_FONT_SIZE`], every other row
/// at [`ROW_FONT_SIZE`]; the tone also picks the brush, through
/// `quick_context::tone_color`, so a theme that never heard of these panels
/// still styles them.
pub fn collect_panel_glyphs(
    rows: &[PanelLine],
    theme: &Theme,
    font: &VelloFont,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
) -> (Vec<PositionedGlyph>, f64) {
    let mut glyphs: Vec<PositionedGlyph> = Vec::new();
    let mut y = PAD;
    for row in rows {
        let size = if row.tone == LineTone::Head {
            HEAD_FONT_SIZE
        } else {
            ROW_FONT_SIZE
        };
        let brush = bevy_color_to_brush(tone_color(row.tone, theme));
        collect_dock_text_glyphs(
            &mut glyphs,
            &row.text,
            PAD,
            y,
            size,
            font,
            &brush,
            atlas,
            font_data_map,
        );
        y += LINE_HEIGHT;
    }
    (glyphs, y + PAD - LINE_HEIGHT * 0.25)
}

/// The resources every panel rebuild needs, bundled so the surfaces' render
/// systems stay inside Bevy's tuple-arity limit for system parameters.
#[derive(bevy::ecs::system::SystemParam)]
pub struct PanelFonts<'w> {
    pub fonts: Res<'w, Assets<VelloFont>>,
    pub handles: Res<'w, ShapingFonts>,
    pub atlas: Option<ResMut<'w, MsdfAtlas>>,
    pub font_data_map: ResMut<'w, FontDataMap>,
}

/// Spawn a centered panel with its MSDF surface child, hidden, as a child of
/// `root` — the same parent the docks and the quick-context overlay use, so
/// it lives in the app's one UI tree.
///
/// `panel_marker` marks the positioned container (visibility and background
/// live there); `surface_marker` marks the glyph surface inside it.
pub fn spawn_centered_panel<P: Component, S: Component>(
    commands: &mut Commands,
    root: Entity,
    theme: &Theme,
    material: Handle<crate::shaders::BlockFxMaterial>,
    z: i32,
    panel_marker: P,
    surface_marker: S,
) {
    let panel = commands
        .spawn((
            panel_marker,
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(PANEL_TOP),
                left: Val::Px(0.0),
                width: Val::Px(0.0),
                border: UiRect::all(Val::Px(1.0)),
                border_radius: BorderRadius::all(Val::Px(6.0)),
                ..default()
            },
            BackgroundColor(theme.panel_bg.with_alpha(PANEL_BG_ALPHA)),
            BorderColor::all(theme.accent),
            GlobalZIndex(z),
            Visibility::Hidden,
        ))
        .with_children(|parent| {
            parent.spawn((
                surface_marker,
                crate::view::block_render::msdf_surface_bundle(material),
                Node {
                    width: Val::Px(0.0),
                    height: Val::Px(LINE_HEIGHT as f32),
                    ..default()
                },
            ));
        })
        .id();
    commands.entity(root).add_child(panel);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str) -> PanelLine {
        PanelLine {
            text: text.to_string(),
            tone: LineTone::Row,
        }
    }

    // ── geometry ──────────────────────────────────────────────────────────

    #[test]
    fn a_panel_takes_the_middle_share_of_the_frame() {
        let w = panel_width(1600.0);
        assert!((w - 1040.0).abs() < 0.5, "got {w}");
        // Centered: the left inset equals the right one.
        let left = panel_left(1600.0);
        assert!((left - (1600.0 - w) / 2.0).abs() < 0.5, "got {left}");
    }

    /// A narrow window must not produce a panel too small to read, and a very
    /// wide one must not produce a row of text nobody can scan.
    #[test]
    fn panel_width_is_clamped_at_both_ends() {
        assert_eq!(panel_width(400.0), 360.0);
        assert_eq!(panel_width(4000.0), 1100.0);
        assert_eq!(panel_left(400.0), 20.0);
    }

    /// A font that has not loaded yet measures zero. That must floor to one
    /// column, never divide by zero.
    #[test]
    fn columns_survive_an_unmeasurable_font() {
        assert_eq!(columns(800.0, 0.0), 1);
        assert_eq!(columns(800.0, -1.0), 1);
        assert_eq!(columns(0.0, 8.0), 1);
        assert_eq!(columns(8.0 * 50.0 + PAD * 2.0, 8.0), 50);
    }

    // ── wrapping ──────────────────────────────────────────────────────────

    #[test]
    fn wrap_breaks_on_spaces_and_indents_continuations() {
        let out = wrap("the quick brown fox jumps", 12, 2);
        assert_eq!(out, vec!["the quick", "  brown fox", "  jumps"]);
    }

    /// A long path has no spaces to break on and still has to fit.
    #[test]
    fn wrap_cuts_a_word_that_cannot_fit() {
        let out = wrap("aaaaaaaaaaaaaaaa", 6, 2);
        for row in &out {
            assert!(row.chars().count() <= 6, "{out:?}");
        }
        assert_eq!(out.concat().replace(' ', ""), "aaaaaaaaaaaaaaaa");
    }

    /// Kernel-supplied text can carry a newline. It must not tear the row
    /// grid apart.
    #[test]
    fn wrap_flattens_control_characters() {
        let out = wrap("one\ntwo", 40, 0);
        assert_eq!(out, vec!["one two"]);
    }

    #[test]
    fn wrap_always_returns_a_row() {
        assert_eq!(wrap("", 20, 0), vec![""]);
    }

    /// An indent wider than the panel would leave no room for text at all.
    #[test]
    fn wrap_never_lets_the_indent_eat_the_whole_row() {
        let out = wrap("aaaa bbbb cccc", 4, 99);
        for row in &out {
            assert!(row.chars().count() <= 4, "{out:?}");
            assert!(!row.trim().is_empty(), "{out:?}");
        }
    }

    // ── truncation ────────────────────────────────────────────────────────

    #[test]
    fn truncate_marks_the_cut() {
        assert_eq!(truncate("abcdefgh", 5), "abcd\u{2026}");
        assert_eq!(truncate("abcd", 5), "abcd", "nothing to cut, nothing marked");
        assert_eq!(truncate("abcde", 5), "abcde", "exactly full is not a cut");
    }

    // ── scrolling ─────────────────────────────────────────────────────────

    #[test]
    fn a_body_that_fits_scrolls_not_at_all() {
        let body: Vec<PanelLine> = (0..3).map(|i| row(&format!("r{i}"))).collect();
        let out = scroll_window(&body, 0, 5);
        assert_eq!(out.len(), 3);
        assert_eq!(max_scroll(3, 5), 0);
    }

    /// What scrolls out of sight is counted, never silently dropped.
    #[test]
    fn a_long_body_says_how_much_is_out_of_sight() {
        let body: Vec<PanelLine> = (0..10).map(|i| row(&format!("r{i}"))).collect();
        let out = scroll_window(&body, 0, 4);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].text, "r0");
        assert_eq!(out[2].text, "r2");
        assert_eq!(out[3].text, "\u{2191}0 \u{2193}7");
        assert_eq!(out[3].tone, LineTone::Dim);
    }

    #[test]
    fn scrolling_down_moves_the_window_and_the_marker_agrees() {
        let body: Vec<PanelLine> = (0..10).map(|i| row(&format!("r{i}"))).collect();
        let out = scroll_window(&body, 4, 4);
        assert_eq!(out[0].text, "r4");
        assert_eq!(out[3].text, "\u{2191}4 \u{2193}3");
    }

    /// A stale offset past the end shows the end of the body, not a blank
    /// panel.
    #[test]
    fn an_offset_past_the_end_clamps_to_the_last_page() {
        let body: Vec<PanelLine> = (0..10).map(|i| row(&format!("r{i}"))).collect();
        let out = scroll_window(&body, 999, 4);
        assert_eq!(out[0].text, "r7");
        assert_eq!(out[3].text, "\u{2191}7 \u{2193}0");
        assert_eq!(max_scroll(10, 4), 7);
    }
}
