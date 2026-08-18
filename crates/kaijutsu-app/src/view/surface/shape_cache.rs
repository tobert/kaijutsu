//! Shaped glyph runs, cached per block, and the measurement they feed back
//! into [`ConversationGeometry`].
//!
//! This is where "render once, re-render on rules" actually lives. Shaping
//! (parley) is the expensive, freeze-causing step; rasterization is not — the
//! MSDF atlas is shared and size-independent. So a block is shaped once into
//! chunk-local [`PositionedGlyph`] runs behind an [`Arc`], and every
//! subsequent frame that draws it is a buffer upload at worst.
//!
//! The re-shape decision is a single equality on [`ShapeKey`]: content
//! version, wrap width, collapsed, indent level, metrics epoch. Anything not
//! in that key is, by construction, not a reason to reshape (scroll offset
//! and atlas version most importantly — the first is a uniform, the second
//! only re-resolves UVs).
//!
//! **Slice 1 scope.** Shaping is synchronous and whole-block: the same cost
//! profile the legacy path already pays when a block enters the band, so no
//! regression, but no win yet either. The async backlog, tail freezing, LRU
//! eviction and the theme-recolor fast path are slice 3 — `glyph_count` and
//! `last_used` are kept live here so that slice has its inputs from day one.
//! Chrome (borders, padding, labels) is slice 2, so a block's shaped height
//! is its text height with no border padding added.

use std::collections::HashMap;
use std::sync::Arc;

use bevy::prelude::*;
use kaijutsu_types::{BlockId, Role};
use peniko::Brush;

use crate::cell::{ConversationScrollState, EditorEntities};
use crate::text::components::bevy_color_to_brush;
use crate::text::msdf::glyph::GlyphKey;
use crate::text::msdf::layout_bridge::{
    collect_msdf_glyphs_deferred, collect_msdf_glyphs_styled_deferred,
};
use crate::text::msdf::{FontDataMap, MsdfAtlas, PositionedGlyph};
use crate::text::rich::SpanBrush;
use crate::text::shaping::{VelloFont, VelloFontAxes, VelloTextAlign, VelloTextStyle};
use crate::text::{ShapingFonts, TextMetrics};
use crate::ui::theme::Theme;
use crate::view::geometry::{ConversationGeometry, RowKey};
use crate::view::role_divider;

use super::chunk::{CHUNK_LINES, chunk_ranges, chunk_shaping_text, slice_spans};

/// How many screens of slack, on each side of the viewport, get shaped.
///
/// Matches the window's slack (`window::WINDOW_SLACK_SCREENS`): shaping a row
/// the window will never assemble is wasted work, and *not* shaping one it
/// will assemble is a hole in the drawing.
pub const SHAPE_SLACK_SCREENS: f32 = 1.0;

// ============================================================================
// METRICS EPOCH
// ============================================================================

/// The font/metrics inputs that invalidate every shaped run when they move.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MetricsFingerprint {
    font_size: f32,
    line_height: f32,
    char_width: f32,
    letter_spacing: f32,
    label_font_size: f32,
}

/// Monotonic token for "text metrics changed, everything shaped is stale".
///
/// Its own resource rather than a `Changed<TextMetrics>` filter because
/// `TextMetrics` also carries `scale_factor`, which changes on a monitor swap
/// and must **not** invalidate shaping — MSDF glyphs are resolution
/// independent, so a DPI change costs a texture realloc and nothing else.
/// Change detection cannot express "this field but not that one"; a
/// fingerprint can.
#[derive(Resource, Debug, Default)]
pub struct SurfaceMetricsEpoch {
    epoch: u64,
    last: Option<MetricsFingerprint>,
}

impl SurfaceMetricsEpoch {
    pub fn get(&self) -> u64 {
        self.epoch
    }
}

/// Bump [`SurfaceMetricsEpoch`] when the shaping-relevant metrics move.
pub fn track_surface_metrics_epoch(
    text_metrics: Res<TextMetrics>,
    theme: Res<Theme>,
    mut epoch: ResMut<SurfaceMetricsEpoch>,
) {
    let now = MetricsFingerprint {
        font_size: text_metrics.cell_font_size,
        line_height: text_metrics.cell_line_height,
        char_width: text_metrics.cell_char_width,
        letter_spacing: text_metrics.letter_spacing,
        label_font_size: theme.label_font_size,
    };
    if epoch.last == Some(now) {
        return;
    }
    let e = epoch.as_mut();
    e.last = Some(now);
    e.epoch = e.epoch.wrapping_add(1);
}

// ============================================================================
// CACHE TYPES
// ============================================================================

/// Everything a shaped block depends on. Equality here is the whole
/// re-shape rule table for slice 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapeKey {
    /// [`super::content::FormattedBlock::version`] — moves only when the
    /// block's rendered text/color actually changed.
    pub content_version: u64,
    /// Wrap width in logical px, as raw bits so the key can be `Eq`. Width is
    /// derived, not authored, so bit equality is the right comparison: a
    /// width that recomputed to the same float is the same layout.
    pub wrap_width_bits: u32,
    /// Read from `GeomRow.collapsed`, which is captured at seed time and
    /// **not** refreshed for a row the reconcile already knows — so a
    /// collapse toggle does not move this field. It is kept in the key
    /// because it is genuinely a shaping input, and because the toggle is
    /// covered anyway: collapsing rewrites the block's formatted text
    /// (`Thinking [collapsed]`, the error stub), which moves
    /// `content_version`. Do not add a re-shape rule that relies on this
    /// field alone without fixing the geometry seed first.
    pub collapsed: bool,
    pub indent_level: u32,
    /// [`SurfaceMetricsEpoch::get`].
    pub metrics_epoch: u64,
}

