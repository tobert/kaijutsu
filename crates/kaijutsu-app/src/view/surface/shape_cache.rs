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
//! **Slice 3 scope** (slice 1 built the cache; this is what it grew into).
//! Four rules now sit on top of the key comparison, and each one exists to
//! keep a different cost off the frame:
//!
//! - **Sync only where it shows.** Rows within [`SYNC_SLACK_SCREENS`] of the
//!   viewport shape on the main thread, because a placeholder there is a
//!   visible hole. Everything else in the shape band goes to [`ShapeTasks`] on
//!   `AsyncComputeTaskPool`, nearest-row-first, and lands through
//!   [`apply_shape_results`]. Off-thread shaping is safe because the parley
//!   context is per-thread (`text/shaping/context.rs`); the atlas is not, so
//!   tasks return glyph keys and fonts for the main thread to replay.
//! - **A streaming block never re-shapes what it already shaped.** Complete
//!   chunks freeze ([`super::chunk::frozen_chunk_count`]) and their `Arc`s are
//!   reused by pointer; a content bump re-shapes the open tail chunk only, on
//!   the main thread, because one chunk is cheap and a streaming block must
//!   never wait on a task. This is what let the 200-char format debounce die.
//! - **The cache is bounded.** [`SHAPE_CACHE_MAX_GLYPHS`] worth of glyphs, LRU
//!   beyond `DESPAWN_MARGIN_SCREENS` screens of the viewport; in-window rows
//!   and streaming blocks are never evicted, and an evicted row keeps its
//!   measured height in the geometry, so it re-shapes (async) on approach
//!   without moving the document.
//! - **A theme color change is not a shaping event** — for the span-free
//!   majority. [`recolor_block`] rewrites glyph colors through
//!   `Arc::make_mut` and does not touch a single position. Spanned blocks
//!   (markdown) can't take that path and re-shape instead; see
//!   [`recolor_block`] for why the split is forced rather than chosen.
//!
//! Where a block's text sits — its wrap width, its inset inside a border box,
//! and therefore its row height — is decided by [`super::chrome`], because
//! the border padding is a layout property of the border. This module asks
//! for the layout and bakes it into [`ShapedBlock`]; it does not re-derive
//! it.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task, block_on, futures_lite::future};
use kaijutsu_types::{BlockId, Role, Status};
use peniko::Brush;

use crate::cell::{ConversationScrollState, EditorEntities};
use crate::text::components::{bevy_color_to_brush, color_to_rgba8};
use crate::text::msdf::geometry::GeometryVertex;
use crate::text::msdf::glyph::GlyphKey;
use crate::text::msdf::layout_bridge::{
    collect_msdf_glyphs_deferred, collect_msdf_glyphs_styled_deferred,
};
use crate::text::msdf::{FontDataMap, MsdfAtlas, PositionedGlyph};
use crate::text::rich::SpanBrush;
use crate::text::shaping::{VelloFont, VelloFontAxes, VelloTextAlign, VelloTextStyle};
use crate::text::{ShapingFonts, TextMetrics};
use crate::ui::theme::Theme;
use crate::view::geometry::{ConversationGeometry, DESPAWN_MARGIN_SCREENS, RowKey};
use crate::view::role_divider;

use super::chrome::BlockLayout;
use super::chunk::{CHUNK_LINES, chunk_ranges, chunk_shaping_text, frozen_chunk_count, slice_spans};
use super::content::RichKindInfo;

/// How many screens of slack, on each side of the viewport, get shaped.
///
/// Matches the window's slack (`window::WINDOW_SLACK_SCREENS`): shaping a row
/// the window will never assemble is wasted work, and *not* shaping one it
/// will assemble is a hole in the drawing.
pub const SHAPE_SLACK_SCREENS: f32 = 1.0;

/// Screens of slack inside which shaping happens **on the main thread**.
///
/// Half a screen either side of the viewport: what is visible now, plus about
/// one wheel detent's worth of what is about to be. A row in here that is not
/// shaped is a visible hole (chrome and reserved height, no text), so it is
/// worth a frame hitch; a row outside it is invisible, so it is worth a task
/// instead. The rest of [`SHAPE_SLACK_SCREENS`] is the backlog.
pub const SYNC_SLACK_SCREENS: f32 = 0.5;

/// Glyphs the shaped cache may hold before LRU eviction starts.
///
/// ~1.5M glyphs ≈ 48 MB of `PositionedGlyph` (32 bytes each). That is several
/// hundred screens of dense text — a budget that only a very long scroll-back
/// reaches, which is exactly the case the old per-block-texture path could not
/// survive at all.
pub const SHAPE_CACHE_MAX_GLYPHS: usize = 1_500_000;

/// How many backlog shaping tasks may be in flight at once.
///
/// The cap is what makes "visible-first" mean anything: candidates are sorted
/// by distance from the viewport and only the nearest ones are spawned, so a
/// jump into the middle of a long document doesn't queue the whole band in
/// arrival-order and then land the far rows first.
pub const MAX_SHAPE_TASKS_IN_FLIGHT: usize = 24;

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

/// Monotonic token for "theme colors changed, everything painted is stale".
///
/// Deliberately coarser than [`SurfaceMetricsEpoch`]: it moves on **any**
/// `Theme` mutation rather than on a fingerprint of the colors that matter.
/// Two reasons, and both are about not lying. First, "the colors that matter"
/// is a wide, growing set — every block-role color, every markdown token
/// color, the divider and label colors, plus whatever a future block kind
/// paints with — and a fingerprint that forgets one fails silently, as text
/// that keeps the old theme until it happens to re-shape. Second, `Theme` is
/// written from exactly one place (`connection::actor_plugin`, on a theme
/// change from the kernel), so "changed" here is a genuine event, not a
/// per-frame churn: over-triggering costs one recolor pass on a real theme
/// swap and nothing at all otherwise.
///
/// The font-size half of a theme change is still [`SurfaceMetricsEpoch`]'s
/// job — that one *must* re-shape, and its fingerprint is what keeps a
/// scale-factor change from being mistaken for it.
#[derive(Resource, Debug, Default)]
pub struct SurfaceThemeEpoch {
    epoch: u64,
}

impl SurfaceThemeEpoch {
    pub fn get(&self) -> u64 {
        self.epoch
    }
}

/// Bump [`SurfaceThemeEpoch`] whenever the theme resource is written.
pub fn track_surface_theme_epoch(theme: Res<Theme>, mut epoch: ResMut<SurfaceThemeEpoch>) {
    if !theme.is_changed() {
        return;
    }
    let e = epoch.as_mut();
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
    /// [`SurfaceThemeEpoch::get`] — **but only for the drawn rich kinds**
    /// ([`super::content::RichKindInfo::is_drawn`]); zero for everything
    /// else.
    ///
    /// A diff band is `theme.diff_insert_bg` and a sparkline stroke is
    /// `theme.sparkline_line_color`: colors that belong to the theme, not to
    /// the block, and that are baked into vertices no recolor pass can find
    /// (`recolor_block` knows about glyphs, and a band is not a glyph). So
    /// for those kinds a theme swap genuinely invalidates the shaping.
    ///
    /// It is zero for plain text on purpose, and that zero is load-bearing:
    /// putting the theme epoch in every block's key would re-shape the whole
    /// conversation on a theme swap, which is the full-document rebake this
    /// rewrite exists to delete.
    pub rich_theme_epoch: u64,
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
    pub byte_range: Range<usize>,
    pub glyphs: Arc<Vec<PositionedGlyph>>,
    /// Flat-colored triangles drawn **under** this chunk's glyphs: an ABC
    /// staff, a diff's background bands and word washes, a sparkline plot,
    /// an image placeholder rect ([`super::rich`]). Same chunk-local
    /// coordinates as `glyphs`, same `Arc` for the same reason — assembly
    /// hands the pointer to the render world.
    ///
    /// Empty for every text chunk, which is nearly all of them; the shared
    /// [`empty_geometry`] allocation keeps that case from costing one `Arc`
    /// per chunk.
    pub geometry: Arc<Vec<GeometryVertex>>,
    /// This chunk's layout height — the amount the next chunk stacks down by.
    pub height: f32,
    /// The chunk is **complete**: `CHUNK_LINES` hard lines closed by a `\n`,
    /// so appending to the end of the block can never change its bytes
    /// ([`super::chunk::frozen_chunk_count`]). A streaming block's frozen
    /// chunks are carried forward by `Arc` clone on every content bump — the
    /// pointer, not the glyphs.
    ///
    /// Frozen is about *bytes*, not about the `Arc`: [`recolor_block`] will
    /// still `make_mut` a frozen chunk, because repainting doesn't move a
    /// glyph.
    pub frozen: bool,
}

/// The one empty geometry vector every text chunk shares.
///
/// `Arc<Vec<_>>` in the struct rather than `Option<Arc<Vec<_>>>` so the
/// drawing side reads `if !chunk.geometry.is_empty()` symmetrically with the
/// glyph check right above it; this keeps that symmetry from costing an
/// allocation on the path that never has geometry.
pub fn empty_geometry() -> Arc<Vec<GeometryVertex>> {
    static EMPTY: std::sync::OnceLock<Arc<Vec<GeometryVertex>>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(|| Arc::new(Vec::new())).clone()
}

