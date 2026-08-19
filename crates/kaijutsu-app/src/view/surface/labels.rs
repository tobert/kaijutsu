//! Block border labels and the gutter inclusion checkbox — the chrome that is
//! *glyphs* rather than quads.
//!
//! `chrome.rs` draws the border box; this module draws the words on it. A
//! fieldset label ("TOOL CALL kaish", "thinking", "running") straddles the
//! stroke: the border moves inward by half the label's ascent, and the stroke
//! is suppressed across the label's x-range so the two never overlap. The
//! checkbox (☑/☐) sits in the right gutter the border's extra right padding
//! already reserves.
//!
//! # Three pieces, and why they are separate
//!
//! 1. **Derivation** ([`block_label_spec`]) — which strings and which colors,
//!    read off the `BlockBorderStyle` `cell::block_border` already computed.
//!    Pure, so the table (`ToolCall` → top + status-bottom, `Thinking` →
//!    "thinking", …) is testable without a font. The *strings themselves* are
//!    not restated here: `compute_border_style` owns them, both paths call it,
//!    and a second copy is exactly the A/B difference nobody can attribute.
//! 2. **Placement** ([`top_label_placement`], [`bottom_label_placement`],
//!    [`checkbox_placement`]) — pure arithmetic over a measured run and a
//!    border rect, ported from `view/block_render.rs`'s label block. The same
//!    functions feed *both* consumers: `window::assemble_runs` positions the
//!    glyphs, `chrome::collect_chrome_instances` cuts the stroke gap. One
//!    arithmetic, so a label can never sit off its own gap.
//! 3. **Shaping** ([`LabelRunCache`]) — a small cache keyed by
//!    `(text, color)`. Labels repeat *heavily*: every tool call in a
//!    conversation says "TOOL CALL kaish" in the same amber, every thinking
//!    block says "thinking". One shaped run is shared by `Arc` across all of
//!    them.
//!
//! # Where the shaped runs live
//!
//! On [`super::shape_cache::ShapedBlock`], not in a cache of their own. That
//! is the one per-block cache whose lifecycle already matches what labels
//! need — populated over the shape band, evicted by LRU budget rather than by
//! band membership, and folded into `WindowKey` through
//! `ShapedBlockCache::generation()`. A separate per-block label map would have
//! to mirror all of that, and a generation that churned with *band membership*
//! (as the chrome cache's would) would re-upload the whole glyph buffer on
//! every frame of a scroll. See the invalidation note on
//! [`super::shape_cache::ShapedBlock::labels`].

use std::collections::HashMap;
use std::sync::Arc;

use bevy::prelude::*;

use crate::cell::block_border::BlockBorderStyle;
use crate::text::components::{bevy_color_to_brush, color_to_rgba8};
use crate::text::msdf::layout_bridge::collect_msdf_glyphs_deferred;
use crate::text::msdf::{FontDataMap, MsdfAtlas, PositionedGlyph};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};

// ============================================================================
// DERIVATION
// ============================================================================

/// Clearance between the checkbox glyph and the border box's right edge.
///
/// `build_block_scenes` hardcodes `4.0` (`view/block_render.rs`, the gutter
/// checkbox block); kept as a named constant here so the two can be compared
/// rather than grepped for.
pub const CHECKBOX_RIGHT_MARGIN: f32 = 4.0;

/// Alpha the checkbox draws at — dimmer when the block is *included*, because
/// an included block is the unremarkable case and the mark is a reassurance,
/// not a warning. Legacy's numbers, unchanged.
const CHECKBOX_ALPHA_INCLUDED: f32 = 0.3;
const CHECKBOX_ALPHA_EXCLUDED: f32 = 0.5;

/// The checkbox color for a block with no border of its own.
const CHECKBOX_FALLBACK: Color = Color::srgb(0.5, 0.55, 0.65);

/// One label to shape: its text and the color it bakes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LabelSpec<'a> {
    pub text: &'a str,
    pub color: Color,
}

/// Everything a block draws in glyphs *outside* its own text.
///
/// The checkbox is not `Option`: legacy draws it on **every** block, bordered
/// or not (the gutter block in `build_block_scenes` sits outside the
/// `if let Some(border_style)` that guards the labels), and a checkbox that
/// appeared only on tool calls would make exclusion invisible on exactly the
/// blocks people exclude most.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockLabelSpec<'a> {
    pub top: Option<LabelSpec<'a>>,
    pub bottom: Option<LabelSpec<'a>>,
    pub checkbox: LabelSpec<'static>,
}

/// The gutter mark for an inclusion state. Excluded is the *hollow* box.
pub fn checkbox_char(excluded: bool) -> &'static str {
    if excluded { "☐" } else { "☑" }
}

/// The gutter mark's color: the block's own border color at a fixed alpha, or
/// a neutral gray for a borderless block.
///
/// `border` is the **unfocused** style color, which already carries the
/// exclusion dim `compute_border_style` applies — so an excluded block's
/// checkbox is both hollow *and* dimmer, exactly as legacy draws it.
pub fn checkbox_color(border: Option<Color>, excluded: bool) -> Color {
    let alpha = if excluded {
        CHECKBOX_ALPHA_EXCLUDED
    } else {
        CHECKBOX_ALPHA_INCLUDED
    };
    border.unwrap_or(CHECKBOX_FALLBACK).with_alpha(alpha)
}

