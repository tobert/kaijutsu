//! Chrome: block borders, focus ring, role dividers — as instanced SDF quads
//! in the surface's own pass.
//!
//! The legacy path draws every one of these with a per-block
//! `BlockFxMaterial` (`shaders::sync_block_fx` + `assets/shaders/block_fx.wgsl`),
//! which means one UI node, one material asset and one full-screen-ish
//! fragment pass **per block**. The surface path has no per-block node to hang
//! a material on, so the same SDF math moves into
//! `assets/shaders/surface_chrome.wgsl` and runs once per pane over an
//! instance buffer — one quad per bordered block, in document space, sharing
//! the glyph pass's scroll uniform.
//!
//! # Three things this module owns
//!
//! 1. **Layout.** [`surface_block_layout`] is the one place that knows a
//!    bordered block is narrower than the column and that its text starts
//!    inside the padding. Both the shaper (wrap width) and the instance
//!    builder (border rect) call it, so text can never disagree with the box
//!    drawn around it.
//! 2. **Style.** [`BlockChromeCache`] runs `cell::block_border`'s pure core
//!    (`compute_border_style`) over the surface's content cache. Same rules,
//!    same theme tokens, no ECS — the legacy system keeps its own caller.
//! 3. **Instances.** [`build_chrome_instances`] reduces the built window's
//!    rows to [`ChromeInstance`]s and bumps its **own** version. Focus moves
//!    and status pulses must never touch `buffer_version`: re-uploading a
//!    screenful of glyphs because a border changed color is exactly the cost
//!    this rewrite exists to remove.
//!
//! # Draw order (the finding)
//!
//! `block_fx.wgsl` composites the border stroke *and* the glow **behind** the
//! text — both are masked by `1.0 - result.a`, where `result` already holds
//! the block's glyphs. The only things it draws in front are the cursor beam
//! and the selection wash, and the conversation surface has neither (the
//! compose box is a different surface; there is no conversation selection
//! yet). So the surface pass draws **all** chrome first, then glyphs over it,
//! and there is no chrome-front pass in this slice. Add one when a selection
//! or cursor lands on the conversation — not before.
//!
//! # Where the words are
//!
//! Block border **labels** ("TOOL CALL kaish", "running") and the gutter
//! inclusion checkbox are glyph runs, not quads, so they live in
//! [`super::labels`] and are drawn by the glyph pass. What this module owns of
//! them is the *hole*: [`border_label_gaps`] turns a block's shaped captions
//! into the stroke-suppressed ranges and fieldset insets its
//! [`ChromeInstance`] carries, using `labels`' own placement arithmetic so the
//! gap and the letters in it are never computed twice.

use std::collections::HashMap;
use std::sync::Arc;

use bevy::prelude::*;
use bytemuck::{Pod, Zeroable};
use kaijutsu_types::{BlockId, Role};

use crate::cell::block_border::{
    BlockBorderStyle, BorderAnimation, BorderContext, BorderKind, BorderPadding, apply_focus_style,
    compute_border_style,
};
use crate::cell::{ConversationScrollState, EditorEntities};
use crate::connection::RpcConnectionState;
use crate::connection::drift::DriftState;
use crate::text::TextMetrics;
use crate::text::components::color_to_rgba8;
use crate::ui::theme::Theme;
use crate::view::components::FocusTarget;
use crate::view::document::DocumentCache;
use crate::view::geometry::{ConversationGeometry, RowKey};
use crate::view::role_divider;

use super::ConversationSurface;
use super::content::BlockContentCache;
use super::labels::{self, BlockLabels};
use super::shape_cache::{
    HeaderLabelCache, ShapedBlockCache, SurfaceMetricsEpoch, surface_wrap_width,
};

// ============================================================================
// LAYOUT
// ============================================================================

/// Where a block's border box sits in the column, and where its text sits
/// inside the box.
///
/// All logical px, all relative to the conversation column's content-box left
/// edge (document x = 0).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockLayout {
    /// Left edge of the border box.
    pub rect_x: f32,
    /// Width of the border box.
    pub rect_width: f32,
    /// Left edge of the text = `rect_x + padding.left`.
    pub text_x: f32,
    /// Top of the text within the block's row = `padding.top`.
    pub text_y: f32,
    /// `padding.bottom` — the row's height is `text_y + text_height + this`.
    pub pad_bottom: f32,
    /// What the shaper gets as `max_advance`.
    pub wrap_width: f32,
}

impl BlockLayout {
    /// Total row height for a measured text height.
    pub fn outer_height(&self, text_height: f32) -> f32 {
        self.text_y + text_height + self.pad_bottom
    }
}

/// Lay one block out: indent, the border's own margin, and the padding the
/// border reserves for text.
///
/// `padding` is `None` for a block with no border, and then this degenerates
/// to slice 1's behavior exactly — full column width, no insets — which is
/// most of the conversation (plain user/assistant text draws no border under
/// the default theme).
///
/// **Divergence from legacy, on purpose.** `update_block_cell_nodes` gives a
/// bordered cell `width: 100%` *plus* a `glow_radius * 0.5` margin on each
/// side, so the node overflows its own column by that margin and the pane's
/// `Overflow::clip` cuts the right border. There is nothing to clip against
/// here, so the box is symmetric: the column loses `glow_radius` of width
/// rather than hanging 5px into the pane padding. The wrap width therefore
/// differs from legacy by that margin on bordered blocks — visible as a
/// slightly earlier line break, not as a layout error.
pub fn surface_block_layout(
    container_width: f32,
    indent_level: u32,
    indent_width: f32,
    glow_radius: f32,
    padding: Option<BorderPadding>,
) -> BlockLayout {
    // The margin exists so the border's SDF glow has somewhere to fall off
    // before the column edge; a borderless block has no glow and no margin.
    let h_margin = if padding.is_some() {
        glow_radius * 0.5
    } else {
        0.0
    };
    let column = surface_wrap_width(container_width, indent_level, indent_width);
    let rect_width = (column - 2.0 * h_margin).max(1.0);
    let rect_x = indent_level as f32 * indent_width + h_margin;
    let padding = padding.unwrap_or(BorderPadding {
        top: 0.0,
        bottom: 0.0,
        left: 0.0,
        right: 0.0,
    });
    BlockLayout {
        rect_x,
        rect_width,
        text_x: rect_x + padding.left,
        text_y: padding.top,
        pad_bottom: padding.bottom,
        wrap_width: (rect_width - padding.left - padding.right).max(1.0),
    }
}

// ============================================================================
// STYLE CACHE
// ============================================================================