/// One chunk's shaped glyphs, in **chunk-local** coordinates: x from 0 at the
/// block's text origin, y from 0 at the chunk's top.
///
/// Chunk-local is what makes the runs reusable — the extractor adds the
/// block's document y and indent x at assembly time, so a block that moves
/// (anything above it resized) costs nothing to re-place. The `Arc` is so
/// extraction never deep-clones a large block's glyphs.
#[derive(Debug, Clone)]
pub struct ShapedChunk {
    /// Byte range within the block's text (see `chunk::chunk_ranges`).
    pub byte_range: std::ops::Range<usize>,
    pub glyphs: Arc<Vec<PositionedGlyph>>,
    /// This chunk's layout height — the amount the next chunk stacks down by.
    pub height: f32,
}

/// A block's cached shaping.
#[derive(Debug, Clone)]
pub struct ShapedBlock {
    pub key: ShapeKey,
    pub chunks: Vec<ShapedChunk>,
    /// Total text height (sum of chunk heights). No border padding — chrome
    /// is slice 2.
    pub height: f32,
    /// Horizontal offset of the block's text origin within the surface
    /// (indent). Baked at shape time so assembly needs no theme access.
    pub x_offset: f32,
    /// Total glyphs held here — the input to slice 3's memory budget.
    pub glyph_count: usize,
    /// [`ShapedBlockCache::tick`] at the last time this block was wanted —
    /// slice 3's LRU input.
    pub last_used: u64,
}

/// Shaped glyph runs for every block the surface might draw.
#[derive(Resource, Debug, Default)]
pub struct ShapedBlockCache {
    blocks: HashMap<BlockId, ShapedBlock>,
    /// Incremented once per [`shape_visible_blocks`] pass; stamped onto every
    /// block the pass wanted.
    tick: u64,
}

impl ShapedBlockCache {
    pub fn get(&self, id: &BlockId) -> Option<&ShapedBlock> {
        self.blocks.get(id)
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    #[allow(dead_code)] // Symmetry with `len`; slice 3's eviction wants it.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Total glyphs held across every cached block — slice 3 evicts against
    /// this, and diagnostics can report it now.
    pub fn total_glyphs(&self) -> usize {
        self.blocks.values().map(|b| b.glyph_count).sum()
    }

    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Seed a shaped block directly. Test-only: the production writer is
    /// [`shape_visible_blocks`], and `window`'s assembly tests need cache
    /// contents without a font context or an atlas.
    #[cfg(test)]
    pub(crate) fn insert_for_test(&mut self, id: BlockId, block: ShapedBlock) {
        self.blocks.insert(id, block);
    }
}

/// One role header's shaped label run.
#[derive(Debug, Clone)]
pub struct ShapedLabel {
    pub glyphs: Arc<Vec<PositionedGlyph>>,
    /// Left edge of the label within the header row (from
    /// `role_divider::compute_role_divider_layout`).
    pub x_offset: f32,
    /// Top of the label within the header row — the label is vertically
    /// centered on the divider line, matching `sync_role_group_headers`.
    pub y_offset: f32,
    pub width: f32,
    pub height: f32,
}

/// Shaped USER/ASSISTANT/… labels, keyed by `(role, metrics_epoch)`.
///
/// Tiny and shared: one entry per role, not per header row. Theme *colors*
/// are baked into the glyphs here and are deliberately not part of the key —
/// recoloring cached runs in place is slice 3's fast path, and a theme swap
/// in slice 1 is handled by the cache being rebuilt on the next metrics bump
/// or app restart. Noted rather than hidden.
///
/// The map is keyed on [`role_index`] rather than `Role` itself: `Role` is a
/// wire type in `kaijutsu-types` and does not derive `Hash`, and teaching a
/// protocol type a trait for one app-side cache is the wrong direction of
/// dependency.
#[derive(Resource, Debug, Default)]
pub struct HeaderLabelCache {
    labels: HashMap<(u8, u64), ShapedLabel>,
}

/// Dense index for a `Role`, for use as a hash key. Exhaustive on purpose —
/// a new role must be given an index here rather than silently colliding.
fn role_index(role: Role) -> u8 {
    match role {
        Role::User => 0,
        Role::Model => 1,
        Role::System => 2,
        Role::Tool => 3,
        Role::Asset => 4,
    }
}

impl HeaderLabelCache {
    pub fn get(&self, role: Role, metrics_epoch: u64) -> Option<&ShapedLabel> {
        self.labels.get(&(role_index(role), metrics_epoch))
    }

    pub fn len(&self) -> usize {
        self.labels.len()
    }