/// Read a block's label set off its computed border style.
///
/// The label *color* is the border's own color for every label — that is what
/// makes a running tool call's caption amber and a fatal error's caption red
/// without a second palette. Because the style handed in here is the
/// **unfocused** one (`chrome::BlockChrome::style`), focusing a block recolors
/// its stroke and leaves its labels alone; see the invalidation note in
/// [`super::shape_cache::ShapedBlock::labels`] for why that is deliberate
/// rather than an oversight.
pub fn block_label_spec(style: Option<&BlockBorderStyle>, excluded: bool) -> BlockLabelSpec<'_> {
    BlockLabelSpec {
        top: style.and_then(|s| {
            s.top_label.as_deref().map(|text| LabelSpec {
                text,
                color: s.color,
            })
        }),
        bottom: style.and_then(|s| {
            s.bottom_label.as_deref().map(|text| LabelSpec {
                text,
                color: s.color,
            })
        }),
        checkbox: LabelSpec {
            text: checkbox_char(excluded),
            color: checkbox_color(style.map(|s| s.color), excluded),
        },
    }
}

// ============================================================================
// PLACEMENT
// ============================================================================

/// Where one label sits on a border box, and where the stroke must break.
///
/// All **rect-local** logical px: x from the border box's left edge, y from
/// its top. The consumers add the box's document origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LabelPlacement {
    /// Left edge of the label's text box.
    pub x: f32,
    /// Top edge of the label's text box.
    pub y: f32,
    /// Stroke-suppressed range, or `(0, 0)` for a label that does not
    /// straddle a stroke (the checkbox).
    pub gap_x0: f32,
    pub gap_x1: f32,
    /// How far the stroke moves inward from the box edge so the label can
    /// straddle it. `0` means "use the default 1px AA inset".
    pub border_inset: f32,
}

impl LabelPlacement {
    /// Width of the stroke-suppressed range.
    #[allow(dead_code)] // Model accessor, exercised by the placement tests.
    pub fn gap_width(&self) -> f32 {
        self.gap_x1 - self.gap_x0
    }
}

/// The fieldset inset: half the label's ascent, plus AA clearance.
///
/// Puts the stroke's center at exactly `ascent / 2` below the box's top edge,
/// which is the vertical middle of the label's cap height — so the stroke runs
/// *through* where the letters would be, and the gap is what keeps it out of
/// them. `view/block_render.rs` computes the same number; the floor of 2.0 is
/// its, and it matters for a label whose ascent is nearly zero (an unshaped
/// run, a missing font).
pub fn fieldset_inset(ascent: f32) -> f32 {
    (ascent * 0.5 + 1.0).max(2.0)
}

/// Place a top label: left-aligned, `label_inset + label_pad` in from the box.
pub fn top_label_placement(
    label_width: f32,
    ascent: f32,
    label_inset: f32,
    label_pad: f32,
) -> LabelPlacement {
    let inset = fieldset_inset(ascent);
    LabelPlacement {
        x: label_inset + label_pad,
        y: (inset - ascent * 0.5).max(0.0),
        gap_x0: label_inset,
        gap_x1: label_inset + label_pad + label_width + label_pad,
        border_inset: inset,
    }
}

/// Place a bottom label: right-aligned, the mirror of [`top_label_placement`].
///
/// `rect_width` / `rect_height` are the **border box's**, not the column's —
/// a bordered block loses its glow margin on each side, and a label placed
/// against the column edge would hang past its own stroke.
pub fn bottom_label_placement(
    label_width: f32,
    ascent: f32,
    rect_width: f32,
    rect_height: f32,
    label_inset: f32,
    label_pad: f32,
) -> LabelPlacement {
    let inset = fieldset_inset(ascent);
    LabelPlacement {
        x: (rect_width - label_inset - label_pad - label_width).max(0.0),
        y: (rect_height - inset - ascent * 0.5).max(0.0),
        gap_x0: rect_width - label_inset - label_pad - label_width - label_pad,
        gap_x1: rect_width - label_inset,
        border_inset: inset,
    }
}

/// Place the gutter checkbox: against the box's right edge, vertically
/// centered on the whole block.
///
/// **Right edge, not top-left.** That is where `build_block_scenes` puts it
/// (`cb_x = width - cb_w - 4`, `cb_y = (total_height - cb_h) / 2`) and what
/// the border's extra right padding (`base * 1.5` in `compute_border_style`)
/// exists to reserve. It carries no gap: the checkbox sits inside the box, not
/// on its stroke.
pub fn checkbox_placement(
    glyph_width: f32,
    glyph_height: f32,
    rect_width: f32,
    rect_height: f32,
) -> LabelPlacement {
    LabelPlacement {
        x: (rect_width - glyph_width - CHECKBOX_RIGHT_MARGIN).max(0.0),
        y: ((rect_height - glyph_height) * 0.5).max(0.0),
        gap_x0: 0.0,
        gap_x1: 0.0,
        border_inset: 0.0,
    }
}

