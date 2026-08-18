//! Logical geometry model for the conversation column.
//!
//! [`ConversationGeometry`] is the document-order height/offset model for
//! every block and role header in the conversation — including ones with **no
//! live entity**. It exists so the virtualized column can answer "how tall is
//! the document and where does block X sit" without spawning entities or
//! paying a taffy layout for offscreen content:
//!
//! - Rows are seeded with an **estimated** height ([`estimate_block_height`])
//!   when first seen, so first load of a long conversation never pays an
//!   O(N) layout pass (`measured_version == 0` marks an estimate).
//! - A row's height is replaced by the **measured** height when its entity is
//!   laid out (`readback_block_heights` calls [`ConversationGeometry::measure`]).
//!   Measured heights survive entity despawn — scrolling back re-seeds the
//!   respawned entity from here instead of re-estimating.
//! - Reconciliation is gated on the document version and touches the block
//!   store only for rows it has never seen (`seed_fn` per NEW id) — never a
//!   full `editor.blocks()` snapshot clone.
//!
//! The unit contract: all heights/offsets here are in the same units as
//! `ComputedNode` sizes and `ConversationScrollState` offsets (whatever
//! Bevy UI layout yields — the same source `visible_height` uses).

use bevy::prelude::*;
use std::collections::HashMap;

use kaijutsu_types::{BlockId, BlockKind, Role};

/// Identity of a geometry row: a block, or the role header shown before it.
///
/// A header is keyed by the block it precedes (same convention as
/// `RoleGroupBorder.block_id`), so header rows survive reconciles as long as
/// the same block still starts its role run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowKey {
    /// Role header preceding this block.
    Header(BlockId),
    /// The block itself.
    Block(BlockId),
}

/// One row of the conversation column: a block or a role header.
#[derive(Debug, Clone)]
pub struct GeomRow {
    pub key: RowKey,
    /// Current best height: estimated until measured, then the last real
    /// taffy measurement (held across despawn).
    pub height: f32,
    /// Bottom margin below this row. Estimated from theme constants at
    /// reconcile; replaced by the live `Node` margin at measure time.
    pub margin_bottom: f32,
    /// Top of this row relative to document start (prefix sum, see
    /// [`ConversationGeometry::recompute_offsets`]).
    pub y_offset: f32,
    /// Document version stamped at the last real measurement of this row.
    /// `0` = never measured — `height` is an estimate.
    pub measured_version: u64,
    /// Content length at seed time (estimation input; not refreshed while
    /// the row is offscreen — heights self-correct on band entry).
    pub text_len: usize,
    /// Newline count at seed time (estimation input).
    pub newline_count: usize,
    /// Role of the block (drives header derivation on reconcile).
    pub role: Role,
    /// Kind of the block (tool blocks take no role header and join their
    /// result with a zero bottom margin).
    pub kind: BlockKind,
    /// Whether the block rendered collapsed at seed time.
    pub collapsed: bool,
    /// Indent level (parent nesting), mirrored to `BlockCellLayout`.
    pub indent_level: u32,
    /// Document version when this row was first created. Persisted here so a
    /// despawned block's `TimelineVisibility.created_at_version` survives
    /// respawn (timeline dimming would otherwise mis-classify it as new).
    pub created_at_version: u64,
}

/// Inputs captured from a `BlockSnapshot` for a row the geometry has never
/// seen. This is the only path that touches block content, and it runs once
/// per new row — never per frame.
#[derive(Debug, Clone)]
pub struct RowSeed {
    pub text_len: usize,
    pub newline_count: usize,
    pub role: Role,
    pub kind: BlockKind,
    pub collapsed: bool,
    pub parent_id: Option<BlockId>,
}

/// Estimation + margin parameters, sampled from `TextMetrics` + `Theme` at
/// reconcile time.
#[derive(Debug, Clone, PartialEq)]
pub struct EstimateParams {
    /// Approximate character columns available to block text.
    pub cols: usize,
    /// Line height in layout units.
    pub line_height: f32,
    /// `theme.block_spacing` — default bottom margin between blocks.
    pub block_spacing: f32,
    /// `theme.role_header_height` — header row height until measured.
    pub role_header_height: f32,
    /// `theme.role_header_spacing` — header bottom margin.
    pub role_header_spacing: f32,
}

impl Default for EstimateParams {
    fn default() -> Self {
        Self {
            cols: 100,
            line_height: 30.0,
            block_spacing: 12.0,
            role_header_height: 20.0,
            role_header_spacing: 4.0,
        }
    }
}

/// Estimate a block's rendered height from cheap text statistics.
///
/// `rows = max(hard_lines, ceil(text_len / cols))` — exact for unwrapped
/// text, close enough for wrapped monospace. Estimates only need to be
/// plausible: they size spacers/scrollbar until the block is first laid out,
/// and the real measurement replaces them just-in-time as the block enters
/// the spawn band (before it becomes visible).
///
/// Collapsed is a flat `line_height` for every kind except `Error`: a
/// collapsed `Error` block renders `format::ERROR_STUB_MAX_LINES` lines
/// (provenance, summary, up to 3 detail lines, a hint line — see
/// `format::format_error_stub`), not the single-line `Thinking [collapsed]`
/// stub, so sizing it like `Thinking` would under-reserve space and cause a
/// visible jump when the row is first measured.
pub fn estimate_block_height(
    text_len: usize,
    newline_count: usize,
    collapsed: bool,
    kind: BlockKind,
    params: &EstimateParams,
) -> f32 {
    if collapsed {
        let lines = if kind == BlockKind::Error {
            crate::view::format::ERROR_STUB_MAX_LINES
        } else {
            1
        };
        return lines as f32 * params.line_height;
    }
    let cols = params.cols.max(20);
    let hard_lines = newline_count + 1;
    let wrapped = text_len.div_ceil(cols).max(1);
    let rows = hard_lines.max(wrapped);
    rows as f32 * params.line_height
}

/// Document-order logical geometry for one conversation column.
///
/// Lives beside `BlockCellContainer` on the main cell entity. The single
/// writer of row *structure* is [`ConversationGeometry::reconcile`]
/// (`sync_conversation_geometry`); heights are refined by
/// [`ConversationGeometry::measure`] (`readback_block_heights`).
#[derive(Component, Debug, Default)]
pub struct ConversationGeometry {
    rows: Vec<GeomRow>,
    /// Block rows only — headers are found by scanning (they always
    /// immediately precede their block row).
    block_index: HashMap<BlockId, usize>,
    /// Total document height: `sum(height + margin_bottom)` over all rows.
    /// Matches what `readback_block_heights` historically computed.
    pub content_height: f32,
    /// Document version at the last reconcile (the reconcile gate).
    pub last_doc_version: u64,
    /// Block ids in document order at the last reconcile. Second reconcile
    /// gate: a context switch can REPLACE the editor's store wholesale with
    /// a coincidentally-equal version (welcome → hydrated context), which
    /// the version gate alone can't see — stale rows then feed the band a
    /// dead id and the spawn/despawn loop never converges.
    block_ids: Vec<BlockId>,
    /// Store instance the last reconcile read
    /// (`CellEditor::store_generation`). Generations start at 1, so a
    /// freshly-inserted geometry never matches a live store and always
    /// reconciles on its first frame.
    pub last_store_generation: u64,
    /// Cols used for the current estimates (re-estimation gate on resize).
    pub cols: usize,
    /// Prefix sums need recomputation.
    dirty: bool,
    /// Bumped whenever laid-out geometry actually changed — see
    /// [`ConversationGeometry::epoch`].
    epoch: u64,
}