    /// Seed a shaped label directly — test-only, same reasoning as
    /// [`ShapedBlockCache::insert_for_test`].
    #[cfg(test)]
    pub(crate) fn insert_for_test(&mut self, role: Role, metrics_epoch: u64, label: ShapedLabel) {
        self.labels.insert((role_index(role), metrics_epoch), label);
    }
}

// ============================================================================
// SHAPING
// ============================================================================

/// The wrap width available to a block's text.
///
/// Derived the same way `sync_conversation_geometry` derives its estimate
/// columns (`geometry.rs:591-598`): the conversation container's
/// **content-box width in logical px**, so every unit in this model — row
/// heights, scroll offsets, wrap widths — comes from one source. Indent
/// (`update_block_cell_nodes`'s left margin) comes off the top.
///
/// Slice 1 has no chrome, so the border glow margin and border padding the
/// legacy node carries are *not* subtracted; that arrives with slice 2 and
/// will narrow every block by a few px at that point.
pub fn surface_wrap_width(container_width: f32, indent_level: u32, indent_width: f32) -> f32 {
    (container_width - indent_level as f32 * indent_width).max(1.0)
}

/// The block-text style, matching `build_block_scenes`' plain-text arm
/// (`view/block_render.rs:660-668`) so heights land where the legacy path
/// puts them.
pub fn block_text_style(text_metrics: &TextMetrics, brush: Brush) -> VelloTextStyle {
    VelloTextStyle {
        brush,
        font_size: text_metrics.cell_font_size,
        font_axes: VelloFontAxes {
            weight: Some(200.0),
            ..default()
        },
        ..default()
    }
}

/// Shape one block's text into chunked, chunk-local glyph runs.
///
/// Returns the chunks, the total height, and the deduplicated glyph keys the
/// caller must replay into [`MsdfAtlas::request`] — the deferred-collector
/// contract, kept even though this slice runs on the main thread, because
/// slice 3 moves exactly this function onto the task pool.
///
/// `font_data` is fed every glyph run's font so the MSDF generator has bytes
/// to rasterize from; skipping it yields a cache full of glyphs the atlas can
/// never fill.
pub fn shape_block(
    font: &VelloFont,
    text: &str,
    spans: &[SpanBrush],
    style: &VelloTextStyle,
    wrap_width: f32,
    chunk_lines: usize,
    font_data: &mut FontDataMap,
) -> (Vec<ShapedChunk>, f32, Vec<GlyphKey>) {
    let max_advance = (wrap_width > 0.0).then_some(wrap_width);
    let fallback = style.brush.clone();

    let mut chunks = Vec::new();
    let mut keys = Vec::new();
    let mut height = 0.0_f32;

    for range in chunk_ranges(text, chunk_lines) {
        let chunk_text = chunk_shaping_text(text, &range);
        let chunk_spans = slice_spans(spans, range.clone());

        // Ranged styles (`layout_spanned` + the styled collector) are the
        // only path that colors a span starting mid-run; the plain path is
        // kept for the span-free majority because it skips parley's run
        // splitting entirely.
        let (glyphs, chunk_keys, layout_height) = if chunk_spans.is_empty() {
            let layout = font.layout(chunk_text, style, VelloTextAlign::Left, max_advance);
            register_fonts(&layout, font_data);
            let (glyphs, keys) =
                collect_msdf_glyphs_deferred(&layout, &[], &fallback, (0.0, 0.0));
            (glyphs, keys, layout.height())
        } else {
            let layout = font.layout_spanned(
                chunk_text,
                style,
                VelloTextAlign::Left,
                max_advance,
                &chunk_spans,
            );
            register_fonts(&layout, font_data);
            let (glyphs, keys) = collect_msdf_glyphs_styled_deferred(&layout, (0.0, 0.0));
            (glyphs, keys, layout.height())
        };

        keys.extend(chunk_keys);
        height += layout_height;
        chunks.push(ShapedChunk {
            byte_range: range,
            glyphs: Arc::new(glyphs),
            height: layout_height,
        });
    }

    (chunks, height, keys)
}

/// Register every font a layout used, so MSDF generation has its bytes.
fn register_fonts(layout: &parley::Layout<Brush>, font_data: &mut FontDataMap) {
    for line in layout.lines() {
        for item in line.items() {
            if let parley::PositionedLayoutItem::GlyphRun(run) = item {
                font_data.register(run.run().font());
            }
        }
    }
}

/// Shape the blocks (and role labels) inside the window band.
///
/// Synchronous and whole-block this slice. A block whose [`ShapeKey`] still
/// matches its cache entry costs one comparison; a mismatch pays the full
/// parley layout right here, on the main thread, where the atlas can be
/// requested on the spot.
#[allow(clippy::too_many_arguments)]
pub fn shape_visible_blocks(
    entities: Res<EditorEntities>,
    geometries: Query<&ConversationGeometry>,
    computed_nodes: Query<&ComputedNode>,
    scroll_state: Res<ConversationScrollState>,
    text_metrics: Res<TextMetrics>,
    theme: Res<Theme>,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    metrics_epoch: Res<SurfaceMetricsEpoch>,
    content: Res<super::content::BlockContentCache>,
    atlas: Option<ResMut<MsdfAtlas>>,
    font_data: Option<ResMut<FontDataMap>>,
    mut shaped: ResMut<ShapedBlockCache>,
    mut headers: ResMut<HeaderLabelCache>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(geom) = geometries.get(main_ent) else {
        return;
    };
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    // No atlas / no font map means the render world isn't up yet. Shaping
    // anyway would fill the cache with glyphs nothing can ever rasterize and
    // — worse — stamp a matching ShapeKey, so the block would never be
    // reshaped once it could be. Wait a frame instead.
    let (Some(mut atlas), Some(mut font_data)) = (atlas, font_data) else {
        return;
    };

    let container_width = entities
        .conversation_container
        .and_then(|e| computed_nodes.get(e).ok())
        .map(|c| crate::view::ui_rtt::logical_content_size(c).x)
        .filter(|w| *w > 1.0);
    let Some(container_width) = container_width else {
        // Before the container is laid out there is no honest wrap width, and
        // shaping at a guessed one would measure heights the next frame
        // contradicts.
        return;
    };

    let vh = if scroll_state.visible_height > 0.0 {
        scroll_state.visible_height
    } else {
        600.0
    };
    let band = geom.visible_rows(scroll_state.offset, vh, SHAPE_SLACK_SCREENS * vh);
    let metrics_epoch = metrics_epoch.get();

    shaped.tick = shaped.tick.wrapping_add(1);
    let tick = shaped.tick;

    for row in &geom.rows()[band] {
        match row.key {
            RowKey::Header(_) => {
                shape_role_label(
                    font,
                    row.role,
                    metrics_epoch,
                    &theme,
                    &mut headers,
                    &mut atlas,
                    &mut font_data,
                );
            }
            RowKey::Block(id) => {
                let Some(formatted) = content.get(&id) else {
                    // Content sync hasn't reached this row yet (it runs with a
                    // wider band, so this is a first-frame ordering case, not
                    // a hole). It will be here next frame.
                    continue;
                };
                let wrap_width =
                    surface_wrap_width(container_width, row.indent_level, theme.indent_width);
                let key = ShapeKey {
                    content_version: formatted.version,
                    wrap_width_bits: wrap_width.to_bits(),
                    collapsed: row.collapsed,
                    indent_level: row.indent_level,
                    metrics_epoch,
                };

                if let Some(existing) = shaped.blocks.get_mut(&id) {
                    if existing.key == key {
                        existing.last_used = tick;
                        continue;
                    }
                }

                let style = block_text_style(&text_metrics, bevy_color_to_brush(formatted.color));
                let (chunks, height, keys) = shape_block(
                    font,
                    &formatted.text,
                    &formatted.spans,
                    &style,
                    wrap_width,
                    CHUNK_LINES,
                    &mut font_data,
                );
                for key in keys {
                    atlas.request(key);
                }

                let glyph_count = chunks.iter().map(|c| c.glyphs.len()).sum();
                shaped.blocks.insert(
                    id,
                    ShapedBlock {
                        key,
                        chunks,
                        height,
                        x_offset: row.indent_level as f32 * theme.indent_width,
                        glyph_count,
                        last_used: tick,
                    },
                );
            }
        }
    }

    // Blocks the document dropped keep neither text nor glyphs.
    if shaped.len() > content.len() {
        shaped.blocks.retain(|id, _| content.get(id).is_some());
    }
}

/// Shape (once per role per metrics epoch) the USER/ASSISTANT/… label a role
/// header draws, positioned exactly where `sync_role_group_headers` puts it.
fn shape_role_label(
    font: &VelloFont,
    role: Role,
    metrics_epoch: u64,
    theme: &Theme,
    headers: &mut HeaderLabelCache,
    atlas: &mut MsdfAtlas,
    font_data: &mut FontDataMap,
) {
    if headers.get(role, metrics_epoch).is_some() {
        return;
    }

    let color = match role {
        Role::User => theme.block_user,
        Role::Model => theme.block_assistant,
        Role::System => theme.fg_dim,
        Role::Tool | Role::Asset => theme.block_tool_call,
    };
    let text = match role {
        Role::User => "USER",
        Role::Model => "ASSISTANT",
        Role::System => "SYSTEM",
        Role::Tool => "TOOL",
        Role::Asset => "ASSET",
    };

    let brush = bevy_color_to_brush(color);
    let style = VelloTextStyle {
        brush: brush.clone(),
        font_size: role_divider::ROLE_LABEL_FONT_SIZE,
        ..default()
    };
    let layout = font.layout(text, &style, VelloTextAlign::Left, None);
    let width = layout.width();
    let height = layout.height();
    let div = role_divider::compute_role_divider_layout(
        width as f64,
        theme.label_inset as f64,
        theme.label_pad as f64,
    );
    // Vertically centered in the header row, whose height on this path is the
    // theme constant the geometry reconcile already reserves for it.
    let y_offset = ((theme.role_header_height - height) * 0.5).max(0.0);

    register_fonts(&layout, font_data);
    let (glyphs, keys) = collect_msdf_glyphs_deferred(&layout, &[], &brush, (0.0, 0.0));
    for key in keys {
        atlas.request(key);
    }

    headers.labels.insert(
        (role_index(role), metrics_epoch),
        ShapedLabel {
            glyphs: Arc::new(glyphs),
            x_offset: div.label_x as f32,
            y_offset,
            width,
            height,
        },
    );
}

// ============================================================================
// MEASUREMENT + SCROLL ANCHORING
// ============================================================================

/// Apply a batch of row measurements and report the scroll anchor delta.
///
/// The pure seam the plan calls for: everything `readback_block_heights`
/// (`view/render.rs:693-745`) does with taffy's numbers, minus taffy.
///
/// `anchor_delta` accumulates the height change of every row **fully above
/// `viewport_top`**, compared against **pre-measure** offsets — the offsets
/// the current scroll position was itself computed against. The predicate is
/// `old_y + old_h <= viewport_top`, so a row whose bottom edge lands exactly
/// on the viewport top counts as above: it is entirely out of sight, and its
/// resize does move everything the user is looking at.
///
/// Offsets are recomputed once at the end, so every row in the batch is
/// judged against the same pre-batch geometry regardless of order.
pub fn apply_measurements(
    geom: &mut ConversationGeometry,
    measurements: &[(RowKey, f32, f32, u64)],
    viewport_top: f32,
) -> f32 {
    let mut anchor_delta = 0.0_f32;
    for &(key, height, margin, version) in measurements {
        let Some((old_y, old_h)) = row_extent(geom, key) else {
            continue;
        };
        let above_viewport = old_y + old_h <= viewport_top;
        let delta = geom.measure(key, height, margin, version);
        if above_viewport {
            anchor_delta += delta;
        }
    }
    geom.recompute_offsets();
    anchor_delta
}

/// Pre-measure `(y_offset, height)` for a row, whichever kind it is.
fn row_extent(geom: &ConversationGeometry, key: RowKey) -> Option<(f32, f32)> {
    match key {
        RowKey::Block(id) => geom.block_row(&id).map(|r| (r.y_offset, r.height)),
        RowKey::Header(id) => geom.header_row(&id).map(|r| (r.y_offset, r.height)),
    }
}

/// Feed shaped heights into the geometry and anchor-compensate the scroll.
///
/// The surface path's replacement for `readback_block_heights`, and it runs
/// **before `smooth_scroll`** rather than in `PostUpdate` — see the module
/// docs on `view::surface`. The `new_blocks_added` / `pending_scroll_anchor`
/// / `content_height` bookkeeping moves here verbatim, because
/// `smooth_scroll` consumes it in the same frame.
///
/// **Margins come from the geometry's own model**, not from a mirror of
/// `update_block_cell_nodes`: `reconcile` already decides them (block
/// spacing, the zero gap that joins a ToolCall to its ToolResult, role-header
/// spacing) from the theme, and re-deriving them here would be a second
/// source of truth for the same number. So each measurement passes the row's
/// current `margin_bottom` straight back through — `measure` treats an
/// unchanged margin as unchanged.
pub fn apply_shaped_measurements(
    entities: Res<EditorEntities>,
    mut geometries: Query<&mut ConversationGeometry>,
    shaped: Res<ShapedBlockCache>,
    theme: Res<Theme>,
    mut scroll_state: ResMut<ConversationScrollState>,
    mut last_tail: Local<Option<BlockId>>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(mut geom) = geometries.get_mut(main_ent) else {
        return;
    };

    // Stamp `new_blocks_added` from the geometry itself: on the legacy path
    // the sole writer is `spawn_block_cells`, which is flag-gated off here,
    // so without this the reveal-from-top anchor for tall new blocks was
    // dead under `Surface` — a streamed block taller than the viewport
    // snapped follow mode to its BOTTOM (found by review, 2026-08-18). A new
    // last block row means new tail content this frame. The first
    // observation (cold start / hydrate) only initializes the tracker; a
    // false positive from tail exclusion is harmless — the anchor degrades
    // to `min(max, anchor) == max`, exactly the no-anchor behavior.
    let tail_now = geom
        .rows()
        .iter()
        .rev()
        .find_map(|r| match r.key {
            RowKey::Block(id) => Some(id),
            RowKey::Header(_) => None,
        });
    if let Some(tail) = tail_now
        && let Some(prev) = *last_tail
        && tail != prev
    {
        scroll_state.new_blocks_added = true;
    }
    if tail_now.is_some() {
        *last_tail = tail_now;
    }

    let mut measurements: Vec<(RowKey, f32, f32, u64)> = Vec::new();
    for row in geom.rows() {
        match row.key {
            RowKey::Block(id) => {
                let Some(block) = shaped.get(&id) else {
                    continue;
                };
                if (row.height - block.height).abs() <= 0.01 && row.measured_version != 0 {
                    continue;
                }
                measurements.push((
                    row.key,
                    block.height,
                    row.margin_bottom,
                    block.key.content_version,
                ));
            }
            RowKey::Header(_) => {
                // Role headers are a fixed-height row on this path: the label
                // is drawn inside the height `reconcile` already reserved, so
                // there is nothing to correct. Stamp it measured once so the
                // model stops classifying it as an estimate.
                if row.measured_version != 0 {
                    continue;
                }
                measurements.push((
                    row.key,
                    theme.role_header_height,
                    row.margin_bottom,
                    1,
                ));
            }
        }
    }

    if measurements.is_empty() && !scroll_state.new_blocks_added {
        // Nothing landed and nothing is waiting on the anchor — don't touch
        // the scroll state's change detection.
        if (scroll_state.content_height - geom.content_height).abs() > 0.5 {
            scroll_state.content_height = geom.content_height;
        }
        return;
    }

    let anchor_delta = apply_measurements(&mut geom, &measurements, scroll_state.offset);

    // When new blocks were added this frame, record the pre-update content
    // height as an anchor; `smooth_scroll` uses `min(max, anchor)` so new
    // content is revealed from its start rather than jumping to its bottom.
    if scroll_state.new_blocks_added {
        scroll_state.pending_scroll_anchor = Some(scroll_state.content_height);
        scroll_state.new_blocks_added = false;
    }

    if (scroll_state.content_height - geom.content_height).abs() > 0.5 {
        scroll_state.content_height = geom.content_height;
    }

    // Anchor correction: keep the viewport visually pinned when content above
    // it changed size. Follow mode is exempt — it re-clamps to the bottom
    // every frame anyway, and correcting under it would fight the clamp
    // during streaming. (Verbatim from `render.rs:799-802`.)
    if anchor_delta.abs() > 0.5 && !scroll_state.following {
        scroll_state.offset = (scroll_state.offset + anchor_delta).max(0.0);
        scroll_state.target_offset = (scroll_state.target_offset + anchor_delta).max(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockKind, ContextId, PrincipalId};

    use crate::text::msdf::layout_bridge::collect_msdf_glyphs_deferred;
    use crate::text::shaping::load_into_font_context;
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

    /// Same-role blocks: header (20 + 4) at y=0, then blocks of 42 pitch
    /// (30 + 12) from y=24.
    fn strip(n: u64) -> ConversationGeometry {
        let mut geom = ConversationGeometry::default();
        let ids: Vec<BlockId> = (1..=n).map(bid).collect();
        geom.reconcile(&ids, |_| Some(seed()), &params(), 1);
        geom.recompute_offsets();
        geom
    }

    fn mono() -> VelloFont {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/fonts/NotoMono-Regular.ttf"
        ))
        .expect("shipped test font must be present");
        load_into_font_context(bytes)
    }