/// A block's cached shaping.
#[derive(Debug, Clone)]
pub struct ShapedBlock {
    pub key: ShapeKey,
    pub chunks: Vec<ShapedChunk>,
    /// The row height the geometry measures: text height **plus** the border
    /// padding above and below it, matching legacy's
    /// `content_height + pad_top + pad_bottom` (`build_block_scenes`).
    pub height: f32,
    /// Sum of the chunk heights — the text alone.
    pub text_height: f32,
    /// Horizontal offset of the block's text origin within the surface:
    /// indent + the border box's margin + its left padding
    /// (`chrome::BlockLayout::text_x`). Baked at shape time so assembly needs
    /// no theme access.
    pub x_offset: f32,
    /// Vertical offset of the text within the block's row — the border's top
    /// padding (`chrome::BlockLayout::text_y`).
    pub y_offset: f32,
    /// Total glyphs held here — the input to the memory budget
    /// ([`SHAPE_CACHE_MAX_GLYPHS`]).
    pub glyph_count: usize,
    /// [`ShapedBlockCache::tick`] at the last pass that wanted this block —
    /// the LRU input. Stamped for every band row the shape pass visits,
    /// whether or not it re-shaped, which is what makes "least recently
    /// wanted" and "least recently drawable" the same ordering.
    pub last_used: u64,
    /// The base color these glyphs were painted with.
    ///
    /// Outside [`ShapeKey`] on purpose: color is not a shaping input, so a
    /// theme swap (or a block going dim on exclusion) must be able to repaint
    /// without re-shaping. [`shape_visible_blocks`] compares this against the
    /// formatted block's current color and takes [`recolor_block`] when it
    /// can.
    pub color: Color,
    /// Byte length of the text these chunks were shaped from — the monotonic
    /// guard on the incremental-tail path.
    pub text_len: usize,
    /// This shaping was produced while the block was `Running`.
    ///
    /// It is what licenses [`incremental_prefix`]: a streaming block's text is
    /// append-only (`docs/crdt-position-2026-08.md`, "streaming is 100%
    /// append"), so a chunk that was complete when it was shaped is still the
    /// same bytes now. A `Done` block's text can be *edited*, where equal byte
    /// ranges say nothing about equal bytes — so an edit to a settled block
    /// always re-shapes whole. The flag survives one frame past the end of the
    /// stream, which is exactly the final Running→Done reconcile the plan
    /// calls for.
    pub streaming: bool,
    /// The block's fieldset captions and its gutter checkbox, shaped and
    /// shared ([`super::labels`]).
    ///
    /// # Why they live here and not in a cache of their own
    ///
    /// Labels are *glyphs*, so they ride the glyph buffer and a change to one
    /// must reach the GPU the way any other glyph change does — through
    /// `ShapedBlockCache::generation`, which `WindowKey` already folds in. Two
    /// alternatives were rejected:
    ///
    /// - **A generation on `BlockChromeCache`.** That cache is rebuilt from
    ///   the *content band* every frame, so its membership churns with the
    ///   scroll: putting its generation in `WindowKey` would re-upload the
    ///   whole window's glyphs on every frame of a drag, which is precisely
    ///   the cost this rewrite exists to delete. This cache does not churn —
    ///   it is written on a real re-shape and evicted on an LRU budget.
    /// - **Folding the label text into [`ShapeKey`].** A tool call finishing
    ///   would then re-shape its entire body to drop the word "running".
    ///
    /// So the shape pass compares the label runs itself
    /// ([`super::labels::BlockLabels::same_runs`]) and calls
    /// [`ShapedBlockCache::touch`] when they moved — the same in-place
    /// mutation path a recolor takes.
    ///
    /// **Focus is deliberately not an input.** These are shaped from the
    /// *unfocused* border style (`chrome::BlockChrome::style`), so moving the
    /// j/k focus ring recolors the stroke and leaves the captions alone. The
    /// alternative — re-shaping every label in the focus color — would put
    /// focus back on the glyph path and re-upload a screenful of glyphs per
    /// keystroke. Legacy behaves the same way for a different reason: its
    /// labels are baked into the block texture, which only rebuilds when the
    /// block's *content* version moves, so a focus change never repaints them
    /// there either.
    pub labels: super::labels::BlockLabels,
}

impl ShapedBlock {
    /// Leading chunks whose bytes are settled — see [`ShapedChunk::frozen`].
    fn frozen_chunks(&self) -> usize {
        self.chunks.iter().take_while(|c| c.frozen).count()
    }
}

/// Shaped glyph runs for every block the surface might draw.
#[derive(Resource, Debug, Default)]
pub struct ShapedBlockCache {
    blocks: HashMap<BlockId, ShapedBlock>,
    /// Incremented once per [`shape_visible_blocks`] pass; stamped onto every
    /// block the pass wanted.
    tick: u64,
    /// Running sum of `blocks[_].glyph_count`.
    ///
    /// Maintained by [`Self::insert`] / [`Self::remove`] rather than summed on
    /// demand, because the eviction check reads it **every frame** and the
    /// whole point of the budget is that a cache big enough to need it is also
    /// big enough that walking it per frame is a cost of its own.
    total_glyphs: usize,
    /// Bumped on every mutation a drawn frame could differ by: insert,
    /// removal, an in-place recolor, a placement refresh. This is how a cache
    /// change REACHES THE GPU — `WindowKey` folds it in, so the window
    /// re-assembles and re-uploads even when nothing else moved. Without it, a
    /// recolor (a tool call finishing amber→fg), an async landing whose
    /// height matched its estimate, or a padding-only placement shift would
    /// mutate the cache and never repaint (found by review, 2026-08-18).
    generation: u64,
}

