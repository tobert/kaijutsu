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

use kaijutsu_crdt::{BlockId, BlockKind, Role};

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
    /// Indent level (parent_id nesting), mirrored to `BlockCellLayout`.
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
    pub indented: bool,
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
pub fn estimate_block_height(
    text_len: usize,
    newline_count: usize,
    collapsed: bool,
    params: &EstimateParams,
) -> f32 {
    if collapsed {
        return params.line_height;
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
    /// Cols used for the current estimates (re-estimation gate on resize).
    pub cols: usize,
    /// Prefix sums need recomputation.
    dirty: bool,
}

impl ConversationGeometry {
    pub fn rows(&self) -> &[GeomRow] {
        &self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Look up the block row for `id`.
    pub fn block_row(&self, id: &BlockId) -> Option<&GeomRow> {
        self.block_index.get(id).map(|&i| &self.rows[i])
    }

    /// Look up the header row preceding block `id`, if that block starts a
    /// role run.
    // Consumed by band spawn/despawn (Slice C); drop the allow there.
    #[allow(dead_code)]
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
                    let mut row = old_rows[i].clone();
                    // y_offset recomputed below; everything else carries.
                    row.y_offset = 0.0;
                    row
                }
                None => {
                    let Some(seed) = seed_fn(id) else {
                        structure_changed = true;
                        continue;
                    };
                    structure_changed = true;
                    GeomRow {
                        key: RowKey::Block(*id),
                        height: estimate_block_height(
                            seed.text_len,
                            seed.newline_count,
                            seed.collapsed,
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
                        indent_level: if seed.indented { 1 } else { 0 },
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
        self.last_doc_version = doc_version;
        self.cols = params.cols;
        self.dirty = true;
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
    pub fn recompute_offsets(&mut self) -> bool {
        if !self.dirty {
            return false;
        }
        let mut y = 0.0_f32;
        for row in &mut self.rows {
            row.y_offset = y;
            y += row.height + row.margin_bottom;
        }
        self.content_height = y;
        self.dirty = false;
        true
    }

    /// Index range of rows intersecting `[top, bottom]` (both in content
    /// coordinates). Requires offsets to be current. Returns an empty range
    /// when nothing intersects.
    // Consumed by band spawn/despawn (Slice C); drop the allow there.
    #[allow(dead_code)]
    pub fn rows_in_range(&self, top: f32, bottom: f32) -> std::ops::Range<usize> {
        if self.rows.is_empty() || bottom < top {
            return 0..0;
        }
        // First row whose bottom edge reaches `top`.
        let start = self
            .rows
            .partition_point(|r| r.y_offset + r.height + r.margin_bottom < top);
        // First row whose top edge is past `bottom`.
        let end = self.rows.partition_point(|r| r.y_offset <= bottom);
        start..end.max(start)
    }
}

/// Build a [`RowSeed`] from a block snapshot — the only place block content
/// is read for geometry, and it runs once per new row.
fn row_seed(snapshot: kaijutsu_crdt::BlockSnapshot) -> RowSeed {
    let is_tool = matches!(
        snapshot.kind,
        BlockKind::ToolCall | BlockKind::ToolResult
    );
    RowSeed {
        text_len: snapshot.content.len(),
        newline_count: snapshot.content.matches('\n').count(),
        role: snapshot.role,
        kind: snapshot.kind,
        collapsed: snapshot.collapsed,
        // Mirror `layout_block_cells`: tool blocks are flush, children indent.
        indented: !is_tool && snapshot.parent_id.is_some(),
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
    let width = entities
        .conversation_container
        .and_then(|e| computed_nodes.get(e).ok())
        .map(|c| c.content_box().width())
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

    let doc_version = editor.version();
    if doc_version != geom.last_doc_version || (geom.is_empty() && editor.has_blocks()) {
        let ids = editor.block_ids();
        geom.reconcile(
            &ids,
            |id| editor.block_snapshot(id).map(row_seed),
            &params,
            doc_version,
        );
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
    use kaijutsu_crdt::{ContextId, PrincipalId};

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
            indented: false,
        }
    }

    fn tool_seed(kind: BlockKind) -> RowSeed {
        RowSeed {
            text_len: 50,
            newline_count: 0,
            role: Role::Tool,
            kind,
            collapsed: false,
            indented: false,
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
        assert_eq!(estimate_block_height(40, 0, false, &params()), 30.0);
    }

    #[test]
    fn estimate_hard_lines_dominate_short_text() {
        // 5 newlines = 6 hard lines of short text.
        assert_eq!(estimate_block_height(60, 5, false, &params()), 6.0 * 30.0);
    }

    #[test]
    fn estimate_wrapping_dominates_one_long_line() {
        // 350 chars at 100 cols = 4 wrapped rows.
        assert_eq!(estimate_block_height(350, 0, false, &params()), 4.0 * 30.0);
    }

    #[test]
    fn estimate_collapsed_is_one_line() {
        assert_eq!(estimate_block_height(10_000, 99, true, &params()), 30.0);
    }

    #[test]
    fn estimate_empty_text_is_one_line_minimum() {
        assert_eq!(estimate_block_height(0, 0, false, &params()), 30.0);
    }

    #[test]
    fn estimate_degenerate_cols_clamped() {
        let p = EstimateParams { cols: 0, ..params() };
        // Clamped to 20 cols: 100 chars → 5 rows.
        assert_eq!(estimate_block_height(100, 0, false, &p), 5.0 * 30.0);
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

    // ---- windowing --------------------------------------------------------

    #[test]
    fn rows_in_range_selects_intersecting_rows() {
        let mut g = ConversationGeometry::default();
        let ids: Vec<BlockId> = (1..=10).map(bid).collect();
        g.reconcile(&ids, |_| Some(text_seed(Role::User, 40, 0)), &params(), 1);
        g.recompute_offsets();
        // Layout: header(24) then 10 blocks of 42 each → content 444.
        let range = g.rows_in_range(100.0, 200.0);
        // Rows in [100, 200]: blocks at y=66..108, 108..150, 150..192, 192..234.
        assert_eq!(range, 2..6);
        for row in &g.rows()[range] {
            let top = row.y_offset;
            let bottom = row.y_offset + row.height + row.margin_bottom;
            assert!(bottom >= 100.0 && top <= 200.0);
        }
    }

    #[test]
    fn rows_in_range_empty_and_out_of_bounds() {
        let mut g = ConversationGeometry::default();
        assert_eq!(g.rows_in_range(0.0, 100.0), 0..0);

        g.reconcile(&[bid(1)], |_| Some(text_seed(Role::User, 40, 0)), &params(), 1);
        g.recompute_offsets();
        // Far past the end.
        let r = g.rows_in_range(10_000.0, 20_000.0);
        assert!(r.is_empty());
        // Inverted window.
        let r = g.rows_in_range(50.0, 10.0);
        assert!(r.is_empty());
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