    fn style() -> VelloTextStyle {
        block_text_style(&TextMetrics::default(), bevy_color_to_brush(Color::WHITE))
    }

    // ---- ShapeKey --------------------------------------------------------

    fn key() -> ShapeKey {
        ShapeKey {
            content_version: 7,
            wrap_width_bits: 800.0_f32.to_bits(),
            collapsed: false,
            indent_level: 0,
            metrics_epoch: 3,
        }
    }

    /// Every field of the key must be load-bearing: flipping any one of them
    /// has to read as a mismatch, or a real re-shape trigger silently stops
    /// firing. The matrix is the test — a field added to `ShapeKey` without a
    /// line here is a field nobody checked.
    #[test]
    fn every_shape_key_field_forces_a_reshape() {
        let base = key();
        let variants = [
            ShapeKey { content_version: 8, ..base },
            ShapeKey { wrap_width_bits: 640.0_f32.to_bits(), ..base },
            ShapeKey { collapsed: true, ..base },
            ShapeKey { indent_level: 1, ..base },
            ShapeKey { metrics_epoch: 4, ..base },
        ];
        for (i, variant) in variants.iter().enumerate() {
            assert_ne!(*variant, base, "ShapeKey field {i} does not affect equality");
        }
        assert_eq!(key(), base, "an identical key must match");
    }