impl ConversationGeometry {
    pub fn rows(&self) -> &[GeomRow] {
        &self.rows
    }

    /// Look up the block row for `id`.
    pub fn block_row(&self, id: &BlockId) -> Option<&GeomRow> {
        self.block_index.get(id).map(|&i| &self.rows[i])
    }

    /// Whether the given document-order id sequence matches the one this
    /// geometry was last reconciled against.
    pub fn ids_match(&self, ids: &[BlockId]) -> bool {
        self.block_ids == ids
    }

    /// Change token for "the laid-out geometry moved".
    ///
    /// Bumped by a structural [`reconcile`](Self::reconcile), and by a
    /// [`recompute_offsets`](Self::recompute_offsets) pass that actually
    /// shifted a row or resized the document. A consumer caching anything
    /// derived from offsets (the GPU row window) compares this instead of
    /// re-deriving every frame.
    ///
    /// Both halves are needed, and neither subsumes the other: heights move
    /// without the structure changing (a measurement lands), and the
    /// structure changes without any offset moving (a row is replaced by one
    /// of identical height, so every prefix sum and the content height come
    /// out the same while the row → block mapping is different).
    #[allow(dead_code)] // Model accessor; exercised by tests, no prod caller yet.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The contiguous index range of rows intersecting the viewport band
    /// `[top - slack, top + height + slack)`.
    ///
    /// Rows tile the document with no gaps — a row's bottom edge
    /// (`y_offset + height + margin_bottom`) *is* the next row's `y_offset`,
    /// which makes both edges of the band a binary search over a monotone
    /// predicate rather than a walk. Margin is part of the row's extent here,
    /// the same as in [`plan_block_band`].
    ///
    /// The band is half-open: a row whose bottom edge lands exactly on
    /// `band_top`, or whose top lands exactly on `band_bottom`, contributes
    /// zero area and is excluded. A band that starts above the document or
    /// ends past it yields an empty range positioned where it belongs
    /// (`0..0` / `len..len`), so callers can use it as a splice point.
    ///
    /// Offsets must be current — call after
    /// [`recompute_offsets`](Self::recompute_offsets).
    #[allow(dead_code)] // Model accessor; exercised by tests, no prod caller yet.
    pub fn visible_rows(&self, top: f32, height: f32, slack: f32) -> std::ops::Range<usize> {
        debug_assert!(
            !self.dirty,
            "visible_rows read stale offsets — recompute_offsets first"
        );
        let band_top = top - slack;
        let band_bottom = top + height + slack;
        let start = self
            .rows
            .partition_point(|r| r.y_offset + r.height + r.margin_bottom <= band_top);
        let end = self.rows.partition_point(|r| r.y_offset < band_bottom);
        // A degenerate band (negative height/slack, i.e. band_bottom <
        // band_top) is a caller bug, not data corruption — the honest answer
        // is "no rows", and an inverted Range would panic on use.
        start..end.max(start)
    }

    /// Look up the header row preceding block `id`, if that block starts a
    /// role run.
    #[allow(dead_code)] // Model accessor; exercised by tests, no prod caller yet.
    pub fn header_row(&self, id: &BlockId) -> Option<&GeomRow> {
        let &i = self.block_index.get(id)?;
        if i == 0 {
            return None;
        }
        let prev = &self.rows[i - 1];
        (prev.key == RowKey::Header(*id)).then_some(prev)
    }

    /// Rebuild the row list against the current document order, reusing
    /// existing rows (their measured heights and creation stamps) and seeding
    /// new ones via `seed_fn`. Returns `true` if the row structure changed.
    ///
    /// `seed_fn` is called once per block id the geometry has never seen; a
    /// `None` seed skips the block this pass (snapshot raced removal — it
    /// will be retried next reconcile).
    pub fn reconcile(
        &mut self,
        ids: &[BlockId],
        mut seed_fn: impl FnMut(&BlockId) -> Option<RowSeed>,
        params: &EstimateParams,
        doc_version: u64,
    ) -> bool {
        let old_rows = std::mem::take(&mut self.rows);
        let old_block_index = std::mem::take(&mut self.block_index);
        // Header rows from the previous structure, so measured header heights
        // survive a reconcile that keeps the same role runs.
        let mut old_headers: HashMap<RowKey, GeomRow> = old_rows
            .iter()
            .filter(|r| matches!(r.key, RowKey::Header(_)))
            .map(|r| (r.key, r.clone()))
            .collect();

        let mut rows: Vec<GeomRow> = Vec::with_capacity(ids.len() + ids.len() / 4);
        let mut block_index: HashMap<BlockId, usize> = HashMap::with_capacity(ids.len());
        let mut prev_role: Option<Role> = None;
        let mut structure_changed = old_block_index.len() != ids.len();

        for id in ids {
            let block_row = match old_block_index.get(id) {
                Some(&i) => {
                    // Everything carries, `y_offset` included: it is
                    // authoritative only after `recompute_offsets` (which
                    // `dirty` below forces), and zeroing it here would make
                    // every reconcile look like it moved every row — the
                    // epoch would then bump on version bumps that changed
                    // nothing.
                    old_rows[i].clone()
                }
                None => {
                    let Some(seed) = seed_fn(id) else {
                        structure_changed = true;
                        continue;
                    };
                    structure_changed = true;
                    // Mirror `layout_block_cells`' historical rules: tool
                    // blocks are flush, children indent one level.
                    let is_tool =
                        matches!(seed.kind, BlockKind::ToolCall | BlockKind::ToolResult);
                    let indent_level = if !is_tool && seed.parent_id.is_some() {
                        1
                    } else {
                        0
                    };
                    GeomRow {
                        key: RowKey::Block(*id),
                        height: estimate_block_height(
                            seed.text_len,
                            seed.newline_count,
                            seed.collapsed,
                            seed.kind,
                            params,
                        ),
                        margin_bottom: params.block_spacing,
                        y_offset: 0.0,
                        measured_version: 0,
                        text_len: seed.text_len,
                        newline_count: seed.newline_count,
                        role: seed.role,
                        kind: seed.kind,
                        collapsed: seed.collapsed,
                        indent_level,
                        created_at_version: doc_version,
                    }
                }
            };

            // Role header derivation — same rules as
            // `interleave_blocks_and_headers` / `sync_role_headers`: tool
            // blocks neither carry nor break a role run.
            let is_tool = matches!(block_row.kind, BlockKind::ToolCall | BlockKind::ToolResult);
            if !is_tool {
                if prev_role != Some(block_row.role) {
                    let key = RowKey::Header(*id);
                    let header = old_headers.remove(&key).unwrap_or(GeomRow {
                        key,
                        height: params.role_header_height,
                        margin_bottom: params.role_header_spacing,
                        y_offset: 0.0,
                        measured_version: 0,
                        text_len: 0,
                        newline_count: 0,
                        role: block_row.role,
                        kind: block_row.kind,
                        collapsed: false,
                        indent_level: 0,
                        created_at_version: doc_version,
                    });
                    rows.push(header);
                }
                prev_role = Some(block_row.role);
            }

            block_index.insert(*id, rows.len());
            rows.push(block_row);
        }

        // Any leftover old header means a role run dissolved.
        structure_changed |= !old_headers.is_empty();

        // Margin pass: a ToolCall immediately followed (in block order) by a
        // ToolResult joins seamlessly (OpenBottom → zero gap) — mirrors
        // `update_block_cell_nodes`. Only estimated margins are touched;
        // measured rows keep the live margin recorded at measure time.
        let mut prev_block: Option<usize> = None;
        for i in 0..rows.len() {
            let RowKey::Block(_) = rows[i].key else {
                continue;
            };
            if let Some(p) = prev_block
                && rows[p].measured_version == 0
            {
                rows[p].margin_bottom = if rows[p].kind == BlockKind::ToolCall
                    && rows[i].kind == BlockKind::ToolResult
                {
                    0.0
                } else {
                    params.block_spacing
                };
            }
            prev_block = Some(i);
        }

        self.rows = rows;
        self.block_index = block_index;
        self.block_ids = ids.to_vec();
        self.last_doc_version = doc_version;
        self.cols = params.cols;
        self.dirty = true;
        if structure_changed {
            // A structural change can land on byte-identical offsets (a row
            // replaced by one of the same height), so `recompute_offsets`
            // cannot be the only thing that bumps the epoch — see
            // [`ConversationGeometry::epoch`].
            self.epoch += 1;
        }
        structure_changed
    }