/// One block's chrome: the border it draws (if any) and the layout that
/// border implies.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockChrome {
    /// The **unfocused** style. Focus is applied at instance-build time
    /// ([`apply_focus_style`]) so moving focus never invalidates layout — it
    /// only recolors, and the ring it gives a borderless block carries zero
    /// padding by construction.
    pub style: Option<BlockBorderStyle>,
    pub layout: BlockLayout,
}

/// Border style + layout for every block the surface might draw.
///
/// Populated from [`BlockContentCache`], which is already the surface's
/// entity-free view of the document — so this never touches the block store.
#[derive(Resource, Debug, Default)]
pub struct BlockChromeCache {
    blocks: HashMap<BlockId, BlockChrome>,
    /// The column width every layout in here was computed against. Kept so
    /// the instance builder can size role dividers without re-reading the
    /// pane's `ComputedNode`.
    container_width: f32,
}

impl BlockChromeCache {
    pub fn get(&self, id: &BlockId) -> Option<&BlockChrome> {
        self.blocks.get(id)
    }

    /// The layout to shape a block at — the borderless default when the block
    /// has no chrome entry yet (first frame, or a row the content sweep
    /// hasn't reached).
    pub fn layout(
        &self,
        id: &BlockId,
        container_width: f32,
        indent_level: u32,
        indent_width: f32,
    ) -> BlockLayout {
        match self.blocks.get(id) {
            Some(chrome) => chrome.layout,
            None => surface_block_layout(container_width, indent_level, indent_width, 0.0, None),
        }
    }

    pub fn container_width(&self) -> f32 {
        self.container_width
    }

    #[allow(dead_code)] // Model accessor, exercised by tests and diagnostics.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    // Seeds a chrome entry without a content cache. No caller since the
    // instance tests moved to building the cache through `chrome_cache`; kept
    // because the surface's other test modules reach for the same shape.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn insert_for_test(&mut self, id: BlockId, chrome: BlockChrome) {
        self.blocks.insert(id, chrome);
    }
}

/// Recompute every cached block's border style and layout.
///
/// Runs in [`super::SurfaceSet::Content`] **after** `sync_block_content` and
/// before shaping, because the wrap width a block shapes at is decided here.
/// Bounded by **the geometry band, not the content cache**: this walks the
/// same rows `sync_block_content` formats (the content band, [`super::content::CONTENT_SLACK_SCREENS`]
/// screens of slack), which is a strict superset of the shape band and of any
/// window the hysteresis can leave built. Walking the content *cache* instead
/// would be O(everything ever scrolled past) — that cache only evicts blocks
/// which left the **document**, never blocks which left the band, so its size
/// grows with scroll-through distance (found by kaibo/deepseek review,
/// 2026-08-18; the first version of this system did exactly that).
///
/// Per-row cost is a match on the block's
/// [`BorderInputs`](crate::cell::block_border::BorderInputs) plus, for the
/// bordered kinds, the label strings `compute_border_style` builds. Those
/// allocations are now *used* — the shape pass reads them straight off this
/// cache ([`super::labels::block_label_spec`]) and probes its run cache with
/// them, allocating nothing further.
#[allow(clippy::too_many_arguments)]
pub fn sync_block_chrome(
    entities: Res<EditorEntities>,
    geometries: Query<&ConversationGeometry>,
    computed_nodes: Query<&ComputedNode>,
    content: Res<BlockContentCache>,
    scroll_state: Res<ConversationScrollState>,
    theme: Res<Theme>,
    text_metrics: Res<TextMetrics>,
    conn_state: Res<RpcConnectionState>,
    drift_state: Res<DriftState>,
    doc_cache: Res<DocumentCache>,
    mut cache: ResMut<BlockChromeCache>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(geom) = geometries.get(main_ent) else {
        return;
    };
    // Same source of truth as `shape_visible_blocks`: the conversation
    // container's content box. A width read from anywhere else would put the
    // border box and the text it wraps in different columns.
    let container_width = entities
        .conversation_container
        .and_then(|e| computed_nodes.get(e).ok())
        .map(|c| crate::view::ui_rtt::logical_content_size(c).x)
        .filter(|w| *w > 1.0);
    let Some(container_width) = container_width else {
        return;
    };

    // The band this pass covers: the same rows `sync_block_content` formats,
    // derived the same way so the two can't disagree about which blocks have
    // content to give a border to.
    let vh = if scroll_state.visible_height > 0.0 {
        scroll_state.visible_height
    } else {
        600.0
    };
    let band = geom.visible_rows(
        scroll_state.offset,
        vh,
        super::content::CONTENT_SLACK_SCREENS * vh,
    );
    let rows = &geom.rows()[band];

    // Which tool calls have a *visible* result below them — the same
    // predicate `determine_block_border_style` uses, over the content band
    // instead of the spawn band. A result is document-adjacent to its call,
    // so a call anywhere in this band has its result here too.
    let has_result: std::collections::HashSet<BlockId> = rows
        .iter()
        .filter_map(|row| match row.key {
            RowKey::Block(id) => content.get(&id),
            RowKey::Header(_) => None,
        })
        .filter(|fb| fb.border.is_visible_tool_result())
        .filter_map(|fb| fb.border.tool_call_id)
        .collect();

    let ctx = BorderContext {
        username: conn_state
            .identity
            .as_ref()
            .map(|i| i.username.clone())
            .unwrap_or_default(),
        model: doc_cache
            .active_id()
            .and_then(|ctx_id| drift_state.contexts.iter().find(|c| c.id == ctx_id))
            .map(|c| c.model.clone())
            .unwrap_or_default(),
    };

    cache.container_width = container_width;
    cache.blocks.clear();
    for row in rows {
        let RowKey::Block(id) = row.key else {
            continue;
        };
        let Some(formatted) = content.get(&id) else {
            // The content sweep hasn't reached this row yet (first frame
            // ordering). Shaping falls back to the borderless layout for one
            // frame, exactly as it did before chrome existed.
            continue;
        };
        let style = compute_border_style(
            &formatted.border,
            &theme,
            &ctx,
            has_result.contains(&id),
            text_metrics.cell_font_size,
        );
        let layout = surface_block_layout(
            container_width,
            row.indent_level,
            theme.indent_width,
            theme.block_border_glow_radius,
            style.as_ref().map(|s| s.padding),
        );
        cache.blocks.insert(id, BlockChrome { style, layout });
    }
}

// ============================================================================
// INSTANCES
// ============================================================================

/// Border kind, as the shader's `BK_*` constants. Same numbering as
/// `block_fx.wgsl` so the two shaders can be read side by side.
// The shader's `BK_NONE`: a block with no border never reaches the instance
// buffer at all, so nothing on this side emits it. It stays for parity with
// `surface_chrome.wgsl`'s constant block — a gap in the numbering would make
// the two impossible to read side by side.
#[allow(dead_code)]
pub const CHROME_KIND_NONE: u32 = 0;
pub const CHROME_KIND_FULL: u32 = 1;
pub const CHROME_KIND_TOP_ACCENT: u32 = 2;
pub const CHROME_KIND_DASHED: u32 = 3;
pub const CHROME_KIND_OPEN_BOTTOM: u32 = 4;
pub const CHROME_KIND_OPEN_TOP: u32 = 5;
pub const CHROME_KIND_CENTER_LINE: u32 = 6;