    // ---- wrap width ------------------------------------------------------

    #[test]
    fn wrap_width_subtracts_the_indent() {
        assert_eq!(surface_wrap_width(800.0, 0, 24.0), 800.0);
        assert_eq!(surface_wrap_width(800.0, 2, 24.0), 752.0);
    }

    #[test]
    fn wrap_width_never_goes_non_positive() {
        // A deeply indented block in a narrow pane still gets a layout width
        // parley can use rather than a zero/negative max_advance.
        assert_eq!(surface_wrap_width(20.0, 10, 24.0), 1.0);
    }

    // ---- shaped heights --------------------------------------------------

    /// A block's shaped height must be `lines * line_height` for unwrapped
    /// text — the same shape the geometry estimator assumes, and the property
    /// the whole prefix-sum model rests on. Pinned against the font's own
    /// single-line height rather than a hardcoded number, because the line
    /// height comes from the font's metrics at `TextMetrics::cell_font_size`.
    #[test]
    fn shaped_height_is_line_height_times_lines() {
        let font = mono();
        let mut font_data = FontDataMap::default();
        let (_, one_line, _) = shape_block(
            &font,
            "one",
            &[],
            &style(),
            10_000.0,
            CHUNK_LINES,
            &mut font_data,
        );
        assert!(one_line > 0.0, "a single line must have a height");

        for lines in [2_usize, 5, 40] {
            let text = vec!["one"; lines].join("\n");
            let (_, height, _) = shape_block(
                &font,
                &text,
                &[],
                &style(),
                10_000.0,
                CHUNK_LINES,
                &mut font_data,
            );
            assert!(
                (height - one_line * lines as f32).abs() < 0.05,
                "{lines} lines measured {height}, expected {}",
                one_line * lines as f32,
            );
        }
    }