    /// Record a real layout measurement for a row. Returns the height delta
    /// (`new - old`) so the caller can anchor-correct scroll when rows above
    /// the viewport change size.
    pub fn measure(
        &mut self,
        key: RowKey,
        height: f32,
        margin_bottom: f32,
        doc_version: u64,
    ) -> f32 {
        let Some(row) = self.row_mut(key) else {
            return 0.0;
        };
        let delta = height - row.height;
        if delta.abs() > 0.01 || (margin_bottom - row.margin_bottom).abs() > 0.01 {
            row.height = height;
            row.margin_bottom = margin_bottom;
            self.dirty = true;
        }
        // Stamp even when the size didn't move: version 0 → measured is a
        // state change (estimates stop being estimates).
        self.row_mut(key).unwrap().measured_version = doc_version.max(1);
        delta
    }

    fn row_mut(&mut self, key: RowKey) -> Option<&mut GeomRow> {
        match key {
            RowKey::Block(id) => {
                let &i = self.block_index.get(&id)?;
                self.rows.get_mut(i)
            }
            RowKey::Header(id) => {
                let &i = self.block_index.get(&id)?;
                if i == 0 {
                    return None;
                }
                let row = self.rows.get_mut(i - 1)?;
                (row.key == RowKey::Header(id)).then_some(row)
            }
        }
    }

    /// Re-estimate every never-measured row (window resize changed the
    /// wrap columns). Measured rows are left alone — taffy re-measures the
    /// live ones and despawned ones self-correct on band entry.
    pub fn reestimate_unmeasured(&mut self, params: &EstimateParams) {
        for row in &mut self.rows {
            if row.measured_version != 0 {
                continue;
            }
            let new_height = match row.key {
                RowKey::Block(_) => estimate_block_height(
                    row.text_len,
                    row.newline_count,
                    row.collapsed,
                    row.kind,
                    params,
                ),
                RowKey::Header(_) => params.role_header_height,
            };
            if (new_height - row.height).abs() > 0.01 {
                row.height = new_height;
                self.dirty = true;
            }
        }
        self.cols = params.cols;
    }

    /// Recompute prefix sums + content height if any row changed. Returns
    /// `true` if offsets were recomputed.
    ///
    /// The pass tracks whether it *moved* anything, not merely whether it
    /// ran: `dirty` says "a row might have changed", the movement check says
    /// "the document laid out differently", and only the latter bumps
    /// [`epoch`](Self::epoch).
    pub fn recompute_offsets(&mut self) -> bool {
        if !self.dirty {
            return false;
        }
        let mut y = 0.0_f32;
        let mut moved = false;
        for row in &mut self.rows {
            if row.y_offset != y {
                row.y_offset = y;
                moved = true;
            }
            y += row.height + row.margin_bottom;
        }
        if self.content_height != y {
            self.content_height = y;
            moved = true;
        }
        self.dirty = false;
        if moved {
            self.epoch += 1;
        }
        true
    }

}

// ============================================================================
// ENTITY BAND PLANNING
// ============================================================================

/// How far past the viewport (in screens) rows keep/get entities. Spawn
/// inside ±[`SPAWN_MARGIN_SCREENS`]; despawn only beyond
/// ±[`DESPAWN_MARGIN_SCREENS`]. The gap between them is hysteresis so a row
/// sitting at a band edge doesn't thrash spawn/despawn while scrolling.
/// Virtualize's show window (±1 screen) sits inside the spawn band, so a
/// row always has an entity — and a rendered texture — before it can
/// become visible.
pub const SPAWN_MARGIN_SCREENS: f32 = 2.0;
pub const DESPAWN_MARGIN_SCREENS: f32 = 4.0;

/// A band plan: which block rows need entities spawned, and which existing
/// entities should be despawned to reclaim their render resources.
#[derive(Debug, Default, PartialEq)]
pub struct BandPlan {
    pub to_spawn: Vec<BlockId>,
    pub to_despawn: Vec<BlockId>,
}