/// Animation mode, as `block_fx.wgsl`'s `anim_mode`.
pub const CHROME_ANIM_NONE: f32 = 0.0;
pub const CHROME_ANIM_BREATHE: f32 = 1.0;
pub const CHROME_ANIM_PULSE: f32 = 2.0;
pub const CHROME_ANIM_CHASE: f32 = 3.0;

/// One SDF chrome quad, in **document-space logical pixels**.
///
/// Field order is the vertex-attribute order — see
/// `ConversationSurfaceRenderer::chrome_instance_layout`, whose offsets are
/// pinned by a test. `#[repr(C)]` with no padding
/// (16+16+16+8+8+4+4 = 72 bytes).
///
/// There is deliberately **no fill color**: `block_fx.wgsl` paints no block
/// background (the block texture's transparent pixels show the pane behind),
/// so a fill here would be a visual the legacy path never had.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct ChromeInstance {
    /// Border box `[x, y, w, h]` in document space, logical px.
    pub rect_doc: [f32; 4],
    /// `[corner_radius, thickness, glow_radius, glow_intensity]`, logical px
    /// for the two radii.
    pub params: [f32; 4],
    /// Stroke-suppressed gaps in **rect-local** logical px:
    /// `[top_x0, top_x1, bottom_x0, bottom_x1]`. All zero = no gap. Same
    /// packing as `block_fx.wgsl`'s `label_gaps` uniform, so the two shaders
    /// read alike.
    ///
    /// `CENTER_LINE` uses only `xy` — a rule through the vertical center has
    /// no top/bottom distinction, and the role divider's label breaks it
    /// there.
    pub label_gap: [f32; 4],
    /// Fieldset insets `[top, bottom]`, logical px: how far the stroke moves
    /// inward from the box edge so a label can straddle it. Zero = the
    /// default 1px AA inset, exactly as `border_stroke.zw` means it on the
    /// legacy path.
    pub insets: [f32; 2],
    /// `[anim_mode, phase]`. Phase is added to the time uniform, so two
    /// instances with the same mode can breathe out of step; every instance
    /// built today passes `0.0`, which is legacy's behavior (one global
    /// phase, every border in sync).
    pub anim: [f32; 2],
    /// Straight (non-premultiplied) RGBA8 — the same `to_srgba` encoding
    /// `color_to_rgba8` gives glyphs, and the same one `sync_block_fx` feeds
    /// `border_color`.
    pub color: [u8; 4],
    /// One of the `CHROME_KIND_*` constants.
    pub kind: u32,
}

impl ChromeInstance {
    /// The `BorderKind` → shader constant mapping.
    pub fn kind_of(kind: BorderKind) -> u32 {
        match kind {
            BorderKind::Full => CHROME_KIND_FULL,
            BorderKind::TopAccent => CHROME_KIND_TOP_ACCENT,
            BorderKind::Dashed => CHROME_KIND_DASHED,
            BorderKind::OpenBottom => CHROME_KIND_OPEN_BOTTOM,
            BorderKind::OpenTop => CHROME_KIND_OPEN_TOP,
            BorderKind::CenterLine => CHROME_KIND_CENTER_LINE,
        }
    }

    /// The `BorderAnimation` → shader mode mapping.
    pub fn anim_of(animation: BorderAnimation) -> f32 {
        match animation {
            BorderAnimation::None => CHROME_ANIM_NONE,
            BorderAnimation::Breathe => CHROME_ANIM_BREATHE,
            BorderAnimation::Pulse => CHROME_ANIM_PULSE,
            BorderAnimation::Chase => CHROME_ANIM_CHASE,
        }
    }
}

/// What a block's captions demand of its border stroke: where to stop
/// drawing, and how far to move inward so the letters straddle the line.
///
/// [`Default`] is "no labels" — an unbroken stroke at the default AA inset,
/// which is what a block with no captions wants and also what a block whose
/// captions have not been shaped yet must get. Guessing a gap for a label
/// that isn't on screen would draw a notch with nothing in it; the role
/// divider already takes exactly that fallback.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BorderLabelGaps {
    /// `[top_x0, top_x1, bottom_x0, bottom_x1]`, rect-local logical px.
    pub gap: [f32; 4],
    /// `[top, bottom]` fieldset insets, logical px. Zero = default.
    pub insets: [f32; 2],
}

/// Derive the stroke gaps from a block's shaped captions.
///
/// The arithmetic is `labels`', not restated here — the same functions the
/// glyph assembly calls, so the hole in the stroke and the letters in it can
/// never be computed two different ways.
pub fn border_label_gaps(
    labels: &BlockLabels,
    rect_width: f32,
    rect_height: f32,
    theme: &Theme,
) -> BorderLabelGaps {
    let mut out = BorderLabelGaps::default();
    if let Some(top) = labels.top.as_ref() {
        let p = labels::top_label_placement(top.width, top.ascent, theme.label_inset, theme.label_pad);
        out.gap[0] = p.gap_x0;
        out.gap[1] = p.gap_x1;
        out.insets[0] = p.border_inset;
    }
    if let Some(bottom) = labels.bottom.as_ref() {
        let p = labels::bottom_label_placement(
            bottom.width,
            bottom.ascent,
            rect_width,
            rect_height,
            theme.label_inset,
            theme.label_pad,
        );
        out.gap[2] = p.gap_x0;
        out.gap[3] = p.gap_x1;
        out.insets[1] = p.border_inset;
    }
    out
}

/// Turn one block's border style into its instance.
///
/// `rect` is the block's border box in document space. The glow is taken from
/// the theme exactly as `sync_block_fx` takes it — including the rule that a
/// `CenterLine` gets none, because the glow SDFs the whole rect and would
/// halo the entire header row rather than the rule through it.
pub fn block_chrome_instance(
    style: &BlockBorderStyle,
    rect: [f32; 4],
    theme: &Theme,
    gaps: BorderLabelGaps,
) -> ChromeInstance {
    let (glow_radius, glow_intensity) = if style.kind == BorderKind::CenterLine {
        (0.0, 0.0)
    } else {
        (
            theme.block_border_glow_radius,
            theme.block_border_glow_intensity,
        )
    };
    ChromeInstance {
        rect_doc: rect,
        params: [
            style.corner_radius,
            style.thickness,
            glow_radius,
            glow_intensity,
        ],
        label_gap: gaps.gap,
        insets: gaps.insets,
        anim: [ChromeInstance::anim_of(style.animation), 0.0],
        color: color_to_rgba8(style.color),
        kind: ChromeInstance::kind_of(style.kind),
    }
}