    /// The block height must be exactly the stack of its chunks, or the
    /// extractor's per-chunk `doc_y` and the geometry's row height describe
    /// different documents.
    #[test]
    fn block_height_is_the_sum_of_its_chunk_heights() {
        let font = mono();
        let mut font_data = FontDataMap::default();
        let text = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (chunks, height, _) = shape_block(
            &font,
            &text,
            &[],
            &style(),
            10_000.0,
            CHUNK_LINES,
            &mut font_data,
        );
        assert!(chunks.len() > 1, "test premise: 200 lines must chunk");
        let stacked: f32 = chunks.iter().map(|c| c.height).sum();
        assert!((height - stacked).abs() < 0.01);
        assert_eq!(
            chunks.last().unwrap().byte_range.end,
            text.len(),
            "chunks must tile the whole text",
        );
    }

    /// Chunked shaping must place glyphs exactly where one whole-block layout
    /// would — the same claim `chunk.rs` pins for the raw helpers, here
    /// through the production `shape_block` path (chunk-local glyphs stacked
    /// by chunk height).
    #[test]
    fn shape_block_stacks_chunks_where_a_whole_layout_would_put_them() {
        let font = mono();
        let mut font_data = FontDataMap::default();
        let text = (0..150)
            .map(|i| format!("line {i} with a little more text on it"))
            .collect::<Vec<_>>()
            .join("\n");
        let wrap = 220.0_f32;

        let whole = font.layout(&text, &style(), VelloTextAlign::Left, Some(wrap));
        let (expected, _) =
            collect_msdf_glyphs_deferred(&whole, &[], &style().brush, (0.0, 0.0));

        let (chunks, _, _) = shape_block(
            &font,
            &text,
            &[],
            &style(),
            wrap,
            CHUNK_LINES,
            &mut font_data,
        );
        assert!(chunks.len() > 1, "test premise: the text must chunk");

        let mut actual = Vec::new();
        let mut y = 0.0_f32;
        for chunk in &chunks {
            for glyph in chunk.glyphs.iter() {
                let mut g = glyph.clone();
                g.y += y;
                actual.push(g);
            }
            y += chunk.height;
        }

        assert_eq!(expected.len(), actual.len(), "glyph count differs");
        for (i, (a, b)) in expected.iter().zip(actual.iter()).enumerate() {
            assert_eq!(a.key, b.key, "glyph {i} differs");
            assert_eq!(a.x, b.x, "glyph {i} moved horizontally");
            // Sub-pixel: parley re-quantizes baselines per layout. See the
            // `chunk` module docs — the bound is one rounding, and it does not
            // accumulate.
            assert!((a.y - b.y).abs() < 1.0, "glyph {i} y: {} vs {}", a.y, b.y);
        }
    }