impl ShapedBlockCache {
    pub fn get(&self, id: &BlockId) -> Option<&ShapedBlock> {
        self.blocks.get(id)
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    #[allow(dead_code)] // Symmetry with `len`; exercised by tests.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Total glyphs held across every cached block — what eviction budgets
    /// against, and what diagnostics report.
    pub fn total_glyphs(&self) -> usize {
        self.total_glyphs
    }

    #[allow(dead_code)] // Model accessor for tests and diagnostics.
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// See the field doc on [`Self::generation`] — a `WindowKey` input.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Record an in-place mutation of an already-cached block (recolor,
    /// placement refresh) so the window re-uploads it.
    fn touch(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// The one write path. Every insert goes through here so `total_glyphs`
    /// can never drift from the map — a drift would either stall eviction
    /// forever or evict a cache that fits.
    fn insert(&mut self, id: BlockId, block: ShapedBlock) {
        let added = block.glyph_count;
        if let Some(old) = self.blocks.insert(id, block) {
            self.total_glyphs -= old.glyph_count;
        }
        self.total_glyphs += added;
        self.generation = self.generation.wrapping_add(1);
    }

    fn remove(&mut self, id: &BlockId) -> Option<ShapedBlock> {
        let old = self.blocks.remove(id)?;
        self.total_glyphs -= old.glyph_count;
        self.generation = self.generation.wrapping_add(1);
        Some(old)
    }

    /// Drop every block the predicate rejects, keeping the glyph total right.
    fn retain(&mut self, mut keep: impl FnMut(&BlockId) -> bool) {
        let mut freed = 0usize;
        self.blocks.retain(|id, block| {
            if keep(id) {
                true
            } else {
                freed += block.glyph_count;
                false
            }
        });
        if freed > 0 {
            self.generation = self.generation.wrapping_add(1);
        }
        self.total_glyphs -= freed;
    }

    /// Seed a shaped block directly. Test-only: the production writer is
    /// [`shape_visible_blocks`], and `window`'s assembly tests need cache
    /// contents without a font context or an atlas.
    #[cfg(test)]
    pub(crate) fn insert_for_test(&mut self, id: BlockId, block: ShapedBlock) {
        self.insert(id, block);
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
    /// Measured label height. Unread on the drawing path — the label is
    /// positioned by `y_offset`, which already centred it in the header row —
    /// but it is what that centring was computed from, and dropping it would
    /// leave the arithmetic in `shape_role_label` unexplained.
    #[allow(dead_code)]
    pub height: f32,
}

/// Shaped USER/ASSISTANT/… labels, keyed by `(role, metrics_epoch)`.
///
/// Tiny and shared: one entry per role, not per header row. Theme *colors* are
/// baked into the glyphs here, and rather than widen the key with a theme
/// epoch — which would ripple a parameter through `assemble_runs` and
/// `collect_chrome_instances` for five entries — the whole map is dropped when
/// the theme moves ([`Self::sync_theme`]). Five re-layouts on a theme swap is
/// not a cost worth designing around.
///
/// The map is keyed on [`role_index`] rather than `Role` itself: `Role` is a
/// wire type in `kaijutsu-types` and does not derive `Hash`, and teaching a
/// protocol type a trait for one app-side cache is the wrong direction of
/// dependency.
#[derive(Resource, Debug, Default)]
pub struct HeaderLabelCache {
    labels: HashMap<(u8, u64), ShapedLabel>,
    theme_epoch: u64,
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

    #[allow(dead_code)] // Model accessor, exercised by tests.
    pub fn len(&self) -> usize {
        self.labels.len()
    }

    /// Drop every cached label when the theme moves, so the next pass
    /// re-shapes them in the new colors. Returns whether anything was
    /// dropped.
    pub fn sync_theme(&mut self, theme_epoch: u64) -> bool {
        if self.theme_epoch == theme_epoch {
            return false;
        }
        self.theme_epoch = theme_epoch;
        let had = !self.labels.is_empty();
        self.labels.clear();
        had
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

/// The column width available to a block, indent removed.
///
/// Derived the same way `sync_conversation_geometry` derives its estimate
/// columns (`geometry.rs:591-598`): the conversation container's
/// **content-box width in logical px**, so every unit in this model — row
/// heights, scroll offsets, wrap widths — comes from one source. Indent
/// (`update_block_cell_nodes`'s left margin) comes off the top.
///
/// This is the *column*, not the wrap width: a bordered block also loses its
/// glow margin and its padding, which `super::chrome::surface_block_layout`
/// takes off on top of this.
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

/// What one shaping pass produced, main-thread effects still owed.
///
/// The atlas and [`FontDataMap`] are main-world resources a task cannot
/// touch, so shaping hands back what it *would* have written into them:
/// `glyph_keys` to replay through [`MsdfAtlas::request`], and `fonts` to
/// replay through [`FontDataMap::register`]. Skipping either yields a cache
/// full of glyphs the atlas can never fill.
#[derive(Debug, Default)]
pub struct ShapeOutput {
    pub chunks: Vec<ShapedChunk>,
    /// Sum of the chunk heights.
    pub text_height: f32,
    pub glyph_keys: Vec<GlyphKey>,
    /// Fonts every glyph run resolved to. Cheap to carry — `FontData` is a
    /// refcounted blob, so this clones pointers, not font files.
    pub fonts: Vec<parley::FontData>,
}

impl ShapeOutput {
    /// Replay the deferred main-thread effects. The one place that knows the
    /// deferred-collector contract has two halves.
    fn apply_deferred(&self, atlas: &mut MsdfAtlas, font_data: &mut FontDataMap) {
        for font in &self.fonts {
            font_data.register(font);
        }
        for key in &self.glyph_keys {
            atlas.request(*key);
        }
    }
}

/// Shape one chunk of a block's text into chunk-local glyphs.
///
/// Pure: no atlas, no font map, no ECS — so it runs identically on the main
/// thread and on `AsyncComputeTaskPool` (the parley context is per-thread,
/// `text/shaping/context.rs`).
pub fn shape_chunk(
    font: &VelloFont,
    text: &str,
    spans: &[SpanBrush],
    style: &VelloTextStyle,
    max_advance: Option<f32>,
    range: Range<usize>,
    frozen: bool,
    out: &mut ShapeOutput,
) {
    let chunk_text = chunk_shaping_text(text, &range);
    let chunk_spans = slice_spans(spans, range.clone());

    // Ranged styles (`layout_spanned` + the styled collector) are the only
    // path that colors a span starting mid-run; the plain path is kept for
    // the span-free majority because it skips parley's run splitting
    // entirely.
    let (glyphs, keys, layout_height) = if chunk_spans.is_empty() {
        let layout = font.layout(chunk_text, style, VelloTextAlign::Left, max_advance);
        collect_fonts(&layout, &mut out.fonts);
        let (glyphs, keys) =
            collect_msdf_glyphs_deferred(&layout, &[], &style.brush, (0.0, 0.0));
        (glyphs, keys, layout.height())
    } else {
        let layout = font.layout_spanned(
            chunk_text,
            style,
            VelloTextAlign::Left,
            max_advance,
            &chunk_spans,
        );
        collect_fonts(&layout, &mut out.fonts);
        let (glyphs, keys) = collect_msdf_glyphs_styled_deferred(&layout, (0.0, 0.0));
        (glyphs, keys, layout.height())
    };

    out.glyph_keys.extend(keys);
    out.text_height += layout_height;
    out.chunks.push(ShapedChunk {
        byte_range: range,
        glyphs: Arc::new(glyphs),
        geometry: empty_geometry(),
        height: layout_height,
        frozen,
    });
}

/// Shape one block's text into chunked, chunk-local glyph runs.
///
/// Pure for the same reason [`shape_chunk`] is — this is what the backlog
/// tasks call.
pub fn shape_block(
    font: &VelloFont,
    text: &str,
    spans: &[SpanBrush],
    style: &VelloTextStyle,
    wrap_width: f32,
    chunk_lines: usize,
) -> ShapeOutput {
    let ranges = chunk_ranges(text, chunk_lines);
    let frozen = frozen_chunk_count(text, &ranges, chunk_lines);
    let max_advance = (wrap_width > 0.0).then_some(wrap_width);

    let mut out = ShapeOutput::default();
    for (i, range) in ranges.into_iter().enumerate() {
        shape_chunk(
            font,
            text,
            spans,
            style,
            max_advance,
            range,
            i < frozen,
            &mut out,
        );
    }
    out
}

/// Collect every font a layout used, so MSDF generation has its bytes.
///
/// Deduplicated on [`FontId`] (a pointer, so the comparison is free) because
/// a chunk is dozens of glyph runs over the same one or two fonts, and the
/// list is carried across a thread boundary.
pub(crate) fn collect_fonts(layout: &parley::Layout<Brush>, fonts: &mut Vec<parley::FontData>) {
    for line in layout.lines() {
        for item in line.items() {
            if let parley::PositionedLayoutItem::GlyphRun(run) = item {
                let font = run.run().font();
                let id = crate::text::msdf::glyph::FontId::from_parley(font);
                if !fonts
                    .iter()
                    .any(|f| crate::text::msdf::glyph::FontId::from_parley(f) == id)
                {
                    fonts.push(font.clone());
                }
            }
        }
    }
}

// ============================================================================
// INCREMENTAL TAIL
// ============================================================================

/// How many of `existing`'s chunks a new shaping of `text` can inherit
/// verbatim, or `None` if the block has to be re-shaped whole.
///
/// This is the streaming fast path's whole decision, and it is deliberately
/// conservative — every clause below rules out a way the cached glyphs could
/// describe different bytes than the ones on screen:
///
/// - **`existing.streaming`.** The cached shaping was made while the block was
///   `Running`, so the only edit that can have happened since is an append
///   (kaijutsu streams by `push_str`; there is no text CRDT and no in-place
///   mutation of a running block). A settled block's text can be *edited*,
///   and then equal byte ranges say nothing about equal bytes.
/// - **Nothing but the content version moved.** Wrap width, indent, collapse
///   and the metrics epoch all change the line grid, and a frozen chunk's
///   glyphs are only valid for the grid they were laid out in.
/// - **The text did not shrink**, and the frozen ranges still tile the same
///   bytes. A truncation or a regenerate mid-stream fails this and takes the
///   whole path.
/// - **The color did not move.** Frozen chunks carry their colors baked into
///   the glyphs, so inheriting them under a new palette would leave the head
///   of a streaming block in the old theme while its tail arrived in the new
///   one. A span-free block never reaches here for a color change (it is
///   repainted in place instead).
/// - **The block is not spanned.** Byte-identical frozen ranges do NOT prove
///   a spanned block's *rendering* is unchanged: markdown re-interprets the
///   whole text on every append, and an append can recolor bytes that are
///   already frozen — stream `"Title\n"`, then append `"==="`, and the
///   frozen line retroactively becomes a heading; an `md_*`-token-only theme
///   change moves span brushes while the base color (the clause above) holds
///   still. Bytes + base color are a proof only for span-free text, so a
///   spanned block always re-shapes whole (found by review, 2026-08-18).
///
/// Returns the number of leading chunks to keep — the caller re-shapes
/// `ranges[n..]` and clones the first `n` `Arc`s by pointer.
pub fn incremental_prefix(
    existing: &ShapedBlock,
    key: &ShapeKey,
    color: Color,
    text: &str,
    ranges: &[Range<usize>],
    spanned: bool,
) -> Option<usize> {
    if spanned || !existing.streaming || text.len() < existing.text_len || existing.color != color
    {
        return None;
    }
    let only_content_moved = ShapeKey {
        content_version: existing.key.content_version,
        ..*key
    } == existing.key;
    if !only_content_moved {
        return None;
    }

    let n = existing.frozen_chunks();
    if n == 0 || n > ranges.len() {
        return None;
    }
    for (chunk, range) in existing.chunks[..n].iter().zip(&ranges[..n]) {
        if chunk.byte_range != *range {
            return None;
        }
    }
    Some(n)
}

// ============================================================================
// RECOLOR
// ============================================================================

/// Repaint a shaped block's glyphs without moving one of them.
///
/// The theme fast path: color is not a shaping property (the same fact
/// `layout_bridge`'s `spanned_and_plain_layouts_position_glyphs_identically`
/// pins), so a theme swap is a memory rewrite rather than a parley run.
/// `Arc::make_mut` clones a chunk's glyphs only if the render world still
/// holds the previous pointer — the frame after, it writes in place.
///
/// **Span-free blocks only, and that split is forced, not chosen.**
/// [`PositionedGlyph`] carries no byte offset, so there is no way to map a
/// re-derived span's byte range back onto the glyphs it colored: parley
/// resolved that mapping during shaping and did not keep it. Carrying a byte
/// index per glyph to enable it would cost every block memory to make one
/// rare event cheaper for the minority of blocks that are spanned (markdown,
/// diff). So a spanned block whose colors moved re-shapes instead — through
/// the ordinary backlog, visible-first, which is what a content bump on it
/// would do anyway. Callers must check `spans.is_empty()`; this function does
/// not know about spans and would happily flatten them.
pub fn recolor_block(block: &mut ShapedBlock, color: Color) {
    let rgba = color_to_rgba8(color);
    for chunk in &mut block.chunks {
        // A chunk already in the target color keeps its `Arc` — worth the
        // scan, because it is what keeps a repaint of an unchanged block from
        // deep-cloning every glyph the render world is holding.
        if chunk.glyphs.iter().all(|g| g.color == rgba) {
            continue;
        }
        for glyph in Arc::make_mut(&mut chunk.glyphs) {
            glyph.color = rgba;
        }
    }
    block.color = color;
}

// ============================================================================
// EVICTION
// ============================================================================

/// Choose least-recently-used blocks to drop until the cache fits `budget`.
///
/// `candidates` are the *evictable* entries — `(id, glyph_count, last_used)`
/// for blocks that are neither in the pinned window nor streaming — and
/// `total` is the whole cache's glyph count, pinned blocks included. Pinned
/// glyphs therefore count against the budget without ever being freed, which
/// is the honest arithmetic: if the pinned window alone exceeds the budget,
/// this evicts everything it may and stops, rather than pretending it
/// succeeded or evicting something that is on screen.
///
/// Ties on `last_used` fall to `sort_by_key`'s stable order; there is nothing
/// to prefer between two blocks last wanted on the same pass.
/// Which cached blocks eviction is even allowed to consider.
///
/// Two pins, and they are different kinds of promise. A block in `pinned` is
/// close enough to the viewport that dropping it would cost a re-shape the
/// user might see. A **streaming** block is pinned wherever it sits, because
/// its frozen chunks are the thing the incremental tail is built on — evicting
/// one turns the next tick's one-chunk update into a whole-block re-shape, and
/// then the tick after that, for as long as the stream runs.
pub fn eviction_candidates(
    shaped: &ShapedBlockCache,
    pinned: &HashSet<BlockId>,
) -> Vec<(BlockId, usize, u64)> {
    shaped
        .blocks
        .iter()
        .filter(|(id, block)| !pinned.contains(id) && !block.streaming)
        .map(|(id, block)| (*id, block.glyph_count, block.last_used))
        .collect()
}

pub fn plan_evictions(
    candidates: &mut [(BlockId, usize, u64)],
    total: usize,
    budget: usize,
) -> Vec<BlockId> {
    if total <= budget {
        return Vec::new();
    }
    candidates.sort_by_key(|(_, _, last_used)| *last_used);

    let mut freed = 0usize;
    let mut evicted = Vec::new();
    for (id, glyphs, _) in candidates.iter() {
        if total - freed <= budget {
            break;
        }
        freed += glyphs;
        evicted.push(*id);
    }
    evicted
}

// ============================================================================
// BACKLOG SHAPING (OFF-THREAD)
// ============================================================================

/// Everything one backlog shaping task needs, snapshotted on the main thread.
///
/// Owned, not borrowed, all the way down: the task outlives the system that
/// spawned it, and the caches it read from are `Res` for the length of one
/// system run. `VelloFont` is a family name, so cloning it is a `String`.
#[derive(Clone)]
pub struct ShapeJob {
    pub block: BlockId,
    pub key: ShapeKey,
    pub font: VelloFont,
    pub text: String,
    pub spans: Vec<SpanBrush>,
    pub style: VelloTextStyle,
    pub wrap_width: f32,
    pub chunk_lines: usize,
    /// The color the glyphs will carry, kept so the landed block can answer
    /// "do I need a recolor?" without re-deriving it from the brush.
    pub color: Color,
    /// Where this block sits in the column at spawn time.
    ///
    /// Carried through the task so [`apply_shape_results`] can land a block
    /// that is no longer in the shape band — its height has to reach the
    /// geometry either way. A padding-only move (a ToolResult joining its
    /// call) that happens while the task runs is not in [`ShapeKey`] and is
    /// therefore not detected here; the shape pass refreshes placement for
    /// every band row on the next frame, which is where that correction has
    /// always come from.
    pub layout: BlockLayout,
}

/// One finished backlog shaping, waiting for the main thread.
pub struct ShapedResult {
    pub block: BlockId,
    pub key: ShapeKey,
    pub output: ShapeOutput,
    pub color: Color,
    pub text_len: usize,
    pub layout: BlockLayout,
}

/// Run one shaping job. Pure — the body of the spawned task, called directly
/// by tests (which have no `AsyncComputeTaskPool`).
pub fn run_shape_job(job: ShapeJob) -> ShapedResult {
    let output = shape_block(
        &job.font,
        &job.text,
        &job.spans,
        &job.style,
        job.wrap_width,
        job.chunk_lines,
    );
    ShapedResult {
        block: job.block,
        key: job.key,
        output,
        color: job.color,
        text_len: job.text.len(),
        layout: job.layout,
    }
}

/// In-flight backlog shaping.
///
/// The `in_flight` map is both the duplicate-spawn guard and the staleness
/// oracle, and it can be both because it holds exactly one key per block: the
/// key of the **most recently spawned** task. A result whose key doesn't match
/// the entry is superseded (a newer task, a sync re-shape, or an eviction has
/// happened since) and is dropped — last-writer-wins, decided by what the main
/// thread most recently asked for rather than by arrival order.
#[derive(Resource, Default)]
pub struct ShapeTasks {
    tasks: Vec<Task<ShapedResult>>,
    in_flight: HashMap<BlockId, ShapeKey>,
}

impl std::fmt::Debug for ShapeTasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShapeTasks")
            .field("tasks", &self.tasks.len())
            .field("in_flight", &self.in_flight.len())
            .finish()
    }
}

impl ShapeTasks {
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Is a task for exactly this `(block, key)` already running?
    pub fn is_pending(&self, block: &BlockId, key: &ShapeKey) -> bool {
        self.in_flight.get(block) == Some(key)
    }

    /// Record a spawn. Overwrites any older key for the block, which is what
    /// makes the older task's result stale on arrival.
    fn note_spawned(&mut self, block: BlockId, key: ShapeKey) {
        self.in_flight.insert(block, key);
    }

    /// Should a landed result be applied? Clears the entry when it should.
    ///
    /// Separate from the task draining so the decision is testable without a
    /// task pool — the same shape `generator.rs` uses for its retry budget.
    pub fn accept(&mut self, block: &BlockId, key: &ShapeKey) -> bool {
        if self.in_flight.get(block) != Some(key) {
            return false;
        }
        self.in_flight.remove(block);
        true
    }

    /// Forget any in-flight task for `block` — its result will be dropped on
    /// arrival. Called when the main thread shapes the block itself, or drops
    /// it from the cache.
    pub fn cancel(&mut self, block: &BlockId) {
        self.in_flight.remove(block);
    }

    /// Spawn a job unless one for the same `(block, key)` is already running.
    /// Returns whether a task was actually spawned.
    pub fn spawn(&mut self, job: ShapeJob) -> bool {
        if self.is_pending(&job.block, &job.key) {
            return false;
        }
        let (block, key) = (job.block, job.key);
        let task = AsyncComputeTaskPool::get().spawn(async move { run_shape_job(job) });
        self.note_spawned(block, key);
        self.tasks.push(task);
        true
    }

    /// Take every finished task's result, leaving the rest running.
    fn drain_finished(&mut self) -> Vec<ShapedResult> {
        let mut done = Vec::new();
        self.tasks.retain_mut(|task| {
            if task.is_finished() {
                done.push(block_on(future::poll_once(task)).expect("finished task must yield"));
                false
            } else {
                true
            }
        });
        done
    }
}

/// Land finished backlog shapings into the cache.
///
/// Runs in [`super::SurfaceSet::Shape`] **before** [`shape_visible_blocks`],
/// so a block whose backlog result arrives this frame is already in the cache
/// when the shape pass looks at it — otherwise the pass would see a key
/// mismatch and shape it a second time, synchronously, which is precisely the
/// hitch the backlog exists to avoid.
///
/// Heights reach `ConversationGeometry` the ordinary way: the landed block's
/// `height` differs from the row's, [`apply_shaped_measurements`] (next set)
/// notices, and the anchor compensation it already performs covers async
/// pop-in above the viewport exactly as it covers any other late measurement.
pub fn apply_shape_results(
    mut tasks: ResMut<ShapeTasks>,
    atlas: Option<ResMut<MsdfAtlas>>,
    font_data: Option<ResMut<FontDataMap>>,
    mut shaped: ResMut<ShapedBlockCache>,
) {
    // Without an atlas there is nowhere to request the glyphs, and inserting
    // the block anyway would stamp a matching `ShapeKey` on a run that can
    // never be rasterized — the same trap `shape_visible_blocks` guards. The
    // check comes **before** the drain on purpose: draining here would throw
    // the results away while leaving their in-flight claims standing, and the
    // block would then never be spawned for again. Leaving the tasks parked
    // costs nothing — they are already finished — and they land intact on the
    // first frame the render world is up.
    let (Some(mut atlas), Some(mut font_data)) = (atlas, font_data) else {
        return;
    };

    let finished = tasks.drain_finished();
    if finished.is_empty() {
        return;
    }

    for result in finished {
        if !tasks.accept(&result.block, &result.key) {
            // Superseded while it ran. Dropping it is the whole point of
            // last-writer-wins: the cache already holds — or is about to hold
            // — a shaping of newer text.
            continue;
        }
        result.output.apply_deferred(&mut atlas, &mut font_data);

        let glyph_count = result.output.chunks.iter().map(|c| c.glyphs.len()).sum();
        let tick = shaped.tick;
        shaped.insert(
            result.block,
            ShapedBlock {
                key: result.key,
                height: result.layout.outer_height(result.output.text_height),
                text_height: result.output.text_height,
                chunks: result.output.chunks,
                x_offset: result.layout.text_x,
                y_offset: result.layout.text_y,
                glyph_count,
                last_used: tick,
                color: result.color,
                text_len: result.text_len,
                // Backlog shaping is never used for a streaming block, so
                // this shaping can never license the incremental tail path.
                streaming: false,
                // Captions are not shaped on the task thread (they need the
                // atlas and the theme, neither of which crosses). The pass
                // that runs immediately after this one fills them in on the
                // same frame — this system is chained before
                // `shape_visible_blocks` precisely so a landed block is
                // complete before anything looks at it.
                labels: super::labels::BlockLabels::default(),
            },
        );
    }
}

/// The caches [`shape_visible_blocks`] reads and writes, bundled so the
/// system stays under Bevy's parameter-tuple limit — and so the shape pass's
/// working set is one thing with a name rather than five loose arguments.
#[derive(bevy::ecs::system::SystemParam)]
pub struct ShapeCaches<'w> {
    pub content: Res<'w, super::content::BlockContentCache>,
    pub chrome: Res<'w, super::chrome::BlockChromeCache>,
    pub shaped: ResMut<'w, ShapedBlockCache>,
    pub headers: ResMut<'w, HeaderLabelCache>,
    pub labels: ResMut<'w, super::labels::LabelRunCache>,
    pub tasks: ResMut<'w, ShapeTasks>,
}

/// The two epochs that invalidate shaped runs, bundled for the same reason.
#[derive(bevy::ecs::system::SystemParam)]
pub struct ShapeEpochs<'w> {
    pub metrics: Res<'w, SurfaceMetricsEpoch>,
    pub theme: Res<'w, SurfaceThemeEpoch>,
}

/// Shape the blocks (and role labels) inside the window band.
///
/// The pass is one walk of the shape band, and every row it visits takes
/// exactly one of five outcomes:
///
/// 1. **Key matches, color matches** — stamp `last_used`, refresh placement,
///    done. The common case, and it costs one comparison.
/// 2. **Key matches, color moved** — [`recolor_block`] in place for a
///    span-free block; a spanned one falls through to a re-shape.
/// 3. **Streaming tail** — [`incremental_prefix`] says the frozen chunks
///    still describe the same bytes, so only the open tail chunk re-shapes.
///    Always synchronous: a `Running` block must never wait on a task, and
///    one chunk is bounded work no matter how tall the block grew.
/// 4. **Inside [`SYNC_SLACK_SCREENS`]** — full re-shape on the main thread,
///    because an unshaped row this close to the viewport is a visible hole.
/// 5. **Anything else in the band** — a [`ShapeJob`] queued for the backlog,
///    nearest row first. The row keeps drawing its chrome and its reserved
///    height until the result lands.
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
    epochs: ShapeEpochs,
    mut caches: ShapeCaches,
    atlas: Option<ResMut<MsdfAtlas>>,
    font_data: Option<ResMut<FontDataMap>>,
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
    let offset = scroll_state.offset;
    let band = geom.visible_rows(offset, vh, SHAPE_SLACK_SCREENS * vh);
    // The rows close enough that not drawing them would show: shaping these
    // is worth a frame hitch, shaping the rest is not.
    let sync_band = geom.visible_rows(offset, vh, SYNC_SLACK_SCREENS * vh);
    let metrics_epoch = epochs.metrics.get();
    let theme_epoch = epochs.theme.get();