/// The role-group divider for a header row: one full-width rule with a gap
/// where the label sits.
///
/// The label itself is already drawn — `window::assemble_runs` emits it as a
/// glyph run from [`HeaderLabelCache`] — so what parity needs here is the
/// rule, its per-role color (`sync_role_group_headers`), and the gap, whose
/// arithmetic is `role_divider`'s and is not restated.
pub fn divider_instance(
    role: Role,
    rect: [f32; 4],
    label_width: Option<f32>,
    theme: &Theme,
) -> ChromeInstance {
    let color = match role {
        Role::User => theme.block_user,
        Role::Model => theme.block_assistant,
        Role::System => theme.fg_dim,
        Role::Tool | Role::Asset => theme.block_tool_call,
    };
    // No label shaped yet (its metrics epoch just moved, or the atlas isn't
    // up): draw the rule unbroken rather than guessing a gap the label won't
    // land in.
    let label_gap = match label_width {
        Some(width) => {
            let div = role_divider::compute_role_divider_layout(
                width as f64,
                theme.label_inset as f64,
                theme.label_pad as f64,
            );
            // Only `xy`: a center line has no top/bottom edge to break.
            [div.gap_x0 as f32, div.gap_x1 as f32, 0.0, 0.0]
        }
        None => [0.0; 4],
    };
    ChromeInstance {
        rect_doc: rect,
        params: [0.0, role_divider::ROLE_DIVIDER_THICKNESS, 0.0, 0.0],
        label_gap,
        insets: [0.0, 0.0],
        anim: [CHROME_ANIM_NONE, 0.0],
        color: color_to_rgba8(color),
        kind: CHROME_KIND_CENTER_LINE,
    }
}

/// A surface's chrome instances and the version that says whether they moved.
///
/// Separate from `ConversationSurface::buffer_version` on purpose: a focus
/// move, a tool call finishing, an error arriving — all of them change chrome
/// and none of them change a glyph. Sharing one version would re-upload the
/// whole glyph buffer every time a border blinked.
#[derive(Component, Debug, Default)]
pub struct ChromeInstances {
    /// `Arc` so extraction hands the render world a pointer, not a copy.
    pub instances: Arc<Vec<ChromeInstance>>,
    pub version: u64,
}

/// Assemble the chrome instances for every surface's built window.
///
/// Runs in [`super::SurfaceSet::Window`] after `build_surface_window`, which
/// is what decides `row_range` and where each row sits.
///
/// # Change detection
///
/// The instances are rebuilt into a scratch buffer every frame and compared
/// against the previous ones; the version only moves when they differ. That
/// is deliberately the dumbest thing that can work, and it was chosen over a
/// hash of the inputs because the input set is genuinely wide — geometry
/// offsets, per-block status/error/exclusion, focus, theme, container width,
/// and the shaped label widths that size the divider gaps — and a hash that
/// forgets one of them fails *silently*, as a border that never updates. The
/// cost it buys off is a walk of at most a window's worth of rows (~100) plus
/// a `memcmp`; the expensive thing (the GPU upload) is still gated on the
/// comparison. Revisit if a profile ever shows it.
///
/// Scroll is not an input at all: instances are document-space, so scrolling
/// inside the built band changes nothing here.
pub fn build_chrome_instances(
    entities: Res<EditorEntities>,
    geometries: Query<&ConversationGeometry>,
    chrome: Res<BlockChromeCache>,
    headers: Res<HeaderLabelCache>,
    shaped: Res<ShapedBlockCache>,
    metrics_epoch: Res<SurfaceMetricsEpoch>,
    focus: Res<FocusTarget>,
    theme: Res<Theme>,
    mut surfaces: Query<(&ConversationSurface, &mut ChromeInstances)>,
    mut scratch: Local<Vec<ChromeInstance>>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(geom) = geometries.get(main_ent) else {
        return;
    };
    let metrics_epoch = metrics_epoch.get();
    let container_width = chrome.container_width();

    for (surface, mut instances) in surfaces.iter_mut() {
        scratch.clear();
        collect_chrome_instances(
            geom,
            &chrome,
            &headers,
            &shaped,
            metrics_epoch,
            &focus,
            &theme,
            container_width,
            surface.window.row_range.clone(),
            &mut scratch,
        );

        if instances.instances.as_slice() == scratch.as_slice() {
            continue;
        }
        let slot = instances.as_mut();
        // Cloned rather than `mem::take`n: the new `Arc` needs its own
        // allocation either way (the render world may still hold the old
        // one), and taking would leave the scratch buffer at capacity zero to
        // regrow every frame a streaming block resizes.
        slot.instances = Arc::new(scratch.clone());
        slot.version = slot.version.wrapping_add(1);
    }
}

