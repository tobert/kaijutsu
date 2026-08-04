//! Role-group divider layout math — pure geometry, no text shaping or drawing.
//!
//! The USER/ASSISTANT/SYSTEM/TOOL/ASSET divider drawn at every role
//! transition is a single shader-drawn horizontal rule
//! (`BorderKind::CenterLine` in `cell::block_border` + `BK_CENTER_LINE` in
//! `assets/shaders/block_fx.wgsl`) with an MSDF-rendered label straddling a
//! gap in the middle of the line — the same `BlockFxMaterial` +
//! `collect_msdf_glyphs` machinery every other block's border label uses.
//! See `view::block_render::sync_role_group_headers` for the ECS wiring.
//!
//! This module holds only the arithmetic that turns a measured label width
//! into where the gap sits — no font access, no ECS, no vello/scene drawing.
//! That split keeps the layout math testable without a font context.

/// Font size for role divider labels (USER/ASSISTANT/SYSTEM/TOOL/ASSET).
///
/// Deliberately distinct from `theme.label_font_size` (11.0, used for
/// tool-call/thinking fieldset labels) — a role transition is a more
/// prominent landmark than a per-block fieldset caption, so it renders
/// slightly larger. No theme token exists for this yet (see
/// `docs/issues.md`'s theme-tokenization entry for the same gap on
/// sparkline/markdown colors).
pub const ROLE_LABEL_FONT_SIZE: f32 = 12.0;

/// Divider stroke thickness — matches the pre-shader Vello `Stroke::new(1.0)`
/// width exactly, so converting to the shader didn't change the line weight.
pub const ROLE_DIVIDER_THICKNESS: f32 = 1.0;

/// Where the label sits, and the x-range where the divider line's stroke
/// must be suppressed so the label can straddle it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoleDividerLayout {
    /// Left edge of the label text (px from the node's left edge).
    pub label_x: f64,
    /// Start of the stroke-suppressed gap (px from the node's left edge).
    pub gap_x0: f64,
    /// End of the stroke-suppressed gap (px from the node's left edge).
    pub gap_x1: f64,
}

/// Compute the divider layout from a measured label width.
///
/// The label starts `label_inset + label_pad` from the left edge; the gap
/// the line must skip runs from `label_inset` to just past the label's
/// right edge (with `label_pad` clearance on both sides). `label_inset`/
/// `label_pad` are passed in (callers use `theme.label_inset`/`label_pad`)
/// rather than hardcoded here, so this stays pure arithmetic.
pub fn compute_role_divider_layout(
    label_width: f64,
    label_inset: f64,
    label_pad: f64,
) -> RoleDividerLayout {
    let label_width = label_width.max(0.0);
    RoleDividerLayout {
        label_x: label_inset + label_pad,
        gap_x0: label_inset,
        gap_x1: label_inset + label_pad + label_width + label_pad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSET: f64 = 12.0;
    const PAD: f64 = 6.0;

    #[test]
    fn gap_starts_at_label_inset() {
        let layout = compute_role_divider_layout(40.0, INSET, PAD);
        assert_eq!(layout.gap_x0, INSET);
    }

    #[test]
    fn label_starts_after_inset_and_pad() {
        let layout = compute_role_divider_layout(40.0, INSET, PAD);
        assert_eq!(layout.label_x, INSET + PAD);
    }

    #[test]
    fn gap_width_grows_exactly_with_label_width() {
        let short = compute_role_divider_layout(20.0, INSET, PAD);
        let long = compute_role_divider_layout(80.0, INSET, PAD);
        let short_width = short.gap_x1 - short.gap_x0;
        let long_width = long.gap_x1 - long.gap_x0;
        assert_eq!(long_width - short_width, 80.0 - 20.0);
    }

    #[test]
    fn zero_width_label_still_leaves_a_pad_sized_gap() {
        let layout = compute_role_divider_layout(0.0, INSET, PAD);
        assert_eq!(layout.gap_x1 - layout.gap_x0, PAD * 2.0);
    }

    #[test]
    fn negative_width_is_clamped_to_zero() {
        // Defensive: a pathological layout measurement should never shrink
        // the gap below the pad-only width.
        let layout = compute_role_divider_layout(-10.0, INSET, PAD);
        assert_eq!(layout.gap_x1 - layout.gap_x0, PAD * 2.0);
    }
}