// ============================================================================
// SHAPED RUNS
// ============================================================================

/// One shaped label: its glyphs at origin `(0, 0)`, plus the metrics
/// [`LabelPlacement`] needs to position them.
///
/// Glyphs are shaped at the origin so a single run can be placed anywhere —
/// left-aligned on a top border, right-aligned on a bottom one, or in the
/// gutter. Placement is the caller's arithmetic, not baked into the glyphs,
/// which is what lets the `Arc` be shared.
#[derive(Debug, Clone)]
pub struct LabelRun {
    pub glyphs: Arc<Vec<PositionedGlyph>>,
    pub width: f32,
    pub height: f32,
    /// First line's ascent — what the fieldset straddle is measured from.
    pub ascent: f32,
}

impl LabelRun {
    /// Whether two runs are the same shaped run: the same glyph allocation
    /// and the same metrics.
    ///
    /// Pointer equality on purpose. `PositionedGlyph` is not `PartialEq`, and
    /// comparing glyph-by-glyph would be the wrong question anyway: runs come
    /// from [`LabelRunCache`], so *same text + same color* already means
    /// *same `Arc`*, and a different pointer always means something changed.
    pub fn same_run(&self, other: &LabelRun) -> bool {
        Arc::ptr_eq(&self.glyphs, &other.glyphs)
            && self.width == other.width
            && self.height == other.height
            && self.ascent == other.ascent
    }
}

/// A block's shaped labels, ready to place.
#[derive(Debug, Clone, Default)]
pub struct BlockLabels {
    pub top: Option<LabelRun>,
    pub bottom: Option<LabelRun>,
    pub checkbox: Option<LabelRun>,
}

impl BlockLabels {
    /// Whether these are the same runs as `other` — the comparison the shape
    /// pass makes before deciding a block's labels moved (and therefore that
    /// the window must re-assemble).
    pub fn same_runs(&self, other: &BlockLabels) -> bool {
        fn same(a: &Option<LabelRun>, b: &Option<LabelRun>) -> bool {
            match (a, b) {
                (None, None) => true,
                (Some(a), Some(b)) => a.same_run(b),
                _ => false,
            }
        }
        same(&self.top, &other.top)
            && same(&self.bottom, &other.bottom)
            && same(&self.checkbox, &other.checkbox)
    }

    /// Whether anything is here to draw. A borderless block still has a
    /// checkbox, so this is false only before the first shape pass reaches
    /// the block.
    #[allow(dead_code)] // Model accessor, exercised by tests.
    pub fn is_empty(&self) -> bool {
        self.top.is_none() && self.bottom.is_none() && self.checkbox.is_none()
    }
}

/// Shaped label runs, keyed by `(text, color)`.
///
/// **Why color is in the key rather than recolored at placement time.** A
/// label's color is its border's color, which encodes *status* — a running
/// tool call is amber, a finished one is dimmed amber, a fatal error is red.
/// Baking it means one run per distinct caption *and shade*, which is still a
/// tiny set shared across every block that says the same thing.
///
/// **Why the map is never pruned by use.** The key space is closed: the
/// captions come from a fixed table in `compute_border_style` (plus one model
/// name and one username per session), the colors from the theme palette, and
/// the whole map is dropped when either epoch moves ([`Self::sync_epochs`]).
/// A hundred entries is the pessimistic ceiling, and each is a handful of
/// glyphs. If that ever stops being true — a label that interpolates block
/// content, say — this needs an LRU, not a bigger comment.
///
/// **Nested by color, then text** rather than keyed on a `(String, [u8; 4])`
/// tuple, because the lookup runs for every band block every frame and a tuple
/// key cannot be probed with a `&str`: it would allocate a `String` per label
/// per block per frame purely to ask whether the run is already there.
#[derive(Resource, Debug, Default)]
pub struct LabelRunCache {
    runs: HashMap<[u8; 4], HashMap<String, LabelRun>>,
    metrics_epoch: u64,
    theme_epoch: u64,
}

impl LabelRunCache {
    /// Drop everything when the metrics or the theme move: the first changes
    /// the shaped advance widths, the second the baked colors of runs whose
    /// key color came from a token that has since been repainted.
    ///
    /// Returns whether anything was dropped.
    pub fn sync_epochs(&mut self, metrics_epoch: u64, theme_epoch: u64) -> bool {
        if self.metrics_epoch == metrics_epoch && self.theme_epoch == theme_epoch {
            return false;
        }
        self.metrics_epoch = metrics_epoch;
        self.theme_epoch = theme_epoch;
        let had = !self.runs.is_empty();
        self.runs.clear();
        had
    }

    /// Distinct runs held, across every color.
    #[allow(dead_code)] // Model accessor for tests and diagnostics.
    pub fn len(&self) -> usize {
        self.runs.values().map(HashMap::len).sum()
    }

    #[allow(dead_code)] // Symmetry with `len`; exercised by tests.
    pub fn is_empty(&self) -> bool {
        self.runs.values().all(HashMap::is_empty)
    }