    // Role labels bake theme colors into their glyphs, and there are five of
    // them — dropping the lot on a theme swap is cheaper than teaching every
    // consumer a wider cache key.
    caches.headers.sync_theme(theme_epoch);
    // Block captions and the gutter checkbox bake colors the same way, and
    // their advance widths move with `label_font_size` — so both epochs clear
    // them. Dropped runs are re-shaped by the walk below, in the same pass.
    caches.labels.sync_epochs(metrics_epoch, theme_epoch);

    caches.shaped.tick = caches.shaped.tick.wrapping_add(1);
    let tick = caches.shaped.tick;

    // Backlog candidates, paired with their distance from the viewport top so
    // the nearest ones win the in-flight budget.
    let mut backlog: Vec<(f32, ShapeJob)> = Vec::new();

    for (i, row) in geom.rows()[band.clone()].iter().enumerate() {
        let row_index = band.start + i;
        match row.key {
            RowKey::Header(_) => {
                shape_role_label(
                    font,
                    row.role,
                    metrics_epoch,
                    &theme,
                    &mut caches.headers,
                    &mut atlas,
                    &mut font_data,
                );
            }
            RowKey::Block(id) => {
                let Some(formatted) = caches.content.get(&id) else {
                    // Content sync hasn't reached this row yet (it runs with a
                    // wider band, so this is a first-frame ordering case, not
                    // a hole). It will be here next frame.
                    continue;
                };
                // Chrome decides the wrap width: a bordered block's text is
                // laid out inside its padding, exactly as `build_block_scenes`
                // lays it out inside the same padding on the legacy path.
                let layout =
                    caches
                        .chrome
                        .layout(&id, container_width, row.indent_level, theme.indent_width);
                let wrap_width = layout.wrap_width;
                // Captions and the gutter checkbox, shaped from the block's
                // *unfocused* border style and shared by `(text, color)`. This
                // happens before the key comparison below because it is
                // independent of it: a tool call that finishes changes its
                // captions without changing a byte of its body, and a body
                // that re-wraps changes no caption at all. See
                // `ShapedBlock::labels` for why they live on the shaped block.
                let label_spec = super::labels::block_label_spec(
                    caches.chrome.get(&id).and_then(|c| c.style.as_ref()),
                    formatted.border.excluded,
                );
                let labels = caches.labels.shape_block(
                    font,
                    &label_spec,
                    theme.label_font_size,
                    &mut atlas,
                    &mut font_data,
                );
                // A drawn rich kind bakes theme colors into vertices, so the
                // theme epoch is part of its key and of nobody else's — see
                // `ShapeKey::rich_theme_epoch`.
                let rich_drawn = formatted.rich.is_some_and(RichKindInfo::is_drawn);
                let key = ShapeKey {
                    content_version: formatted.version,
                    wrap_width_bits: wrap_width.to_bits(),
                    collapsed: row.collapsed,
                    indent_level: row.indent_level,
                    metrics_epoch,
                    rich_theme_epoch: if rich_drawn { theme_epoch } else { 0 },
                };
                let streaming = formatted.status == Status::Running;
                let color = formatted.color;
                // The two kinds of block that can't be repainted in place and
                // can't inherit a frozen prefix: one because parley resolved
                // its span→glyph mapping and threw it away, the other because
                // its drawing is geometry a glyph recolor cannot reach.
                let complex = !formatted.spans.is_empty() || rich_drawn;

                let mut key_match_settled = false;
                if let Some(existing) = caches.shaped.blocks.get_mut(&id)
                    && existing.key == key
                {
                    existing.last_used = tick;
                    // Padding can move without the wrap width moving — a
                    // ToolResult that joins the call above it loses half its
                    // top padding, and nothing horizontal changes. Refresh the
                    // placement every pass rather than adding a non-shaping
                    // input to `ShapeKey` and re-shaping for it — but compare
                    // first: an actual move (like a recolor below) must bump
                    // the cache generation or the GPU never re-uploads it.
                    let height = layout.outer_height(existing.text_height);
                    let mut mutated = existing.x_offset != layout.text_x
                        || existing.y_offset != layout.text_y
                        || existing.height != height;
                    existing.x_offset = layout.text_x;
                    existing.y_offset = layout.text_y;
                    existing.height = height;
                    // A caption that appeared, vanished, or changed shade is
                    // a glyph change like any other: it has to bump the
                    // generation or the window never re-assembles and the
                    // GPU keeps drawing "running" on a finished call.
                    if !existing.labels.same_runs(&labels) {
                        existing.labels = labels.clone();
                        mutated = true;
                    }

                    if existing.color == color || !complex {
                        if existing.color != color {
                            recolor_block(existing, color);
                            mutated = true;
                        }
                        // The incremental-tail licence expires the moment the
                        // block settles: from here on its text can be *edited*,
                        // and equal byte ranges would no longer imply equal
                        // bytes. Cleared only on this arm — the re-shape path
                        // below still needs the old value to make the final
                        // Running→Done reconcile incremental.
                        existing.streaming &= streaming;
                        key_match_settled = true;
                    }
                    // Otherwise: a spanned or drawn block whose colors moved
                    // cannot be repainted in place (see `recolor_block`, and
                    // `complex` above); it re-shapes like any other content
                    // change.
                    if mutated {
                        caches.shaped.touch();
                    }
                }
                if key_match_settled {
                    continue;
                }

                // Can the streaming fast path carry most of this block
                // forward? Only asked of a block that was shaped mid-stream,
                // so `chunk_ranges` never runs for a settled one.
                let inherit = match caches.shaped.get(&id) {
                    Some(existing) if existing.streaming => {
                        let ranges = chunk_ranges(&formatted.text, CHUNK_LINES);
                        incremental_prefix(existing, &key, color, &formatted.text, &ranges, complex)
                            .map(|n| (n, ranges))
                    }
                    _ => None,
                };

                let style = block_text_style(&text_metrics, bevy_color_to_brush(color));

                // Everything close enough to see, everything streaming, and
                // everything the tail path can finish cheaply stays on this
                // thread. So does every drawn rich kind, which needs the
                // atlas and the theme — neither of which crosses to a task.
                // The rest becomes a job.
                if !sync_band.contains(&row_index) && !streaming && !rich_drawn && inherit.is_none()
                {
                    if !caches.tasks.is_pending(&id, &key) {
                        backlog.push((
                            (row.y_offset - offset).abs(),
                            ShapeJob {
                                block: id,
                                key,
                                font: font.clone(),
                                text: formatted.text.clone(),
                                spans: formatted.spans.clone(),
                                style,
                                wrap_width,
                                chunk_lines: CHUNK_LINES,
                                color,
                                layout,
                            },
                        ));
                    }
                    continue;
                }

                // Shaping it here supersedes anything in flight for it.
                caches.tasks.cancel(&id);

                let output = if rich_drawn {
                    // Detection kept the payload; without it there is nothing
                    // to draw and shaping the empty string would stamp a
                    // matching key on an empty block. Skipping leaves the key
                    // mismatched, so the next frame tries again.
                    let Some(kind) = formatted.rich_content.as_ref().map(|p| p.kind()) else {
                        continue;
                    };
                    super::rich::RichShaper {
                        font,
                        theme: &theme,
                        atlas: &mut atlas,
                        font_data: &mut font_data,
                    }
                    .shape(kind, &formatted.text, &formatted.spans, &style, wrap_width)
                } else {
                    match inherit {
                    Some((n, ranges)) => {
                        // Frozen chunks come across by `Arc` clone — the
                        // pointer, not the glyphs. This is the whole win of
                        // chunking: a 5000-line streamed block costs one chunk
                        // of parley per tick, forever.
                        let inherited: Vec<ShapedChunk> =
                            caches.shaped.blocks[&id].chunks[..n].to_vec();
                        shape_tail(
                            font,
                            inherited,
                            &formatted.text,
                            &formatted.spans,
                            &style,
                            wrap_width,
                            &ranges,
                        )
                    }
                    None => shape_block(
                        font,
                        &formatted.text,
                        &formatted.spans,
                        &style,
                        wrap_width,
                        CHUNK_LINES,
                    ),
                    }
                };
                output.apply_deferred(&mut atlas, &mut font_data);

                let glyph_count = output.chunks.iter().map(|c| c.glyphs.len()).sum();
                caches.shaped.insert(
                    id,
                    ShapedBlock {
                        key,
                        height: layout.outer_height(output.text_height),
                        text_height: output.text_height,
                        chunks: output.chunks,
                        x_offset: layout.text_x,
                        y_offset: layout.text_y,
                        glyph_count,
                        last_used: tick,
                        color,
                        text_len: formatted.text.len(),
                        streaming,
                        labels,
                    },
                );
            }
        }
    }