/// Pure core of [`build_chrome_instances`] — no ECS, so the row → instance
/// mapping is testable without a window.
///
/// **Appends** to `out`; the caller owns clearing it (the system reuses one
/// scratch buffer across surfaces).
#[allow(clippy::too_many_arguments)]
pub fn collect_chrome_instances(
    geom: &ConversationGeometry,
    chrome: &BlockChromeCache,
    headers: &HeaderLabelCache,
    shaped: &ShapedBlockCache,
    metrics_epoch: u64,
    focus: &FocusTarget,
    theme: &Theme,
    container_width: f32,
    row_range: std::ops::Range<usize>,
    out: &mut Vec<ChromeInstance>,
) {
    let rows = geom.rows();
    let start = row_range.start.min(rows.len());
    let end = row_range.end.min(rows.len());

    for row in &rows[start..end] {
        match row.key {
            RowKey::Block(id) => {
                let Some(entry) = chrome.get(&id) else {
                    continue;
                };
                // Focus keys off `FocusTarget.block_id` — the conversation
                // is entity-free, so there is no per-block marker component
                // to key off instead.
                //
                // Only the focused block pays a clone — `apply_focus_style`
                // takes ownership (its other caller, before slice 5, was an
                // entity `insert`, which also wanted ownership), and this
                // runs over every in-window row every frame.
                let focused_style;
                let style = if focus.is_block_focused(&id) {
                    focused_style = apply_focus_style(entry.style.clone(), true, theme);
                    focused_style.as_ref()
                } else {
                    entry.style.as_ref()
                };
                let Some(style) = style else {
                    continue;
                };
                let rect = [
                    entry.layout.rect_x,
                    row.y_offset,
                    entry.layout.rect_width,
                    row.height,
                ];
                // Gaps come from the *shaped* captions, so a block whose
                // labels haven't landed yet draws an unbroken stroke rather
                // than a notch with nothing in it — the same fallback the
                // role divider takes.
                let gaps = shaped
                    .get(&id)
                    .map(|block| {
                        border_label_gaps(
                            &block.labels,
                            entry.layout.rect_width,
                            row.height,
                            theme,
                        )
                    })
                    .unwrap_or_default();
                out.push(block_chrome_instance(style, rect, theme, gaps));
            }
            RowKey::Header(_) => {
                if container_width <= 0.0 {
                    continue;
                }
                let label_width = headers.get(row.role, metrics_epoch).map(|l| l.width);
                out.push(divider_instance(
                    row.role,
                    [0.0, row.y_offset, container_width, row.height],
                    label_width,
                    theme,
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockKind, ContextId, ErrorCategory, ErrorSeverity, PrincipalId, Status};

    use crate::cell::block_border::BorderInputs;

    use crate::view::geometry::{EstimateParams, RowSeed};

    fn bid(seq: u64) -> BlockId {
        use std::sync::OnceLock;
        static IDS: OnceLock<(ContextId, PrincipalId)> = OnceLock::new();
        let (ctx, prin) = IDS.get_or_init(|| (ContextId::new(), PrincipalId::new()));
        BlockId::new(*ctx, *prin, seq)
    }

    fn params() -> EstimateParams {
        EstimateParams {
            cols: 100,
            line_height: 30.0,
            block_spacing: 12.0,
            role_header_height: 20.0,
            role_header_spacing: 4.0,
        }
    }

    fn seed() -> RowSeed {
        RowSeed {
            text_len: 40,
            newline_count: 0,
            role: Role::User,
            kind: BlockKind::Text,
            collapsed: false,
            parent_id: None,
        }
    }

    /// header (20 + 4) at y=0, then `n` blocks of 42 pitch from y=24.
    fn strip(n: u64) -> ConversationGeometry {
        let mut geom = ConversationGeometry::default();
        let ids: Vec<BlockId> = (1..=n).map(bid).collect();
        geom.reconcile(&ids, |_| Some(seed()), &params(), 1);
        geom.recompute_offsets();
        geom
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

    /// "No captions shaped" — an unbroken stroke at the default AA inset.
    fn no_gaps() -> BorderLabelGaps {
        BorderLabelGaps::default()
    }

    fn style_of(inputs: &BorderInputs, theme: &Theme, has_result: bool) -> BlockBorderStyle {
        compute_border_style(inputs, theme, &BorderContext::default(), has_result, 14.0)
            .expect("this block kind must draw a border")
    }

    // ---- layout ----------------------------------------------------------

    /// The round trip the wrap-width fix rests on: what the shaper is given
    /// plus the insets the border reserves is exactly the box drawn around
    /// it. If these drift, text either overlaps its own border or stops
    /// short of it.
    #[test]
    fn wrap_width_plus_padding_is_the_border_box_width() {
        let padding = BorderPadding {
            top: 8.0,
            bottom: 6.0,
            left: 12.0,
            right: 18.0,
        };
        let layout = surface_block_layout(800.0, 0, 24.0, 10.0, Some(padding));
        assert_eq!(
            layout.wrap_width + padding.left + padding.right,
            layout.rect_width,
        );
        assert_eq!(layout.text_x, layout.rect_x + padding.left);
        assert_eq!(layout.text_y, padding.top);
        // The glow margin comes off both sides, and the box starts inside it.
        assert_eq!(layout.rect_x, 5.0);
        assert_eq!(layout.rect_width, 790.0);
    }

    /// A borderless block is slice 1 exactly: full column, no insets. Most of
    /// the conversation is this case, so a regression here moves every line
    /// break in the document.
    #[test]
    fn a_borderless_block_keeps_the_whole_column() {
        let layout = surface_block_layout(800.0, 0, 24.0, 10.0, None);
        assert_eq!(layout.rect_x, 0.0);
        assert_eq!(layout.rect_width, 800.0);
        assert_eq!(layout.wrap_width, 800.0);
        assert_eq!(layout.text_x, 0.0);
        assert_eq!(layout.text_y, 0.0);
        assert_eq!(layout.outer_height(120.0), 120.0);
    }

    #[test]
    fn indent_shifts_the_box_and_narrows_it() {
        let layout = surface_block_layout(800.0, 2, 24.0, 0.0, None);
        assert_eq!(layout.rect_x, 48.0);
        assert_eq!(layout.rect_width, 752.0);
    }

    #[test]
    fn outer_height_is_the_text_plus_both_paddings() {
        let padding = BorderPadding {
            top: 8.0,
            bottom: 6.0,
            left: 12.0,
            right: 12.0,
        };
        let layout = surface_block_layout(800.0, 0, 24.0, 10.0, Some(padding));
        assert_eq!(layout.outer_height(100.0), 114.0);
    }

    /// A pane narrow enough to be swallowed by padding must still hand the
    /// shaper a usable width rather than a zero/negative `max_advance`.
    #[test]
    fn a_degenerate_column_still_yields_a_positive_wrap_width() {
        let padding = BorderPadding {
            top: 8.0,
            bottom: 6.0,
            left: 40.0,
            right: 40.0,
        };
        let layout = surface_block_layout(20.0, 0, 24.0, 10.0, Some(padding));
        assert!(layout.wrap_width >= 1.0);
        assert!(layout.rect_width >= 1.0);
    }

    // ---- style → instance ------------------------------------------------

    /// Every field of `BlockBorderStyle` that the shader can see must land in
    /// the instance. A field that silently doesn't travel is a border that
    /// draws with the wrong weight/corner/animation and nobody can tell why.
    #[test]
    fn every_border_style_field_lands_in_the_instance() {
        let theme = Theme::default();
        let style = BlockBorderStyle {
            kind: BorderKind::Dashed,
            color: Color::srgba(1.0, 0.5, 0.25, 0.75),
            thickness: 2.5,
            corner_radius: 7.0,
            padding: BorderPadding::default(),
            animation: BorderAnimation::Breathe,
            top_label: Some("thinking".into()),
            bottom_label: None,
        };
        let inst =
            block_chrome_instance(&style, [10.0, 200.0, 400.0, 60.0], &theme, no_gaps());

        assert_eq!(inst.rect_doc, [10.0, 200.0, 400.0, 60.0]);
        assert_eq!(inst.params[0], 7.0, "corner radius");
        assert_eq!(inst.params[1], 2.5, "thickness");
        assert_eq!(inst.params[2], theme.block_border_glow_radius);
        assert_eq!(inst.params[3], theme.block_border_glow_intensity);
        assert_eq!(inst.kind, CHROME_KIND_DASHED);
        assert_eq!(inst.anim, [CHROME_ANIM_BREATHE, 0.0]);
        assert_eq!(inst.color, color_to_rgba8(style.color));
        assert_eq!(inst.label_gap, [0.0; 4]);
        assert_eq!(inst.insets, [0.0, 0.0]);
    }

    /// The `BorderKind`/`BorderAnimation` mappings are a contract with
    /// `surface_chrome.wgsl`'s `BK_*` / `anim_mode` constants — and with
    /// `sync_block_fx`, which numbers them the same way so the two paths can
    /// be compared.
    #[test]
    fn kind_and_animation_map_to_the_shader_constants() {
        assert_eq!(ChromeInstance::kind_of(BorderKind::Full), 1);
        assert_eq!(ChromeInstance::kind_of(BorderKind::TopAccent), 2);
        assert_eq!(ChromeInstance::kind_of(BorderKind::Dashed), 3);
        assert_eq!(ChromeInstance::kind_of(BorderKind::OpenBottom), 4);
        assert_eq!(ChromeInstance::kind_of(BorderKind::OpenTop), 5);
        assert_eq!(ChromeInstance::kind_of(BorderKind::CenterLine), 6);

        assert_eq!(ChromeInstance::anim_of(BorderAnimation::None), 0.0);
        assert_eq!(ChromeInstance::anim_of(BorderAnimation::Breathe), 1.0);
        assert_eq!(ChromeInstance::anim_of(BorderAnimation::Pulse), 2.0);
        assert_eq!(ChromeInstance::anim_of(BorderAnimation::Chase), 3.0);
    }

    /// `sync_block_fx`'s one kind-specific rule: the divider gets no glow,
    /// because the glow SDFs the whole rect and would ring the header row.
    #[test]
    fn a_center_line_carries_no_glow() {
        let theme = Theme::default();
        let mut style = style_of(&inputs(BlockKind::Thinking), &theme, false);
        style.kind = BorderKind::CenterLine;
        let inst = block_chrome_instance(&style, [0.0, 0.0, 100.0, 20.0], &theme, no_gaps());
        assert_eq!(inst.params[2], 0.0);
        assert_eq!(inst.params[3], 0.0);
    }

    /// A running tool call chases, an error pulses — the two animations a
    /// live conversation actually shows, carried end to end.
    #[test]
    fn status_reaches_the_instance_as_an_animation_mode() {
        let theme = Theme::default();
        let mut running = inputs(BlockKind::ToolCall);
        running.status = Status::Running;
        let inst = block_chrome_instance(
            &style_of(&running, &theme, false),
            [0.0, 0.0, 10.0, 10.0],
            &theme,
            no_gaps(),
        );
        assert_eq!(inst.anim[0], CHROME_ANIM_CHASE);

        let mut failed = inputs(BlockKind::Error);
        failed.error = Some((ErrorCategory::Tool, ErrorSeverity::Fatal));
        let inst = block_chrome_instance(
            &style_of(&failed, &theme, false),
            [0.0, 0.0, 10.0, 10.0],
            &theme,
            no_gaps(),
        );
        assert_eq!(inst.anim[0], CHROME_ANIM_PULSE);
    }

    /// Exclusion dims the stroke and stops the animation — the same
    /// post-process the legacy path applies, reaching the GPU as color+mode.
    #[test]
    fn an_excluded_block_is_dimmed_and_still() {
        let theme = Theme::default();
        let mut running = inputs(BlockKind::ToolCall);
        running.status = Status::Running;
        let lit = block_chrome_instance(
            &style_of(&running, &theme, false),
            [0.0, 0.0, 10.0, 10.0],
            &theme,
            no_gaps(),
        );

        running.excluded = true;
        let dim = block_chrome_instance(
            &style_of(&running, &theme, false),
            [0.0, 0.0, 10.0, 10.0],
            &theme,
            no_gaps(),
        );
        assert_eq!(dim.anim[0], CHROME_ANIM_NONE);
        assert!(dim.color[3] < lit.color[3], "excluded must dim the stroke");
    }

    // ---- label gaps ------------------------------------------------------

    /// A hand-built caption run, so the gap arithmetic can be checked without
    /// a font context.
    fn label_run(width: f32, ascent: f32) -> super::labels::LabelRun {
        super::labels::LabelRun {
            glyphs: Arc::new(Vec::new()),
            width,
            height: ascent + 3.0,
            ascent,
        }
    }

    /// The gap the shader cuts is exactly the caption's width plus a pad each
    /// side, and the stroke moves inward by the fieldset inset so the letters
    /// straddle it. This is the whole contract between `labels` and the
    /// shader — get it wrong and the border draws through the words.
    #[test]
    fn a_caption_cuts_a_gap_of_its_own_width_in_the_stroke() {
        let theme = Theme::default();
        let (w, ascent) = (60.0_f32, 9.0_f32);
        let gaps = border_label_gaps(
            &BlockLabels {
                top: Some(label_run(w, ascent)),
                bottom: None,
                checkbox: Some(label_run(11.0, 11.0)),
            },
            400.0,
            80.0,
            &theme,
        );
        // Top pair set, bottom pair untouched.
        assert_eq!(gaps.gap[0], theme.label_inset);
        assert_eq!(gaps.gap[1] - gaps.gap[0], w + 2.0 * theme.label_pad);
        assert_eq!([gaps.gap[2], gaps.gap[3]], [0.0, 0.0]);
        assert_eq!(gaps.insets, [labels::fieldset_inset(ascent), 0.0]);

        // The checkbox lives inside the box and must never cut the stroke.
        let checkbox_only = border_label_gaps(
            &BlockLabels {
                top: None,
                bottom: None,
                checkbox: Some(label_run(11.0, 11.0)),
            },
            400.0,
            80.0,
            &theme,
        );
        assert_eq!(checkbox_only, BorderLabelGaps::default());

        // A bottom caption fills the other pair and the other inset.
        let both = border_label_gaps(
            &BlockLabels {
                top: Some(label_run(w, ascent)),
                bottom: Some(label_run(30.0, ascent)),
                checkbox: None,
            },
            400.0,
            80.0,
            &theme,
        );
        assert_eq!(both.gap[3], 400.0 - theme.label_inset);
        assert_eq!(both.gap[3] - both.gap[2], 30.0 + 2.0 * theme.label_pad);
        assert_eq!(both.insets[1], labels::fieldset_inset(ascent));
    }

    /// The gap has to *reach the instance*, and only from a block whose
    /// captions are actually shaped: an unshaped one draws an unbroken box
    /// rather than a notch with nothing in it.
    #[test]
    fn the_instance_carries_the_gap_only_once_the_caption_is_shaped() {
        let theme = Theme::default();
        let geom = strip(3);
        let cache = chrome_cache(&theme, &[(bid(2), BlockKind::Thinking)]);
        let headers = HeaderLabelCache::default();
        let focus = FocusTarget::default();

        // Nothing shaped: the box is whole.
        let mut out = Vec::new();
        collect_chrome_instances(
            &geom,
            &cache,
            &headers,
            &ShapedBlockCache::default(),
            0,
            &focus,
            &theme,
            800.0,
            2..3,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label_gap, [0.0; 4]);
        assert_eq!(out[0].insets, [0.0, 0.0]);

        // Shape the "thinking" caption and the gap appears, sized to it.
        let mut shaped = ShapedBlockCache::default();
        let mut block = super::super::shape_cache::ShapedBlock {
            key: super::super::shape_cache::ShapeKey {
                content_version: 1,
                wrap_width_bits: 800.0_f32.to_bits(),
                collapsed: false,
                indent_level: 0,
                metrics_epoch: 0,
                baked_theme_epoch: 0,
            },
            chunks: Vec::new(),
            height: 40.0,
            text_height: 40.0,
            x_offset: 0.0,
            y_offset: 0.0,
            glyph_count: 0,
            last_used: 0,
            color: Color::WHITE,
            text_len: 0,
            streaming: false,
            labels: BlockLabels::default(),
        };
        block.labels.top = Some(label_run(52.0, 9.0));
        shaped.insert_for_test(bid(2), block);

        out.clear();
        collect_chrome_instances(
            &geom, &cache, &headers, &shaped, 0, &focus, &theme, 800.0, 2..3, &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label_gap[0], theme.label_inset);
        assert_eq!(
            out[0].label_gap[1] - out[0].label_gap[0],
            52.0 + 2.0 * theme.label_pad,
        );
        assert_eq!(out[0].insets[0], labels::fieldset_inset(9.0));
    }

    // ---- divider ---------------------------------------------------------

    /// The gap is `role_divider`'s arithmetic and nothing else — pinned
    /// against that module's own function so the two can never drift.
    #[test]
    fn the_divider_gap_matches_role_divider() {
        let theme = Theme::default();
        let inst = divider_instance(Role::User, [0.0, 100.0, 800.0, 20.0], Some(48.0), &theme);
        let expected = role_divider::compute_role_divider_layout(
            48.0,
            theme.label_inset as f64,
            theme.label_pad as f64,
        );
        assert_eq!(
            inst.label_gap,
            [expected.gap_x0 as f32, expected.gap_x1 as f32, 0.0, 0.0],
            "a center line breaks only on the xy pair — it has no bottom edge",
        );
        assert_eq!(inst.kind, CHROME_KIND_CENTER_LINE);
        assert_eq!(inst.params[1], role_divider::ROLE_DIVIDER_THICKNESS);
        assert_eq!(inst.rect_doc, [0.0, 100.0, 800.0, 20.0]);
    }

    #[test]
    fn an_unshaped_label_leaves_the_rule_unbroken() {
        let theme = Theme::default();
        let inst = divider_instance(Role::Model, [0.0, 0.0, 800.0, 20.0], None, &theme);
        assert_eq!(inst.label_gap, [0.0; 4]);
    }

    #[test]
    fn divider_color_follows_the_role() {
        let theme = Theme::default();
        let rect = [0.0, 0.0, 800.0, 20.0];
        assert_eq!(
            divider_instance(Role::User, rect, None, &theme).color,
            color_to_rgba8(theme.block_user),
        );
        assert_eq!(
            divider_instance(Role::Model, rect, None, &theme).color,
            color_to_rgba8(theme.block_assistant),
        );
        assert_eq!(
            divider_instance(Role::System, rect, None, &theme).color,
            color_to_rgba8(theme.fg_dim),
        );
    }

    // ---- collect ---------------------------------------------------------

    fn chrome_cache(theme: &Theme, ids: &[(BlockId, BlockKind)]) -> BlockChromeCache {
        let mut cache = BlockChromeCache {
            blocks: HashMap::new(),
            container_width: 800.0,
        };
        for (id, kind) in ids {
            let mut inputs = inputs(*kind);
            inputs.id = *id;
            let style =
                compute_border_style(&inputs, theme, &BorderContext::default(), false, 14.0);
            let layout = surface_block_layout(
                800.0,
                0,
                theme.indent_width,
                theme.block_border_glow_radius,
                style.as_ref().map(|s| s.padding),
            );
            cache.blocks.insert(*id, BlockChrome { style, layout });
        }
        cache
    }

    /// The focus ring's rect comes from the geometry row (its `y_offset` and
    /// `height`) and the block's layout — nothing else. That is what lets
    /// focus move without re-measuring or re-shaping anything.
    #[test]
    fn the_focus_ring_takes_the_focused_rows_rect() {
        let theme = Theme::default();
        let geom = strip(3);
        // Plain user text: borderless under the default theme, so the only
        // thing that can produce an instance here is the ring.
        let cache = chrome_cache(&theme, &[(bid(2), BlockKind::Text)]);
        let headers = HeaderLabelCache::default();

        let mut focus = FocusTarget::default();
        let mut out = Vec::new();
        collect_chrome_instances(
            &geom,
            &cache,
            &headers,
            &ShapedBlockCache::default(),
            0,
            &focus,
            &theme,
            800.0,
            2..3,
            &mut out,
        );
        assert!(out.is_empty(), "an unfocused plain block draws no chrome");

        focus.focus_block(bid(2));
        collect_chrome_instances(
            &geom,
            &cache,
            &headers,
            &ShapedBlockCache::default(),
            0,
            &focus,
            &theme,
            800.0,
            2..3,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        let row = geom.block_row(&bid(2)).unwrap();
        assert_eq!(out[0].rect_doc, [0.0, row.y_offset, 800.0, row.height]);
        assert_eq!(out[0].kind, CHROME_KIND_FULL);
        assert_eq!(out[0].color, color_to_rgba8(theme.block_border_focus));
    }

    #[test]
    fn header_rows_contribute_a_divider() {
        let theme = Theme::default();
        let geom = strip(2);
        let cache = chrome_cache(&theme, &[]);
        let mut out = Vec::new();
        collect_chrome_instances(
            &geom,
            &cache,
            &HeaderLabelCache::default(),
            &ShapedBlockCache::default(),
            0,
            &FocusTarget::default(),
            &theme,
            800.0,
            0..1,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, CHROME_KIND_CENTER_LINE);
        assert_eq!(out[0].rect_doc[2], 800.0);
    }

    #[test]
    fn a_range_past_the_end_is_clamped_rather_than_panicking() {
        let theme = Theme::default();
        let geom = strip(2);
        let mut out = Vec::new();
        collect_chrome_instances(
            &geom,
            &chrome_cache(&theme, &[]),
            &HeaderLabelCache::default(),
            &ShapedBlockCache::default(),
            0,
            &FocusTarget::default(),
            &theme,
            800.0,
            0..999,
            &mut out,
        );
        // Two header rows' dividers (User run only starts once here), no
        // block chrome — the point is that it returned at all.
        assert!(out.len() <= geom.rows().len());
    }

    // ---- sync_block_chrome (the system) ----------------------------------

    /// The chrome pass must cost the **band**, not the document.
    ///
    /// `BlockContentCache` never evicts a block that merely left the band, so
    /// iterating it (which the first version of this system did) makes every
    /// frame O(everything scrolled past) — with a `String` allocation per
    /// bordered block. Found by kaibo/deepseek review, 2026-08-18.
    #[test]
    fn chrome_is_computed_for_the_band_not_the_whole_content_cache() {
        let mut app = App::new();
        app.init_resource::<EditorEntities>();
        app.init_resource::<Theme>();
        app.init_resource::<TextMetrics>();
        app.init_resource::<RpcConnectionState>();
        app.init_resource::<DriftState>();
        app.init_resource::<DocumentCache>();
        app.init_resource::<BlockContentCache>();
        app.init_resource::<BlockChromeCache>();
        app.insert_resource(ConversationScrollState {
            offset: 0.0,
            target_offset: 0.0,
            visible_height: 300.0,
            ..default()
        });
        app.add_systems(Update, sync_block_chrome);

        // 200 blocks of 42px pitch — far more document than the 2-screen
        // content band (300 viewport + 600 slack each side) can reach.
        const BLOCKS: u64 = 200;
        let geom_ent = app.world_mut().spawn(strip(BLOCKS)).id();
        let container = app
            .world_mut()
            .spawn(ComputedNode {
                size: Vec2::new(800.0, 300.0),
                inverse_scale_factor: 1.0,
                ..Default::default()
            })
            .id();
        {
            let mut entities = app.world_mut().resource_mut::<EditorEntities>();
            entities.main_cell = Some(geom_ent);
            entities.conversation_container = Some(container);
        }
        {
            // The cache holds the WHOLE document, as it does after a scroll
            // from top to bottom and back.
            let mut content = app.world_mut().resource_mut::<BlockContentCache>();
            for seq in 1..=BLOCKS {
                let mut border = inputs(BlockKind::ToolCall);
                border.id = bid(seq);
                content.insert_for_test(bid(seq), border);
            }
        }

        app.update();

        let cache = app.world().resource::<BlockChromeCache>();
        assert!(
            cache.len() < 40,
            "chrome computed {} entries for a 300px viewport — it is walking \
             the content cache instead of the band",
            cache.len(),
        );
        assert!(cache.len() > 5, "the band itself must still be covered");
        // And the entries it did compute are the ones at the top of the
        // document, where the viewport is.
        assert!(cache.get(&bid(1)).is_some());
        assert!(cache.get(&bid(BLOCKS)).is_none());
    }

    // ---- build_chrome_instances (the system) -----------------------------

    fn instance_app() -> App {
        let mut app = App::new();
        app.init_resource::<EditorEntities>();
        app.init_resource::<Theme>();
        app.init_resource::<FocusTarget>();
        app.init_resource::<HeaderLabelCache>();
        app.init_resource::<SurfaceMetricsEpoch>();
        app.init_resource::<BlockChromeCache>();
        app.init_resource::<ShapedBlockCache>();
        app.add_systems(Update, build_chrome_instances);
        app
    }

    fn setup(focused: Option<BlockId>) -> (App, Entity) {
        let mut app = instance_app();
        let theme = Theme::default();
        let geom_ent = app.world_mut().spawn(strip(4)).id();
        app.world_mut().resource_mut::<EditorEntities>().main_cell = Some(geom_ent);
        *app.world_mut().resource_mut::<BlockChromeCache>() =
            chrome_cache(&theme, &[(bid(1), BlockKind::Text), (bid(2), BlockKind::Text)]);
        if let Some(id) = focused {
            app.world_mut().resource_mut::<FocusTarget>().focus_block(id);
        }
        let pane = app.world_mut().spawn_empty().id();
        let mut surface = ConversationSurface::new(pane);
        surface.window.row_range = 0..5;
        let ent = app.world_mut().spawn((surface, ChromeInstances::default())).id();
        app.update();
        (app, ent)
    }

    fn chrome_of(app: &App, ent: Entity) -> &ChromeInstances {
        app.world().get::<ChromeInstances>(ent).unwrap()
    }

    /// The whole reason chrome has its own version: moving focus must bump it
    /// (the ring has to appear) and must NOT be able to touch the glyph
    /// buffer's version.
    #[test]
    fn a_focus_change_bumps_the_chrome_version() {
        let (mut app, ent) = setup(None);
        let before = chrome_of(&app, ent).version;
        let glyph_version_before = app
            .world()
            .get::<ConversationSurface>(ent)
            .unwrap()
            .buffer_version;

        app.world_mut()
            .resource_mut::<FocusTarget>()
            .focus_block(bid(1));
        app.update();

        let after = chrome_of(&app, ent);
        assert_eq!(after.version, before + 1);
        // The window also holds the role header's divider; the ring is the
        // one box quad in it.
        assert_eq!(
            after
                .instances
                .iter()
                .filter(|i| i.kind == CHROME_KIND_FULL)
                .count(),
            1,
            "the focused block gains a ring",
        );
        assert_eq!(
            app.world()
                .get::<ConversationSurface>(ent)
                .unwrap()
                .buffer_version,
            glyph_version_before,
            "chrome must never touch the glyph buffer's version",
        );
    }

    /// Instances are document-space, so a scroll inside the built window is
    /// not a reason to re-upload them.
    #[test]
    fn scrolling_does_not_bump_the_chrome_version() {
        let (mut app, ent) = setup(Some(bid(1)));
        let before = chrome_of(&app, ent).version;
        // Nothing about the surface's row range or the caches moved — this is
        // exactly the state a scroll inside the band leaves behind.
        app.update();
        app.update();
        assert_eq!(chrome_of(&app, ent).version, before);
    }

    /// Rows moving (a block above resized) must rebuild: the rects are
    /// absolute document positions.
    #[test]
    fn a_moved_row_bumps_the_chrome_version() {
        let (mut app, ent) = setup(Some(bid(2)));
        let before = chrome_of(&app, ent).version;
        let geom_ent = app.world().resource::<EditorEntities>().main_cell.unwrap();
        {
            let mut geom = app
                .world_mut()
                .get_mut::<ConversationGeometry>(geom_ent)
                .unwrap();
            geom.measure(RowKey::Block(bid(1)), 400.0, 12.0, 2);
            geom.recompute_offsets();
        }
        app.update();
        assert_eq!(chrome_of(&app, ent).version, before + 1);
    }
}