    /// Look up a run without shaping it — no allocation, which is what the
    /// nesting buys. Used by the tests and by anything that must not touch
    /// the atlas.
    pub fn get(&self, spec: &LabelSpec<'_>) -> Option<&LabelRun> {
        self.runs
            .get(&color_to_rgba8(spec.color))
            .and_then(|by_text| by_text.get(spec.text))
    }

    /// Shape `spec` if it is not already cached, and hand back the (shared)
    /// run.
    ///
    /// An empty caption shapes to nothing rather than to an empty run: a
    /// zero-width gap in the stroke would be a visible nick with no label in
    /// it.
    pub fn shape(
        &mut self,
        font: &VelloFont,
        spec: &LabelSpec<'_>,
        font_size: f32,
        atlas: &mut MsdfAtlas,
        font_data: &mut FontDataMap,
    ) -> Option<LabelRun> {
        if spec.text.is_empty() {
            return None;
        }
        if let Some(run) = self.get(spec) {
            return Some(run.clone());
        }

        let brush = bevy_color_to_brush(spec.color);
        let style = VelloTextStyle {
            brush: brush.clone(),
            font_size,
            ..default()
        };
        let layout = font.layout(spec.text, &style, VelloTextAlign::Left, None);
        let width = layout.width();
        let height = layout.height();
        let ascent = layout
            .lines()
            .next()
            .map(|l| l.metrics().ascent)
            .unwrap_or(height);

        let mut fonts = Vec::new();
        super::shape_cache::collect_fonts(&layout, &mut fonts);
        for font in &fonts {
            font_data.register(font);
        }
        let (glyphs, keys) = collect_msdf_glyphs_deferred(&layout, &[], &brush, (0.0, 0.0));
        for glyph_key in keys {
            atlas.request(glyph_key);
        }

        let run = LabelRun {
            glyphs: Arc::new(glyphs),
            width,
            height,
            ascent,
        };
        self.runs
            .entry(color_to_rgba8(spec.color))
            .or_default()
            .insert(spec.text.to_string(), run.clone());
        Some(run)
    }

    /// Shape every label a block draws.
    pub fn shape_block(
        &mut self,
        font: &VelloFont,
        spec: &BlockLabelSpec<'_>,
        font_size: f32,
        atlas: &mut MsdfAtlas,
        font_data: &mut FontDataMap,
    ) -> BlockLabels {
        BlockLabels {
            top: spec
                .top
                .as_ref()
                .and_then(|s| self.shape(font, s, font_size, atlas, font_data)),
            bottom: spec
                .bottom
                .as_ref()
                .and_then(|s| self.shape(font, s, font_size, atlas, font_data)),
            checkbox: self.shape(font, &spec.checkbox, font_size, atlas, font_data),
        }
    }

    /// Seed a run directly — test-only, same reasoning as
    /// [`super::shape_cache::ShapedBlockCache::insert_for_test`].
    #[cfg(test)]
    pub(crate) fn insert_for_test(&mut self, spec: &LabelSpec<'_>, run: LabelRun) {
        self.runs
            .entry(color_to_rgba8(spec.color))
            .or_default()
            .insert(spec.text.to_string(), run);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockId, ContextId, ErrorCategory, ErrorSeverity, PrincipalId, Status};

    use crate::cell::block_border::{
        BorderContext, BorderInputs, BorderKind, compute_border_style,
    };
    use crate::text::msdf::glyph::{FontId, GlyphKey};
    use crate::ui::theme::Theme;
    use crate::view::{BlockKind, DriftKind, Role};
    use kaijutsu_types::ToolKind;

    const INSET: f32 = 12.0;
    const PAD: f32 = 6.0;

    fn bid(seq: u64) -> BlockId {
        use std::sync::OnceLock;
        static IDS: OnceLock<(ContextId, PrincipalId)> = OnceLock::new();
        let (ctx, prin) = IDS.get_or_init(|| (ContextId::new(), PrincipalId::new()));
        BlockId::new(*ctx, *prin, seq)
    }

    fn inputs(kind: BlockKind) -> BorderInputs {
        BorderInputs {
            id: bid(1),
            kind,
            role: Role::Model,
            status: Status::Done,
            tool_kind: None,
            tool_call_id: None,
            content_empty: false,
            has_output: false,
            is_error: false,
            collapsed: false,
            excluded: false,
            drift_kind: None,
            error: None,
        }
    }

    fn ctx() -> BorderContext {
        BorderContext {
            username: "amy".into(),
            model: "anthropic/claude-fable-5".into(),
        }
    }

    /// Derive the label set a block draws, the way the shape pass does.
    fn spec_for(inputs: &BorderInputs, has_result: bool) -> (Option<BlockBorderStyle>, bool) {
        let theme = Theme::default();
        (
            compute_border_style(inputs, &theme, &ctx(), has_result, 14.0),
            inputs.excluded,
        )
    }

    fn labels_of(inputs: &BorderInputs, has_result: bool) -> (Option<String>, Option<String>) {
        let (style, excluded) = spec_for(inputs, has_result);
        let spec = block_label_spec(style.as_ref(), excluded);
        (
            spec.top.map(|s| s.text.to_string()),
            spec.bottom.map(|s| s.text.to_string()),
        )
    }