    spawn_backlog(&mut caches.tasks, backlog);

    // Blocks the document dropped keep neither text nor glyphs.
    if caches.shaped.len() > caches.content.len() {
        let content = &caches.content;
        let tasks = &mut caches.tasks;
        caches.shaped.retain(|id| {
            if content.get(id).is_some() {
                true
            } else {
                tasks.cancel(id);
                false
            }
        });
    }

    evict_shaped_blocks(&mut caches, geom, offset, vh);
}

/// Spawn the nearest backlog jobs, up to the in-flight cap.
///
/// Sorting by distance is what "visible-first" means with a bounded pool: a
/// jump into the middle of a long document queues the whole band, and without
/// an ordering the rows that land first would be whichever the map iterated
/// first — reliably the wrong ones.
fn spawn_backlog(tasks: &mut ShapeTasks, mut backlog: Vec<(f32, ShapeJob)>) {
    if backlog.is_empty() {
        return;
    }
    let room = MAX_SHAPE_TASKS_IN_FLIGHT.saturating_sub(tasks.in_flight());
    if room == 0 {
        return;
    }
    backlog.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (_, job) in backlog.into_iter().take(room) {
        tasks.spawn(job);
    }
}

/// Re-shape a streaming block's open tail, inheriting its frozen chunks.
///
/// `inherited` is the frozen prefix [`incremental_prefix`] approved, already
/// `Arc`-cloned; `ranges` is the new chunk plan, whose first `inherited.len()`
/// entries are byte-for-byte the ranges those chunks were shaped from.
fn shape_tail(
    font: &VelloFont,
    inherited: Vec<ShapedChunk>,
    text: &str,
    spans: &[SpanBrush],
    style: &VelloTextStyle,
    wrap_width: f32,
    ranges: &[Range<usize>],
) -> ShapeOutput {
    let keep = inherited.len();
    let frozen = frozen_chunk_count(text, ranges, CHUNK_LINES);
    let max_advance = (wrap_width > 0.0).then_some(wrap_width);

    let mut out = ShapeOutput {
        text_height: inherited.iter().map(|c| c.height).sum(),
        chunks: inherited,
        ..Default::default()
    };
    for (i, range) in ranges.iter().enumerate().skip(keep) {
        shape_chunk(
            font,
            text,
            spans,
            style,
            max_advance,
            range.clone(),
            i < frozen,
            &mut out,
        );
    }
    out
}