/// Decide entity existence for every block row against the viewport band.
///
/// `viewport_height <= 0` (first frames, before the container is measured)
/// falls back to one nominal screen so a fresh conversation still spawns
/// its initial window instead of nothing.
///
/// `exempt` (the focused block) is never despawned: focus survives
/// scrolling away, and the `FocusedBlockCell` marker rides the entity.
pub fn plan_block_band(
    rows: &[GeomRow],
    has_entity: impl Fn(&BlockId) -> bool,
    viewport_top: f32,
    viewport_height: f32,
    exempt: Option<BlockId>,
) -> BandPlan {
    let vh = if viewport_height > 0.0 {
        viewport_height
    } else {
        600.0
    };
    let spawn_top = viewport_top - SPAWN_MARGIN_SCREENS * vh;
    let spawn_bottom = viewport_top + vh + SPAWN_MARGIN_SCREENS * vh;
    let keep_top = viewport_top - DESPAWN_MARGIN_SCREENS * vh;
    let keep_bottom = viewport_top + vh + DESPAWN_MARGIN_SCREENS * vh;

    let mut plan = BandPlan::default();
    for row in rows {
        let RowKey::Block(id) = row.key else {
            continue;
        };
        let bottom_edge = row.y_offset + row.height + row.margin_bottom;
        let in_spawn = bottom_edge >= spawn_top && row.y_offset <= spawn_bottom;
        let in_keep = bottom_edge >= keep_top && row.y_offset <= keep_bottom;
        let exists = has_entity(&id);

        if in_spawn && !exists {
            plan.to_spawn.push(id);
        } else if !in_keep && exists && Some(id) != exempt {
            plan.to_despawn.push(id);
        }
    }
    plan
}

/// Header-row variant of [`plan_block_band`]: same bands, but spawn entries
/// carry the role the header renders. Headers have no focus exemption.
pub fn plan_header_band(
    rows: &[GeomRow],
    has_entity: impl Fn(&BlockId) -> bool,
    viewport_top: f32,
    viewport_height: f32,
) -> (Vec<(Role, BlockId)>, Vec<BlockId>) {
    let vh = if viewport_height > 0.0 {
        viewport_height
    } else {
        600.0
    };
    let spawn_top = viewport_top - SPAWN_MARGIN_SCREENS * vh;
    let spawn_bottom = viewport_top + vh + SPAWN_MARGIN_SCREENS * vh;
    let keep_top = viewport_top - DESPAWN_MARGIN_SCREENS * vh;
    let keep_bottom = viewport_top + vh + DESPAWN_MARGIN_SCREENS * vh;

    let mut to_spawn = Vec::new();
    let mut to_despawn = Vec::new();
    for row in rows {
        let RowKey::Header(id) = row.key else {
            continue;
        };
        let bottom_edge = row.y_offset + row.height + row.margin_bottom;
        let in_spawn = bottom_edge >= spawn_top && row.y_offset <= spawn_bottom;
        let in_keep = bottom_edge >= keep_top && row.y_offset <= keep_bottom;
        let exists = has_entity(&id);

        if in_spawn && !exists {
            to_spawn.push((row.role, id));
        } else if !in_keep && exists {
            to_despawn.push(id);
        }
    }
    (to_spawn, to_despawn)
}

/// Build a [`RowSeed`] from a block snapshot — the only place block content
/// is read for geometry, and it runs once per new row.
fn row_seed(snapshot: kaijutsu_types::BlockSnapshot) -> RowSeed {
    RowSeed {
        text_len: snapshot.content.len(),
        newline_count: snapshot.content.matches('\n').count(),
        role: snapshot.role,
        kind: snapshot.kind,
        collapsed: snapshot.collapsed,
        parent_id: snapshot.parent_id,
    }
}