    fn run(width: f32, ascent: f32) -> LabelRun {
        LabelRun {
            glyphs: Arc::new(vec![PositionedGlyph {
                key: GlyphKey::new(FontId::for_test(1), 7),
                x: 0.0,
                y: 0.0,
                font_size: 11.0,
                color: [255, 255, 255, 255],
                importance: 0.5,
                style_index: 0,
            }]),
            width,
            height: ascent + 2.0,
            ascent,
        }
    }

    // ---- derivation table ------------------------------------------------

    /// The table this port exists to reproduce: which block kind says what,
    /// on which edge. Every arm of `compute_border_style` that produces a
    /// caption is here, so a change to the wording is a test change and not a
    /// silent A/B drift between the two render paths.
    #[test]
    fn the_label_table_matches_the_border_rules() {
        // Shell tool call, no result yet: COMMAND @user on top, status below.
        let mut shell = inputs(BlockKind::ToolCall);
        shell.tool_kind = Some(ToolKind::Shell);
        shell.status = Status::Running;
        assert_eq!(
            labels_of(&shell, false),
            (Some("COMMAND @amy".into()), Some("running".into())),
        );

        // Non-shell tool call names the model, last path segment only.
        let mut call = inputs(BlockKind::ToolCall);
        call.status = Status::Done;
        assert_eq!(
            labels_of(&call, false),
            (
                Some("TOOL CALL claude-fable-5".into()),
                Some("done".into())
            ),
        );

        // Paired with a result, the status moves off the call entirely.
        assert_eq!(
            labels_of(&call, true),
            (Some("TOOL CALL claude-fable-5".into()), None),
        );

        // A tool result carries no top label (its top edge is the divider
        // joining it to the call) and reports only unfinished states.
        let mut result = inputs(BlockKind::ToolResult);
        result.tool_call_id = Some(bid(1));
        result.status = Status::Running;
        assert_eq!(labels_of(&result, false), (None, Some("running".into())));
        result.status = Status::Done;
        assert_eq!(labels_of(&result, false), (None, None));
        result.status = Status::Error;
        result.is_error = true;
        assert_eq!(labels_of(&result, false), (None, Some("error".into())));

        // Thinking, drift, error: top caption only.
        assert_eq!(
            labels_of(&inputs(BlockKind::Thinking), false),
            (Some("thinking".into()), None),
        );
        let mut drift = inputs(BlockKind::Drift);
        drift.drift_kind = Some(DriftKind::Pull);
        assert_eq!(labels_of(&drift, false), (Some("drift: pull".into()), None));
        drift.drift_kind = Some(DriftKind::Fork);
        assert_eq!(labels_of(&drift, false), (Some("fork".into()), None));

        let mut err = inputs(BlockKind::Error);
        err.error = Some((ErrorCategory::Stream, ErrorSeverity::Fatal));
        assert_eq!(
            labels_of(&err, false),
            (
                Some(format!(
                    "{} {}",
                    ErrorCategory::Stream.as_str(),
                    ErrorSeverity::Fatal.as_str()
                )),
                None
            ),
        );

        // Plain text draws a border only when the theme paints one, and
        // never a caption either way.
        assert_eq!(labels_of(&inputs(BlockKind::Text), false), (None, None));
    }

    /// A label takes its border's color, which is how status reads at a
    /// glance — and an excluded block's dim carries through to its caption,
    /// because `compute_border_style` dims the style itself.
    #[test]
    fn labels_take_the_border_color_including_the_exclusion_dim() {
        let mut thinking = inputs(BlockKind::Thinking);
        let (bright, _) = spec_for(&thinking, false);
        let bright = bright.expect("thinking draws a border");

        thinking.excluded = true;
        let (dim, _) = spec_for(&thinking, false);
        let dim = dim.expect("an excluded thinking block still draws a border");

        let bright_label = block_label_spec(Some(&bright), false);
        let dim_label = block_label_spec(Some(&dim), true);
        assert_eq!(bright_label.top.unwrap().color, bright.color);
        assert_eq!(dim_label.top.unwrap().color, dim.color);
        assert!(
            dim.color.alpha() < bright.color.alpha(),
            "exclusion must dim the caption, not only the stroke"
        );
    }

    // ---- checkbox --------------------------------------------------------

    /// Char and color per inclusion state, and the rule that every block gets
    /// one — a borderless block included.
    #[test]
    fn the_checkbox_marks_inclusion_on_every_block() {
        assert_eq!(checkbox_char(false), "☑");
        assert_eq!(checkbox_char(true), "☐");

        // Borderless: neutral gray, and still present.
        let none = block_label_spec(None, false);
        assert_eq!(none.checkbox.text, "☑");
        assert_eq!(none.checkbox.color, CHECKBOX_FALLBACK.with_alpha(0.3));
        assert!(none.top.is_none() && none.bottom.is_none());

        // Bordered: the border's color, and excluded reads *stronger* than
        // included so the mark you need to notice is the one you can see.
        let theme = Theme::default();
        let style = compute_border_style(&inputs(BlockKind::Thinking), &theme, &ctx(), false, 14.0)
            .expect("thinking draws a border");
        let included = block_label_spec(Some(&style), false);
        let excluded = block_label_spec(Some(&style), true);
        assert_eq!(included.checkbox.color, style.color.with_alpha(0.3));
        assert_eq!(excluded.checkbox.color, style.color.with_alpha(0.5));
        assert!(excluded.checkbox.color.alpha() > included.checkbox.color.alpha());
        assert_ne!(included.checkbox.text, excluded.checkbox.text);
    }