/// Drop least-recently-used shaped blocks until the cache fits its budget.
///
/// The pinned window is `DESPAWN_MARGIN_SCREENS` on each side — the same
/// hysteresis band the legacy entity lifecycle uses, and deliberately much
/// wider than the shape band, so a block does not get evicted and re-shaped by
/// scrolling one screen back and forth. Streaming blocks are pinned wherever
/// they sit: evicting one would throw away the frozen chunks the incremental
/// tail is built on, and the next tick would re-shape the whole thing.
///
/// An evicted row keeps its **measured height** in `ConversationGeometry` —
/// rows outlive their glyphs there — so eviction never moves the document. The
/// row re-shapes through the backlog when it comes back within a screen.
fn evict_shaped_blocks(
    caches: &mut ShapeCaches,
    geom: &ConversationGeometry,
    offset: f32,
    vh: f32,
) {
    if caches.shaped.total_glyphs() <= SHAPE_CACHE_MAX_GLYPHS {
        return;
    }

    let pinned_rows = geom.visible_rows(offset, vh, DESPAWN_MARGIN_SCREENS * vh);
    let pinned: HashSet<BlockId> = geom.rows()[pinned_rows]
        .iter()
        .filter_map(|row| match row.key {
            RowKey::Block(id) => Some(id),
            RowKey::Header(_) => None,
        })
        .collect();

    let mut candidates = eviction_candidates(&caches.shaped, &pinned);
    let total = caches.shaped.total_glyphs();
    for id in plan_evictions(&mut candidates, total, SHAPE_CACHE_MAX_GLYPHS) {
        caches.shaped.remove(&id);
        // An in-flight result for an evicted block would re-insert it only to
        // be evicted again next frame.
        caches.tasks.cancel(&id);
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

    let mut fonts = Vec::new();
    collect_fonts(&layout, &mut fonts);
    for font in &fonts {
        font_data.register(font);
    }
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
            rich_theme_epoch: 0,
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
            ShapeKey { rich_theme_epoch: 1, ..base },
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
        let one_line = shape_block(&font, "one", &[], &style(), 10_000.0, CHUNK_LINES).text_height;
        assert!(one_line > 0.0, "a single line must have a height");

        for lines in [2_usize, 5, 40] {
            let text = vec!["one"; lines].join("\n");
            let height =
                shape_block(&font, &text, &[], &style(), 10_000.0, CHUNK_LINES).text_height;
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
        let text = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = shape_block(&font, &text, &[], &style(), 10_000.0, CHUNK_LINES);
        let chunks = &out.chunks;
        assert!(chunks.len() > 1, "test premise: 200 lines must chunk");
        let stacked: f32 = chunks.iter().map(|c| c.height).sum();
        assert!((out.text_height - stacked).abs() < 0.01);
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
        let text = (0..150)
            .map(|i| format!("line {i} with a little more text on it"))
            .collect::<Vec<_>>()
            .join("\n");
        let wrap = 220.0_f32;

        let whole = font.layout(&text, &style(), VelloTextAlign::Left, Some(wrap));
        let (expected, _) =
            collect_msdf_glyphs_deferred(&whole, &[], &style().brush, (0.0, 0.0));

        let chunks = shape_block(&font, &text, &[], &style(), wrap, CHUNK_LINES).chunks;
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
        let out = shape_block(&font, "", &[], &style(), 400.0, CHUNK_LINES);
        assert_eq!(out.chunks.len(), 1);
        assert!(out.chunks[0].glyphs.is_empty());
        assert!(out.glyph_keys.is_empty());
        assert!(out.text_height >= 0.0);
        assert!(!out.chunks[0].frozen, "an empty chunk is still open");
    }


    // ---- slice 3: incremental tail ---------------------------------------

    /// Wrap a `ShapeOutput` the way the shape pass does, so the tail-path
    /// tests exercise the same `ShapedBlock` shape production builds.
    fn shaped_from(out: ShapeOutput, key: ShapeKey, text: &str, streaming: bool) -> ShapedBlock {
        ShapedBlock {
            key,
            glyph_count: out.chunks.iter().map(|c| c.glyphs.len()).sum(),
            height: out.text_height,
            text_height: out.text_height,
            chunks: out.chunks,
            x_offset: 0.0,
            y_offset: 0.0,
            last_used: 0,
            color: Color::WHITE,
            text_len: text.len(),
            streaming,
            labels: crate::view::surface::labels::BlockLabels::default(),
        }
    }

    /// A block long enough to fill one chunk and open a second.
    fn streamed_text(extra_lines: usize) -> String {
        (0..CHUNK_LINES + extra_lines)
            .map(|i| format!("line {i} of a streamed reply"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// **The incremental tail's whole point.** Appending to a streaming block
    /// must reuse its frozen chunks *by pointer* — not re-shape them, not even
    /// copy them. `Arc::ptr_eq` is the assertion because that is exactly what
    /// the render world sees: an unchanged pointer is an upload it can skip.
    ///
    /// And the result must still be the document: the stacked chunks have to
    /// agree with a whole re-shape of the grown text, or the fast path is
    /// drawing something the geometry never measured.
    #[test]
    fn a_tail_append_reuses_the_frozen_chunks_by_pointer() {
        let font = mono();
        let text = streamed_text(5);
        let key = key();

        let first = shape_block(&font, &text, &[], &style(), 10_000.0, CHUNK_LINES);
        assert!(first.chunks.len() >= 2, "test premise: the text must chunk");
        assert!(first.chunks[0].frozen, "a full chunk must freeze");
        assert!(
            !first.chunks.last().unwrap().frozen,
            "the open tail must not freeze",
        );
        let existing = shaped_from(first, key, &text, true);

        // The stream appends one more line.
        let grown = format!("{text}\nline extra");
        let grown_key = ShapeKey {
            content_version: key.content_version + 1,
            ..key
        };
        let ranges = chunk_ranges(&grown, CHUNK_LINES);
        let inherit = incremental_prefix(&existing, &grown_key, Color::WHITE, &grown, &ranges, false)
            .expect("an append to a streaming block must take the tail path");
        assert_eq!(inherit, 1, "exactly the frozen prefix comes across");

        let out = shape_tail(
            &font,
            existing.chunks[..inherit].to_vec(),
            &grown,
            &[],
            &style(),
            10_000.0,
            &ranges,
        );

        assert!(
            Arc::ptr_eq(&out.chunks[0].glyphs, &existing.chunks[0].glyphs),
            "a frozen chunk must be carried forward as the same allocation",
        );
        assert!(
            !Arc::ptr_eq(
                &out.chunks[1].glyphs,
                &existing.chunks[1].glyphs,
            ),
            "the open tail must actually have been re-shaped",
        );

        // The cheap answer and the expensive answer must be the same answer.
        let whole = shape_block(&font, &grown, &[], &style(), 10_000.0, CHUNK_LINES);
        assert_eq!(out.chunks.len(), whole.chunks.len());
        assert!(
            (out.text_height - whole.text_height).abs() < 0.01,
            "tail-shaped height {} vs whole {}",
            out.text_height,
            whole.text_height,
        );
        for (i, (a, b)) in out.chunks.iter().zip(whole.chunks.iter()).enumerate() {
            assert_eq!(a.byte_range, b.byte_range, "chunk {i} range");
            assert_eq!(a.glyphs.len(), b.glyphs.len(), "chunk {i} glyph count");
            assert_eq!(a.frozen, b.frozen, "chunk {i} frozen flag");
        }
    }

    /// Appending repeatedly must keep freezing: after enough lines the second
    /// chunk closes too, and from then on it is carried by pointer as well.
    /// Without this the "cost per tick is one chunk" claim degrades to "one
    /// chunk, and then everything after it".
    #[test]
    fn a_chunk_freezes_as_soon_as_it_fills() {
        let font = mono();
        let text = streamed_text(5);
        let mut block = shaped_from(shape_block(&font, &text, &[], &style(), 10_000.0, CHUNK_LINES), key(), &text, true);
        let mut text = text;

        // Push past the second chunk boundary one line at a time.
        for i in 0..CHUNK_LINES {
            text = format!("{text}\nappended {i}");
            let next = ShapeKey {
                content_version: block.key.content_version + 1,
                ..block.key
            };
            let ranges = chunk_ranges(&text, CHUNK_LINES);
            let inherit = incremental_prefix(&block, &next, Color::WHITE, &text, &ranges, false)
                .expect("every append must stay on the tail path");
            let out = shape_tail(
                &font,
                block.chunks[..inherit].to_vec(),
                &text,
                &[],
                &style(),
                10_000.0,
                &ranges,
            );
            block = shaped_from(out, next, &text, true);
        }

        assert!(block.chunks.len() >= 3, "test premise: a third chunk opened");
        assert!(
            block.chunks[1].frozen,
            "the second chunk must have frozen once it filled",
        );
        let whole = shape_block(&font, &text, &[], &style(), 10_000.0, CHUNK_LINES);
        assert!(
            (block.text_height - whole.text_height).abs() < 0.05,
            "{} incremental appends drifted: {} vs {}",
            CHUNK_LINES,
            block.text_height,
            whole.text_height,
        );
    }

    /// Every clause of the incremental gate must actually gate. A block that
    /// slips through one of these reuses glyphs against different bytes, which
    /// is the one failure mode of this design that is silent.
    #[test]
    fn the_incremental_gate_rejects_everything_it_should() {
        let font = mono();
        let text = streamed_text(5);
        let out = shape_block(&font, &text, &[], &style(), 10_000.0, CHUNK_LINES);
        let base = key();
        let streaming = shaped_from(out, base, &text, true);
        let grown = format!("{text}\nline extra");
        let grown_ranges = chunk_ranges(&grown, CHUNK_LINES);
        let next = ShapeKey {
            content_version: base.content_version + 1,
            ..base
        };

        // Sanity: the honest case is accepted.
        assert!(incremental_prefix(&streaming, &next, Color::WHITE, &grown, &grown_ranges, false).is_some());

        // A settled block: its text can have been *edited*, so equal byte
        // ranges say nothing about equal bytes.
        let mut settled = streaming.clone();
        settled.streaming = false;
        assert_eq!(
            incremental_prefix(&settled, &next, Color::WHITE, &grown, &grown_ranges, false),
            None,
            "a Done block must re-shape whole",
        );

        // A wrap-width change re-flows every line, frozen or not.
        let rewrapped = ShapeKey {
            wrap_width_bits: 320.0_f32.to_bits(),
            ..next
        };
        assert_eq!(
            incremental_prefix(&streaming, &rewrapped, Color::WHITE, &grown, &grown_ranges, false),
            None,
            "a resize must re-shape whole",
        );

        // A metrics change (font size) likewise.
        let remetriced = ShapeKey {
            metrics_epoch: next.metrics_epoch + 1,
            ..next
        };
        assert_eq!(
            incremental_prefix(&streaming, &remetriced, Color::WHITE, &grown, &grown_ranges, false),
            None,
        );

        // Text that shrank is not an append.
        let shrunk = &text[..text.len() / 2];
        let shrunk_ranges = chunk_ranges(shrunk, CHUNK_LINES);
        assert_eq!(
            incremental_prefix(&streaming, &next, Color::WHITE, shrunk, &shrunk_ranges, false),
            None,
            "a truncation must re-shape whole",
        );

        // A frozen range that no longer tiles the same bytes (an edit that
        // grew an early line) must be rejected even though the text got
        // longer.
        let edited = format!("prefix inserted at the very top\n{grown}");
        let edited_ranges = chunk_ranges(&edited, CHUNK_LINES);
        assert_eq!(
            incremental_prefix(&streaming, &next, Color::WHITE, &edited, &edited_ranges, false),
            None,
            "a shifted frozen range must re-shape whole",
        );

        // A color change: frozen chunks carry their colors baked in, so
        // inheriting them under a new palette would leave the head of the
        // block in the old theme while its tail arrived in the new one.
        assert_eq!(
            incremental_prefix(
                &streaming,
                &next,
                Color::srgb(1.0, 0.0, 0.5),
                &grown,
                &grown_ranges,
                false,
            ),
            None,
            "a repainted streaming block must re-shape whole",
        );

        // Spanned blocks NEVER take the tail path: byte-identical frozen
        // ranges don't prove the rendering is unchanged when spans re-derive
        // over the whole text (setext heading retro-coloring a frozen line;
        // an md_*-token theme change moving brushes under a still base
        // color). Same inputs that pass for span-free text must refuse.
        assert_eq!(
            incremental_prefix(&streaming, &next, Color::WHITE, &grown, &grown_ranges, true),
            None,
            "a spanned streaming block must re-shape whole",
        );

        // Nothing frozen yet: a short streaming block has one open chunk and
        // there is nothing to inherit.
        let short = "one\ntwo";
        let short_block = shaped_from(
            shape_block(&font, short, &[], &style(), 10_000.0, CHUNK_LINES),
            base,
            short,
            true,
        );
        let short_grown = "one\ntwo\nthree";
        assert_eq!(
            incremental_prefix(
                &short_block,
                &next,
                Color::WHITE,
                short_grown,
                &chunk_ranges(short_grown, CHUNK_LINES),
                false,
            ),
            None,
            "with no frozen chunk the tail path has nothing to save",
        );
    }

    // ---- slice 3: async backlog ------------------------------------------

    /// A task per `(block, key)`, not per frame. The shape band is walked
    /// every tick, so without this guard a backlog row would spawn a duplicate
    /// task 60 times a second until the first one finished.
    #[test]
    fn a_second_task_is_not_spawned_for_an_in_flight_key() {
        let mut tasks = ShapeTasks::default();
        let id = bid(1);
        let k = key();

        assert!(!tasks.is_pending(&id, &k));
        tasks.note_spawned(id, k);
        assert!(tasks.is_pending(&id, &k), "the spawn must be remembered");
        assert_eq!(tasks.in_flight(), 1);

        // Same block, newer key: a different request, so it is *not* pending
        // and the newer spawn replaces the older one's claim.
        let newer = ShapeKey {
            content_version: k.content_version + 1,
            ..k
        };
        assert!(!tasks.is_pending(&id, &newer));
        tasks.note_spawned(id, newer);
        assert_eq!(tasks.in_flight(), 1, "one claim per block, always the newest");
    }

    /// Last-writer-wins, decided by what the main thread most recently asked
    /// for. The old task still finishes — nothing cancels a running future —
    /// so the drop has to happen on arrival or the cache ends up holding a
    /// shaping of text that has already moved on.
    #[test]
    fn a_stale_async_result_is_dropped() {
        let mut tasks = ShapeTasks::default();
        let id = bid(1);
        let old = key();
        let new = ShapeKey {
            content_version: old.content_version + 1,
            ..old
        };

        tasks.note_spawned(id, old);
        tasks.note_spawned(id, new);

        assert!(
            !tasks.accept(&id, &old),
            "the superseded task's result must be dropped",
        );
        assert!(
            tasks.accept(&id, &new),
            "the current task's result must be applied",
        );
        assert_eq!(tasks.in_flight(), 0, "accepting clears the claim");
        assert!(
            !tasks.accept(&id, &new),
            "a result can only be accepted once",
        );
    }

    /// The main thread shaping a block itself supersedes anything in flight
    /// for it — otherwise a backlog result landing a frame later would
    /// overwrite the synchronous answer with an older one.
    #[test]
    fn a_cancelled_blocks_result_is_dropped() {
        let mut tasks = ShapeTasks::default();
        let id = bid(1);
        let k = key();
        tasks.note_spawned(id, k);
        tasks.cancel(&id);
        assert!(!tasks.accept(&id, &k));
        assert_eq!(tasks.in_flight(), 0);
    }

    /// The job → result path is a pure function, so it can be checked without
    /// a task pool: what a task would produce must equal what the synchronous
    /// path produces for the same inputs. If these ever diverge, whether a
    /// block was near the viewport when it shaped becomes visible.
    #[test]
    fn a_backlog_job_shapes_exactly_what_the_sync_path_would() {
        let font = mono();
        let text = streamed_text(3);
        let layout = super::super::chrome::surface_block_layout(800.0, 0, 24.0, 0.0, None);

        let result = run_shape_job(ShapeJob {
            block: bid(1),
            key: key(),
            font: font.clone(),
            text: text.clone(),
            spans: Vec::new(),
            style: style(),
            wrap_width: layout.wrap_width,
            chunk_lines: CHUNK_LINES,
            color: Color::WHITE,
            layout,
        });
        let direct = shape_block(&font, &text, &[], &style(), layout.wrap_width, CHUNK_LINES);

        assert_eq!(result.block, bid(1));
        assert_eq!(result.text_len, text.len());
        assert_eq!(result.output.chunks.len(), direct.chunks.len());
        assert_eq!(result.output.text_height, direct.text_height);
        assert_eq!(result.output.glyph_keys, direct.glyph_keys);
        for (a, b) in result.output.chunks.iter().zip(direct.chunks.iter()) {
            assert_eq!(a.byte_range, b.byte_range);
            assert_eq!(a.frozen, b.frozen);
            assert_eq!(a.glyphs.len(), b.glyphs.len());
        }
        assert!(
            !result.output.fonts.is_empty(),
            "the task must carry back the fonts the main thread has to register",
        );
    }

    // ---- slice 3: eviction ------------------------------------------------

    fn cached(glyphs: usize, last_used: u64, streaming: bool) -> ShapedBlock {
        ShapedBlock {
            key: key(),
            chunks: Vec::new(),
            height: 10.0,
            text_height: 10.0,
            x_offset: 0.0,
            y_offset: 0.0,
            glyph_count: glyphs,
            last_used,
            color: Color::WHITE,
            text_len: 0,
            streaming,
            labels: crate::view::surface::labels::BlockLabels::default(),
        }
    }

    #[test]
    fn a_cache_inside_its_budget_evicts_nothing() {
        let mut candidates = vec![(bid(1), 100, 1), (bid(2), 100, 2)];
        assert!(plan_evictions(&mut candidates, 200, 1000).is_empty());
        // Exactly at the budget is inside it.
        assert!(plan_evictions(&mut candidates, 200, 200).is_empty());
    }

    /// Least-recently-used first, and only as many as the budget demands —
    /// eviction is a trim, not a purge.
    #[test]
    fn eviction_drops_the_oldest_until_the_budget_fits() {
        let mut candidates = vec![
            (bid(1), 100, 30), // newest
            (bid(2), 100, 10), // oldest
            (bid(3), 100, 20),
        ];
        let evicted = plan_evictions(&mut candidates, 300, 150);
        assert_eq!(
            evicted,
            vec![bid(2), bid(3)],
            "oldest first, stopping as soon as 300 - freed <= 150",
        );
    }

    /// Pinned glyphs count against the budget but can never be freed. When the
    /// pinned set alone busts it, eviction takes everything it may and stops —
    /// the honest outcome, rather than dropping a row that is on screen.
    #[test]
    fn a_budget_smaller_than_the_pinned_set_evicts_only_what_it_may() {
        // 1000 glyphs total, of which only 200 are evictable.
        let mut candidates = vec![(bid(9), 200, 1)];
        let evicted = plan_evictions(&mut candidates, 1000, 100);
        assert_eq!(evicted, vec![bid(9)]);
    }

    /// The two pins, at the seam that applies them: a block in the window and
    /// a streaming block anywhere are both invisible to eviction.
    #[test]
    fn in_window_and_streaming_blocks_are_never_eviction_candidates() {
        let mut cache = ShapedBlockCache::default();
        cache.insert_for_test(bid(1), cached(10, 1, false)); // pinned by window
        cache.insert_for_test(bid(2), cached(20, 2, true)); // streaming
        cache.insert_for_test(bid(3), cached(30, 3, false)); // evictable

        let pinned: HashSet<BlockId> = [bid(1)].into_iter().collect();
        let candidates = eviction_candidates(&cache, &pinned);
        assert_eq!(
            candidates,
            vec![(bid(3), 30, 3)],
            "only the out-of-window, settled block may be evicted",
        );
    }

    /// The budget is read every frame, so the running total has to be right —
    /// a drift either stalls eviction forever or evicts a cache that fits.
    #[test]
    fn the_glyph_total_tracks_every_insert_and_remove() {
        let mut cache = ShapedBlockCache::default();
        assert_eq!(cache.total_glyphs(), 0);

        cache.insert_for_test(bid(1), cached(100, 1, false));
        cache.insert_for_test(bid(2), cached(250, 1, false));
        assert_eq!(cache.total_glyphs(), 350);

        // Replacing an entry accounts for both sides.
        cache.insert_for_test(bid(1), cached(40, 2, false));
        assert_eq!(cache.total_glyphs(), 290);

        cache.remove(&bid(2));
        assert_eq!(cache.total_glyphs(), 40);

        cache.insert_for_test(bid(3), cached(7, 1, false));
        cache.retain(|id| *id == bid(3));
        assert_eq!(cache.total_glyphs(), 7);
        assert_eq!(
            cache.total_glyphs(),
            cache.blocks.values().map(|b| b.glyph_count).sum::<usize>(),
            "the running total must equal the recomputed one",
        );
    }

    // ---- slice 3: theme recolor -------------------------------------------

    /// **Recolor moves colors and nothing else.** Bit-identical positions is
    /// the assertion, not "close enough": the whole claim that a theme swap
    /// needs no shaping rests on it, and a float comparison with a tolerance
    /// would hide exactly the bug that would break it.
    #[test]
    fn recolor_repaints_without_moving_a_single_glyph() {
        let font = mono();
        let text = "the quick brown fox\njumps over the lazy dog";
        let out = shape_block(&font, text, &[], &style(), 200.0, CHUNK_LINES);
        let before: Vec<PositionedGlyph> =
            out.chunks.iter().flat_map(|c| c.glyphs.iter().cloned()).collect();
        assert!(!before.is_empty(), "test premise: the text must have glyphs");

        let mut block = shaped_from(out, key(), text, false);
        let new_color = Color::srgb(1.0, 0.0, 0.5);
        recolor_block(&mut block, new_color);

        let after: Vec<PositionedGlyph> =
            block.chunks.iter().flat_map(|c| c.glyphs.iter().cloned()).collect();
        assert_eq!(before.len(), after.len());
        let want = color_to_rgba8(new_color);
        for (i, (a, b)) in before.iter().zip(after.iter()).enumerate() {
            assert_eq!(a.key, b.key, "glyph {i} changed identity");
            assert_eq!(a.x.to_bits(), b.x.to_bits(), "glyph {i} moved in x");
            assert_eq!(a.y.to_bits(), b.y.to_bits(), "glyph {i} moved in y");
            assert_eq!(
                a.font_size.to_bits(),
                b.font_size.to_bits(),
                "glyph {i} resized",
            );
            assert_eq!(b.color, want, "glyph {i} kept the old color");
        }
        assert_eq!(block.color, new_color, "the cached color must be restamped");
    }

    /// A repaint to the color a block already has must keep its allocations —
    /// otherwise the "no-op" path deep-clones every glyph the render world is
    /// holding, which is worse than doing nothing at all.
    #[test]
    fn recoloring_to_the_same_color_keeps_the_allocation() {
        let font = mono();
        let text = "unchanged";
        let out = shape_block(&font, text, &[], &style(), 200.0, CHUNK_LINES);
        let mut block = shaped_from(out, key(), text, false);
        // `style()` paints white, so this is genuinely a no-op.
        let held: Vec<Arc<Vec<PositionedGlyph>>> =
            block.chunks.iter().map(|c| c.glyphs.clone()).collect();

        recolor_block(&mut block, Color::WHITE);

        for (i, (chunk, before)) in block.chunks.iter().zip(held.iter()).enumerate() {
            assert!(
                Arc::ptr_eq(&chunk.glyphs, before),
                "chunk {i} was cloned for a repaint that changed nothing",
            );
        }
    }

    /// The theme epoch is an event counter, not a per-frame one: a frame that
    /// doesn't touch the theme must not invalidate anything.
    #[test]
    fn the_theme_epoch_moves_once_per_theme_write() {
        let mut app = App::new();
        app.init_resource::<Theme>();
        app.init_resource::<SurfaceThemeEpoch>();
        app.add_systems(Update, track_surface_theme_epoch);

        app.update();
        let first = app.world().resource::<SurfaceThemeEpoch>().get();
        app.update();
        app.update();
        assert_eq!(
            app.world().resource::<SurfaceThemeEpoch>().get(),
            first,
            "an untouched theme must not bump the epoch",
        );

        app.world_mut().resource_mut::<Theme>().block_user = Color::srgb(0.1, 0.2, 0.3);
        app.update();
        assert_eq!(app.world().resource::<SurfaceThemeEpoch>().get(), first + 1);
        app.update();
        assert_eq!(
            app.world().resource::<SurfaceThemeEpoch>().get(),
            first + 1,
            "the bump is per write, not per frame after one",
        );
    }

    /// Role labels bake theme colors into their glyphs, so a theme swap has to
    /// drop them — this is the cache-clearing half of the recolor story.
    #[test]
    fn a_theme_bump_drops_the_cached_role_labels() {
        let mut headers = HeaderLabelCache::default();
        headers.insert_for_test(
            Role::User,
            1,
            ShapedLabel {
                glyphs: Arc::new(Vec::new()),
                x_offset: 0.0,
                y_offset: 0.0,
                width: 10.0,
                height: 10.0,
            },
        );
        assert_eq!(headers.len(), 1);

        assert!(!headers.sync_theme(0), "epoch 0 is where it started");
        assert_eq!(headers.len(), 1);

        assert!(headers.sync_theme(1), "a theme bump must clear the labels");
        assert_eq!(headers.len(), 0);
        assert!(!headers.sync_theme(1), "and only once per bump");
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
        app.world_mut()
            .resource_mut::<ShapedBlockCache>()
            .insert_for_test(id, block_of_height(height));
    }

    fn block_of_height(height: f32) -> ShapedBlock {
        ShapedBlock {
            key: ShapeKey {
                content_version: 5,
                wrap_width_bits: 800.0_f32.to_bits(),
                collapsed: false,
                indent_level: 0,
                metrics_epoch: 1,
                rich_theme_epoch: 0,
            },
            chunks: Vec::new(),
            height,
            text_height: height,
            x_offset: 0.0,
            y_offset: 0.0,
            glyph_count: 0,
            last_used: 0,
            color: Color::WHITE,
            text_len: 0,
            streaming: false,
            labels: crate::view::surface::labels::BlockLabels::default(),
        }
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

    /// **Async pop-in is anchor-compensated.** A backlog row above the
    /// viewport lands its real height a frame or more after the reader
    /// scrolled past it. The compensation is the same one a synchronous
    /// measurement gets — it keys off the height changing, not off who
    /// changed it — but that is worth a test of its own, because "the row was
    /// already measured once" is exactly the assumption that would break it,
    /// and deep-scrolling through a backfilling document is the case where a
    /// drift is most visible.
    #[test]
    fn a_late_async_landing_above_the_viewport_still_shifts_the_offset() {
        let mut app = measure_app(false, 500.0);
        install_geometry(&mut app, strip(4));

        // Frame 1: block 1 has no shaped entry — it draws chrome and the
        // height the geometry estimated, which is what an unshaped band row
        // does on this path.
        app.update();
        let settled = app.world().resource::<ConversationScrollState>().offset;

        // Frame 2: the backlog result lands, 60px taller than the estimate.
        insert_shaped(&mut app, bid(1), 90.0);
        app.update();

        let state = app.world().resource::<ConversationScrollState>();
        assert_eq!(
            state.offset,
            settled + 60.0,
            "a late landing above the viewport must carry the viewport with it",
        );
        assert_eq!(state.target_offset, settled + 60.0);
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