    #[test]
    fn empty_text_still_shapes_one_chunk() {
        let font = mono();
        let mut font_data = FontDataMap::default();
        let (chunks, height, keys) =
            shape_block(&font, "", &[], &style(), 400.0, CHUNK_LINES, &mut font_data);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].glyphs.is_empty());
        assert!(keys.is_empty());
        assert!(height >= 0.0);
    }

    // ---- apply_measurements ----------------------------------------------

    #[test]
    fn a_row_above_the_viewport_contributes_its_delta_to_the_anchor() {
        let mut geom = strip(4);
        // Row 1 (block 1) spans [24, 66). Viewport top well below it.
        let delta = apply_measurements(
            &mut geom,
            &[(RowKey::Block(bid(1)), 90.0, 12.0, 5)],
            500.0,
        );
        assert_eq!(delta, 60.0);
        assert_eq!(geom.block_row(&bid(1)).unwrap().height, 90.0);
    }

    #[test]
    fn a_row_below_the_viewport_does_not_move_the_anchor() {
        let mut geom = strip(4);
        let delta = apply_measurements(
            &mut geom,
            &[(RowKey::Block(bid(4)), 200.0, 12.0, 5)],
            0.0,
        );
        assert_eq!(delta, 0.0);
        assert_eq!(geom.block_row(&bid(4)).unwrap().height, 200.0);
    }

    /// The boundary case the `<=` predicate decides: a row whose bottom edge
    /// lands *exactly* on the viewport top is fully out of sight, so its
    /// resize does shift what the user is looking at and must be
    /// compensated. Flipping this to `<` is a silent one-row anchor drift.
    #[test]
    fn a_row_ending_exactly_at_the_viewport_top_counts_as_above() {
        let mut geom = strip(4);
        let row = geom.block_row(&bid(1)).unwrap();
        let bottom = row.y_offset + row.height; // 24 + 30 = 54
        let delta = apply_measurements(
            &mut geom,
            &[(RowKey::Block(bid(1)), 40.0, 12.0, 5)],
            bottom,
        );
        assert_eq!(delta, 10.0);
    }

    /// Every row in a batch is judged against the same pre-batch offsets —
    /// measuring row 1 must not push row 2 below the viewport top mid-pass.
    #[test]
    fn a_batch_is_judged_against_pre_measure_offsets() {
        let mut geom = strip(4);
        // Rows 1 and 2 both sit above y=120 before anything is measured.
        let viewport_top = 120.0;
        let delta = apply_measurements(
            &mut geom,
            &[
                (RowKey::Block(bid(1)), 60.0, 12.0, 5),
                (RowKey::Block(bid(2)), 60.0, 12.0, 5),
            ],
            viewport_top,
        );
        assert_eq!(delta, 60.0, "both rows were above the viewport pre-measure");
    }

    #[test]
    fn measurements_for_unknown_rows_are_skipped() {
        let mut geom = strip(2);
        let delta = apply_measurements(
            &mut geom,
            &[(RowKey::Block(bid(99)), 500.0, 12.0, 5)],
            0.0,
        );
        assert_eq!(delta, 0.0);
    }

    #[test]
    fn apply_measurements_recomputes_offsets() {
        let mut geom = strip(3);
        let before = geom.content_height;
        apply_measurements(&mut geom, &[(RowKey::Block(bid(1)), 90.0, 12.0, 5)], 0.0);
        assert_eq!(geom.content_height, before + 60.0);
        // Prefix sums moved with it.
        assert_eq!(geom.block_row(&bid(2)).unwrap().y_offset, 24.0 + 90.0 + 12.0);
    }

    #[test]
    fn header_rows_measure_through_the_header_key() {
        let mut geom = strip(2);
        let delta = apply_measurements(
            &mut geom,
            &[(RowKey::Header(bid(1)), 26.0, 4.0, 1)],
            1000.0,
        );
        assert_eq!(delta, 6.0);
        assert_eq!(geom.header_row(&bid(1)).unwrap().height, 26.0);
    }

    // ---- apply_shaped_measurements (the system) ---------------------------

    fn measure_app(following: bool, offset: f32) -> App {
        let mut app = App::new();
        app.init_resource::<EditorEntities>();
        app.init_resource::<Theme>();
        app.init_resource::<ShapedBlockCache>();
        app.insert_resource(ConversationScrollState {
            offset,
            target_offset: offset,
            following,
            visible_height: 300.0,
            ..default()
        });
        app.add_systems(Update, apply_shaped_measurements);
        app
    }

    fn install_geometry(app: &mut App, geom: ConversationGeometry) -> Entity {
        let ent = app.world_mut().spawn(geom).id();
        app.world_mut().resource_mut::<EditorEntities>().main_cell = Some(ent);
        ent
    }

    fn insert_shaped(app: &mut App, id: BlockId, height: f32) {
        let block = ShapedBlock {
            key: ShapeKey {
                content_version: 5,
                wrap_width_bits: 800.0_f32.to_bits(),
                collapsed: false,
                indent_level: 0,
                metrics_epoch: 1,
            },
            chunks: Vec::new(),
            height,
            x_offset: 0.0,
            glyph_count: 0,
            last_used: 0,
        };
        app.world_mut()
            .resource_mut::<ShapedBlockCache>()
            .blocks
            .insert(id, block);
    }

    #[test]
    fn shaped_heights_land_in_the_geometry_and_shift_the_scroll_offset() {
        let mut app = measure_app(false, 500.0);
        install_geometry(&mut app, strip(4));
        insert_shaped(&mut app, bid(1), 90.0); // +60 above the viewport

        app.update();

        let state = app.world().resource::<ConversationScrollState>();
        assert_eq!(state.offset, 560.0);
        assert_eq!(state.target_offset, 560.0);
    }

    /// Follow mode is exempt: it re-clamps to the bottom every frame, and
    /// compensating under it fights that clamp during streaming.
    #[test]
    fn follow_mode_is_exempt_from_anchor_compensation() {
        let mut app = measure_app(true, 500.0);
        install_geometry(&mut app, strip(4));
        insert_shaped(&mut app, bid(1), 90.0);

        app.update();

        let state = app.world().resource::<ConversationScrollState>();
        assert_eq!(state.offset, 500.0, "follow mode must not be anchor-shifted");
    }

    #[test]
    fn content_height_is_mirrored_into_the_scroll_state() {
        let mut app = measure_app(false, 0.0);
        let ent = install_geometry(&mut app, strip(4));
        insert_shaped(&mut app, bid(1), 90.0);

        app.update();

        let geom_height = app
            .world()
            .get::<ConversationGeometry>(ent)
            .unwrap()
            .content_height;
        assert_eq!(
            app.world().resource::<ConversationScrollState>().content_height,
            geom_height,
        );
    }

    /// `new_blocks_added` becomes the reveal-from-the-top anchor that
    /// `smooth_scroll` consumes the same frame — the bookkeeping
    /// `readback_block_heights` used to own.
    #[test]
    fn new_blocks_added_becomes_the_pending_scroll_anchor() {
        let mut app = measure_app(true, 0.0);
        install_geometry(&mut app, strip(4));
        {
            let mut state = app.world_mut().resource_mut::<ConversationScrollState>();
            state.content_height = 1234.0;
            state.new_blocks_added = true;
        }

        app.update();

        let state = app.world().resource::<ConversationScrollState>();
        assert_eq!(state.pending_scroll_anchor, Some(1234.0));
        assert!(!state.new_blocks_added);
    }

    /// The surface path must stamp `new_blocks_added` ITSELF when a new tail
    /// block row appears — its legacy writer (`spawn_block_cells`) is
    /// flag-gated off, and without a stamp the reveal-from-top anchor is
    /// dead: a streamed block taller than the viewport snaps follow mode to
    /// its bottom (review find, 2026-08-18).
    #[test]
    fn a_new_tail_block_row_stamps_the_pending_scroll_anchor() {
        let mut app = measure_app(true, 0.0);
        let ent = install_geometry(&mut app, strip(2));

        // First observation initializes the tail tracker — no stamp.
        app.update();
        assert_eq!(
            app.world()
                .resource::<ConversationScrollState>()
                .pending_scroll_anchor,
            None,
            "cold start must not anchor",
        );
        let pre_growth_height = app
            .world()
            .resource::<ConversationScrollState>()
            .content_height;

        // The document grows a new tail block (same reconcile path the live
        // sync uses).
        {
            let mut geom = app.world_mut().get_mut::<ConversationGeometry>(ent).unwrap();
            let ids: Vec<BlockId> = (1..=3).map(bid).collect();
            geom.reconcile(&ids, |_| Some(seed()), &params(), 2);
            geom.recompute_offsets();
        }
        app.update();

        let state = app.world().resource::<ConversationScrollState>();
        assert_eq!(
            state.pending_scroll_anchor,
            Some(pre_growth_height),
            "a new tail block must anchor the reveal at the pre-growth height",
        );
        assert!(!state.new_blocks_added, "the stamp is consumed same-frame");
    }
}