    /// The checkbox rides the box's right edge, vertically centered — the
    /// gutter the border's extra right padding reserves. Position parity with
    /// `build_block_scenes`.
    #[test]
    fn the_checkbox_sits_in_the_right_gutter() {
        let p = checkbox_placement(10.0, 12.0, 400.0, 60.0);
        assert_eq!(p.x, 400.0 - 10.0 - CHECKBOX_RIGHT_MARGIN);
        assert_eq!(p.y, (60.0 - 12.0) * 0.5);
        // It sits inside the box, so it must never cut the stroke.
        assert_eq!(p.gap_width(), 0.0);
        assert_eq!(p.border_inset, 0.0);

        // A box narrower than its own mark clamps rather than going negative.
        let tiny = checkbox_placement(10.0, 12.0, 4.0, 2.0);
        assert_eq!(tiny.x, 0.0);
        assert_eq!(tiny.y, 0.0);
    }

    // ---- placement -------------------------------------------------------

    /// The fieldset straddle: the stroke moves in by half the ascent, and the
    /// label's vertical middle lands on it. That relationship is what makes
    /// the caption look like it is *on* the line rather than above it.
    #[test]
    fn a_top_label_straddles_the_stroke_it_breaks() {
        let ascent = 9.0;
        let p = top_label_placement(50.0, ascent, INSET, PAD);
        assert_eq!(p.border_inset, fieldset_inset(ascent));
        // Label's vertical center == the stroke's center.
        assert!((p.y + ascent * 0.5 - p.border_inset).abs() < 1e-5);
        assert_eq!(p.x, INSET + PAD);
        // The gap opens before the text and closes a pad past it.
        assert_eq!(p.gap_x0, INSET);
        assert_eq!(p.gap_x1, INSET + PAD + 50.0 + PAD);
        assert!(p.gap_x0 < p.x && p.x + 50.0 < p.gap_x1);
    }

    /// The gap is the run's width plus a pad on each side — grow the caption,
    /// grow the hole in the stroke by exactly as much and no more.
    #[test]
    fn the_gap_tracks_the_label_width_exactly() {
        let short = top_label_placement(20.0, 9.0, INSET, PAD);
        let long = top_label_placement(80.0, 9.0, INSET, PAD);
        assert_eq!(short.gap_width(), 20.0 + 2.0 * PAD);
        assert_eq!(long.gap_width() - short.gap_width(), 60.0);

        let short = bottom_label_placement(20.0, 9.0, 400.0, 60.0, INSET, PAD);
        let long = bottom_label_placement(80.0, 9.0, 400.0, 60.0, INSET, PAD);
        assert_eq!(short.gap_width(), 20.0 + 2.0 * PAD);
        assert_eq!(long.gap_width() - short.gap_width(), 60.0);
    }

    /// The bottom label mirrors the top one against the box's right edge and
    /// its bottom stroke.
    #[test]
    fn a_bottom_label_mirrors_the_top_one() {
        let (w, h, ascent) = (400.0_f32, 60.0_f32, 9.0_f32);
        let p = bottom_label_placement(50.0, ascent, w, h, INSET, PAD);
        assert_eq!(p.border_inset, fieldset_inset(ascent));
        // Its vertical center lands on the bottom stroke.
        assert!((p.y + ascent * 0.5 - (h - p.border_inset)).abs() < 1e-5);
        assert_eq!(p.x, w - INSET - PAD - 50.0);
        assert_eq!(p.gap_x1, w - INSET);
        assert!(p.gap_x0 < p.x && p.x + 50.0 < p.gap_x1);

        // Mirror symmetry: the right-hand gap is the left-hand gap reflected.
        let top = top_label_placement(50.0, ascent, INSET, PAD);
        assert!((w - p.gap_x1 - top.gap_x0).abs() < 1e-5);
        assert!((w - p.gap_x0 - top.gap_x1).abs() < 1e-5);
    }

    /// A near-zero ascent (an unshaped run, a missing font) must still leave a
    /// stroke inset the AA can work with rather than collapsing to the edge.
    #[test]
    fn the_fieldset_inset_has_a_floor() {
        assert_eq!(fieldset_inset(0.0), 2.0);
        assert_eq!(fieldset_inset(1.0), 2.0);
        assert!(fieldset_inset(20.0) > 2.0);
    }

    // ---- cache -----------------------------------------------------------