/// Maintain [`ConversationGeometry`] for the main conversation (Update,
/// before `spawn_block_cells`).
///
/// Reconcile is gated on the document version; block content is only read
/// (one snapshot at a time) for rows the geometry has never seen. Wrap-column
/// changes (window resize) re-estimate never-measured rows from cached text
/// stats without touching the store at all.
pub fn sync_conversation_geometry(
    mut commands: Commands,
    entities: Res<crate::cell::EditorEntities>,
    main_cells: Query<&crate::cell::CellEditor, With<crate::cell::MainCell>>,
    mut geometries: Query<&mut ConversationGeometry>,
    computed_nodes: Query<&ComputedNode>,
    text_metrics: Res<crate::text::TextMetrics>,
    theme: Res<crate::ui::theme::Theme>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };
    let Ok(mut geom) = geometries.get_mut(main_ent) else {
        // First sight of this main cell — attach the model, reconcile next
        // frame once the insert has applied.
        commands
            .entity(main_ent)
            .insert(ConversationGeometry::default());
        return;
    };

    // Wrap columns from the conversation container's content box — the same
    // ComputedNode source `visible_height` uses (view/scroll.rs), keeping
    // every geometry unit consistent with scroll offsets.
    let char_w = (text_metrics.cell_char_width + text_metrics.letter_spacing).max(1.0);
    // ComputedNode is physical px; char_w/line_height above are logical.
    let width = entities
        .conversation_container
        .and_then(|e| computed_nodes.get(e).ok())
        .map(|c| crate::view::ui_rtt::logical_content_size(c).x)
        .filter(|w| *w > 1.0);
    let cols = width
        .map(|w| (w / char_w).floor().max(20.0) as usize)
        .unwrap_or(if geom.cols > 0 { geom.cols } else { 100 });

    let params = EstimateParams {
        cols,
        line_height: text_metrics.cell_line_height.max(1.0),
        block_spacing: theme.block_spacing,
        role_header_height: theme.role_header_height,
        role_header_spacing: theme.role_header_spacing,
    };

    // Reconcile on version change OR id-sequence change: a context switch
    // can replace the store wholesale at a coincidentally-equal version
    // (welcome → hydrated context), which stalls a version-only gate on
    // stale rows and feeds the entity band a dead id forever.
    //
    // The id-sequence check is the expensive one — `block_ids()` allocates a
    // Vec every call — and the swap it guards against is not something a
    // version comparison can detect. The store's instance generation is: it
    // is minted per `RenderBlockStore`, and the swap site (`view/sync.rs`'s
    // `editor.store = store`) can't forget to move it. So gate the walk on
    // "version moved OR the store under us was replaced" — two u64 compares
    // on the frame where neither happened — and keep `ids_match` as the
    // decision it always was for the swap case, since a swap that reproduced
    // the same document order needs no reconcile.
    let doc_version = editor.version();
    let store_generation = editor.store_generation();
    let version_changed = doc_version != geom.last_doc_version;
    let store_swapped = store_generation != geom.last_store_generation;
    if version_changed || store_swapped {
        let ids = editor.block_ids();
        if version_changed || !geom.ids_match(&ids) {
            geom.reconcile(
                &ids,
                |id| editor.block_snapshot(id).map(row_seed),
                &params,
                doc_version,
            );
        }
        geom.last_store_generation = store_generation;
    } else if cols.abs_diff(geom.cols) > 2 {
        // Resize changed the wrap width materially — refresh estimates from
        // cached text stats (no store access).
        geom.reestimate_unmeasured(&params);
    }

    geom.recompute_offsets();
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{ContextId, PrincipalId};

    fn bid(seq: u64) -> BlockId {
        // Fixed context/principal so ids are stable within a test.
        use std::sync::OnceLock;
        static IDS: OnceLock<(ContextId, PrincipalId)> = OnceLock::new();
        let (ctx, prin) = IDS.get_or_init(|| (ContextId::new(), PrincipalId::new()));
        BlockId::new(*ctx, *prin, seq)
    }

    fn text_seed(role: Role, text_len: usize, newlines: usize) -> RowSeed {
        RowSeed {
            text_len,
            newline_count: newlines,
            role,
            kind: BlockKind::Text,
            collapsed: false,
            parent_id: None,
        }
    }

    fn tool_seed(kind: BlockKind) -> RowSeed {
        RowSeed {
            text_len: 50,
            newline_count: 0,
            role: Role::Tool,
            kind,
            collapsed: false,
            parent_id: None,
        }
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

    // ---- estimate_block_height ------------------------------------------

    #[test]
    fn estimate_single_short_line_is_one_line_height() {
        assert_eq!(
            estimate_block_height(40, 0, false, BlockKind::Text, &params()),
            30.0
        );
    }

    #[test]
    fn estimate_hard_lines_dominate_short_text() {
        // 5 newlines = 6 hard lines of short text.
        assert_eq!(
            estimate_block_height(60, 5, false, BlockKind::Text, &params()),
            6.0 * 30.0
        );
    }

    #[test]
    fn estimate_wrapping_dominates_one_long_line() {
        // 350 chars at 100 cols = 4 wrapped rows.
        assert_eq!(
            estimate_block_height(350, 0, false, BlockKind::Text, &params()),
            4.0 * 30.0
        );
    }

    #[test]
    fn estimate_collapsed_is_one_line() {
        assert_eq!(
            estimate_block_height(10_000, 99, true, BlockKind::Thinking, &params()),
            30.0
        );
    }

    #[test]
    fn estimate_collapsed_error_uses_stub_line_budget() {
        // A collapsed Error stub renders more than one line (provenance,
        // summary, detail preview, hint) — sizing it like a one-line
        // `Thinking [collapsed]` stub would under-reserve space.
        assert_eq!(
            estimate_block_height(10_000, 99, true, BlockKind::Error, &params()),
            crate::view::format::ERROR_STUB_MAX_LINES as f32 * 30.0
        );
    }

    #[test]
    fn estimate_empty_text_is_one_line_minimum() {
        assert_eq!(
            estimate_block_height(0, 0, false, BlockKind::Text, &params()),
            30.0
        );
    }

    #[test]
    fn estimate_degenerate_cols_clamped() {
        let p = EstimateParams { cols: 0, ..params() };
        // Clamped to 20 cols: 100 chars → 5 rows.
        assert_eq!(
            estimate_block_height(100, 0, false, BlockKind::Text, &p),
            5.0 * 30.0
        );
    }

    // ---- reconcile: structure -------------------------------------------

    #[test]
    fn reconcile_seeds_blocks_and_role_headers() {
        let mut g = ConversationGeometry::default();
        let ids = vec![bid(1), bid(2), bid(3)];
        let changed = g.reconcile(
            &ids,
            |id| {
                Some(if *id == bid(3) {
                    text_seed(Role::Model, 40, 0)
                } else {
                    text_seed(Role::User, 40, 0)
                })
            },
            &params(),
            7,
        );
        assert!(changed);
        // user, user, model → header before block 1, header before block 3.
        let keys: Vec<RowKey> = g.rows().iter().map(|r| r.key).collect();
        assert_eq!(
            keys,
            vec![
                RowKey::Header(bid(1)),
                RowKey::Block(bid(1)),
                RowKey::Block(bid(2)),
                RowKey::Header(bid(3)),
                RowKey::Block(bid(3)),
            ]
        );
        assert!(g.rows().iter().all(|r| r.measured_version == 0));
        assert!(g.rows().iter().all(|r| r.created_at_version == 7));
    }

    #[test]
    fn reconcile_tool_blocks_take_no_header_and_do_not_break_runs() {
        let mut g = ConversationGeometry::default();
        let ids = vec![bid(1), bid(2), bid(3)];
        g.reconcile(
            &ids,
            |id| {
                Some(if *id == bid(2) {
                    tool_seed(BlockKind::ToolCall)
                } else {
                    text_seed(Role::Model, 40, 0)
                })
            },
            &params(),
            1,
        );
        // model, tool, model → ONE header, before block 1 only.
        let headers: Vec<RowKey> = g
            .rows()
            .iter()
            .filter(|r| matches!(r.key, RowKey::Header(_)))
            .map(|r| r.key)
            .collect();
        assert_eq!(headers, vec![RowKey::Header(bid(1))]);
    }

    #[test]
    fn reconcile_reuses_measured_heights_for_surviving_rows() {
        let mut g = ConversationGeometry::default();
        let ids = vec![bid(1), bid(2)];
        g.reconcile(&ids, |_| Some(text_seed(Role::User, 40, 0)), &params(), 1);
        g.measure(RowKey::Block(bid(1)), 123.0, 12.0, 5);
        g.recompute_offsets();

        // Append a block; existing measured height must survive.
        let ids2 = vec![bid(1), bid(2), bid(3)];
        let changed =
            g.reconcile(&ids2, |_| Some(text_seed(Role::User, 40, 0)), &params(), 6);
        assert!(changed);
        let row = g.block_row(&bid(1)).unwrap();
        assert_eq!(row.height, 123.0);
        assert_eq!(row.measured_version, 5);
        // Creation stamp also survives.
        assert_eq!(row.created_at_version, 1);
        // The new block is an estimate stamped with the new version.
        assert_eq!(g.block_row(&bid(3)).unwrap().created_at_version, 6);
    }

    #[test]
    fn reconcile_unchanged_ids_reports_no_structure_change() {
        let mut g = ConversationGeometry::default();
        let ids = vec![bid(1), bid(2)];
        g.reconcile(&ids, |_| Some(text_seed(Role::User, 40, 0)), &params(), 1);
        let changed = g.reconcile(
            &ids,
            |_| panic!("seed_fn must not be called for known rows"),
            &params(),
            2,
        );
        assert!(!changed);
    }

    #[test]
    fn reconcile_removed_block_drops_row_and_reports_change() {
        let mut g = ConversationGeometry::default();
        g.reconcile(
            &[bid(1), bid(2)],
            |_| Some(text_seed(Role::User, 40, 0)),
            &params(),
            1,
        );
        let changed = g.reconcile(&[bid(2)], |_| None, &params(), 2);
        assert!(changed);
        assert!(g.block_row(&bid(1)).is_none());
        assert!(g.block_row(&bid(2)).is_some());
    }

    #[test]
    fn reconcile_none_seed_skips_block_without_panicking() {
        let mut g = ConversationGeometry::default();
        let changed = g.reconcile(
            &[bid(1), bid(2)],
            |id| (*id == bid(2)).then(|| text_seed(Role::User, 40, 0)),
            &params(),
            1,
        );
        assert!(changed);
        assert!(g.block_row(&bid(1)).is_none());
        assert!(g.block_row(&bid(2)).is_some());
    }

    #[test]
    fn reconcile_toolcall_before_toolresult_gets_zero_margin() {
        let mut g = ConversationGeometry::default();
        g.reconcile(
            &[bid(1), bid(2), bid(3)],
            |id| {
                Some(if *id == bid(1) {
                    tool_seed(BlockKind::ToolCall)
                } else if *id == bid(2) {
                    tool_seed(BlockKind::ToolResult)
                } else {
                    text_seed(Role::Model, 40, 0)
                })
            },
            &params(),
            1,
        );
        assert_eq!(g.block_row(&bid(1)).unwrap().margin_bottom, 0.0);
        assert_eq!(g.block_row(&bid(2)).unwrap().margin_bottom, 12.0);
    }

    // ---- measure / offsets ----------------------------------------------

    #[test]
    fn recompute_offsets_prefix_sums_and_content_height() {
        let mut g = ConversationGeometry::default();
        g.reconcile(
            &[bid(1), bid(2)],
            |_| Some(text_seed(Role::User, 40, 0)),
            &params(),
            1,
        );
        assert!(g.recompute_offsets());
        // header(20+4), block(30+12), block(30+12)
        let rows = g.rows();
        assert_eq!(rows[0].y_offset, 0.0);
        assert_eq!(rows[1].y_offset, 24.0);
        assert_eq!(rows[2].y_offset, 66.0);
        assert_eq!(g.content_height, 108.0);
        // Second call is a no-op.
        assert!(!g.recompute_offsets());
    }

    #[test]
    fn measure_replaces_estimate_and_returns_delta() {
        let mut g = ConversationGeometry::default();
        g.reconcile(
            &[bid(1), bid(2)],
            |_| Some(text_seed(Role::User, 40, 0)),
            &params(),
            1,
        );
        g.recompute_offsets();
        let before = g.content_height;

        let delta = g.measure(RowKey::Block(bid(1)), 90.0, 12.0, 3);
        assert_eq!(delta, 60.0);
        g.recompute_offsets();
        assert_eq!(g.content_height, before + 60.0);
        let row = g.block_row(&bid(1)).unwrap();
        assert_eq!(row.height, 90.0);
        assert_eq!(row.measured_version, 3);
    }

    #[test]
    fn measure_version_zero_still_marks_measured() {
        // A block measured while the doc is at version 0 must not stay
        // classified as an estimate (measured_version 0 is the sentinel).
        let mut g = ConversationGeometry::default();
        g.reconcile(&[bid(1)], |_| Some(text_seed(Role::User, 40, 0)), &params(), 0);
        g.measure(RowKey::Block(bid(1)), 30.0, 12.0, 0);
        assert_ne!(g.block_row(&bid(1)).unwrap().measured_version, 0);
    }

    #[test]
    fn measure_header_row_via_header_key() {
        let mut g = ConversationGeometry::default();
        g.reconcile(&[bid(1)], |_| Some(text_seed(Role::User, 40, 0)), &params(), 1);
        let delta = g.measure(RowKey::Header(bid(1)), 26.0, 4.0, 2);
        assert_eq!(delta, 6.0);
        assert_eq!(g.header_row(&bid(1)).unwrap().height, 26.0);
    }

    #[test]
    fn measure_unknown_row_is_a_noop() {
        let mut g = ConversationGeometry::default();
        g.reconcile(&[bid(1)], |_| Some(text_seed(Role::User, 40, 0)), &params(), 1);
        assert_eq!(g.measure(RowKey::Block(bid(99)), 500.0, 0.0, 2), 0.0);
        assert_eq!(g.measure(RowKey::Header(bid(99)), 500.0, 0.0, 2), 0.0);
    }

    #[test]
    fn reestimate_unmeasured_respects_measured_rows() {
        let mut g = ConversationGeometry::default();
        g.reconcile(
            &[bid(1), bid(2)],
            |_| Some(text_seed(Role::User, 350, 0)), // 4 rows at 100 cols
            &params(),
            1,
        );
        g.measure(RowKey::Block(bid(1)), 77.0, 12.0, 2);

        // Narrower: 350 chars at 50 cols = 7 rows.
        let narrow = EstimateParams { cols: 50, ..params() };
        g.reestimate_unmeasured(&narrow);
        assert_eq!(g.block_row(&bid(1)).unwrap().height, 77.0); // measured: untouched
        assert_eq!(g.block_row(&bid(2)).unwrap().height, 7.0 * 30.0); // re-estimated
        assert_eq!(g.cols, 50);
    }

    /// Regression (live-found on zorak): a context switch can replace the
    /// editor's store wholesale while the document version coincidentally
    /// matches the old one. `ids_match` is the second gate that catches it —
    /// without it, stale rows feed the entity band a dead id and the
    /// spawn/despawn loop never converges.
    #[test]
    fn ids_match_detects_store_swap_at_equal_version() {
        let mut g = ConversationGeometry::default();
        g.reconcile(&[bid(1)], |_| Some(text_seed(Role::User, 40, 0)), &params(), 1);
        assert!(g.ids_match(&[bid(1)]));
        // Same version, different ids — the swap signature.
        assert!(!g.ids_match(&[bid(2)]));

        let changed = g.reconcile(&[bid(2)], |_| Some(text_seed(Role::User, 40, 0)), &params(), 1);
        assert!(changed);
        assert!(g.block_row(&bid(1)).is_none());
        assert!(g.block_row(&bid(2)).is_some());
        assert!(g.ids_match(&[bid(2)]));
    }

    // ---- sync_conversation_geometry: the reconcile gate --------------------

    /// Minimal headless `App` running just `sync_conversation_geometry` —
    /// enough resources for it to reach the gate, no rendering plugins.
    fn gate_app() -> App {
        let mut app = App::new();
        app.init_resource::<crate::cell::EditorEntities>();
        app.init_resource::<crate::text::TextMetrics>();
        app.init_resource::<crate::ui::theme::Theme>();
        app.add_systems(Update, sync_conversation_geometry);
        app
    }

    /// A fresh store holding one block, at version 1 — the same version a
    /// one-block store reaches on its own, which is the whole point.
    fn one_block_store() -> (crate::view::render_store::RenderBlockStore, BlockId) {
        use kaijutsu_types::{ContentType, Status};
        let (ctx, prin) = (ContextId::new(), PrincipalId::new());
        let mut store = crate::view::render_store::RenderBlockStore::new(ctx, prin);
        let id = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "hello",
                Status::Done,
                ContentType::Plain,
            )
            .expect("insert_block");
        (store, id)
    }

    /// Regression (live-found on zorak), at the system level: a context
    /// switch replaces `CellEditor.store` wholesale and the new store can sit
    /// at the same document version the old one was at (welcome → hydrated
    /// context). The reconcile gate must still fire, or stale rows feed the
    /// entity band a dead id and the spawn/despawn loop never converges.
    ///
    /// This is the test the per-frame `block_ids()` walk existed to satisfy;
    /// it now rides `CellEditor::store_generation`, so it fails loudly if
    /// that signal is dropped or the gate regresses to version-only.
    #[test]
    fn store_swap_at_equal_version_still_reconciles() {
        let mut app = gate_app();
        let (store_a, id_a) = one_block_store();
        let mut editor = crate::cell::CellEditor::new();
        editor.store = store_a;
        let version = editor.version();

        let main_ent = app.world_mut().spawn((editor, crate::cell::MainCell)).id();
        app.world_mut()
            .resource_mut::<crate::cell::EditorEntities>()
            .main_cell = Some(main_ent);

        // First run inserts the component, second reconciles against it.
        app.update();
        app.update();
        assert!(
            app.world()
                .get::<ConversationGeometry>(main_ent)
                .unwrap()
                .block_row(&id_a)
                .is_some(),
            "test premise: the first document reconciled"
        );

        let (store_b, id_b) = one_block_store();
        assert_eq!(
            store_b.version(),
            version,
            "test premise: the swapped-in store sits at the same version"
        );
        app.world_mut()
            .get_mut::<crate::cell::CellEditor>(main_ent)
            .unwrap()
            .store = store_b;

        app.update();
        let geom = app.world().get::<ConversationGeometry>(main_ent).unwrap();
        assert!(
            geom.block_row(&id_b).is_some(),
            "the swapped-in document must reconcile despite the equal version"
        );
        assert!(
            geom.block_row(&id_a).is_none(),
            "the replaced document's rows must be gone, not left to feed the band a dead id"
        );
    }

    /// The other half of the gate: an idle frame — same store, same version —
    /// must not reconcile. `reconcile` is what stamps `last_doc_version` and
    /// rebuilds the row vector; observing the epoch is the cheap proxy for
    /// "nothing was redone".
    #[test]
    fn idle_frames_do_not_reconcile() {
        let mut app = gate_app();
        let (store, id) = one_block_store();
        let mut editor = crate::cell::CellEditor::new();
        editor.store = store;

        let main_ent = app.world_mut().spawn((editor, crate::cell::MainCell)).id();
        app.world_mut()
            .resource_mut::<crate::cell::EditorEntities>()
            .main_cell = Some(main_ent);

        app.update();
        app.update();
        let settled = app
            .world()
            .get::<ConversationGeometry>(main_ent)
            .unwrap()
            .epoch();
        assert!(
            app.world()
                .get::<ConversationGeometry>(main_ent)
                .unwrap()
                .block_row(&id)
                .is_some(),
            "test premise: the document reconciled"
        );

        for _ in 0..5 {
            app.update();
        }
        assert_eq!(
            app.world()
                .get::<ConversationGeometry>(main_ent)
                .unwrap()
                .epoch(),
            settled,
            "an idle frame must not reconcile or move offsets"
        );
    }

    // ---- visible_rows -----------------------------------------------------

    /// Same-role blocks, so the row strip is: header (20 + 4 margin) at y=0,
    /// then `n` blocks of 42 pitch (30 + 12) starting at y=24. Row `i >= 1`
    /// spans `[24 + (i-1) * 42, 24 + i * 42)`.
    fn strip(n: u64) -> ConversationGeometry {
        let mut g = ConversationGeometry::default();
        let ids: Vec<BlockId> = (1..=n).map(bid).collect();
        g.reconcile(&ids, |_| Some(text_seed(Role::User, 40, 0)), &params(), 1);
        g.recompute_offsets();
        g
    }

    #[test]
    fn visible_rows_empty_geometry_is_empty_at_zero() {
        let g = ConversationGeometry::default();
        assert_eq!(g.visible_rows(0.0, 300.0, 0.0), 0..0);
    }

    #[test]
    fn visible_rows_selects_exactly_the_rows_the_band_covers() {
        let g = strip(4);
        // Band [66, 108) is precisely row 2's extent.
        let range = g.visible_rows(66.0, 42.0, 0.0);
        assert_eq!(range, 2..3);
        assert_eq!(g.rows()[range.start].key, RowKey::Block(bid(2)));
    }

    #[test]
    fn visible_rows_excludes_rows_that_only_touch_the_band_edges() {
        let g = strip(4);
        // Row 1 ends exactly at 66 (the band top) and row 3 starts exactly at
        // 108 (the band bottom): both contribute zero area, so a half-open
        // band must leave them out rather than spawn work for a sliver.
        assert_eq!(g.rows()[1].y_offset + g.rows()[1].height + g.rows()[1].margin_bottom, 66.0);
        assert_eq!(g.rows()[3].y_offset, 108.0);
        assert_eq!(g.visible_rows(66.0, 42.0, 0.0), 2..3);
    }

    #[test]
    fn visible_rows_slack_pulls_in_the_neighbours() {
        let g = strip(4);
        // One unit of slack on each side reaches past both touching edges.
        assert_eq!(g.visible_rows(66.0, 42.0, 1.0), 1..4);
    }

    #[test]
    fn visible_rows_band_above_the_document_is_empty_at_the_start() {
        let g = strip(4);
        assert_eq!(g.visible_rows(-1000.0, 100.0, 0.0), 0..0);
    }

    #[test]
    fn visible_rows_band_past_the_end_is_empty_at_the_end() {
        let g = strip(4);
        let len = g.rows().len();
        assert_eq!(g.visible_rows(g.content_height + 10.0, 100.0, 0.0), len..len);
    }

    #[test]
    fn visible_rows_negative_top_clamps_to_the_first_row() {
        let g = strip(4);
        // Band [-50, 50): the header and the first block, nothing above.
        assert_eq!(g.visible_rows(-50.0, 100.0, 0.0), 0..2);
    }

    #[test]
    fn visible_rows_single_row_taller_than_the_band() {
        let mut g = ConversationGeometry::default();
        g.reconcile(&[bid(1)], |_| Some(text_seed(Role::User, 40, 0)), &params(), 1);
        g.measure(RowKey::Block(bid(1)), 1000.0, 12.0, 2);
        g.recompute_offsets();
        // The band sits entirely inside row 1 — it must still be returned,
        // which a "row top inside the band" test would miss.
        let range = g.visible_rows(300.0, 100.0, 0.0);
        assert_eq!(range, 1..2);
        assert_eq!(g.rows()[1].key, RowKey::Block(bid(1)));
    }

    #[test]
    fn visible_rows_whole_document_band_covers_every_row() {
        let g = strip(10);
        assert_eq!(g.visible_rows(0.0, g.content_height, 0.0), 0..g.rows().len());
    }

    // ---- epoch ------------------------------------------------------------

    #[test]
    fn epoch_does_not_move_when_recompute_has_nothing_to_do() {
        let mut g = strip(3);
        let before = g.epoch();
        assert!(!g.recompute_offsets(), "test premise: not dirty");
        assert_eq!(g.epoch(), before);
    }

    #[test]
    fn epoch_bumps_when_a_measure_shifts_offsets() {
        let mut g = strip(3);
        let before = g.epoch();
        g.measure(RowKey::Block(bid(1)), 90.0, 12.0, 2);
        assert!(g.recompute_offsets());
        assert!(
            g.epoch() > before,
            "a measurement that moved every row below it must bump the epoch"
        );
    }

    #[test]
    fn epoch_is_stable_across_a_version_bump_that_changes_no_row() {
        let mut g = strip(3);
        let before = g.epoch();
        let ids: Vec<BlockId> = (1..=3).map(bid).collect();
        // The reconcile the version gate fires on every document bump: same
        // ids, same heights. Nothing about the layout moved, so the GPU
        // window has no reason to rebuild.
        assert!(!g.reconcile(
            &ids,
            |_| panic!("seed_fn must not be called for known rows"),
            &params(),
            2,
        ));
        assert!(g.recompute_offsets());
        assert_eq!(
            g.epoch(),
            before,
            "a version bump that moved no row must not bump the geometry epoch"
        );
    }

    #[test]
    fn epoch_bumps_on_a_structural_reconcile_before_offsets_are_recomputed() {
        let mut g = strip(3);
        let before = g.epoch();
        // Replace the tail block with a different id of identical height: the
        // prefix sums and content height come out byte-identical, so only the
        // structural bump can tell a consumer the row → block mapping moved.
        let changed = g.reconcile(
            &[bid(1), bid(2), bid(4)],
            |_| Some(text_seed(Role::User, 40, 0)),
            &params(),
            2,
        );
        assert!(changed);
        assert!(
            g.epoch() > before,
            "a structural reconcile must bump the epoch on its own — offsets \
             alone cannot report an equal-height row replacement"
        );
    }

    // ---- band planning ----------------------------------------------------

    /// 100 one-line user blocks (42px pitch after the 24px header): plan
    /// against a 300px viewport at the top of the document.
    fn banded_geometry() -> ConversationGeometry {
        let mut g = ConversationGeometry::default();
        let ids: Vec<BlockId> = (1..=100).map(bid).collect();
        g.reconcile(&ids, |_| Some(text_seed(Role::User, 40, 0)), &params(), 1);
        g.recompute_offsets();
        g
    }

    #[test]
    fn plan_spawns_only_the_spawn_band_when_nothing_exists() {
        let g = banded_geometry();
        let plan = plan_block_band(g.rows(), |_| false, 0.0, 300.0, None);
        // Spawn band = [-600, 1500]: blocks 0..~35 of 100.
        assert!(!plan.to_spawn.is_empty());
        assert!(
            plan.to_spawn.len() < 45,
            "spawn band must not cover the whole document: {} of 100",
            plan.to_spawn.len(),
        );
        assert!(plan.to_despawn.is_empty());
        // Must include the very first block (viewport at top).
        assert_eq!(plan.to_spawn[0], bid(1));
    }

    #[test]
    fn plan_despawns_only_beyond_the_keep_band() {
        let g = banded_geometry();
        // Everything exists; viewport at the top. Keep band = [-1200, 2700]:
        // blocks past y=2700 (block ~64 on) despawn, the hysteresis zone
        // between spawn and keep bands (blocks ~36..=64) stays untouched.
        let plan = plan_block_band(g.rows(), |_| true, 0.0, 300.0, None);
        assert!(plan.to_spawn.is_empty());
        assert!(!plan.to_despawn.is_empty());
        let first_despawned_y = g
            .block_row(plan.to_despawn.first().unwrap())
            .unwrap()
            .y_offset;
        assert!(
            first_despawned_y > 300.0 + DESPAWN_MARGIN_SCREENS * 300.0,
            "despawn must start beyond the keep band, got y={first_despawned_y}",
        );
    }

    #[test]
    fn plan_exempts_the_focused_block_from_despawn() {
        let g = banded_geometry();
        let focused = bid(100); // far outside the keep band
        let plan = plan_block_band(g.rows(), |_| true, 0.0, 300.0, Some(focused));
        assert!(!plan.to_despawn.contains(&focused));
        // Its neighbors still despawn.
        assert!(plan.to_despawn.contains(&bid(99)));
    }

    #[test]
    fn plan_zero_viewport_height_falls_back_to_a_nominal_screen() {
        let g = banded_geometry();
        let plan = plan_block_band(g.rows(), |_| false, 0.0, 0.0, None);
        // With the 600px fallback the initial window still spawns.
        assert!(!plan.to_spawn.is_empty());
        assert!(plan.to_spawn.len() < 100);
    }

    #[test]
    fn plan_header_band_spawns_and_despawns_headers() {
        // Alternate roles so every block starts a role run (header pitch
        // matches block pitch).
        let mut g = ConversationGeometry::default();
        let ids: Vec<BlockId> = (1..=100).map(bid).collect();
        g.reconcile(
            &ids,
            |id| {
                Some(text_seed(
                    if id.seq % 2 == 0 { Role::Model } else { Role::User },
                    40,
                    0,
                ))
            },
            &params(),
            1,
        );
        g.recompute_offsets();

        let (to_spawn, to_despawn) = plan_header_band(g.rows(), |_| false, 0.0, 300.0);
        assert!(!to_spawn.is_empty());
        assert!(to_spawn.len() < 60, "got {} headers", to_spawn.len());
        assert!(to_despawn.is_empty());
        assert_eq!(to_spawn[0], (Role::User, bid(1)));

        let (to_spawn, to_despawn) = plan_header_band(g.rows(), |_| true, 0.0, 300.0);
        assert!(to_spawn.is_empty());
        assert!(!to_despawn.is_empty());
    }

    #[test]
    fn header_row_lookup_only_when_run_starts_there() {
        let mut g = ConversationGeometry::default();
        g.reconcile(
            &[bid(1), bid(2)],
            |_| Some(text_seed(Role::User, 40, 0)),
            &params(),
            1,
        );
        assert!(g.header_row(&bid(1)).is_some());
        assert!(g.header_row(&bid(2)).is_none());
    }
}