    /// The reuse the cache exists for: the same caption in the same color is
    /// literally the same allocation, however many blocks draw it. A
    /// conversation of fifty tool calls shapes "TOOL CALL kaish" once.
    #[test]
    fn one_shaped_run_is_shared_by_every_block_that_says_the_same_thing() {
        let mut cache = LabelRunCache::default();
        let amber = Color::srgb(0.9, 0.7, 0.2);
        let spec = LabelSpec {
            text: "TOOL CALL kaish",
            color: amber,
        };
        cache.insert_for_test(&spec, run(60.0, 9.0));

        let a = cache.get(&spec).expect("seeded").clone();
        let b = cache
            .get(&LabelSpec {
                text: "TOOL CALL kaish",
                color: amber,
            })
            .expect("same key")
            .clone();
        assert!(Arc::ptr_eq(&a.glyphs, &b.glyphs));
        assert!(a.same_run(&b));
        assert_eq!(cache.len(), 1);

        // A different shade of the same caption is a different run: the
        // color is baked into the glyphs.
        assert!(
            cache
                .get(&LabelSpec {
                    text: "TOOL CALL kaish",
                    color: Color::srgb(0.5, 0.5, 0.5),
                })
                .is_none()
        );
    }

    /// Either epoch moving drops the lot — advance widths after a font-size
    /// change and baked colors after a theme swap are both stale.
    #[test]
    fn an_epoch_move_drops_every_cached_run() {
        let mut cache = LabelRunCache::default();
        let spec = LabelSpec {
            text: "thinking",
            color: Color::WHITE,
        };
        // First observation of (0, 0) is a no-op: the cache starts there.
        assert!(!cache.sync_epochs(0, 0));
        cache.insert_for_test(&spec, run(40.0, 9.0));

        assert!(cache.sync_epochs(1, 0), "a metrics move must clear");
        assert!(cache.is_empty());

        cache.insert_for_test(&spec, run(40.0, 9.0));
        assert!(cache.sync_epochs(1, 1), "a theme move must clear");
        assert!(cache.is_empty());

        // Steady state clears nothing.
        cache.insert_for_test(&spec, run(40.0, 9.0));
        assert!(!cache.sync_epochs(1, 1));
        assert_eq!(cache.len(), 1);
    }

    /// `same_runs` is what the shape pass compares to decide the window must
    /// re-assemble — it has to notice a caption appearing, disappearing, and
    /// changing.
    #[test]
    fn label_set_comparison_notices_every_kind_of_change() {
        let a = run(40.0, 9.0);
        let b = run(40.0, 9.0); // same metrics, different allocation
        let with_a = BlockLabels {
            top: Some(a.clone()),
            bottom: None,
            checkbox: Some(a.clone()),
        };
        assert!(with_a.same_runs(&with_a.clone()));

        // Running → Done: the bottom caption goes away.
        let gained_bottom = BlockLabels {
            bottom: Some(a.clone()),
            ..with_a.clone()
        };
        assert!(!with_a.same_runs(&gained_bottom));

        // A re-shape hands back a different allocation even for equal text.
        let reshaped = BlockLabels {
            top: Some(b),
            ..with_a.clone()
        };
        assert!(!with_a.same_runs(&reshaped));

        assert!(BlockLabels::default().is_empty());
        assert!(!with_a.is_empty());
    }

    /// The same test against the *real* shaper: two blocks asking for the
    /// same caption get one shaped run and one atlas population, and the
    /// metrics it measures are the ones placement will use.
    ///
    /// Seeded runs can't prove this — `insert_for_test` puts the `Arc` there
    /// itself. This goes through `shape`, which is the path a frame takes.
    #[test]
    fn the_shaper_returns_the_same_allocation_for_a_repeated_caption() {
        let font = crate::text::shaping::load_into_font_context(
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../assets/fonts/NotoMono-Regular.ttf"
            ))
            .expect("shipped test font must be present"),
        );
        let mut images = bevy::asset::Assets::<bevy::image::Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 256, 256);
        let mut font_data = FontDataMap::default();
        let mut cache = LabelRunCache::default();

        let spec = LabelSpec {
            text: "TOOL CALL kaish",
            color: Color::srgb(0.9, 0.7, 0.2),
        };
        let first = cache
            .shape(&font, &spec, 11.0, &mut atlas, &mut font_data)
            .expect("a non-empty caption must shape");
        let second = cache
            .shape(&font, &spec, 11.0, &mut atlas, &mut font_data)
            .expect("still there");
        assert!(
            Arc::ptr_eq(&first.glyphs, &second.glyphs),
            "the second block must reuse the first block's glyphs",
        );
        assert_eq!(cache.len(), 1);

        // The metrics placement depends on are real, not zero — a zero-width
        // run would cut a gap the label doesn't fill.
        assert!(first.width > 0.0);
        assert!(first.ascent > 0.0);
        assert_eq!(first.glyphs.len(), "TOOL CALL kaish".len());

        // A caption with nothing in it shapes to nothing, so no gap opens.
        assert!(
            cache
                .shape(
                    &font,
                    &LabelSpec {
                        text: "",
                        color: Color::WHITE
                    },
                    11.0,
                    &mut atlas,
                    &mut font_data,
                )
                .is_none()
        );

        // A whole block's set comes back in one call, checkbox included.
        let theme = Theme::default();
        let mut running = inputs(BlockKind::ToolCall);
        running.status = Status::Running;
        let style = compute_border_style(&running, &theme, &ctx(), false, 14.0)
            .expect("a running tool call draws a border");
        let labels = cache.shape_block(
            &font,
            &block_label_spec(Some(&style), false),
            theme.label_font_size,
            &mut atlas,
            &mut font_data,
        );
        assert!(labels.top.is_some(), "TOOL CALL …");
        assert!(labels.bottom.is_some(), "running");
        assert!(labels.checkbox.is_some());
    }

    // ---- invalidation ----------------------------------------------------

    /// The two state changes that must reach the GPU, checked through the
    /// real derivation *and* the real shaper, ending on the comparison
    /// `shape_visible_blocks` makes before it calls
    /// `ShapedBlockCache::touch()`.
    ///
    /// A tool call finishing has to stop saying "running"; excluding a block
    /// has to hollow its checkbox. Both go: block field → `compute_border_style`
    /// → `block_label_spec` → shaped run → `same_runs() == false`. If any link
    /// stops noticing, the window never re-assembles and the screen keeps the
    /// stale caption.
    #[test]
    fn a_status_flip_and_an_exclusion_toggle_both_move_the_shaped_runs() {
        let font = crate::text::shaping::load_into_font_context(
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../assets/fonts/NotoMono-Regular.ttf"
            ))
            .expect("shipped test font must be present"),
        );
        let mut images = bevy::asset::Assets::<bevy::image::Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 256, 256);
        let mut font_data = FontDataMap::default();
        let mut cache = LabelRunCache::default();
        let theme = Theme::default();

        let mut shape = |inputs: &BorderInputs| {
            let style = compute_border_style(inputs, &theme, &ctx(), false, 14.0);
            cache.shape_block(
                &font,
                &block_label_spec(style.as_ref(), inputs.excluded),
                theme.label_font_size,
                &mut atlas,
                &mut font_data,
            )
        };

        // Running → Done: the bottom caption changes word AND shade.
        let mut call = inputs(BlockKind::ToolCall);
        call.status = Status::Running;
        let running = shape(&call);
        call.status = Status::Done;
        let done = shape(&call);
        assert!(
            !running.same_runs(&done),
            "a finished call must stop drawing the running caption",
        );

        // Re-deriving the same state is free — no churn, no re-upload.
        assert!(
            running.same_runs(&shape(&{
                let mut c = call;
                c.status = Status::Running;
                c
            })),
            "an unchanged block must reuse its runs, or every frame re-uploads",
        );

        // Excluding a block hollows the checkbox and dims everything.
        let included = shape(&call);
        call.excluded = true;
        let excluded = shape(&call);
        assert!(
            !included.same_runs(&excluded),
            "an exclusion toggle must reach the glyph buffer",
        );
        assert!(
            !included
                .checkbox
                .as_ref()
                .unwrap()
                .same_run(excluded.checkbox.as_ref().unwrap()),
            "specifically: the checkbox itself changes",
        );
    }

    /// Focus is deliberately *not* a caption input on this path: the labels
    /// are derived from the unfocused style, so a j/k move recolors the stroke
    /// (in `collect_chrome_instances`) and re-uploads no glyphs.
    ///
    /// This test exists to make the choice visible rather than accidental —
    /// the focused style really does carry a different color, and we really do
    /// ignore it here.
    #[test]
    fn focus_recolors_the_stroke_and_leaves_the_captions_alone() {
        use crate::cell::block_border::apply_focus_style;

        let theme = Theme::default();
        let style = compute_border_style(&inputs(BlockKind::Thinking), &theme, &ctx(), false, 14.0)
            .expect("thinking draws a border");
        let focused = apply_focus_style(Some(style.clone()), true, &theme)
            .expect("focus keeps the border");
        assert_ne!(
            focused.color, style.color,
            "focus must change the stroke, or this test proves nothing",
        );

        // What the shape pass reads is the unfocused style, both times.
        let unfocused_spec = block_label_spec(Some(&style), false);
        assert_eq!(unfocused_spec.top.unwrap().color, style.color);
        assert_eq!(unfocused_spec.checkbox.color, style.color.with_alpha(0.3));
    }

    /// The one border kind that must never get a fieldset inset: the role
    /// divider's rule has no box to move inward, and legacy's shader takes the
    /// `CENTER_LINE` branch before it ever reads the insets. This pins the
    /// derivation side of that — a divider style carries no captions, so
    /// nothing here can hand one an inset.
    #[test]
    fn a_center_line_style_carries_no_fieldset_labels() {
        let divider = BlockBorderStyle {
            kind: BorderKind::CenterLine,
            color: Color::WHITE,
            thickness: 1.0,
            corner_radius: 0.0,
            padding: Default::default(),
            animation: Default::default(),
            top_label: None,
            bottom_label: None,
        };
        let spec = block_label_spec(Some(&divider), false);
        assert!(spec.top.is_none() && spec.bottom.is_none());
    }
}

