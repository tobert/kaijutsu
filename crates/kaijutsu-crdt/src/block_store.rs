//! Block store — collection of per-block DTE instances.
//!
//! Replaces the old `BlockDocument` (single shared DTE Document) with a
//! collection of `BlockContent` instances, each owning its own DTE Document.
//! Metadata lives in `BlockHeader` (plain data), content in per-block CRDTs.

use std::collections::{BTreeMap, HashMap, HashSet};

use diamond_types_extended::{Frontier, SerializedOpsOwned};

use crate::content::{BlockContent, base62_encode_padded, order_key_successor, order_midpoint};
use crate::selection::IntervalSet;
use crate::{
    BlockHeader, BlockId, BlockKind, BlockSnapshot, ContentType, ContextId, CrdtError, DriftKind,
    MAX_DAG_DEPTH, PrincipalId, Result, Role, Status, Tick, ToolKind,
};

/// Filter criteria for selective block inclusion during fork.
///
/// All criteria are exclusionary — blocks matching any criterion are skipped.
/// An empty filter (all defaults) includes everything.
#[derive(Debug, Clone, Default)]
pub struct ForkBlockFilter {
    /// Skip blocks marked as compacted.
    pub exclude_compacted: bool,
    /// Skip blocks with these BlockKind names (e.g., "Thinking", "ToolCall").
    pub exclude_kinds: HashSet<String>,
    /// Skip blocks with these Role names (e.g., "Tool", "Model").
    pub exclude_roles: HashSet<String>,
    /// Skip specific blocks by BlockId key (context:agent:seq format).
    pub exclude_block_ids: HashSet<String>,
    /// Positional keep-set over the order_key-sorted, non-deleted,
    /// before-`before_timestamp` snapshot at the fork instant — the interval
    /// selection (`kept = (base ∩ ∪inc) \ ∪exc`, see `crate::selection` and
    /// `docs/fork-filters.md`). `None` keeps every position; the predicate
    /// excludes above still apply on top. Positions are resolved upstream
    /// (preset + CLI ranges + include-invariant check), so applying it here is
    /// infallible.
    pub selection: Option<IntervalSet>,
}

impl ForkBlockFilter {
    /// Check if a block snapshot passes this filter (should be included).
    pub fn matches(&self, snap: &BlockSnapshot) -> bool {
        if self.exclude_compacted && snap.compacted {
            return false;
        }
        if !self.exclude_kinds.is_empty() {
            let kind_name = format!("{:?}", snap.kind);
            if self.exclude_kinds.contains(&kind_name) {
                return false;
            }
        }
        if !self.exclude_roles.is_empty() {
            let role_name = format!("{:?}", snap.role);
            if self.exclude_roles.contains(&role_name) {
                return false;
            }
        }
        if !self.exclude_block_ids.is_empty() && self.exclude_block_ids.contains(&snap.id.to_key())
        {
            return false;
        }
        true
    }
}

/// Collection of blocks with per-block DTE instances.
///
/// Each block owns its own DTE Document for content. Metadata lives in
/// `BlockHeader` (plain data). Ordering uses fractional indexing.
///
/// Uses a Lamport clock (not wall-clock) for LWW conflict resolution on
/// mutable header fields (status, collapsed). The clock advances on every
/// local mutation and on merge (`max(local, remote) + 1`).
pub struct BlockStore {
    /// Context this store belongs to.
    context_id: ContextId,

    /// Principal ID stamped on blocks created through this store.
    principal_id: PrincipalId,

    /// Blocks indexed by ID.
    blocks: BTreeMap<BlockId, BlockContent>,

    /// Per-principal next-seq lanes. The next seq for principal P = max seq of
    /// ANY block in this doc minted under P (tombstones included) + 1. Maintained
    /// on every insert/merge/restore/fork path; the block log is the single
    /// source of truth and these lanes are derived from it.
    ///
    /// BlockId uniqueness is per (context, principal) — NOT per track. Two tracks
    /// may share a scheduling principal (one model on two chairs; beat() covering
    /// fallbacks across every track), so the seq lane is keyed by principal, never
    /// by track. CONTRACT: the principal records who PLAYED (authorship); it is
    /// never a lane identity. The lane (a forthcoming `track` field) and the
    /// principal are independent axes — one track's blocks span multiple
    /// principals (player + beat). Here, seq is just a row id; the musical
    /// coordinate is `tick`, and ordering is the successor-key axis.
    seq_lanes: HashMap<PrincipalId, u64>,

    /// Store version (bumped on any mutation).
    version: u64,

    /// Lamport clock for LWW conflict resolution on header fields.
    /// Monotonically increasing. Advanced on local mutations and on merge.
    lamport_clock: u64,

    /// Next hyoushigi timeline tick — a per-context, per-block monotonic ordinal
    /// stamped on every inserted block (distinct from the Lamport clock, which
    /// bumps on many metadata ops). The append `order_key` is derived from it.
    next_tick: i64,
}

impl BlockStore {
    /// Create a new empty store.
    pub fn new(context_id: ContextId, principal_id: PrincipalId) -> Self {
        Self {
            context_id,
            principal_id,
            blocks: BTreeMap::new(),
            seq_lanes: HashMap::new(),
            version: 0,
            lamport_clock: 0,
            next_tick: 0,
        }
    }

    // =========================================================================
    // Lamport clock
    // =========================================================================

    /// Advance the Lamport clock and return the new value.
    fn tick(&mut self) -> u64 {
        self.lamport_clock += 1;
        self.lamport_clock
    }

    /// Advance the Lamport clock to at least `remote_ts + 1`.
    fn merge_clock(&mut self, remote_ts: u64) {
        self.lamport_clock = self.lamport_clock.max(remote_ts) + 1;
    }

    // =========================================================================
    // Accessors
    // =========================================================================

    /// Get the context ID.
    pub fn context_id(&self) -> ContextId {
        self.context_id
    }

    /// Get the principal ID.
    pub fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    /// Override the principal ID for subsequent block operations.
    ///
    /// This changes who "authored" newly created blocks (via `BlockId.principal_id`).
    /// The DTE agent within each per-block Document is unrelated — it tracks
    /// CRDT operation identity, not block authorship.
    pub fn set_principal_id(&mut self, principal_id: PrincipalId) {
        self.principal_id = principal_id;
    }

    /// Get the current version.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Override the version counter.
    ///
    /// Used after `from_snapshot()` to set the version to the upstream sync
    /// version so that downstream dirty checks (e.g. `last_render_version <
    /// version`) correctly detect that content needs re-rendering.
    pub fn set_version(&mut self, v: u64) {
        self.version = v;
    }

    /// Get the number of live (non-deleted) blocks.
    pub fn block_count(&self) -> usize {
        self.blocks.values().filter(|b| !b.is_deleted()).count()
    }

    /// Check if the store has no live blocks.
    pub fn is_empty(&self) -> bool {
        self.block_count() == 0
    }

    /// Get a block snapshot by ID.
    pub fn get_block_snapshot(&self, id: &BlockId) -> Option<BlockSnapshot> {
        self.blocks
            .get(id)
            .filter(|b| !b.is_deleted())
            .map(|b| b.snapshot())
    }

    /// Get block IDs in document order (sorted by order_key, BlockId tiebreak).
    pub fn block_ids_ordered(&self) -> Vec<BlockId> {
        let mut ordered: Vec<_> = self
            .blocks
            .iter()
            .filter(|(_, b)| !b.is_deleted())
            .map(|(id, b)| (b.order_key().to_string(), *id))
            .collect();
        ordered.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        ordered.into_iter().map(|(_, id)| id).collect()
    }

    /// The maximum `Tick` over live blocks, or `None` if none carry one. A direct
    /// O(N) scan — no ordering sort, no snapshot allocation — for the re-arm
    /// playhead seed (the ordered/allocating `blocks_ordered` path is gratuitous
    /// here since only the max matters, not document order).
    pub fn max_tick(&self) -> Option<Tick> {
        self.blocks
            .values()
            .filter(|b| !b.is_deleted())
            .filter_map(|b| b.tick())
            .max()
    }

    /// Get blocks in document order as snapshots.
    pub fn blocks_ordered(&self) -> Vec<BlockSnapshot> {
        self.block_ids_ordered()
            .into_iter()
            .filter_map(|id| self.get_block_snapshot(&id))
            .collect()
    }

    /// Get full text content (concatenation of all blocks).
    pub fn full_text(&self) -> String {
        self.blocks_ordered()
            .into_iter()
            .map(|b| b.content)
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    // =========================================================================
    // DAG Operations
    // =========================================================================

    /// Get children of a block (blocks with this block as parent).
    pub fn get_children(&self, parent_id: &BlockId) -> Vec<BlockId> {
        self.blocks
            .iter()
            .filter(|(_, b)| !b.is_deleted() && b.header().parent_id.as_ref() == Some(parent_id))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Get ancestors of a block (walk up the parent chain).
    pub fn get_ancestors(&self, id: &BlockId) -> Vec<BlockId> {
        let mut ancestors = Vec::new();
        let mut current_id = self.blocks.get(id).and_then(|b| b.header().parent_id);

        while let Some(pid) = current_id {
            if ancestors.len() >= MAX_DAG_DEPTH {
                tracing::warn!("get_ancestors() hit MAX_DAG_DEPTH ({MAX_DAG_DEPTH}), truncating");
                break;
            }
            ancestors.push(pid);
            current_id = self.blocks.get(&pid).and_then(|b| b.header().parent_id);
        }

        ancestors
    }

    /// Get root blocks (blocks with no parent).
    pub fn get_roots(&self) -> Vec<BlockId> {
        self.blocks
            .iter()
            .filter(|(_, b)| !b.is_deleted() && b.header().parent_id.is_none())
            .map(|(id, _)| *id)
            .collect()
    }

    /// Get the depth of a block in the DAG (0 for roots).
    pub fn get_depth(&self, id: &BlockId) -> usize {
        self.get_ancestors(id).len()
    }

    // =========================================================================
    // Block ID generation
    // =========================================================================

    fn new_block_id(&mut self) -> BlockId {
        let principal = self.principal_id;
        let seq = self.seq_lanes.entry(principal).or_insert(0);
        let id = BlockId::new(self.context_id, principal, *seq);
        *seq += 1;
        id
    }

    /// Reserve a fresh `BlockId` under `principal` WITHOUT inserting — bumps the
    /// principal's seq lane and returns the id. The caller is expected to insert
    /// it (via `insert_from_snapshot`), but a reserve-then-failed-insert leaves a
    /// seq gap, which is benign: BlockId requires monotonic-unique, NOT dense.
    /// Do not "fix" the gap — re-using a reserved seq risks DuplicateBlock.
    ///
    /// Materialization reserves under `cell.played_by` (the player, or beat() for
    /// fallback repeats): fallback repeats are authored by beat(), not the player,
    /// so the reserved lane is the player's only when the player produced the ABC.
    pub fn reserve_block_id(&mut self, principal: PrincipalId) -> BlockId {
        let seq = self.seq_lanes.entry(principal).or_insert(0);
        let id = BlockId::new(self.context_id, principal, *seq);
        *seq += 1;
        id
    }

    /// The next seq this store would mint for `principal` (test accessor). Equals
    /// max persisted seq for `principal` + 1 after any restore/merge/fork.
    pub fn next_seq_for(&self, principal: PrincipalId) -> u64 {
        self.seq_lanes.get(&principal).copied().unwrap_or(0)
    }

    /// Advance `principal`'s seq lane to at least `seq + 1`, observing a block
    /// minted under `principal` (live OR tombstoned). Called on every
    /// insert/merge/restore/fork path so the lane is always derived from the
    /// block log — the single source of truth. The `== self.principal_id` guard
    /// that previously gated this was the DuplicateBlock bug class: it left
    /// foreign lanes (beat(), other players) invisible to restore.
    fn observe_seq(&mut self, principal: PrincipalId, seq: u64) {
        let lane = self.seq_lanes.entry(principal).or_insert(0);
        *lane = (*lane).max(seq + 1);
    }

    // =========================================================================
    // Order key calculation
    // =========================================================================

    /// 4-char base62 agent suffix derived from agent ID.
    ///
    /// Each agent gets a unique "lane" in the keyspace. Since the chars are
    /// valid base62, they participate normally in midpoint calculations — no
    /// stripping needed. Sequential inserts by the same agent naturally group
    /// because they all share the same suffix in their key lineage.
    fn agent_order_suffix(&self) -> String {
        let bytes = self.principal_id.as_bytes();
        // Use bytes 10-13 (random portion of UUIDv7, not the timestamp prefix)
        // to maximize entropy and minimize suffix collisions between agents.
        let c1 = crate::content::BASE62[(bytes[10] as usize) % 62] as char;
        let c2 = crate::content::BASE62[(bytes[11] as usize) % 62] as char;
        let c3 = crate::content::BASE62[(bytes[12] as usize) % 62] as char;
        let c4 = crate::content::BASE62[(bytes[13] as usize) % 62] as char;
        format!("{c1}{c2}{c3}{c4}")
    }

    /// A bounded, monotonic `order_key` derived from a timeline `tick`.
    ///
    /// Width-11 base62 covers the full `i64` range; the `'V'` prefix (mid-
    /// alphabet) leaves keyspace below for an explicit move-to-front via
    /// `order_midpoint`. Because the key length is fixed, a monotonic tick yields
    /// monotonically-sorting keys in O(1) — no per-append growth, no full sort.
    ///
    /// NOTE (order/tick decoupling, design §2): appends no longer derive their
    /// key from this function — they take the *successor* of the predecessor's
    /// key (`order_key_successor`), so a stale tick counter cannot mis-sort. This
    /// survives only for the empty-store first insert (the sole case where the
    /// tick legitimately seeds the keyspace) and for `normalize_timeline`
    /// re-keying of legacy pre-tick blocks. The tick stays the *semantic*
    /// coordinate stamped on every block; ordering is a separate axis.
    fn order_key_for_tick(&self, tick: i64) -> String {
        // Tick dominates the sort (chronological order within a store); the agent
        // suffix is a low-order tiebreak that keeps keys unique across replicas
        // that independently assign the same tick (a multi-writer concern, still
        // deferred — see docs/hyoushigi.md).
        format!("V{}{}", base62_encode_padded(tick, 11), self.agent_order_suffix())
    }

    /// Assign the next timeline tick and a matching `order_key` for a freshly
    /// created block inserted at `after`. Returns the tick to stamp on the block.
    fn next_position(&mut self, after: Option<&BlockId>) -> (Option<Tick>, String) {
        let tick = self.next_tick;
        self.next_tick += 1;
        let order_key = self.calc_order_key(after, Some(tick));
        (Some(Tick::new(tick)), order_key)
    }

    /// Compute an `order_key` for a block inserted after `after`.
    ///
    /// When `tick` is `Some` (a freshly-created block) and the insertion is an
    /// append (after the last block, or the first block), the key is derived
    /// directly from the tick — bounded and O(1), the hot path that retires the
    /// old append cliff. A genuine insert-between (e.g. a tool_result placed
    /// after a non-last tool_call during parallel tool use) keeps a fractional
    /// `order_midpoint` so placement is preserved; those keys carry the 4-char
    /// agent suffix for concurrent-insert tiebreak. `tick == None` is the legacy
    /// path used by `move_block` and snapshot restore.
    fn calc_order_key(&self, after: Option<&BlockId>, tick: Option<i64>) -> String {
        let ordered = self.block_ids_ordered();
        let suffix = self.agent_order_suffix();

        let base = match after {
            None => {
                // A freshly-created block (tick is Some) appends to the end of
                // the timeline. The append key derives from the current tail's
                // key (successor), NEVER from the tick counter — a stale counter
                // structurally cannot mis-sort an append. The empty store is the
                // sole case where the tick legitimately seeds the keyspace.
                // Prepending before the first block stays the legacy
                // `tick == None` move-to-front path (move_block, snapshot restore).
                if let Some(t) = tick {
                    return match ordered.last() {
                        None => self.order_key_for_tick(t),
                        Some(last) => {
                            let last_key = self.blocks[last].order_key();
                            let new_key = order_key_successor(last_key, &suffix);
                            debug_assert!(new_key.as_str() > last_key);
                            new_key
                        }
                    };
                }
                if ordered.is_empty() {
                    // First block on the timeline.
                    "V".to_string()
                } else {
                    let first_key = self.blocks[&ordered[0]].order_key();
                    order_midpoint("", first_key)
                }
            }
            Some(after_id) => {
                let after_idx = ordered.iter().position(|id| id == after_id);
                match after_idx {
                    Some(idx) => {
                        let after_key = self.blocks[&ordered[idx]].order_key().to_string();
                        if idx + 1 < ordered.len() {
                            let next_key = self.blocks[&ordered[idx + 1]].order_key();
                            order_midpoint(&after_key, next_key)
                        } else {
                            // Appending after the last block. The key is the
                            // successor of after_key — derived from the
                            // predecessor, NEVER from the tick counter (the
                            // documented bug arm: it previously read the tick
                            // while already holding after_key, so a stale counter
                            // mis-sorted the append).
                            if tick.is_some() {
                                let new_key = order_key_successor(&after_key, &suffix);
                                debug_assert!(new_key > after_key);
                                return new_key;
                            }
                            format!("{after_key}V")
                        }
                    }
                    None => {
                        // after_id not found — block was deleted or never existed.
                        // Fall back to appending after the last block.
                        tracing::warn!(
                            after_id = %after_id.to_key(),
                            total_blocks = ordered.len(),
                            "calc_order_key: after_id not found in ordered list, appending to end"
                        );
                        if let Some(last) = ordered.last() {
                            let last_key = self.blocks[last].order_key().to_string();
                            // Successor of the tail — derived from the predecessor,
                            // not the tick counter (same substitution as the
                            // append-after-last arm).
                            if tick.is_some() {
                                let new_key = order_key_successor(&last_key, &suffix);
                                debug_assert!(new_key > last_key);
                                return new_key;
                            }
                            format!("{last_key}V")
                        } else {
                            if let Some(t) = tick {
                                return self.order_key_for_tick(t);
                            }
                            "V".to_string()
                        }
                    }
                }
            }
        };

        format!("{base}{suffix}")
    }

    /// Normalize legacy ordering: assign timeline ticks to any blocks created
    /// before the tick coordinate existed, re-keying them into the tick scheme
    /// in their current visual order. Idempotent — a no-op once every live block
    /// has a tick. Also seeds `next_tick` so subsequent inserts stay monotonic.
    fn normalize_timeline(&mut self) {
        let live_missing_tick = self
            .blocks
            .values()
            .any(|b| !b.is_deleted() && b.tick().is_none());

        if live_missing_tick {
            // Re-key every live block in its established order, eliminating the
            // legacy `order_key`s entirely.
            let ordered = self.block_ids_ordered();
            for (i, id) in ordered.iter().enumerate() {
                let t = i as i64;
                let key = self.order_key_for_tick(t);
                if let Some(block) = self.blocks.get_mut(id) {
                    block.set_order_key(key);
                    block.set_tick(Tick::new(t));
                }
            }
            self.next_tick = ordered.len() as i64;
        } else {
            // Already ticked — resume the counter past the maximum.
            self.next_tick = self
                .blocks
                .values()
                .filter(|b| !b.is_deleted())
                .filter_map(|b| b.tick())
                .map(|t| t.get())
                .max()
                .map(|m| m + 1)
                .unwrap_or(0);
        }
    }

    // =========================================================================
    // Block Operations
    // =========================================================================

    /// Insert a new block.
    ///
    /// Author is implicit — derived from `self.principal_id` via the BlockId.
    pub fn insert_block(
        &mut self,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        role: Role,
        kind: BlockKind,
        content: impl Into<String>,
        status: Status,
        content_type: ContentType,
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let content_str = content.into();

        // Validate references
        if let Some(after_id) = after
            && (!self.blocks.contains_key(after_id) || self.blocks[after_id].is_deleted())
        {
            return Err(CrdtError::InvalidReference(*after_id));
        }
        if let Some(pid) = parent_id
            && (!self.blocks.contains_key(pid) || self.blocks[pid].is_deleted())
        {
            return Err(CrdtError::InvalidReference(*pid));
        }

        let (block_tick, order_key) = self.next_position(after);
        let ts = self.tick();
        let header = BlockHeader {
            id,
            parent_id: parent_id.copied(),
            role,
            kind,
            status,
            compacted: false,
            collapsed: false,
            ephemeral: false,
            excluded: false,
            created_at: now_millis(),
            updated_at: ts,
            tool_kind: None,
            exit_code: None,
            is_error: false,
            status_at: ts,
            collapsed_at: ts,
            ephemeral_at: ts,
            excluded_at: ts,
            compacted_at: ts,
            tool_meta_at: ts,
            content_type,
            content_type_at: ts,
        };

        let block =
            BlockContent::with_content(header, &content_str, self.principal_id, order_key, block_tick);
        self.blocks.insert(id, block);
        self.version += 1;
        Ok(id)
    }

    /// Insert a tool call block.
    pub fn insert_tool_call(
        &mut self,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        tool_name: impl Into<String>,
        tool_input: serde_json::Value,
        tool_kind: Option<ToolKind>,
        role: Option<Role>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let input_json = serde_json::to_string_pretty(&tool_input)
            .map_err(|e| CrdtError::Serialization(e.to_string()))?;

        // Validate references
        if let Some(after_id) = after
            && (!self.blocks.contains_key(after_id) || self.blocks[after_id].is_deleted())
        {
            return Err(CrdtError::InvalidReference(*after_id));
        }
        if let Some(pid) = parent_id
            && (!self.blocks.contains_key(pid) || self.blocks[pid].is_deleted())
        {
            return Err(CrdtError::InvalidReference(*pid));
        }

        let (block_tick, order_key) = self.next_position(after);
        let now = now_millis();
        let ts = self.tick();
        let header = BlockHeader {
            id,
            parent_id: parent_id.copied(),
            role: role.unwrap_or(Role::Model),
            kind: BlockKind::ToolCall,
            status: Status::Running,
            compacted: false,
            collapsed: false,
            ephemeral: false,
            excluded: false,
            created_at: now,
            updated_at: ts,
            tool_kind,
            exit_code: None,
            is_error: false,
            status_at: ts,
            collapsed_at: ts,
            ephemeral_at: ts,
            excluded_at: ts,
            compacted_at: ts,
            tool_meta_at: ts,
            content_type: ContentType::Plain,
            content_type_at: ts,
        };

        let mut block =
            BlockContent::with_content(header, &input_json, self.principal_id, order_key, block_tick);
        block.set_tool_name(Some(tool_name.into()));
        block.set_tool_input(Some(input_json));
        self.blocks.insert(id, block);
        self.version += 1;
        Ok(id)
    }

    /// Insert a tool result block.
    pub fn insert_tool_result_block(
        &mut self,
        tool_call_id: &BlockId,
        after: Option<&BlockId>,
        content: impl Into<String>,
        is_error: bool,
        exit_code: Option<i32>,
        tool_kind: Option<ToolKind>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let after = after.or(Some(tool_call_id));

        // Validate tool call exists
        if !self.blocks.contains_key(tool_call_id) || self.blocks[tool_call_id].is_deleted() {
            return Err(CrdtError::InvalidReference(*tool_call_id));
        }
        if let Some(after_id) = after
            && (!self.blocks.contains_key(after_id) || self.blocks[after_id].is_deleted())
        {
            return Err(CrdtError::InvalidReference(*after_id));
        }

        let (block_tick, order_key) = self.next_position(after);
        let now = now_millis();
        let ts = self.tick();
        let header = BlockHeader {
            id,
            parent_id: Some(*tool_call_id),
            role: Role::Tool,
            kind: BlockKind::ToolResult,
            status: if is_error {
                Status::Error
            } else {
                Status::Done
            },
            compacted: false,
            collapsed: false,
            ephemeral: false,
            excluded: false,
            created_at: now,
            updated_at: ts,
            tool_kind,
            exit_code,
            is_error,
            status_at: ts,
            collapsed_at: ts,
            ephemeral_at: ts,
            excluded_at: ts,
            compacted_at: ts,
            tool_meta_at: ts,
            content_type: ContentType::Plain,
            content_type_at: ts,
        };

        let mut block =
            BlockContent::with_content(header, &content.into(), self.principal_id, order_key, block_tick);
        block.set_tool_call_id(Some(*tool_call_id));
        self.blocks.insert(id, block);
        self.version += 1;
        Ok(id)
    }

    /// Insert a drift block.
    pub fn insert_drift_block(
        &mut self,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        content: impl Into<String>,
        source_context: ContextId,
        source_model: Option<String>,
        drift_kind: DriftKind,
    ) -> Result<BlockId> {
        let id = self.new_block_id();

        if let Some(after_id) = after
            && (!self.blocks.contains_key(after_id) || self.blocks[after_id].is_deleted())
        {
            return Err(CrdtError::InvalidReference(*after_id));
        }
        if let Some(pid) = parent_id
            && (!self.blocks.contains_key(pid) || self.blocks[pid].is_deleted())
        {
            return Err(CrdtError::InvalidReference(*pid));
        }

        let (block_tick, order_key) = self.next_position(after);

        let snap = BlockSnapshot::drift(
            id,
            parent_id.copied(),
            content,
            source_context,
            source_model,
            drift_kind,
        );
        let mut block = BlockContent::from_snapshot(&snap, self.principal_id, order_key);
        if let Some(t) = block_tick {
            block.set_tick(t);
        }
        self.blocks.insert(id, block);
        self.version += 1;
        Ok(id)
    }

    /// Insert a file block.
    pub fn insert_file_block(
        &mut self,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        file_path: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();

        if let Some(after_id) = after
            && (!self.blocks.contains_key(after_id) || self.blocks[after_id].is_deleted())
        {
            return Err(CrdtError::InvalidReference(*after_id));
        }
        if let Some(pid) = parent_id
            && (!self.blocks.contains_key(pid) || self.blocks[pid].is_deleted())
        {
            return Err(CrdtError::InvalidReference(*pid));
        }

        let (block_tick, order_key) = self.next_position(after);
        let snap = BlockSnapshot::file(id, parent_id.copied(), file_path, content);
        let mut block = BlockContent::from_snapshot(&snap, self.principal_id, order_key);
        if let Some(t) = block_tick {
            block.set_tick(t);
        }
        self.blocks.insert(id, block);
        self.version += 1;
        Ok(id)
    }

    /// Insert an error block attached to a parent.
    pub fn insert_error_block(
        &mut self,
        parent_id: &BlockId,
        after: Option<&BlockId>,
        payload: &kaijutsu_types::ErrorPayload,
        summary: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();

        if !self.blocks.contains_key(parent_id) || self.blocks[parent_id].is_deleted() {
            return Err(CrdtError::InvalidReference(*parent_id));
        }
        if let Some(after_id) = after
            && (!self.blocks.contains_key(after_id) || self.blocks[after_id].is_deleted())
        {
            return Err(CrdtError::InvalidReference(*after_id));
        }

        let (block_tick, order_key) = self.next_position(after);
        let snap =
            BlockSnapshot::error_for(id, *parent_id, payload.clone(), summary);
        let mut block = BlockContent::from_snapshot(&snap, self.principal_id, order_key);
        if let Some(t) = block_tick {
            block.set_tick(t);
        }
        self.blocks.insert(id, block);
        self.version += 1;
        Ok(id)
    }

    /// Insert a notification block (broker-emitted tool/log event).
    ///
    /// `parent_id` is typically `None` — notifications are root-level events.
    /// Callers may pass a parent when the notification is about a specific
    /// block (e.g. a failed tool call).
    pub fn insert_notification_block(
        &mut self,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        payload: &kaijutsu_types::NotificationPayload,
        summary: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();

        if let Some(after_id) = after
            && (!self.blocks.contains_key(after_id) || self.blocks[after_id].is_deleted())
        {
            return Err(CrdtError::InvalidReference(*after_id));
        }
        if let Some(pid) = parent_id
            && (!self.blocks.contains_key(pid) || self.blocks[pid].is_deleted())
        {
            return Err(CrdtError::InvalidReference(*pid));
        }

        let (block_tick, order_key) = self.next_position(after);
        let snap = BlockSnapshot::notification_block(
            id,
            parent_id.copied(),
            payload.clone(),
            summary,
        );
        let mut block = BlockContent::from_snapshot(&snap, self.principal_id, order_key);
        if let Some(t) = block_tick {
            block.set_tick(t);
        }
        self.blocks.insert(id, block);
        self.version += 1;
        Ok(id)
    }

    /// Insert a resource block (MCP resource read-through — Phase 3, D-43).
    ///
    /// `parent_id` is `None` for the initial read (root block) and `Some(root)`
    /// for subscription-update children emitted by the broker on
    /// `ServerNotification::ResourceUpdated` flush. Callers are responsible for
    /// keeping `payload.parent_resource_block_id` in sync with `parent_id`.
    pub fn insert_resource_block(
        &mut self,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        payload: &kaijutsu_types::ResourcePayload,
        summary: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();

        if let Some(after_id) = after
            && (!self.blocks.contains_key(after_id) || self.blocks[after_id].is_deleted())
        {
            return Err(CrdtError::InvalidReference(*after_id));
        }
        if let Some(pid) = parent_id
            && (!self.blocks.contains_key(pid) || self.blocks[pid].is_deleted())
        {
            return Err(CrdtError::InvalidReference(*pid));
        }

        let (block_tick, order_key) = self.next_position(after);
        let snap = BlockSnapshot::resource_block(
            id,
            parent_id.copied(),
            payload.clone(),
            summary,
        );
        let mut block = BlockContent::from_snapshot(&snap, self.principal_id, order_key);
        if let Some(t) = block_tick {
            block.set_tick(t);
        }
        self.blocks.insert(id, block);
        self.version += 1;
        Ok(id)
    }

    /// Insert a block from a complete snapshot (for remote sync / restore).
    pub fn insert_from_snapshot(
        &mut self,
        snapshot: BlockSnapshot,
        after: Option<&BlockId>,
    ) -> Result<BlockId> {
        let block_id = snapshot.id;
        // Seed the minting principal's seq lane from this observed block — for
        // ANY principal, not just our own. Foreign lanes (beat(), other players)
        // must advance on restore or the first post-restart materialization
        // collides (the DuplicateBlock bug class).
        self.observe_seq(snapshot.id.principal_id, snapshot.id.seq);

        if self.blocks.contains_key(&block_id) {
            return Err(CrdtError::DuplicateBlock(block_id));
        }

        // Keep the snapshot's own tick (via from_snapshot) — the tick is the pure
        // *semantic* coordinate, never a row id. The `order_key`, by contrast, is
        // the successor of the predecessor's key on an append (design §2): CRDT
        // order tracks insertion order at the sole sequencer, so within-beat order
        // = insertion order and a stale upstream tick cannot mis-sort. Ties at the
        // tick coordinate remain allowed (shared-coordinate doctrine); a snapshot
        // with no tick (drift / error / legacy sync) keeps the fractional legacy
        // path — `None` consumes no local tick.
        //
        // Observability: a regressing tick (snapshot tick < predecessor tick) is
        // safe to sort now but signals an upstream seeding bug — emit and move on.
        if let Some(snap_tick) = snapshot.tick {
            let pred_tick = match after {
                Some(after_id) => self.blocks.get(after_id).and_then(|b| b.tick()),
                None => self
                    .block_ids_ordered()
                    .last()
                    .and_then(|id| self.blocks.get(id))
                    .and_then(|b| b.tick()),
            };
            if let Some(pred_tick) = pred_tick
                && snap_tick.get() < pred_tick.get()
            {
                tracing::warn!(
                    block_id = %block_id.to_key(),
                    snap_tick = snap_tick.get(),
                    pred_tick = pred_tick.get(),
                    "insert_from_snapshot: tick regresses below predecessor — upstream seeding bug"
                );
            }
        }
        // Advance next_tick past a carried tick (design §11.4) — mirroring the
        // own-mint `next_position` (:355), `merge_ops` (§2.3, :1268), and the fork
        // paths. A beat block enters here at a tick well above the conversation's
        // last; without the bump the next ORDINARY append (which mints via
        // next_position from next_tick) would stamp a lower tick and calc_order_key
        // would sort the now-visible staff mid-document. next_tick is the durable
        // tick high-water; an insert that carries a tick must move it.
        if let Some(snap_tick) = snapshot.tick {
            self.next_tick = self.next_tick.max(snap_tick.get() + 1);
        }
        let order_key = self.calc_order_key(after, snapshot.tick.map(|t| t.get()));
        let block = BlockContent::from_snapshot(&snapshot, self.principal_id, order_key);
        self.blocks.insert(block_id, block);
        self.version += 1;
        Ok(block_id)
    }

    /// Delete a block (tombstone — preserves DAG integrity).
    pub fn delete_block(&mut self, id: &BlockId) -> Result<()> {
        let ts = self.tick();
        let block = self
            .blocks
            .get_mut(id)
            .ok_or(CrdtError::BlockNotFound(*id))?;
        block.mark_deleted(ts);
        self.version += 1;
        Ok(())
    }

    // =========================================================================
    // Content Mutation
    // =========================================================================

    /// Edit text within a block.
    pub fn edit_text(
        &mut self,
        id: &BlockId,
        pos: usize,
        insert: &str,
        delete: usize,
    ) -> Result<()> {
        let block = self
            .blocks
            .get_mut(id)
            .filter(|b| !b.is_deleted())
            .ok_or(CrdtError::BlockNotFound(*id))?;

        let len = block.content_len();
        if pos > len {
            return Err(CrdtError::PositionOutOfBounds { pos, len });
        }
        if pos + delete > len {
            return Err(CrdtError::PositionOutOfBounds {
                pos: pos + delete,
                len,
            });
        }

        block.edit_text(pos, insert, delete);
        self.version += 1;
        Ok(())
    }

    /// Append text to a block.
    pub fn append_text(&mut self, id: &BlockId, text: &str) -> Result<()> {
        let block = self
            .blocks
            .get_mut(id)
            .filter(|b| !b.is_deleted())
            .ok_or(CrdtError::BlockNotFound(*id))?;
        block.append_text(text);
        self.version += 1;
        Ok(())
    }

    /// Set the status of a block.
    pub fn set_status(&mut self, id: &BlockId, status: Status) -> Result<()> {
        let ts = self.tick();
        let block = self
            .blocks
            .get_mut(id)
            .filter(|b| !b.is_deleted())
            .ok_or(CrdtError::BlockNotFound(*id))?;
        block.set_status(status, ts);
        self.version += 1;
        Ok(())
    }

    /// Set collapsed state.
    pub fn set_collapsed(&mut self, id: &BlockId, collapsed: bool) -> Result<()> {
        let ts = self.tick();
        let block = self
            .blocks
            .get_mut(id)
            .filter(|b| !b.is_deleted())
            .ok_or(CrdtError::BlockNotFound(*id))?;
        block.set_collapsed(collapsed, ts);
        self.version += 1;
        Ok(())
    }

    /// Set structured output data on a block.
    pub fn set_output(
        &mut self,
        id: &BlockId,
        output: Option<kaijutsu_types::OutputData>,
    ) -> Result<()> {
        let block = self
            .blocks
            .get_mut(id)
            .filter(|b| !b.is_deleted())
            .ok_or(CrdtError::BlockNotFound(*id))?;
        block.set_output(output);
        self.version += 1;
        Ok(())
    }

    /// Set the standard-error stream on a ToolResult block. The shell
    /// execution path calls this at completion so `BlockSnapshot::stderr`
    /// carries stderr separately from `content` (stdout). Write-once — no
    /// LWW clock; the value is replicated via `MetadataChanged` / snapshot.
    pub fn set_stderr(&mut self, id: &BlockId, stderr: Option<String>) -> Result<()> {
        let block = self
            .blocks
            .get_mut(id)
            .filter(|b| !b.is_deleted())
            .ok_or(CrdtError::BlockNotFound(*id))?;
        block.set_stderr(stderr);
        self.version += 1;
        Ok(())
    }

    /// Set the LLM-assigned tool invocation ID on a block.
    pub fn set_tool_use_id(&mut self, id: &BlockId, tool_use_id: Option<String>) -> Result<()> {
        let block = self
            .blocks
            .get_mut(id)
            .filter(|b| !b.is_deleted())
            .ok_or(CrdtError::BlockNotFound(*id))?;
        block.set_tool_use_id(tool_use_id);
        self.version += 1;
        Ok(())
    }

    /// Set the reasoning-continuity token on a block (Thinking blocks).
    /// Write-once at `ThinkingEnd`; replicated via snapshot. See
    /// [`kaijutsu_types::BlockSnapshot::signature`].
    pub fn set_signature(&mut self, id: &BlockId, signature: Option<String>) -> Result<()> {
        let block = self
            .blocks
            .get_mut(id)
            .filter(|b| !b.is_deleted())
            .ok_or(CrdtError::BlockNotFound(*id))?;
        block.set_signature(signature);
        self.version += 1;
        Ok(())
    }

    /// Set the content type on a block using LWW semantics.
    pub fn set_content_type(&mut self, id: &BlockId, content_type: ContentType) -> Result<()> {
        let ts = self.tick();
        let block = self
            .blocks
            .get_mut(id)
            .ok_or(CrdtError::BlockNotFound(*id))?;
        block.set_content_type(content_type, ts);
        self.version += 1;
        Ok(())
    }

    /// Set the exit_code on a ToolResult block using LWW semantics on the
    /// shared tool_meta clock. The shell execution path calls this after the
    /// underlying command finishes, capturing the real exit code instead of
    /// truncating to the binary Done/Error status.
    pub fn set_exit_code(&mut self, id: &BlockId, exit_code: Option<i32>) -> Result<()> {
        let ts = self.tick();
        let block = self
            .blocks
            .get_mut(id)
            .ok_or(CrdtError::BlockNotFound(*id))?;
        block.set_exit_code(exit_code, ts);
        self.version += 1;
        Ok(())
    }

    /// Set the ephemeral flag on a block.
    pub fn set_ephemeral(&mut self, id: &BlockId, ephemeral: bool) -> Result<()> {
        let ts = self.tick();
        let block = self
            .blocks
            .get_mut(id)
            .ok_or(CrdtError::BlockNotFound(*id))?;
        block.set_ephemeral(ephemeral, ts);
        self.version += 1;
        Ok(())
    }

    /// Set the excluded flag on a block (user-curated exclusion during staging).
    pub fn set_excluded(&mut self, id: &BlockId, excluded: bool) -> Result<()> {
        let ts = self.tick();
        let block = self
            .blocks
            .get_mut(id)
            .ok_or(CrdtError::BlockNotFound(*id))?;
        block.set_excluded(excluded, ts);
        self.version += 1;
        Ok(())
    }

    /// Set the compacted flag on a block. Used by auto-compaction to mark
    /// older blocks as superseded by a Drift summary so the hydrator skips
    /// them when reconstructing LLM history (M1-A5).
    pub fn set_compacted(&mut self, id: &BlockId, compacted: bool) -> Result<()> {
        let ts = self.tick();
        let block = self
            .blocks
            .get_mut(id)
            .ok_or(CrdtError::BlockNotFound(*id))?;
        block.set_compacted(compacted, ts);
        self.version += 1;
        Ok(())
    }

    /// Move a block to a new position.
    pub fn move_block(&mut self, id: &BlockId, after: Option<&BlockId>) -> Result<()> {
        if !self.blocks.contains_key(id) || self.blocks[id].is_deleted() {
            return Err(CrdtError::BlockNotFound(*id));
        }
        if let Some(after_id) = after
            && (!self.blocks.contains_key(after_id) || self.blocks[after_id].is_deleted())
        {
            return Err(CrdtError::InvalidReference(*after_id));
        }

        // Reorder keeps the block's original tick; only its order_key moves.
        let order_key = self.calc_order_key(after, None);
        self.blocks.get_mut(id).unwrap().set_order_key(order_key);
        self.version += 1;
        Ok(())
    }

    // =========================================================================
    // Sync Operations
    // =========================================================================

    /// Get per-block operations since given frontiers.
    ///
    /// For known blocks: sends DTE text delta + current header.
    /// For new blocks: sends full snapshot (includes order_key for correct positioning).
    /// For deleted blocks: sends block ID so receiver can apply tombstone.
    pub fn ops_since(&self, frontiers: &HashMap<BlockId, Frontier>) -> SyncPayload {
        let mut block_ops = Vec::new();
        let mut new_blocks = Vec::new();
        let mut updated_headers = Vec::new();
        let mut deleted_blocks = Vec::new();

        for (id, block) in &self.blocks {
            if block.is_deleted() {
                // If the receiver knows about this block, tell them it's deleted
                if frontiers.contains_key(id) {
                    deleted_blocks.push(*id);
                }
                continue;
            }
            let frontier = frontiers.get(id);
            match frontier {
                Some(f) => {
                    // Known block: send DTE delta + header
                    let ops = block.ops_since(f);
                    if !ops.is_empty() {
                        block_ops.push((*id, ops));
                    }
                    // Always send header for known blocks so metadata
                    // changes (status, collapsed) propagate via LWW
                    updated_headers.push(*block.header());
                }
                None => {
                    // New block: send snapshot (metadata) + full DTE ops (content history).
                    // The receiver creates with empty content then merges the DTE ops,
                    // preserving causal history for subsequent incremental sync.
                    new_blocks.push(block.snapshot());
                    // Full ops from root so receiver gets complete DTE causal graph
                    let full_ops = block.ops_since(&Frontier::root());
                    if !full_ops.is_empty() {
                        block_ops.push((*id, full_ops));
                    }
                }
            }
        }

        SyncPayload {
            block_ops,
            new_blocks,
            updated_headers,
            deleted_blocks,
        }
    }

    /// Merge a sync payload from a remote peer.
    pub fn merge_ops(&mut self, payload: SyncPayload) -> Result<()> {
        // Track max remote Lamport timestamp for clock advancement
        let mut max_remote_ts: u64 = 0;

        // First, create blocks from snapshots (new blocks).
        // Use empty content — DTE ops in block_ops will fill it in,
        // preserving causal history for subsequent incremental sync.
        // Falls back to from_snapshot (with content) if no DTE ops are
        // present for this block (e.g., persistence restore).
        let has_ops_for: std::collections::HashSet<BlockId> =
            payload.block_ops.iter().map(|(id, _)| *id).collect();

        // Restore the tick high-water across the merge: a freshly-stamped tick
        // after this merge must exceed every merged tick (design §2.3). Mirrors
        // the seq-lane restore — semantic correctness (tick values), separate
        // from ordering correctness which is now successor-key driven.
        let mut max_tick: Option<i64> = None;

        for snap in &payload.new_blocks {
            if !self.blocks.contains_key(&snap.id) {
                // Key-less fallback: when the snapshot carries no order_key (no
                // live path today, but this function is the merge boundary), an
                // appended block must sort AFTER existing blocks. The old decimal
                // `{:020}` minted keys below every 'V' key — a latent PREPEND.
                // Take the successor of the current tail instead (or the tick key
                // when the store is empty). from_snapshot only uses this when
                // snap.order_key is None.
                // Only the key-less legacy path needs a fallback; the
                // successor-of-tail costs an O(n) ordered scan, so skip it when the
                // snapshot already carries its order_key (the common case — keeps
                // restore linear instead of O(n²) over a large merge).
                let fallback_key = if snap.order_key.is_some() {
                    String::new()
                } else {
                    match self.block_ids_ordered().last() {
                        Some(last) => {
                            let last_key = self.blocks[last].order_key().to_string();
                            order_key_successor(&last_key, &self.agent_order_suffix())
                        }
                        None => self.order_key_for_tick(self.next_tick),
                    }
                };
                let block = if has_ops_for.contains(&snap.id) {
                    // DTE ops will provide content with proper causal history.
                    // Bare DTE — no structure creation, sender's ops bring everything.
                    BlockContent::from_snapshot_for_sync(snap, self.principal_id, fallback_key)
                } else {
                    // No DTE ops (persistence restore) — use snapshot content
                    BlockContent::from_snapshot(snap, self.principal_id, fallback_key)
                };
                max_remote_ts = max_remote_ts.max(snap.updated_at);
                if let Some(t) = snap.tick {
                    max_tick = Some(max_tick.map_or(t.get(), |m| m.max(t.get())));
                }
                self.blocks.insert(snap.id, block);
                // Seed the minting principal's seq lane for ANY principal (the
                // guard deletion — beat()/foreign lanes must advance on restore).
                self.observe_seq(snap.id.principal_id, snap.id.seq);
            }
        }

        // Advance next_tick past the high-water of merged ticks (§2.3).
        if let Some(t) = max_tick {
            self.next_tick = self.next_tick.max(t + 1);
        }

        // Apply header updates (LWW merge)
        for header in &payload.updated_headers {
            max_remote_ts = max_remote_ts.max(header.updated_at);
            if let Some(block) = self.blocks.get_mut(&header.id) {
                block.merge_header(header);
            }
        }

        // Merge per-block incremental DTE ops
        let mut had_dte_merges = false;
        for (id, ops) in payload.block_ops {
            if let Some(block) = self.blocks.get_mut(&id) {
                if !ops.is_empty() {
                    had_dte_merges = true;
                }
                block.merge_ops(ops)?;
            } else {
                tracing::warn!("sync payload has ops for unknown block {id}, skipping");
            }
        }

        // Apply tombstone deletions
        for id in &payload.deleted_blocks {
            // Tick once per deletion to get a unique Lamport timestamp
            let ts = self.tick();
            if let Some(block) = self.blocks.get_mut(id)
                && !block.is_deleted()
            {
                block.mark_deleted(ts);
            }
        }

        // Advance Lamport clock past any remote timestamp, or bump if
        // DTE ops were merged (even without header/new-block timestamps)
        if max_remote_ts > 0 || had_dte_merges {
            self.merge_clock(max_remote_ts);
        }

        self.version += 1;
        Ok(())
    }

    /// Get per-block frontiers.
    pub fn frontier(&self) -> HashMap<BlockId, Frontier> {
        self.blocks
            .iter()
            .map(|(id, block)| (*id, block.frontier()))
            .collect()
    }

    // =========================================================================
    // Fork
    // =========================================================================

    /// Fork the store, creating a copy with a new context ID.
    ///
    /// Each block gets a fresh DTE Document with the current content
    /// (text is copied, but DTE operation history is NOT preserved).
    /// This is intentional: forks are isolated explorations. Content
    /// that "merges back" travels via drift blocks, not DTE merge.
    /// The clean DTE history also means forked contexts rarely need
    /// compaction.
    pub fn fork(&self, new_context_id: ContextId, new_principal_id: PrincipalId) -> Self {
        let mut forked = Self::new(new_context_id, new_principal_id);

        for block in self.blocks.values() {
            if block.is_deleted() {
                continue;
            }
            let snap = block.snapshot();

            // Remap IDs: only context_id changes
            let new_id = BlockId::new(new_context_id, snap.id.principal_id, snap.id.seq);
            let new_parent_id = snap
                .parent_id
                .map(|pid| BlockId::new(new_context_id, pid.principal_id, pid.seq));
            let new_tool_call_id = snap
                .tool_call_id
                .map(|tcid| BlockId::new(new_context_id, tcid.principal_id, tcid.seq));

            // Seed the forked store's seq lane for EVERY copied block's principal
            // (not just the fork principal) and advance next_tick past the copied
            // tick. Chameleon rotation is a shallow fork with a hard tick-
            // continuity invariant — without all-principal lane + tick seeding,
            // a rotated head re-mints duplicate low ids/ticks (design §2.4).
            forked.observe_seq(snap.id.principal_id, snap.id.seq);
            if let Some(t) = snap.tick {
                forked.next_tick = forked.next_tick.max(t.get() + 1);
            }

            let mut remapped = snap;
            remapped.id = new_id;
            remapped.parent_id = new_parent_id;
            remapped.tool_call_id = new_tool_call_id;

            let order_key = block.order_key().to_string();
            let content = BlockContent::from_snapshot(&remapped, new_principal_id, order_key);
            forked.blocks.insert(new_id, content);
        }

        forked.version = 1;
        forked
    }

    /// Fork the store, excluding blocks with `created_at` after `before_timestamp` (wall-clock millis).
    ///
    /// Preserves original authorship — see [`fork`] for details.
    pub fn fork_at_version(
        &self,
        new_context_id: ContextId,
        new_principal_id: PrincipalId,
        before_timestamp: u64,
    ) -> Self {
        let mut forked = Self::new(new_context_id, new_principal_id);

        for block in self.blocks.values() {
            if block.is_deleted() {
                continue;
            }
            if block.header().created_at > before_timestamp {
                continue;
            }
            let snap = block.snapshot();

            // Remap IDs: only context_id changes
            let new_id = BlockId::new(new_context_id, snap.id.principal_id, snap.id.seq);
            let new_parent_id = snap
                .parent_id
                .map(|pid| BlockId::new(new_context_id, pid.principal_id, pid.seq));
            let new_tool_call_id = snap
                .tool_call_id
                .map(|tcid| BlockId::new(new_context_id, tcid.principal_id, tcid.seq));

            // Seed the forked store's seq lane for EVERY copied block's principal
            // (not just the fork principal) and advance next_tick past the copied
            // tick. Chameleon rotation is a shallow fork with a hard tick-
            // continuity invariant — without all-principal lane + tick seeding,
            // a rotated head re-mints duplicate low ids/ticks (design §2.4).
            forked.observe_seq(snap.id.principal_id, snap.id.seq);
            if let Some(t) = snap.tick {
                forked.next_tick = forked.next_tick.max(t.get() + 1);
            }

            let mut remapped = snap;
            remapped.id = new_id;
            remapped.parent_id = new_parent_id;
            remapped.tool_call_id = new_tool_call_id;

            let order_key = block.order_key().to_string();
            let content = BlockContent::from_snapshot(&remapped, new_principal_id, order_key);
            forked.blocks.insert(new_id, content);
        }

        forked.version = 1;
        forked
    }

    /// Fork the store with block filtering, excluding blocks with `created_at` after `before_timestamp` (wall-clock millis).
    ///
    /// Like [`fork_at_version`] but additionally filters blocks via `BlockFilter`.
    /// Blocks that don't pass the filter (positional `selection` and/or the
    /// predicate excludes) are left out of the fork.
    pub fn fork_filtered(
        &self,
        new_context_id: ContextId,
        new_principal_id: PrincipalId,
        before_timestamp: u64,
        filter: &ForkBlockFilter,
    ) -> Self {
        let mut forked = Self::new(new_context_id, new_principal_id);

        // Build the positional universe in DOCUMENT order — order_key, BlockId
        // tiebreak — NOT `blocks.values()` (a `BTreeMap<BlockId>` is ordered by
        // (context, principal, seq), i.e. principal-major, which diverges from
        // timeline order in any multi-principal context). The interval
        // selection indexes this ordering.
        let mut universe: Vec<(&BlockId, &BlockContent)> = self
            .blocks
            .iter()
            .filter(|(_, b)| !b.is_deleted() && b.header().created_at <= before_timestamp)
            .collect();
        universe.sort_by(|(ia, a), (ib, b)| a.order_key().cmp(b.order_key()).then_with(|| ia.cmp(ib)));

        // Survivors, kept in document order: drop positions outside the resolved
        // selection (if any), then drop blocks the predicate excludes
        // (kind/role/compacted/id). Both are subtractions and compose.
        let mut passing: Vec<(&BlockContent, BlockSnapshot)> = Vec::new();
        for (pos, (_id, block)) in universe.iter().enumerate() {
            if let Some(sel) = &filter.selection
                && !sel.contains_position(pos)
            {
                continue;
            }
            let snap = block.snapshot();
            if !filter.matches(&snap) {
                continue;
            }
            passing.push((*block, snap));
        }

        for (block, snap) in passing {
            let new_id = BlockId::new(new_context_id, snap.id.principal_id, snap.id.seq);
            let new_parent_id = snap
                .parent_id
                .map(|pid| BlockId::new(new_context_id, pid.principal_id, pid.seq));
            let new_tool_call_id = snap
                .tool_call_id
                .map(|tcid| BlockId::new(new_context_id, tcid.principal_id, tcid.seq));

            // Seed the forked store's seq lane for EVERY copied block's principal
            // (not just the fork principal) and advance next_tick past the copied
            // tick. Chameleon rotation is a shallow fork with a hard tick-
            // continuity invariant — without all-principal lane + tick seeding,
            // a rotated head re-mints duplicate low ids/ticks (design §2.4).
            forked.observe_seq(snap.id.principal_id, snap.id.seq);
            if let Some(t) = snap.tick {
                forked.next_tick = forked.next_tick.max(t.get() + 1);
            }

            let mut remapped = snap;
            remapped.id = new_id;
            remapped.parent_id = new_parent_id;
            remapped.tool_call_id = new_tool_call_id;

            let order_key = block.order_key().to_string();
            let content = BlockContent::from_snapshot(&remapped, new_principal_id, order_key);
            forked.blocks.insert(new_id, content);
        }

        forked.version = 1;
        forked
    }

    // =========================================================================
    // Snapshot / Restore
    // =========================================================================

    /// Create a snapshot of the entire store.
    ///
    /// Includes full per-block DTE history (ops from root) so that recovery
    /// from a snapshot followed by incremental oplog replay can merge
    /// correctly. Without the history, restored blocks have fresh DTE
    /// Documents whose frontiers don't match journaled ops.
    pub fn snapshot(&self) -> StoreSnapshot {
        let ids = self.block_ids_ordered();
        let mut blocks = Vec::with_capacity(ids.len());
        let mut block_history = Vec::with_capacity(ids.len());

        for id in ids {
            if let Some(block) = self.blocks.get(&id) {
                blocks.push(block.snapshot());
                block_history.push(block.root_ops());
            }
        }

        // Preserve tombstones so deletions propagate to peers after compaction.
        let deleted_blocks: Vec<BlockId> = self
            .blocks
            .iter()
            .filter(|(_, b)| b.is_deleted())
            .map(|(id, _)| *id)
            .collect();

        StoreSnapshot {
            context_id: self.context_id,
            blocks,
            block_history,
            deleted_blocks,
        }
    }

    /// Restore from a snapshot, preserving full DTE causal history.
    ///
    /// Each block is rebuilt with `from_snapshot_for_sync` (bare DTE Document)
    /// and then its saved DTE ops are merged in. This preserves the causal
    /// graph so subsequent oplog replay can merge incremental ops.
    pub fn from_snapshot(snapshot: StoreSnapshot, principal_id: PrincipalId) -> Result<Self> {
        let mut store = Self::new(snapshot.context_id, principal_id);

        for (block_snap, history) in snapshot
            .blocks
            .iter()
            .zip(snapshot.block_history.iter())
        {
            // Seed the seq lane for EVERY observed principal — players, system(),
            // beat(), drift authors. The old `== principal_id` guard left foreign
            // lanes invisible, so the first post-restart materialization re-minted
            // beat()'s seq 0 → DuplicateBlock → silent retry loop. Lane = max
            // persisted seq for P + 1 (design §3, §6).
            store.observe_seq(block_snap.id.principal_id, block_snap.id.seq);

            // Key-less fallback: a pre-tick legacy snapshot carries no order_key,
            // so a restored block must sort AFTER the blocks restored before it.
            // The old decimal `{:020}` minted keys below every 'V' canonical key
            // (since '0' < 'V') — the same latent PREPEND hazard §2.3 killed in
            // merge_ops. Take the successor of the current tail instead (or the
            // tick key when the store is still empty). `from_snapshot` only reaches
            // this when block_snap.order_key is None.
            // Only the key-less legacy path needs a fallback; the successor-of-tail
            // costs an O(n) ordered scan, so skip it when the snapshot already
            // carries its order_key (the common case — keeps restore linear instead
            // of O(n²) over a large document).
            let fallback_key = if block_snap.order_key.is_some() {
                String::new()
            } else {
                match store.block_ids_ordered().last() {
                    Some(last) => {
                        let last_key = store.blocks[last].order_key().to_string();
                        order_key_successor(&last_key, &store.agent_order_suffix())
                    }
                    None => store.order_key_for_tick(store.next_tick),
                }
            };

            if history.is_empty() {
                let content =
                    BlockContent::from_snapshot(block_snap, principal_id, fallback_key);
                store.blocks.insert(block_snap.id, content);
            } else {
                let mut content =
                    BlockContent::from_snapshot_for_sync(block_snap, principal_id, fallback_key);
                content.merge_ops(history.clone())?;
                store.blocks.insert(block_snap.id, content);
            }
        }

        // Restore tombstones so deletions propagate to peers after compaction.
        // A deleted block's seq must never be re-minted, so its lane is seeded
        // too (tombstones included in the max-seq+1 derivation, design §3).
        for id in &snapshot.deleted_blocks {
            store.observe_seq(id.principal_id, id.seq);
            if !store.blocks.contains_key(id) {
                let snap = crate::BlockSnapshotBuilder::new(*id, crate::BlockKind::Text)
                    .build();
                let mut block = BlockContent::from_snapshot(&snap, principal_id, "Z".to_string());
                block.mark_deleted(0);
                store.blocks.insert(*id, block);
            }
        }

        // Seed lamport clock from the max header timestamp so local
        // metadata mutations produce monotonically higher timestamps.
        store.lamport_clock = snapshot
            .blocks
            .iter()
            .map(|b| b.updated_at)
            .max()
            .unwrap_or(0);

        // Seed `next_tick` past the max existing tick, and re-key any pre-tick
        // (legacy) blocks into the tick scheme so the old order_keys don't linger.
        store.normalize_timeline();

        Ok(store)
    }
}

/// Current time in milliseconds since Unix epoch.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// =========================================================================
// Sync + Snapshot types
// =========================================================================

/// Snapshot of a block store (serializable).
///
/// BREAKING: the `block_history` field was added as part of the oplog
/// persistence migration. Old serialized snapshots will fail to deserialize.
/// Delete existing databases when upgrading.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StoreSnapshot {
    /// Context ID.
    pub context_id: ContextId,
    /// Blocks in order.
    pub blocks: Vec<BlockSnapshot>,
    /// Full per-block DTE ops from root, parallel to `blocks`.
    ///
    /// Required for round-trip-correct replay: after restoring from a
    /// snapshot, incremental oplog entries reference DTE frontiers that
    /// only exist if the per-block DTE history is preserved.
    pub block_history: Vec<SerializedOpsOwned>,
    /// IDs of deleted blocks (tombstones) so deletions survive compaction.
    #[serde(default)]
    pub deleted_blocks: Vec<BlockId>,
}

/// Per-block sync payload.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SyncPayload {
    /// Per-block DTE ops (incremental delta for known blocks).
    pub block_ops: Vec<(BlockId, diamond_types_extended::SerializedOpsOwned)>,
    /// Full snapshots of blocks the receiver doesn't know about.
    /// Receivers reconstruct these from scratch rather than merging ops
    /// against a fresh DTE Document (which would fail with DataMissing).
    pub new_blocks: Vec<BlockSnapshot>,
    /// Updated headers for known blocks (LWW merge via `merge_header()`).
    /// Propagates metadata changes like status, collapsed, compacted.
    pub updated_headers: Vec<BlockHeader>,
    /// Block IDs that have been deleted (tombstoned) on the sender.
    /// Receiver should apply tombstones for these.
    pub deleted_blocks: Vec<BlockId>,
}

impl SyncPayload {
    /// Check if this payload contains no operations.
    pub fn is_empty(&self) -> bool {
        self.block_ops.is_empty()
            && self.new_blocks.is_empty()
            && self.updated_headers.is_empty()
            && self.deleted_blocks.is_empty()
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> BlockStore {
        BlockStore::new(ContextId::new(), PrincipalId::new())
    }

    /// Measure the block-insert hot path at coder scale (append-only, the way a
    /// coding session grows). Run with:
    ///   cargo test -p kaijutsu-crdt bench_append_hot_path -- --ignored --nocapture
    /// This is the budget a hyoushigi `Tick` (an O(1) counter bump) would be added
    /// against — and it exposes the pre-existing O(N log N) `calc_order_key` cost.
    #[test]
    #[ignore = "timing benchmark, run explicitly with --ignored --nocapture"]
    fn bench_append_hot_path() {
        use std::time::Instant;
        for n in [200usize, 1000, 4000] {
            let mut store = test_store();
            let mut last: Option<BlockId> = None;
            let start = Instant::now();
            for _ in 0..n {
                let id = store
                    .insert_block(
                        last.as_ref(),
                        last.as_ref(),
                        Role::Model,
                        BlockKind::Text,
                        "a typical line of streamed model text for a coding turn",
                        Status::Done,
                        ContentType::Plain,
                    )
                    .unwrap();
                last = Some(id);
            }
            let elapsed = start.elapsed();
            println!(
                "N={n:>5}: total={:>8.2}ms  per-insert={:>7.2}µs",
                elapsed.as_secs_f64() * 1000.0,
                elapsed.as_secs_f64() * 1e6 / n as f64,
            );
        }
    }

    #[test]
    fn test_new_store() {
        let store = test_store();
        assert!(store.is_empty());
        assert_eq!(store.block_count(), 0);
        assert_eq!(store.version(), 0);
    }

    #[test]
    fn test_insert_block() {
        let mut store = test_store();

        let id1 = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Hello!",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let id2 = store
            .insert_block(
                Some(&id1),
                Some(&id1),
                Role::Model,
                BlockKind::Text,
                "Hi!",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        assert_eq!(store.block_count(), 2);

        let blocks = store.blocks_ordered();
        assert_eq!(blocks[0].id, id1);
        assert_eq!(blocks[0].role, Role::User);
        assert_eq!(blocks[0].content, "Hello!");
        assert_eq!(blocks[1].id, id2);
        assert_eq!(blocks[1].role, Role::Model);
        assert_eq!(blocks[1].parent_id, Some(id1));
    }

    #[test]
    fn test_text_editing() {
        let mut store = test_store();
        let id = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Hello",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        store.append_text(&id, " World").unwrap();
        let snap = store.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.content, "Hello World");

        store.edit_text(&id, 5, ",", 0).unwrap();
        let snap = store.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.content, "Hello, World");
    }

    #[test]
    fn test_insert_and_order() {
        let mut store = test_store();

        let id1 = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "First",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let id2 = store
            .insert_block(
                None,
                Some(&id1),
                Role::User,
                BlockKind::Text,
                "Second",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let id3 = store
            .insert_block(
                None,
                Some(&id2),
                Role::User,
                BlockKind::Text,
                "Third",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let order: Vec<_> = store.blocks_ordered().iter().map(|b| b.id).collect();
        assert_eq!(order, vec![id1, id2, id3]);
    }

    #[test]
    fn test_insert_after_none_appends_to_end() {
        // Regression: a freshly-created block carries a fresh monotonic tick, so
        // `after = None` must APPEND to the end of the timeline — not prepend to
        // the top. Every production caller that omits `after` (kj block create,
        // search results, doc/beat/drive inserts) wants append. Move-to-front is
        // reserved for the legacy `tick == None` path (move_block, snapshot
        // restore), which is unaffected by this contract.
        let mut store = test_store();

        let id1 = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "First",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let id2 = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Second",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let id3 = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Third",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let order: Vec<_> = store.blocks_ordered().iter().map(|b| b.id).collect();
        assert_eq!(order, vec![id1, id2, id3]);
    }

    #[test]
    fn test_set_status() {
        let mut store = test_store();
        let id = store
            .insert_block(
                None,
                None,
                Role::Model,
                BlockKind::ToolCall,
                "{}",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        store.set_status(&id, Status::Running).unwrap();
        assert_eq!(
            store.get_block_snapshot(&id).unwrap().status,
            Status::Running
        );

        store.set_status(&id, Status::Error).unwrap();
        assert_eq!(store.get_block_snapshot(&id).unwrap().status, Status::Error);
    }

    #[test]
    fn test_set_compacted_toggles_flag() {
        let mut store = test_store();
        let id = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "old turn",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        assert!(!store.get_block_snapshot(&id).unwrap().compacted);

        store.set_compacted(&id, true).unwrap();
        assert!(store.get_block_snapshot(&id).unwrap().compacted);

        store.set_compacted(&id, false).unwrap();
        assert!(!store.get_block_snapshot(&id).unwrap().compacted);
    }

    #[test]
    fn test_set_collapsed() {
        let mut store = test_store();
        let id = store
            .insert_block(
                None,
                None,
                Role::Model,
                BlockKind::Thinking,
                "Thinking...",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        store.set_collapsed(&id, true).unwrap();
        assert!(store.get_block_snapshot(&id).unwrap().collapsed);

        store.set_collapsed(&id, false).unwrap();
        assert!(!store.get_block_snapshot(&id).unwrap().collapsed);
    }

    #[test]
    fn test_collapsed_not_restricted_to_thinking() {
        let mut store = test_store();
        let id = store
            .insert_block(
                None,
                None,
                Role::Model,
                BlockKind::ToolCall,
                "{}",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Should succeed for ToolCall blocks too
        store.set_collapsed(&id, true).unwrap();
        assert!(store.get_block_snapshot(&id).unwrap().collapsed);
    }

    #[test]
    fn test_tool_call_and_result() {
        let mut store = test_store();

        let call_id = store
            .insert_tool_call(
                None,
                None,
                "read_file",
                serde_json::json!({"path": "/etc/hosts"}),
                None,
                None,
            )
            .unwrap();

        let snap = store.get_block_snapshot(&call_id).unwrap();
        assert_eq!(snap.kind, BlockKind::ToolCall);
        assert_eq!(snap.tool_name, Some("read_file".to_string()));
        assert_eq!(snap.status, Status::Running);

        let result_id = store
            .insert_tool_result_block(
                &call_id,
                Some(&call_id),
                "127.0.0.1 localhost",
                false,
                Some(0),
                None,
            )
            .unwrap();

        let snap = store.get_block_snapshot(&result_id).unwrap();
        assert_eq!(snap.kind, BlockKind::ToolResult);
        assert_eq!(snap.parent_id, Some(call_id));
        assert_eq!(snap.tool_call_id, Some(call_id));
        assert!(!snap.is_error);
        assert_eq!(snap.exit_code, Some(0));
    }

    #[test]
    fn test_set_stderr_roundtrips_through_snapshot() {
        let mut store = test_store();
        let call_id = store
            .insert_tool_call(None, None, "sh", serde_json::json!({"cmd": "x"}), None, None)
            .unwrap();
        let result_id = store
            .insert_tool_result_block(&call_id, Some(&call_id), "out\n", false, Some(0), None)
            .unwrap();

        // Default: no stderr until set.
        assert_eq!(store.get_block_snapshot(&result_id).unwrap().stderr, None);

        store
            .set_stderr(&result_id, Some("warning: deprecated\n".to_string()))
            .unwrap();

        let snap = store.get_block_snapshot(&result_id).unwrap();
        assert_eq!(snap.content, "out\n", "stdout stays in content");
        assert_eq!(
            snap.stderr.as_deref(),
            Some("warning: deprecated\n"),
            "stderr persisted separately"
        );

        // Survives a StoreSnapshot round-trip (the sync/persistence path).
        let bytes = kaijutsu_types::codec::encode(&store.snapshot()).unwrap();
        let restored = BlockStore::from_snapshot(
            kaijutsu_types::codec::decode(&bytes).unwrap(),
            store.principal_id(),
        )
        .unwrap();
        assert_eq!(
            restored.get_block_snapshot(&result_id).unwrap().stderr.as_deref(),
            Some("warning: deprecated\n"),
        );
    }

    #[test]
    fn test_drift_block() {
        let mut store = test_store();
        let source_ctx = ContextId::new();

        let id = store
            .insert_drift_block(
                None,
                None,
                "Found a race condition",
                source_ctx,
                Some("claude-opus-4-6".to_string()),
                DriftKind::Push,
            )
            .unwrap();

        let snap = store.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.kind, BlockKind::Drift);
        assert_eq!(snap.source_context, Some(source_ctx));
        assert_eq!(snap.drift_kind, Some(DriftKind::Push));
    }

    #[test]
    fn test_file_block() {
        let mut store = test_store();

        let id = store
            .insert_file_block(None, None, "/etc/hosts", "127.0.0.1 localhost")
            .unwrap();

        let snap = store.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.kind, BlockKind::File);
        assert_eq!(snap.role, Role::Asset);
        assert_eq!(snap.file_path, Some("/etc/hosts".to_string()));
        assert_eq!(snap.content, "127.0.0.1 localhost");
    }

    #[test]
    fn test_delete_block() {
        let mut store = test_store();

        let id1 = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "First",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let id2 = store
            .insert_block(
                None,
                Some(&id1),
                Role::User,
                BlockKind::Text,
                "Second",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _id3 = store
            .insert_block(
                None,
                Some(&id2),
                Role::User,
                BlockKind::Text,
                "Third",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        assert_eq!(store.block_count(), 3);

        store.delete_block(&id2).unwrap();

        assert_eq!(store.block_count(), 2);
        assert!(store.get_block_snapshot(&id2).is_none());
        assert!(!store.full_text().contains("Second"));
    }

    #[test]
    fn test_move_block() {
        let mut store = test_store();

        let a = store
            .insert_block(None, None, Role::User, BlockKind::Text, "A", Status::Done, ContentType::Plain)
            .unwrap();
        let b = store
            .insert_block(
                None,
                Some(&a),
                Role::User,
                BlockKind::Text,
                "B",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let c = store
            .insert_block(
                None,
                Some(&b),
                Role::User,
                BlockKind::Text,
                "C",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let ordered = store.blocks_ordered();
        assert_eq!(ordered[0].id, a);
        assert_eq!(ordered[1].id, b);
        assert_eq!(ordered[2].id, c);

        store.move_block(&c, None).unwrap();
        let ordered = store.blocks_ordered();
        assert_eq!(ordered[0].id, c);
        assert_eq!(ordered[1].id, a);
        assert_eq!(ordered[2].id, b);
    }

    #[test]
    fn test_dag_operations() {
        let mut store = test_store();

        let parent = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Question",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let child1 = store
            .insert_block(
                Some(&parent),
                Some(&parent),
                Role::Model,
                BlockKind::Thinking,
                "Thinking...",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let child2 = store
            .insert_block(
                Some(&parent),
                Some(&child1),
                Role::Model,
                BlockKind::Text,
                "Answer",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let children = store.get_children(&parent);
        assert_eq!(children.len(), 2);
        assert!(children.contains(&child1));
        assert!(children.contains(&child2));

        let ancestors = store.get_ancestors(&child1);
        assert_eq!(ancestors, vec![parent]);

        let roots = store.get_roots();
        assert_eq!(roots, vec![parent]);

        assert_eq!(store.get_depth(&parent), 0);
        assert_eq!(store.get_depth(&child1), 1);
    }

    #[test]
    fn test_snapshot_roundtrip() {
        let mut store = test_store();

        store
            .insert_block(
                None,
                None,
                Role::Model,
                BlockKind::Thinking,
                "Thinking...",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        store
            .insert_block(
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "Response",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let snapshot = store.snapshot();
        let restored = BlockStore::from_snapshot(snapshot.clone(), PrincipalId::new()).unwrap();

        assert_eq!(restored.block_count(), store.block_count());
        assert_eq!(restored.full_text(), store.full_text());
    }

    #[test]
    fn test_tool_use_id_snapshot_roundtrip() {
        let mut store = test_store();

        // Insert a tool call and set tool_use_id
        let tc_id = store
            .insert_tool_call(
                None,
                None,
                "shell",
                serde_json::json!({"cmd": "ls"}),
                None,
                None,
            )
            .unwrap();
        store
            .set_tool_use_id(&tc_id, Some("toolu_01ABC".to_string()))
            .unwrap();

        // Verify it's on the snapshot
        let snap = store.get_block_snapshot(&tc_id).unwrap();
        assert_eq!(snap.tool_use_id, Some("toolu_01ABC".to_string()));

        // Round-trip through StoreSnapshot
        let store_snapshot = store.snapshot();
        let restored = BlockStore::from_snapshot(store_snapshot, PrincipalId::new()).unwrap();

        let restored_snap = restored.get_block_snapshot(&tc_id).unwrap();
        assert_eq!(restored_snap.tool_use_id, Some("toolu_01ABC".to_string()));
        assert_eq!(restored_snap.tool_name, Some("shell".to_string()));
    }

    #[test]
    fn test_fork() {
        let mut original = test_store();
        let original_agent = original.principal_id();

        let user_msg = original
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Hello Claude!",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _model_response = original
            .insert_block(
                Some(&user_msg),
                Some(&user_msg),
                Role::Model,
                BlockKind::Text,
                "Hi Amy!",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let fork_ctx = ContextId::new();
        let fork_agent = PrincipalId::new();
        let forked = original.fork(fork_ctx, fork_agent);

        assert_eq!(forked.block_count(), 2);
        assert_eq!(forked.context_id(), fork_ctx);

        let blocks = forked.blocks_ordered();
        assert_eq!(blocks[0].content, "Hello Claude!");
        assert_eq!(blocks[1].content, "Hi Amy!");
        assert_eq!(blocks[0].id.context_id, fork_ctx);
        assert_eq!(blocks[1].id.context_id, fork_ctx);
        // Authorship preserved
        assert_eq!(blocks[0].id.principal_id, original_agent);
    }

    #[test]
    fn test_insert_from_snapshot() {
        let mut store = test_store();
        let source_ctx = ContextId::new();

        let drift_id = BlockId::new(store.context_id(), store.principal_id(), 0);
        let drift_snap = BlockSnapshot::drift(
            drift_id,
            None,
            "CAS race condition",
            source_ctx,
            Some("claude-opus-4-6".to_string()),
            DriftKind::Push,
        );

        let id = store.insert_from_snapshot(drift_snap, None).unwrap();
        let snap = store.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.kind, BlockKind::Drift);
        assert_eq!(snap.source_context, Some(source_ctx));
    }

    /// Order/tick decoupling (design §2.2): an appended snapshot's order_key is
    /// the *successor* of the predecessor's key, so within-store order follows
    /// INSERTION order at the sole sequencer — NOT the tick coordinate. A cell
    /// appended after another with an earlier tick still sorts last. The tick
    /// stays the semantic coordinate (stamped on the block), a separate axis from
    /// ordering. (This replaces the pre-decoupling tick-drives-order assertion.)
    #[test]
    fn appended_snapshot_orders_by_insertion_not_tick() {
        let mut store = test_store();
        let (ctx, prin) = (store.context_id(), store.principal_id());
        let ticked = |seq: u64, tick: i64| {
            crate::BlockSnapshotBuilder::new(BlockId::new(ctx, prin, seq), BlockKind::Text)
                .tick(Tick::new(tick))
                .content(format!("beat-{tick}"))
                .build()
        };

        // Insert tick=5 first (first block), then APPEND tick=3 after it.
        let id5 = store.insert_from_snapshot(ticked(0, 5), None).unwrap();
        let _id3 = store.insert_from_snapshot(ticked(1, 3), Some(&id5)).unwrap();

        let order: Vec<i64> = store
            .blocks_ordered()
            .iter()
            .map(|b| b.tick.unwrap().get())
            .collect();
        // Successor keys → the later-inserted append sorts last, regardless of its
        // (earlier) tick. Ticks themselves are preserved verbatim.
        assert_eq!(
            order,
            vec![5, 3],
            "append order follows insertion at the sequencer, not the tick coordinate"
        );
    }

    /// Two blocks on the *same* beat share a tick — a coordinate, not a row id.
    /// With successor keys (design §2.2) the append's key is strictly greater
    /// than its predecessor's, so within-beat order = INSERTION order. Ties at the
    /// tick coordinate remain allowed; ordering no longer depends on a key-equal
    /// BlockId tiebreak.
    #[test]
    fn blocks_on_same_tick_order_by_insertion() {
        let mut store = test_store();
        let (ctx, prin) = (store.context_id(), store.principal_id());
        let ticked = |seq: u64| {
            crate::BlockSnapshotBuilder::new(BlockId::new(ctx, prin, seq), BlockKind::Text)
                .tick(Tick::new(7))
                .content(format!("note-{seq}"))
                .build()
        };

        // Two cells coalesced onto beat 7, appended in seq order 2 then 1.
        let first = store.insert_from_snapshot(ticked(2), None).unwrap();
        let _second = store.insert_from_snapshot(ticked(1), Some(&first)).unwrap();

        let ordered = store.blocks_ordered();
        assert_eq!(ordered.len(), 2);
        // Same tick on both — ties at the coordinate are expected and fine.
        assert!(ordered.iter().all(|b| b.tick == Some(Tick::new(7))));
        // Insertion order wins: seq=2 was inserted first, then seq=1 appended.
        let seqs: Vec<u64> = ordered.iter().map(|b| b.id.seq).collect();
        assert_eq!(seqs, vec![2, 1], "within-beat order follows insertion at the sequencer");
        // And the keys are strictly increasing (successor-derived), not equal.
        let keys: Vec<String> =
            ordered.iter().map(|b| b.order_key.clone().unwrap()).collect();
        assert!(keys[0] < keys[1], "successor keys are strictly increasing within a beat");
    }

    #[test]
    fn test_ordering_stress_100_bisections() {
        let mut store = test_store();

        let first = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "First",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _last = store
            .insert_block(
                None,
                Some(&first),
                Role::User,
                BlockKind::Text,
                "Last",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        for i in 0..100 {
            store
                .insert_block(
                    None,
                    Some(&first),
                    Role::User,
                    BlockKind::Text,
                    &format!("Middle-{i}"),
                    Status::Done,
                    ContentType::Plain,
                )
                .unwrap();
        }

        let blocks = store.blocks_ordered();
        assert_eq!(blocks.len(), 102);

        // Verify all blocks present
        assert_eq!(blocks[0].content, "First");
    }

    #[test]
    fn test_sync_round_trip() {
        let ctx = ContextId::new();
        let mut store1 = BlockStore::new(ctx, PrincipalId::new());
        let mut store2 = BlockStore::new(ctx, PrincipalId::new());

        let id1 = store1
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Hello from store1",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Sync store1 → store2
        let payload = store1.ops_since(&HashMap::new());
        store2.merge_ops(payload).unwrap();

        assert_eq!(store2.block_count(), 1);
        let snap = store2.get_block_snapshot(&id1).unwrap();
        assert_eq!(snap.content, "Hello from store1");
    }

    #[test]
    fn test_incremental_sync_new_block() {
        let ctx = ContextId::new();
        let mut store1 = BlockStore::new(ctx, PrincipalId::new());
        let mut store2 = BlockStore::new(ctx, PrincipalId::new());

        let id1 = store1
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Hello",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Initial sync
        let payload = store1.ops_since(&HashMap::new());
        store2.merge_ops(payload).unwrap();
        assert_eq!(store2.block_count(), 1);

        // Store1 adds a new block
        let id2 = store1
            .insert_block(
                Some(&id1),
                Some(&id1),
                Role::Model,
                BlockKind::Text,
                "World",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Incremental sync — new block arrives as full snapshot
        let frontiers = store2.frontier();
        let payload = store1.ops_since(&frontiers);
        assert_eq!(
            payload.new_blocks.len(),
            1,
            "new block should be in new_blocks"
        );
        store2.merge_ops(payload).unwrap();

        assert_eq!(store2.block_count(), 2);
        let snap = store2.get_block_snapshot(&id2).unwrap();
        assert_eq!(snap.content, "World");
        assert_eq!(snap.parent_id, Some(id1));
    }

    #[test]
    fn test_new_block_sync_no_redundant_header() {
        // Create store1 with a block, sync to store2, then add a new block to store1
        let ctx = ContextId::new();
        let mut store1 = BlockStore::new(ctx, PrincipalId::new());
        let mut store2 = BlockStore::new(ctx, PrincipalId::new());

        let id1 = store1
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "First",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Initial sync so store2 knows about id1
        let payload = store1.ops_since(&HashMap::new());
        store2.merge_ops(payload).unwrap();
        assert_eq!(store2.block_count(), 1);

        // Add a new block to store1
        let id2 = store1
            .insert_block(
                Some(&id1),
                Some(&id1),
                Role::Model,
                BlockKind::Text,
                "Second",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Generate incremental sync payload
        let frontiers = store2.frontier();
        let payload = store1.ops_since(&frontiers);

        // The new block should appear in new_blocks (as a snapshot)
        assert_eq!(
            payload.new_blocks.len(),
            1,
            "new block should be in new_blocks"
        );
        assert_eq!(payload.new_blocks[0].id, id2);

        // The new block's header should NOT also appear in updated_headers —
        // the snapshot already contains all header data; sending both is redundant.
        let redundant = payload.updated_headers.iter().any(|h| h.id == id2);
        assert!(
            !redundant,
            "new block header should not appear in updated_headers (snapshot is sufficient)"
        );

        // Merge into store2 and verify all header fields match
        store2.merge_ops(payload).unwrap();
        assert_eq!(store2.block_count(), 2);

        let snap1 = store1.get_block_snapshot(&id2).unwrap();
        let snap2 = store2.get_block_snapshot(&id2).unwrap();
        assert_eq!(snap2.role, snap1.role);
        assert_eq!(snap2.kind, snap1.kind);
        assert_eq!(snap2.status, snap1.status);
        assert_eq!(snap2.parent_id, snap1.parent_id);
        assert_eq!(snap2.content, snap1.content);
        assert_eq!(snap2.compacted, snap1.compacted);
        assert_eq!(snap2.collapsed, snap1.collapsed);
        assert_eq!(snap2.ephemeral, snap1.ephemeral);
    }

    // ── Sync: header propagation ──────────────────────────────────────

    #[test]
    fn test_sync_propagates_status_change() {
        let ctx = ContextId::new();
        let mut store1 = BlockStore::new(ctx, PrincipalId::new());
        let mut store2 = BlockStore::new(ctx, PrincipalId::new());

        let id = store1
            .insert_block(
                None,
                None,
                Role::Model,
                BlockKind::ToolCall,
                "{}",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Initial sync
        let payload = store1.ops_since(&HashMap::new());
        store2.merge_ops(payload).unwrap();

        // Store1 changes status
        store1.set_status(&id, Status::Done).unwrap();

        // Incremental sync — header update should propagate
        let frontiers = store2.frontier();
        let payload = store1.ops_since(&frontiers);
        assert!(
            !payload.updated_headers.is_empty(),
            "should include updated header"
        );
        store2.merge_ops(payload).unwrap();

        let snap = store2.get_block_snapshot(&id).unwrap();
        assert_eq!(
            snap.status,
            Status::Done,
            "status change should propagate via sync"
        );
    }

    #[test]
    fn test_sync_propagates_collapsed() {
        let ctx = ContextId::new();
        let mut store1 = BlockStore::new(ctx, PrincipalId::new());
        let mut store2 = BlockStore::new(ctx, PrincipalId::new());

        let id = store1
            .insert_block(
                None,
                None,
                Role::Model,
                BlockKind::Thinking,
                "Thinking...",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Initial sync
        let payload = store1.ops_since(&HashMap::new());
        store2.merge_ops(payload).unwrap();

        // Store1 collapses the block
        store1.set_collapsed(&id, true).unwrap();

        let frontiers = store2.frontier();
        let payload = store1.ops_since(&frontiers);
        store2.merge_ops(payload).unwrap();

        assert!(
            store2.get_block_snapshot(&id).unwrap().collapsed,
            "collapsed state should propagate via sync"
        );
    }

    // ── Sync: deletion propagation ────────────────────────────────────

    #[test]
    fn test_sync_propagates_deletion() {
        let ctx = ContextId::new();
        let mut store1 = BlockStore::new(ctx, PrincipalId::new());
        let mut store2 = BlockStore::new(ctx, PrincipalId::new());

        let id1 = store1
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Keep",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let id2 = store1
            .insert_block(
                None,
                Some(&id1),
                Role::User,
                BlockKind::Text,
                "Delete me",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Initial sync
        let payload = store1.ops_since(&HashMap::new());
        store2.merge_ops(payload).unwrap();
        assert_eq!(store2.block_count(), 2);

        // Store1 deletes a block
        store1.delete_block(&id2).unwrap();

        // Incremental sync — deletion should propagate
        let frontiers = store2.frontier();
        let payload = store1.ops_since(&frontiers);
        assert_eq!(
            payload.deleted_blocks.len(),
            1,
            "should include deleted block ID"
        );
        assert_eq!(payload.deleted_blocks[0], id2);
        store2.merge_ops(payload).unwrap();

        assert_eq!(
            store2.block_count(),
            1,
            "deletion should propagate via sync"
        );
        assert!(store2.get_block_snapshot(&id2).is_none());
    }

    // ── Sync: order_key propagation ───────────────────────────────────

    #[test]
    fn test_sync_preserves_order_key() {
        let ctx = ContextId::new();
        let mut store1 = BlockStore::new(ctx, PrincipalId::new());
        let mut store2 = BlockStore::new(ctx, PrincipalId::new());

        let id1 = store1
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "First",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let id2 = store1
            .insert_block(
                None,
                Some(&id1),
                Role::User,
                BlockKind::Text,
                "Second",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _id3 = store1
            .insert_block(
                None,
                Some(&id2),
                Role::User,
                BlockKind::Text,
                "Third",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Sync to store2
        let payload = store1.ops_since(&HashMap::new());
        store2.merge_ops(payload).unwrap();

        // Ordering should match
        let order1: Vec<_> = store1
            .blocks_ordered()
            .iter()
            .map(|b| b.content.clone())
            .collect();
        let order2: Vec<_> = store2
            .blocks_ordered()
            .iter()
            .map(|b| b.content.clone())
            .collect();
        assert_eq!(
            order1, order2,
            "synced store should preserve document order"
        );
    }

    // ── Lamport clock ─────────────────────────────────────────────────

    #[test]
    fn test_lamport_clock_advances() {
        let mut store = test_store();

        let id = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Hello",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Each mutation bumps the Lamport clock
        // Verify via mutations — each set_status bumps the Lamport clock
        store.set_status(&id, Status::Running).unwrap();
        store.set_status(&id, Status::Done).unwrap();

        // The lamport clock should be > 0 now
        assert!(store.lamport_clock > 0);
    }

    #[test]
    fn test_lamport_clock_advances_on_merge() {
        let ctx = ContextId::new();
        let mut store1 = BlockStore::new(ctx, PrincipalId::new());
        let mut store2 = BlockStore::new(ctx, PrincipalId::new());

        // Store1 does many operations, advancing its Lamport clock
        let id = store1
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Hello",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        for _ in 0..10 {
            store1.set_status(&id, Status::Running).unwrap();
            store1.set_status(&id, Status::Done).unwrap();
        }
        let store1_clock = store1.lamport_clock;

        // Sync to store2 (which has Lamport = 0)
        let payload = store1.ops_since(&HashMap::new());
        store2.merge_ops(payload).unwrap();

        // Store2's clock should have advanced past store1's
        assert!(
            store2.lamport_clock > store1_clock,
            "merge should advance Lamport clock past remote: {} > {}",
            store2.lamport_clock,
            store1_clock
        );
    }

    // ── Order key: agent suffix ───────────────────────────────────────

    #[test]
    fn test_order_key_has_agent_suffix() {
        let mut store = test_store();
        let suffix = store.agent_order_suffix();

        let id = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Hello",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let snap = store.get_block_snapshot(&id).unwrap();
        let key = snap.order_key.expect("snapshot should have order_key");
        assert!(
            key.ends_with(&suffix),
            "order_key '{}' should end with agent suffix '{}'",
            key,
            suffix
        );
    }

    #[test]
    fn test_concurrent_inserts_no_interleaving() {
        // Two stores inserting sequentially at the end — after sync,
        // blocks from each store should be grouped, not interleaved.
        let ctx = ContextId::new();
        let agent1 = PrincipalId::new();
        let agent2 = PrincipalId::new();
        let mut store1 = BlockStore::new(ctx, agent1);
        let mut store2 = BlockStore::new(ctx, agent2);

        // Store1 inserts A, B
        let a = store1
            .insert_block(None, None, Role::User, BlockKind::Text, "A", Status::Done, ContentType::Plain)
            .unwrap();
        let _b = store1
            .insert_block(
                None,
                Some(&a),
                Role::User,
                BlockKind::Text,
                "B",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Store2 inserts C, D (independently)
        let c = store2
            .insert_block(None, None, Role::User, BlockKind::Text, "C", Status::Done, ContentType::Plain)
            .unwrap();
        let _d = store2
            .insert_block(
                None,
                Some(&c),
                Role::User,
                BlockKind::Text,
                "D",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Sync both ways
        let payload1 = store1.ops_since(&HashMap::new());
        let payload2 = store2.ops_since(&HashMap::new());
        store1.merge_ops(payload2).unwrap();
        store2.merge_ops(payload1).unwrap();

        // Both stores should see 4 blocks in the same order
        let order1: Vec<_> = store1
            .blocks_ordered()
            .iter()
            .map(|b| b.content.clone())
            .collect();
        let order2: Vec<_> = store2
            .blocks_ordered()
            .iter()
            .map(|b| b.content.clone())
            .collect();
        // Deterministic convergence still holds: both replicas agree on order.
        assert_eq!(order1, order2, "both stores should converge to same order");

        let a_pos = order1.iter().position(|c| c == "A").unwrap();
        let b_pos = order1.iter().position(|c| c == "B").unwrap();
        let c_pos = order1.iter().position(|c| c == "C").unwrap();
        let d_pos = order1.iter().position(|c| c == "D").unwrap();

        // Per-replica tick order is preserved (each replica's own counter is
        // monotonic): A precedes B, C precedes D.
        assert!(a_pos < b_pos, "A should precede B (tick order within replica 1)");
        assert!(c_pos < d_pos, "C should precede D (tick order within replica 2)");

        // NOTE: cross-replica *grouping* (A,B adjacent; C,D adjacent) is no longer
        // guaranteed. With tick-derived order_keys, two replicas that independently
        // assign the same tick interleave by tick instead of grouping by author.
        // Non-interleaving across writers is a multi-writer-timeline property that
        // is explicitly deferred — see docs/hyoushigi.md ("single-writer first").
    }

    #[test]
    fn test_inserts_get_monotonic_ticks() {
        let mut store = test_store();
        let mut ids = Vec::new();
        for i in 0..5 {
            ids.push(
                store
                    .insert_block(
                        ids.last(),
                        ids.last(),
                        Role::Model,
                        BlockKind::Text,
                        format!("line {i}"),
                        Status::Done,
                        ContentType::Plain,
                    )
                    .unwrap(),
            );
        }
        // Ticks are 0..5, gap-free, in insertion order.
        let ticks: Vec<i64> = ids
            .iter()
            .map(|id| store.get_block_snapshot(id).unwrap().tick.unwrap().get())
            .collect();
        assert_eq!(ticks, vec![0, 1, 2, 3, 4]);

        // And blocks_ordered matches tick order.
        let ordered: Vec<String> = store.blocks_ordered().iter().map(|b| b.content.clone()).collect();
        assert_eq!(ordered, vec!["line 0", "line 1", "line 2", "line 3", "line 4"]);
    }

    #[test]
    fn test_tick_order_keys_are_bounded() {
        // The old append path grew order_keys by a char per insert (the O(N^3)
        // cliff). Tick-derived keys stay fixed-width no matter how many blocks.
        let mut store = test_store();
        let mut last = None;
        for _ in 0..500 {
            last = Some(
                store
                    .insert_block(
                        last.as_ref(),
                        last.as_ref(),
                        Role::Model,
                        BlockKind::Text,
                        "x",
                        Status::Done,
                        ContentType::Plain,
                    )
                    .unwrap(),
            );
        }
        let lens: std::collections::HashSet<usize> = store
            .blocks_ordered()
            .iter()
            .map(|b| b.order_key.as_ref().unwrap().len())
            .collect();
        assert_eq!(lens.len(), 1, "all tick-derived order_keys share one bounded length");
    }

    #[test]
    fn test_normalize_rekeys_legacy_blocks() {
        // Simulate a pre-tick store: blocks with legacy order_keys and no tick.
        let mut store = test_store();
        for (key, text) in [("V", "first"), ("VV", "second"), ("VVV", "third")] {
            let id = store.new_block_id();
            let header = BlockHeader {
                id,
                parent_id: None,
                role: Role::User,
                kind: BlockKind::Text,
                status: Status::Done,
                compacted: false,
                collapsed: false,
                ephemeral: false,
                excluded: false,
                created_at: now_millis(),
                updated_at: 0,
                tool_kind: None,
                exit_code: None,
                is_error: false,
                status_at: 0,
                collapsed_at: 0,
                ephemeral_at: 0,
                excluded_at: 0,
                compacted_at: 0,
                tool_meta_at: 0,
                content_type: ContentType::Plain,
                content_type_at: 0,
            };
            // tick = None — legacy.
            let block = BlockContent::with_content(header, text, store.principal_id, key.to_string(), None);
            store.blocks.insert(id, block);
        }

        store.normalize_timeline();

        // Visual order preserved, every block now ticked 0..3, next_tick seeded.
        let ordered = store.blocks_ordered();
        let texts: Vec<String> = ordered.iter().map(|b| b.content.clone()).collect();
        assert_eq!(texts, vec!["first", "second", "third"]);
        let ticks: Vec<i64> = ordered.iter().map(|b| b.tick.unwrap().get()).collect();
        assert_eq!(ticks, vec![0, 1, 2]);
        assert_eq!(store.next_tick, 3);
        // Legacy "V"/"VV"/"VVV" keys are gone — all bounded tick keys now.
        assert!(ordered.iter().all(|b| b.order_key.as_ref().unwrap().starts_with("V0")));
    }

    #[test]
    fn test_incremental_text_sync_after_merge() {
        // Verifies that ops_since sends full DTE ops for new blocks,
        // so the receiver gets causal history and subsequent incremental
        // text ops can merge successfully.
        let ctx = ContextId::new();
        let mut store1 = BlockStore::new(ctx, PrincipalId::new());
        let mut store2 = BlockStore::new(ctx, PrincipalId::new());

        let id = store1
            .insert_block(None, None, Role::Model, BlockKind::Text, "", Status::Done, ContentType::Plain)
            .unwrap();

        // Sync to store2
        let payload = store1.ops_since(&HashMap::new());
        store2.merge_ops(payload).unwrap();
        assert_eq!(store2.block_count(), 1);

        // Store1 appends text
        store1.append_text(&id, "Hello").unwrap();

        // Try incremental sync using store2's frontier
        let frontiers = store2.frontier();
        let payload = store1.ops_since(&frontiers);
        let result = store2.merge_ops(payload);

        assert!(result.is_ok(), "incremental text sync failed: {:?}", result);
        let snap = store2.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.content, "Hello");
    }

    #[test]
    fn test_snapshot_cbor_roundtrip() {
        let mut store = test_store();
        store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Hello",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let snapshot = store.snapshot();
        let bytes = kaijutsu_types::codec::encode(&snapshot).expect("serialize");
        let restored: StoreSnapshot = kaijutsu_types::codec::decode(&bytes).expect("deserialize");

        assert_eq!(restored.blocks.len(), 1);
        assert_eq!(restored.blocks[0].content, "Hello");
    }

    /// The reasoning-continuity signature set on a Thinking block survives the
    /// snapshot → cbor → snapshot round-trip (the path persistence and
    /// fork-copy take). Without this, a rehydrated thinking block would lose
    /// its verifier and the next Anthropic turn would 400.
    #[test]
    fn signature_survives_snapshot_cbor_roundtrip() {
        let mut store = test_store();
        let id = store
            .insert_block(
                None,
                None,
                Role::Model,
                BlockKind::Thinking,
                "reasoning",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        store.set_signature(&id, Some("sig_xyz".into())).unwrap();
        // Visible on the live snapshot…
        let snap = store.snapshot();
        assert_eq!(snap.blocks[0].signature.as_deref(), Some("sig_xyz"));

        // …and after a persistence round-trip.
        let bytes = kaijutsu_types::codec::encode(&snap).expect("serialize");
        let restored: StoreSnapshot = kaijutsu_types::codec::decode(&bytes).expect("deserialize");
        assert_eq!(restored.blocks[0].signature.as_deref(), Some("sig_xyz"));
    }

    /// Verify that CrdtBlockStore → snapshot → BlockDocument preserves block ordering.
    ///
    /// This is the exact path used by the rendering pipeline:
    /// sync_main_cell_to_conversation takes a store snapshot and rebuilds a
    /// BlockDocument via from_snapshot. If ordering diverges, blocks appear
    /// out of order on screen.
    #[test]
    fn test_store_to_document_ordering_consistency() {
        use crate::document::BlockDocument;

        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let mut store = BlockStore::new(ctx, agent);

        // Create a realistic conversation: user, model, tool call, tool result, model
        let b1 = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Hello",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let b2 = store
            .insert_block(
                None,
                Some(&b1),
                Role::Model,
                BlockKind::Text,
                "Let me check...",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let b3 = store
            .insert_block(
                Some(&b2),
                Some(&b2),
                Role::Model,
                BlockKind::ToolCall,
                "search",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let b4 = store
            .insert_block(
                Some(&b3),
                Some(&b3),
                Role::Model,
                BlockKind::ToolResult,
                "results here",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let b5 = store
            .insert_block(
                None,
                Some(&b4),
                Role::Model,
                BlockKind::Text,
                "Based on the results...",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Take snapshot (what sync_main_cell does)
        let store_snap = store.snapshot();
        let store_ids: Vec<BlockId> = store_snap.blocks.iter().map(|b| b.id).collect();

        // Convert to DocumentSnapshot and rebuild BlockDocument (the rendering path)
        let doc_snap = crate::DocumentSnapshot {
            context_id: store_snap.context_id,
            blocks: store_snap.blocks,
            version: 1,
        };
        let doc = BlockDocument::from_snapshot(doc_snap, agent);
        let doc_ids: Vec<BlockId> = doc.blocks_ordered().iter().map(|b| b.id).collect();

        // Ordering must match
        assert_eq!(
            store_ids, doc_ids,
            "Store and Document block ordering diverged"
        );
        assert_eq!(store_ids, vec![b1, b2, b3, b4, b5]);
    }

    // =====================================================================
    // fork_filtered tests
    // =====================================================================

    #[test]
    fn test_fork_filtered_empty_filter_includes_all() {
        let mut store = test_store();

        let b1 = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "hello",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _b2 = store
            .insert_block(
                Some(&b1),
                Some(&b1),
                Role::Model,
                BlockKind::Text,
                "world",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let filter = ForkBlockFilter::default();
        let new_ctx = ContextId::new();
        let new_agent = PrincipalId::new();
        let forked = store.fork_filtered(new_ctx, new_agent, u64::MAX, &filter);

        assert_eq!(forked.block_count(), 2);
    }

    #[test]
    fn test_fork_filtered_exclude_kinds() {
        let mut store = test_store();

        let b1 = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "hello",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _b2 = store
            .insert_block(
                Some(&b1),
                Some(&b1),
                Role::Model,
                BlockKind::Thinking,
                "hmm",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _b3 = store
            .insert_block(
                Some(&b1),
                None,
                Role::Model,
                BlockKind::Text,
                "response",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let mut filter = ForkBlockFilter::default();
        filter.exclude_kinds.insert("Thinking".to_string());

        let new_ctx = ContextId::new();
        let new_agent = PrincipalId::new();
        let forked = store.fork_filtered(new_ctx, new_agent, u64::MAX, &filter);

        assert_eq!(forked.block_count(), 2, "Thinking block should be excluded");
    }

    #[test]
    fn test_fork_filtered_exclude_roles() {
        let mut store = test_store();

        let b1 = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "hello",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _b2 = store
            .insert_block(
                Some(&b1),
                Some(&b1),
                Role::Tool,
                BlockKind::ToolResult,
                "result",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _b3 = store
            .insert_block(
                Some(&b1),
                None,
                Role::Model,
                BlockKind::Text,
                "response",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let mut filter = ForkBlockFilter::default();
        filter.exclude_roles.insert("Tool".to_string());

        let new_ctx = ContextId::new();
        let new_agent = PrincipalId::new();
        let forked = store.fork_filtered(new_ctx, new_agent, u64::MAX, &filter);

        assert_eq!(
            forked.block_count(),
            2,
            "Tool role blocks should be excluded"
        );
    }

    #[test]
    fn test_fork_filtered_exclude_compacted() {
        let mut store = test_store();

        let b1 = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "old message",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _b2 = store
            .insert_block(
                Some(&b1),
                Some(&b1),
                Role::Model,
                BlockKind::Text,
                "summary",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Mark b1 as compacted via SyncPayload with updated header
        let mut header = BlockHeader::from_snapshot(&store.get_block_snapshot(&b1).unwrap());
        header.compacted = true;
        header.updated_at = 1000; // Ensure LWW wins
        let payload = SyncPayload {
            block_ops: vec![],
            new_blocks: vec![],
            updated_headers: vec![header],
            deleted_blocks: vec![],
        };
        store.merge_ops(payload).unwrap();

        // Verify compacted flag took effect
        assert!(
            store.get_block_snapshot(&b1).unwrap().compacted,
            "Block should be compacted"
        );

        let mut filter = ForkBlockFilter::default();
        filter.exclude_compacted = true;

        let new_ctx = ContextId::new();
        let new_agent = PrincipalId::new();
        let forked = store.fork_filtered(new_ctx, new_agent, u64::MAX, &filter);

        assert_eq!(
            forked.block_count(),
            1,
            "Compacted block should be excluded"
        );
    }

    #[test]
    fn test_fork_filtered_selection_keeps_tail() {
        let mut store = test_store();

        let b1 = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "msg 1",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let b2 = store
            .insert_block(
                Some(&b1),
                Some(&b1),
                Role::Model,
                BlockKind::Text,
                "msg 2",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let b3 = store
            .insert_block(
                Some(&b2),
                Some(&b2),
                Role::User,
                BlockKind::Text,
                "msg 3",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _b4 = store
            .insert_block(
                Some(&b3),
                Some(&b3),
                Role::Model,
                BlockKind::Text,
                "msg 4",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // The retired `max_blocks: Some(2)` is now `--include end-2:` — over a
        // 4-block log that's positions 2..4 (the last two).
        let filter = ForkBlockFilter {
            selection: Some(IntervalSet::from_ranges([2..4])),
            ..Default::default()
        };

        let new_ctx = ContextId::new();
        let new_agent = PrincipalId::new();
        let forked = store.fork_filtered(new_ctx, new_agent, u64::MAX, &filter);

        assert_eq!(forked.block_count(), 2, "Should only keep last 2 blocks");
    }

    /// A log authored by two principals, interleaved in TIME: document order
    /// (by tick/order_key) is t0,t1,t2,t3, but `BTreeMap<BlockId>` order is
    /// principal-major — all of `p_lo`'s blocks (ticks 1,3) then all of
    /// `p_hi`'s (ticks 0,2) → [1,3,0,2]. Any position-dependent fork op that
    /// walks `blocks.values()` instead of order_key order will pick wrong.
    fn interleaved_principal_log() -> BlockStore {
        let ctx = ContextId::new();
        let (a, b) = (PrincipalId::new(), PrincipalId::new());
        let (p_lo, p_hi) = if a < b { (a, b) } else { (b, a) };
        let mut store = BlockStore::new(ctx, p_lo);
        let new_blocks = vec![
            snap_for(ctx, p_hi, 0, 0),
            snap_for(ctx, p_lo, 0, 1),
            snap_for(ctx, p_hi, 1, 2),
            snap_for(ctx, p_lo, 1, 3),
        ];
        store
            .merge_ops(SyncPayload {
                block_ops: vec![],
                new_blocks,
                updated_headers: vec![],
                deleted_blocks: vec![],
            })
            .unwrap();
        store
    }

    /// Ticks of a store's blocks in document order.
    fn doc_ticks(store: &BlockStore) -> Vec<i64> {
        store
            .block_ids_ordered()
            .iter()
            .map(|id| store.get_block_snapshot(id).unwrap().tick.unwrap().get())
            .collect()
    }

    #[test]
    fn fork_filtered_selection_indexes_document_order_not_blockid() {
        let store = interleaved_principal_log();
        // Fixture sanity: document order is by tick, BTreeMap order is not.
        assert_eq!(doc_ticks(&store), vec![0, 1, 2, 3], "fixture: document order is by tick");

        // `end-2:` over the 4-block document = positions 2..4 = ticks {2,3}.
        let filter = ForkBlockFilter {
            selection: Some(IntervalSet::from_ranges([2..4])),
            ..Default::default()
        };
        let forked = store.fork_filtered(ContextId::new(), PrincipalId::new(), u64::MAX, &filter);
        assert_eq!(
            doc_ticks(&forked),
            vec![2, 3],
            "selection must index document (order_key) order, not principal-major BlockId order"
        );
    }

    #[test]
    fn test_fork_filtered_exclude_block_ids() {
        let mut store = test_store();

        let b1 = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "keep me",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let b2 = store
            .insert_block(
                Some(&b1),
                Some(&b1),
                Role::Model,
                BlockKind::Text,
                "exclude me",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _b3 = store
            .insert_block(
                Some(&b2),
                Some(&b2),
                Role::User,
                BlockKind::Text,
                "also keep",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let mut filter = ForkBlockFilter::default();
        filter.exclude_block_ids.insert(b2.to_key());

        let new_ctx = ContextId::new();
        let new_agent = PrincipalId::new();
        let forked = store.fork_filtered(new_ctx, new_agent, u64::MAX, &filter);

        assert_eq!(forked.block_count(), 2, "Specific block should be excluded");
    }

    #[test]
    fn test_fork_filtered_combined_criteria() {
        let mut store = test_store();

        let b1 = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "question",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _b2 = store
            .insert_block(
                Some(&b1),
                Some(&b1),
                Role::Model,
                BlockKind::Thinking,
                "thinking...",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _b3 = store
            .insert_block(
                Some(&b1),
                None,
                Role::Tool,
                BlockKind::ToolResult,
                "tool output",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _b4 = store
            .insert_block(
                Some(&b1),
                None,
                Role::Model,
                BlockKind::Text,
                "answer",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let mut filter = ForkBlockFilter::default();
        filter.exclude_kinds.insert("Thinking".to_string());
        filter.exclude_roles.insert("Tool".to_string());

        let new_ctx = ContextId::new();
        let new_agent = PrincipalId::new();
        let forked = store.fork_filtered(new_ctx, new_agent, u64::MAX, &filter);

        // Should keep: user text + model text = 2
        assert_eq!(
            forked.block_count(),
            2,
            "Thinking + Tool blocks should be excluded"
        );
    }

    // =====================================================================
    // Lamport clock: DTE-only merge
    // =====================================================================

    #[test]
    fn test_lamport_clock_advances_after_dte_only_sync() {
        // Two stores for the same context
        let ctx = ContextId::new();
        let mut store1 = BlockStore::new(ctx, PrincipalId::new());
        let mut store2 = BlockStore::new(ctx, PrincipalId::new());

        // Create a block in store1 and sync it to store2 (initial sync)
        let id = store1
            .insert_block(None, None, Role::Model, BlockKind::Text, "", Status::Done, ContentType::Plain)
            .unwrap();
        let payload = store1.ops_since(&HashMap::new());
        store2.merge_ops(payload).unwrap();
        assert_eq!(store2.block_count(), 1);

        // Record store2's clock after initial sync
        let clock_after_initial = store2.lamport_clock;

        // Store1 appends text (DTE ops only — no header changes, no new blocks)
        store1.append_text(&id, "Hello world").unwrap();

        // Generate incremental sync and strip headers to simulate a DTE-only payload.
        // This happens when a relay or middleware forwards only the DTE delta
        // without header metadata (e.g., ephemeral streaming, partial sync).
        let frontiers = store2.frontier();
        let mut payload = store1.ops_since(&frontiers);
        payload.updated_headers.clear(); // DTE-only: no header updates

        // Confirm this is a DTE-only payload
        assert!(!payload.block_ops.is_empty(), "payload should have DTE ops");
        assert!(
            payload.new_blocks.is_empty(),
            "payload should have no new blocks"
        );
        assert!(
            payload.updated_headers.is_empty(),
            "payload should have no updated headers"
        );

        // Merge into store2
        store2.merge_ops(payload).unwrap();

        // Verify the text arrived
        let snap = store2.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.content, "Hello world");

        // The bug: store2's Lamport clock should have advanced, but it didn't
        assert!(
            store2.lamport_clock > clock_after_initial,
            "Lamport clock should advance after DTE-only merge: got {} (should be > {})",
            store2.lamport_clock,
            clock_after_initial
        );
    }

    // =====================================================================
    // fork_at_version / fork_filtered timestamp semantics
    // =====================================================================

    #[test]
    fn test_fork_at_version_uses_wall_clock_timestamp() {
        // Create blocks with explicit wall-clock timestamps to verify
        // fork_at_version filters by created_at (millis), not by version counter.
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let mut store = BlockStore::new(ctx, agent);

        // Insert 3 blocks with distinct created_at timestamps.
        // We construct BlockContent directly so we can control created_at.
        let ts_early = 1_000_000u64; // 1 second after epoch
        let ts_mid = 2_000_000u64; // 2 seconds
        let ts_late = 3_000_000u64; // 3 seconds

        let id1 = store.new_block_id();
        let h1 = BlockHeader {
            id: id1,
            parent_id: None,
            role: Role::User,
            kind: BlockKind::Text,
            status: Status::Done,
            compacted: false,
            collapsed: false,
            ephemeral: false,
            excluded: false,
            created_at: ts_early,
            updated_at: 0,
            tool_kind: None,
            exit_code: None,
            is_error: false,
            status_at: 0,
            collapsed_at: 0,
            ephemeral_at: 0,
            excluded_at: 0,
            compacted_at: 0,
            tool_meta_at: 0,
            content_type: ContentType::Plain,
            content_type_at: 0,
        };
        let b1 = BlockContent::with_content(h1, "early", agent, "V".to_string(), None);
        store.blocks.insert(id1, b1);
        store.version += 1;

        let id2 = store.new_block_id();
        let h2 = BlockHeader {
            id: id2,
            parent_id: Some(id1),
            role: Role::Model,
            kind: BlockKind::Text,
            status: Status::Done,
            compacted: false,
            collapsed: false,
            ephemeral: false,
            excluded: false,
            created_at: ts_mid,
            updated_at: 0,
            tool_kind: None,
            exit_code: None,
            is_error: false,
            status_at: 0,
            collapsed_at: 0,
            ephemeral_at: 0,
            excluded_at: 0,
            compacted_at: 0,
            tool_meta_at: 0,
            content_type: ContentType::Plain,
            content_type_at: 0,
        };
        let b2 = BlockContent::with_content(h2, "mid", agent, "W".to_string(), None);
        store.blocks.insert(id2, b2);
        store.version += 1;

        let id3 = store.new_block_id();
        let h3 = BlockHeader {
            id: id3,
            parent_id: Some(id2),
            role: Role::User,
            kind: BlockKind::Text,
            status: Status::Done,
            compacted: false,
            collapsed: false,
            ephemeral: false,
            excluded: false,
            created_at: ts_late,
            updated_at: 0,
            tool_kind: None,
            exit_code: None,
            is_error: false,
            status_at: 0,
            collapsed_at: 0,
            ephemeral_at: 0,
            excluded_at: 0,
            compacted_at: 0,
            tool_meta_at: 0,
            content_type: ContentType::Plain,
            content_type_at: 0,
        };
        let b3 = BlockContent::with_content(h3, "late", agent, "X".to_string(), None);
        store.blocks.insert(id3, b3);
        store.version += 1;

        assert_eq!(store.block_count(), 3);
        assert_eq!(store.version(), 3);

        // Fork at a timestamp between ts_mid and ts_late.
        // Should include "early" and "mid", exclude "late".
        let cutoff = 2_500_000u64;
        let new_ctx = ContextId::new();
        let new_agent = PrincipalId::new();
        let forked = store.fork_at_version(new_ctx, new_agent, cutoff);

        assert_eq!(
            forked.block_count(),
            2,
            "Fork at timestamp {} should include 2 blocks (created_at {} and {}), not block with created_at {}",
            cutoff,
            ts_early,
            ts_mid,
            ts_late
        );

        // Verify the correct blocks are present by checking content
        let snaps: Vec<BlockSnapshot> = forked.blocks_ordered();
        let contents: Vec<&str> = snaps.iter().map(|s| s.content.as_str()).collect();
        assert!(contents.contains(&"early"), "Should include early block");
        assert!(contents.contains(&"mid"), "Should include mid block");
        assert!(!contents.contains(&"late"), "Should NOT include late block");
    }

    #[test]
    fn test_fork_filtered_uses_wall_clock_timestamp() {
        // Same as above but through fork_filtered to verify both paths.
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let mut store = BlockStore::new(ctx, agent);

        let ts_early = 1_000_000u64;
        let ts_late = 3_000_000u64;

        let id1 = store.new_block_id();
        let h1 = BlockHeader {
            id: id1,
            parent_id: None,
            role: Role::User,
            kind: BlockKind::Text,
            status: Status::Done,
            compacted: false,
            collapsed: false,
            ephemeral: false,
            excluded: false,
            created_at: ts_early,
            updated_at: 0,
            tool_kind: None,
            exit_code: None,
            is_error: false,
            status_at: 0,
            collapsed_at: 0,
            ephemeral_at: 0,
            excluded_at: 0,
            compacted_at: 0,
            tool_meta_at: 0,
            content_type: ContentType::Plain,
            content_type_at: 0,
        };
        store.blocks.insert(
            id1,
            BlockContent::with_content(h1, "keep", agent, "V".to_string(), None),
        );
        store.version += 1;

        let id2 = store.new_block_id();
        let h2 = BlockHeader {
            id: id2,
            parent_id: Some(id1),
            role: Role::Model,
            kind: BlockKind::Text,
            status: Status::Done,
            compacted: false,
            collapsed: false,
            ephemeral: false,
            excluded: false,
            created_at: ts_late,
            updated_at: 0,
            tool_kind: None,
            exit_code: None,
            is_error: false,
            status_at: 0,
            collapsed_at: 0,
            ephemeral_at: 0,
            excluded_at: 0,
            compacted_at: 0,
            tool_meta_at: 0,
            content_type: ContentType::Plain,
            content_type_at: 0,
        };
        store.blocks.insert(
            id2,
            BlockContent::with_content(h2, "drop", agent, "W".to_string(), None),
        );
        store.version += 1;

        // Fork at cutoff between the two timestamps, with empty filter
        let cutoff = 2_000_000u64;
        let filter = ForkBlockFilter::default();
        let new_ctx = ContextId::new();
        let new_agent = PrincipalId::new();
        let forked = store.fork_filtered(new_ctx, new_agent, cutoff, &filter);

        assert_eq!(
            forked.block_count(),
            1,
            "Only the early block should survive the timestamp cutoff"
        );
        let snaps = forked.blocks_ordered();
        assert_eq!(snaps[0].content, "keep");
    }

    /// Per-field LWW: concurrent mutations to different fields are both preserved.
    /// (Was: regression baseline showing whole-header LWW dropping one change.)
    #[test]
    fn test_lww_race_ephemeral_overwritten_by_status() {
        let ctx = ContextId::new();
        let agent_a = PrincipalId::new();
        let agent_b = PrincipalId::new();

        let mut store_a = BlockStore::new(ctx, agent_a);
        let block_id = store_a
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "test",
                Status::Running,
                ContentType::Plain,
            )
            .unwrap();

        // Sync to store_b so both have the same block
        let mut store_b = BlockStore::new(ctx, agent_b);
        let payload = store_a.ops_since(&HashMap::new());
        store_b.merge_ops(payload).unwrap();

        // Peer A sets ephemeral=true
        store_a.set_ephemeral(&block_id, true).unwrap();
        // Peer B sets status=Done (at the same logical time)
        store_b.set_status(&block_id, Status::Done).unwrap();

        // Merge A→B
        let payload_a = store_a.ops_since(&store_b.frontier());
        store_b.merge_ops(payload_a).unwrap();

        // Merge B→A
        let payload_b = store_b.ops_since(&store_a.frontier());
        store_a.merge_ops(payload_b).unwrap();

        let header_a = store_a.blocks.get(&block_id).unwrap().header();
        let header_b = store_b.blocks.get(&block_id).unwrap().header();

        // Both stores converge
        assert_eq!(
            header_a.status, header_b.status,
            "stores must converge on status"
        );
        assert_eq!(
            header_a.ephemeral, header_b.ephemeral,
            "stores must converge on ephemeral"
        );

        // Per-field LWW: BOTH concurrent changes are preserved
        assert!(
            header_a.ephemeral,
            "ephemeral=true must survive (independent field)"
        );
        assert_eq!(
            header_a.status,
            Status::Done,
            "status=Done must survive (independent field)"
        );
    }

    /// Per-field LWW: different fields with different timestamps both preserved.
    #[test]
    fn test_per_field_lww_independent_merge() {
        let ctx = ContextId::new();
        let agent_a = PrincipalId::new();
        let agent_b = PrincipalId::new();

        let mut store_a = BlockStore::new(ctx, agent_a);
        let block_id = store_a
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "test",
                Status::Running,
                ContentType::Plain,
            )
            .unwrap();

        let mut store_b = BlockStore::new(ctx, agent_b);
        let payload = store_a.ops_since(&HashMap::new());
        store_b.merge_ops(payload).unwrap();

        // A: set collapsed=true (tick 1 after sync)
        store_a.set_collapsed(&block_id, true).unwrap();
        // A: then set status=Done (tick 2) — higher ts
        store_a.set_status(&block_id, Status::Done).unwrap();

        // B: just set ephemeral=true (tick 1 after sync)
        store_b.set_ephemeral(&block_id, true).unwrap();

        // Merge both ways
        let payload_a = store_a.ops_since(&store_b.frontier());
        store_b.merge_ops(payload_a).unwrap();
        let payload_b = store_b.ops_since(&store_a.frontier());
        store_a.merge_ops(payload_b).unwrap();

        let ha = store_a.blocks.get(&block_id).unwrap().header();
        let hb = store_b.blocks.get(&block_id).unwrap().header();

        // All three field changes survive
        assert_eq!(ha.status, Status::Done);
        assert!(ha.collapsed);
        assert!(ha.ephemeral);

        // Convergence
        assert_eq!(ha.status, hb.status);
        assert_eq!(ha.collapsed, hb.collapsed);
        assert_eq!(ha.ephemeral, hb.ephemeral);
    }

    /// Per-field LWW: same field, higher timestamp wins.
    #[test]
    fn test_per_field_lww_same_field_higher_ts_wins() {
        let ctx = ContextId::new();
        let agent_a = PrincipalId::new();
        let agent_b = PrincipalId::new();

        let mut store_a = BlockStore::new(ctx, agent_a);
        let block_id = store_a
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "test",
                Status::Running,
                ContentType::Plain,
            )
            .unwrap();

        let mut store_b = BlockStore::new(ctx, agent_b);
        let payload = store_a.ops_since(&HashMap::new());
        store_b.merge_ops(payload).unwrap();

        // A sets status to Done
        store_a.set_status(&block_id, Status::Done).unwrap();

        // B does two ticks before setting status to Error → higher Lamport ts
        store_b.set_collapsed(&block_id, true).unwrap(); // tick to advance clock
        store_b.set_status(&block_id, Status::Error).unwrap();

        // Merge
        let payload_a = store_a.ops_since(&store_b.frontier());
        store_b.merge_ops(payload_a).unwrap();
        let payload_b = store_b.ops_since(&store_a.frontier());
        store_a.merge_ops(payload_b).unwrap();

        let ha = store_a.blocks.get(&block_id).unwrap().header();
        let hb = store_b.blocks.get(&block_id).unwrap().header();

        // B's status wins (higher timestamp)
        assert_eq!(ha.status, Status::Error, "higher-ts status should win");
        assert_eq!(ha.status, hb.status, "stores must converge");
    }

    /// Per-field LWW tiebreaker: equal timestamps, greater value wins.
    /// Both peers must converge to the same result.
    #[test]
    fn test_per_field_lww_tiebreaker_convergence() {
        let ctx = ContextId::new();
        let agent_a = PrincipalId::new();
        let agent_b = PrincipalId::new();

        let mut store_a = BlockStore::new(ctx, agent_a);
        let block_id = store_a
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "test",
                Status::Pending,
                ContentType::Plain,
            )
            .unwrap();

        let mut store_b = BlockStore::new(ctx, agent_b);
        let payload = store_a.ops_since(&HashMap::new());
        store_b.merge_ops(payload).unwrap();

        // Both peers set status at the same Lamport tick.
        // A sets Done, B sets Error. Error > Done, so Error should win.
        store_a.set_status(&block_id, Status::Done).unwrap();
        store_b.set_status(&block_id, Status::Error).unwrap();

        // Merge A→B then B→A
        let payload_a = store_a.ops_since(&store_b.frontier());
        store_b.merge_ops(payload_a).unwrap();
        let payload_b = store_b.ops_since(&store_a.frontier());
        store_a.merge_ops(payload_b).unwrap();

        let ha = store_a.blocks.get(&block_id).unwrap().header();
        let hb = store_b.blocks.get(&block_id).unwrap().header();

        // Both converge to Error (greater value wins on tie)
        assert_eq!(ha.status, Status::Error, "Error > Done on tiebreak");
        assert_eq!(ha.status, hb.status, "stores must converge");
    }

    // ── Order/tick decoupling + seq lanes (design §2, §3) ─────────────────

    /// Build a canonical-keyed, ticked snapshot under an explicit principal.
    fn snap_for(
        ctx: ContextId,
        principal: PrincipalId,
        seq: u64,
        tick: i64,
    ) -> BlockSnapshot {
        let key = format!("V{}AAAA", base62_encode_padded(tick, 11));
        crate::BlockSnapshotBuilder::new(BlockId::new(ctx, principal, seq), BlockKind::Text)
            .tick(Tick::new(tick))
            .order_key(key)
            .content(format!("beat-{tick}"))
            .build()
    }

    /// T2 — THE locked regression. After restoring a snapshot and merging more
    /// blocks via the real restore path, a fresh local insert must sort LAST.
    /// Fails today: a stale `next_tick` mints a mid-document order_key.
    #[test]
    fn appends_after_merge_ops_sort_last() {
        let ctx = ContextId::new();
        let prin = PrincipalId::new();

        // Store A: 10 blocks (ticks 0..9).
        let mut store_a = BlockStore::new(ctx, prin);
        let mut last: Option<BlockId> = None;
        for _ in 0..10 {
            let id = store_a
                .insert_block(
                    last.as_ref(),
                    last.as_ref(),
                    Role::Model,
                    BlockKind::Text,
                    "line",
                    Status::Done,
                    ContentType::Plain,
                )
                .unwrap();
            last = Some(id);
        }

        // Restore into B via the real snapshot path.
        let snapshot = store_a.snapshot();
        let mut store_b = BlockStore::from_snapshot(snapshot, prin).unwrap();

        // Merge 5 more blocks (ticks 10..14, canonical keys) under a foreign
        // principal — exactly the oplog-replay restore shape.
        let foreign = PrincipalId::new();
        let new_blocks: Vec<BlockSnapshot> = (10..15)
            .map(|t| snap_for(ctx, foreign, (t - 10) as u64, t))
            .collect();
        store_b
            .merge_ops(SyncPayload {
                block_ops: vec![],
                new_blocks,
                updated_headers: vec![],
                deleted_blocks: vec![],
            })
            .unwrap();

        // A fresh local insert must sort LAST.
        let fresh = store_b
            .insert_block(
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "fresh",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let ordered = store_b.block_ids_ordered();
        assert_eq!(
            *ordered.last().unwrap(),
            fresh,
            "fresh append must sort last; order_key must not derive from a stale counter"
        );
    }

    /// T3 — tick high-water restore. After merging new_blocks with max tick N,
    /// a fresh insert stamps tick N+1. Pins tick *semantics* separately from
    /// T2's key ordering. Fails today: merge_ops never touches next_tick.
    #[test]
    fn merge_ops_restores_next_tick_high_water() {
        let ctx = ContextId::new();
        let prin = PrincipalId::new();
        let mut store = BlockStore::new(ctx, prin);

        let foreign = PrincipalId::new();
        let new_blocks: Vec<BlockSnapshot> =
            (0..5).map(|t| snap_for(ctx, foreign, t as u64, t)).collect();
        store
            .merge_ops(SyncPayload {
                block_ops: vec![],
                new_blocks,
                updated_headers: vec![],
                deleted_blocks: vec![],
            })
            .unwrap();

        // Max merged tick is 4 → fresh insert stamps tick 5.
        let fresh = store
            .insert_block(
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "fresh",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let snap = store.get_block_snapshot(&fresh).unwrap();
        assert_eq!(snap.tick, Some(Tick::new(5)), "tick high-water must advance past merged ticks");
    }

    /// T19b (design-chameleon-batch1-f2-notation §16, §11.4) — `insert_from_snapshot`
    /// (the runtime single-snapshot insert that `materialize_committed` rides) must
    /// bump `next_tick` past a carried tick, exactly as `next_position` /
    /// `merge_ops` / `fork` already do. Without it, a beat block inserted at tick 50
    /// leaves `next_tick` at 0, so the next ORDINARY conversation append stamps a
    /// low tick — and `calc_order_key`'s tick-derived path sorts the now-visible
    /// staff mid-document. Insert at tick 50; a subsequent `insert_block` must
    /// carry tick > 50 and sort LAST. *Red: `insert_from_snapshot` never advances
    /// `next_tick`.* Coordinate with the F0 ordering fork (same counter family).
    #[test]
    fn insert_from_snapshot_bumps_next_tick() {
        let ctx = ContextId::new();
        let prin = PrincipalId::new();
        let foreign = PrincipalId::new();
        let mut store = BlockStore::new(ctx, prin);

        // A beat block lands at tick 50 via the single-snapshot insert path (the
        // materialize-barrier shape). On a fresh store next_tick starts at 0.
        let beat_at_50 = snap_for(ctx, foreign, 0, 50);
        let beat_id = store.insert_from_snapshot(beat_at_50, None).unwrap();

        // next_tick must now be past 50, so an ordinary append stamps tick > 50.
        let fresh = store
            .insert_block(
                Some(&beat_id),
                Some(&beat_id),
                Role::Model,
                BlockKind::Text,
                "ordinary append",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let snap = store.get_block_snapshot(&fresh).unwrap();
        let appended_tick = snap.tick.expect("the appended block carries a tick");
        assert!(
            appended_tick.get() > 50,
            "an ordinary append after a tick-50 snapshot insert must stamp tick > 50, got {}",
            appended_tick.get(),
        );

        // ...and it sorts LAST: a stale next_tick would mint a mid-document key.
        let ordered = store.block_ids_ordered();
        assert_eq!(
            *ordered.last().unwrap(),
            fresh,
            "the ordinary append must sort after the snapshot-inserted beat block"
        );
    }

    /// T4 — fork variants seed tick + seq lanes. Parameterized over fork /
    /// fork_at_version / fork_filtered. Fails today (Self::new + own-principal
    /// guards). The rotation-critical graft (design §2.4, §3). ("Seq lanes" =
    /// per-principal id counters; "lane" the track concept is unrelated.)
    #[test]
    fn fork_seeds_tick_and_seq_lanes() {
        #[derive(Clone, Copy)]
        enum Variant {
            Fork,
            AtVersion,
            Filtered,
        }

        for variant in [Variant::Fork, Variant::AtVersion, Variant::Filtered] {
            let ctx = ContextId::new();
            let prin = PrincipalId::new();
            let other = PrincipalId::new(); // P != fork principal
            let mut store = BlockStore::new(ctx, prin);

            // Blocks authored by P (other), ticks 0..5.
            for t in 0..5 {
                store
                    .insert_from_snapshot(snap_for(ctx, other, t as u64, t), None)
                    .unwrap();
            }
            let max_p_seq = 4u64;

            let new_ctx = ContextId::new();
            let fork_prin = PrincipalId::new();
            let forked = match variant {
                Variant::Fork => store.fork(new_ctx, fork_prin),
                Variant::AtVersion => store.fork_at_version(new_ctx, fork_prin, u64::MAX),
                Variant::Filtered => {
                    store.fork_filtered(new_ctx, fork_prin, u64::MAX, &ForkBlockFilter::default())
                }
            };
            let mut forked = forked;

            // next_seq_for(P) == max P seq + 1.
            assert_eq!(
                forked.next_seq_for(other),
                max_p_seq + 1,
                "fork must seed the foreign principal's seq lane"
            );

            // Fresh insert stamps tick N+1 (max copied tick was 4) and sorts last.
            let fresh = forked
                .insert_block(
                    None,
                    None,
                    Role::Model,
                    BlockKind::Text,
                    "fresh",
                    Status::Done,
                    ContentType::Plain,
                )
                .unwrap();
            let snap = forked.get_block_snapshot(&fresh).unwrap();
            assert_eq!(snap.tick, Some(Tick::new(5)), "fork must seed next_tick past max copied tick");
            assert_eq!(
                *forked.block_ids_ordered().last().unwrap(),
                fresh,
                "fresh insert into a fork must sort last"
            );
        }
    }

    /// T5 — key-less merge new_blocks append, not prepend. Fails today: the
    /// decimal `{:020}` fallback sorts below 'V'.
    #[test]
    fn keyless_merge_new_blocks_append_not_prepend() {
        let ctx = ContextId::new();
        let prin = PrincipalId::new();
        let mut store = BlockStore::new(ctx, prin);

        // Existing canonical-keyed blocks.
        let mut last: Option<BlockId> = None;
        for _ in 0..3 {
            let id = store
                .insert_block(
                    last.as_ref(),
                    last.as_ref(),
                    Role::Model,
                    BlockKind::Text,
                    "existing",
                    Status::Done,
                    ContentType::Plain,
                )
                .unwrap();
            last = Some(id);
        }

        // Merge a new block WITHOUT a canonical order_key.
        let foreign = PrincipalId::new();
        let keyless = crate::BlockSnapshotBuilder::new(
            BlockId::new(ctx, foreign, 0),
            BlockKind::Text,
        )
        .content("keyless")
        .build();
        let keyless_id = keyless.id;
        store
            .merge_ops(SyncPayload {
                block_ops: vec![],
                new_blocks: vec![keyless],
                updated_headers: vec![],
                deleted_blocks: vec![],
            })
            .unwrap();

        let ordered = store.block_ids_ordered();
        assert_eq!(
            *ordered.last().unwrap(),
            keyless_id,
            "key-less merged block must append after existing blocks, not prepend"
        );
    }

    /// T6 — seq lanes cover foreign principals after restore. from_snapshot /
    /// merge_ops / fork each restore blocks authored by P != store principal
    /// (one a tombstone). Fails today (API absent; own-principal guards).
    #[test]
    fn seq_lanes_cover_foreign_principals_after_restore() {
        let ctx = ContextId::new();
        let prin = PrincipalId::new();
        let foreign = PrincipalId::new();

        // Build a source store with foreign-authored blocks (one deleted).
        let mut src = BlockStore::new(ctx, prin);
        for t in 0..3 {
            src.insert_from_snapshot(snap_for(ctx, foreign, t as u64, t), None)
                .unwrap();
        }
        let deleted_id = BlockId::new(ctx, foreign, 3);
        src.insert_from_snapshot(snap_for(ctx, foreign, 3, 3), None)
            .unwrap();
        src.delete_block(&deleted_id).unwrap();

        // (a) from_snapshot restores the foreign lane (tombstone included → max+1).
        let snapshot = src.snapshot();
        let restored = BlockStore::from_snapshot(snapshot, prin).unwrap();
        assert_eq!(
            restored.next_seq_for(foreign),
            4,
            "from_snapshot must seed the foreign lane past the tombstoned max seq"
        );

        // A subsequent insert of the reserved id does not DuplicateBlock.
        let mut restored = restored;
        let reserved = restored.reserve_block_id(foreign);
        assert_eq!(reserved.seq, 4, "reserved seq is max+1 in the foreign lane");
        let snap = snap_for(ctx, foreign, reserved.seq, 100);
        assert!(
            restored.insert_from_snapshot(snap, None).is_ok(),
            "inserting the reserved id must not DuplicateBlock"
        );

        // Own-principal minting via insert_block stays in the store's own lane.
        let own_before = restored.next_seq_for(prin);
        restored
            .insert_block(None, None, Role::Model, BlockKind::Text, "own", Status::Done, ContentType::Plain)
            .unwrap();
        assert_eq!(
            restored.next_seq_for(prin),
            own_before + 1,
            "own-principal minting advances only the own lane"
        );

        // (b) merge_ops seeds a fresh foreign lane.
        let mut merged = BlockStore::new(ContextId::new(), prin);
        let mctx = merged.context_id();
        let merge_blocks: Vec<BlockSnapshot> =
            (0..3).map(|t| snap_for(mctx, foreign, t as u64, t)).collect();
        merged
            .merge_ops(SyncPayload {
                block_ops: vec![],
                new_blocks: merge_blocks,
                updated_headers: vec![],
                deleted_blocks: vec![],
            })
            .unwrap();
        assert_eq!(merged.next_seq_for(foreign), 3, "merge_ops must seed the foreign lane");

        // (c) fork seeds the foreign lane from the LIVE blocks it copies.
        // Forks do not carry tombstones, so the deleted seq=3 does not travel —
        // the fork's lane is max-live-seq + 1 = 3, and seq 3 is free to re-mint
        // in the fork. (from_snapshot above DOES carry the tombstone → lane 4.)
        let forked = src.fork(ContextId::new(), prin);
        assert_eq!(
            forked.next_seq_for(foreign),
            3,
            "fork seeds the foreign lane from copied live blocks (tombstones do not travel)"
        );
    }

    /// T6 sub-case — a `from_snapshot`-restored store with MIXED keys (some blocks
    /// canonical-keyed, later blocks key-less, as a partial pre-tick migration)
    /// must keep its stored order and interleave with a subsequent `insert_block`.
    /// The old `format!("{:020}", order_seq)` fallback minted decimal keys that
    /// sort BELOW every 'V' canonical key (the latent PREPEND §2.3 killed in
    /// merge_ops but left in from_snapshot): the key-less tail blocks would sort
    /// ahead of the canonical head blocks in `block_ids_ordered()`, and
    /// `normalize_timeline` would re-key them in that scrambled order. The
    /// successor-of-tail fallback keeps the key-less blocks after their canonical
    /// predecessors.
    #[test]
    fn from_snapshot_mixed_keys_interleave_with_fresh_insert() {
        let ctx = ContextId::new();
        let prin = PrincipalId::new();

        // Build a real store and snapshot it, then strip the order_key + tick from
        // the LATER half only — a partial pre-tick migration: the head keeps its
        // canonical 'V…' keys, the tail is key-less and must still sort after it.
        let mut src = BlockStore::new(ctx, prin);
        let mut last: Option<BlockId> = None;
        let mut ids = Vec::new();
        for i in 0..4 {
            let id = src
                .insert_block(
                    last.as_ref(),
                    last.as_ref(),
                    Role::Model,
                    BlockKind::Text,
                    format!("block-{i}"),
                    Status::Done,
                    ContentType::Plain,
                )
                .unwrap();
            ids.push(id);
            last = Some(id);
        }
        let mut snapshot = src.snapshot();
        // Strip keys from the tail half: blocks 2 and 3 become key-less legacy.
        for b in snapshot.blocks.iter_mut().skip(2) {
            b.order_key = None;
            b.tick = None;
        }

        let mut restored = BlockStore::from_snapshot(snapshot, prin).unwrap();
        let restored_order = restored.block_ids_ordered();
        assert_eq!(
            restored_order, ids,
            "mixed-key restore keeps the key-less tail after the canonical head"
        );

        // A fresh local insert must sort AFTER every restored block.
        let fresh = restored
            .insert_block(
                restored_order.last(),
                restored_order.last(),
                Role::Model,
                BlockKind::Text,
                "fresh",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let after = restored.block_ids_ordered();
        assert_eq!(
            *after.last().unwrap(),
            fresh,
            "a fresh insert after a mixed-key restore sorts last, not below the restored blocks"
        );
    }

    /// T7 — reserve_block_id claims and advances. Reserve → seq claimed;
    /// failed-insert gap is tolerated; subsequent reserve mints +1.
    #[test]
    fn reserve_block_id_claims_and_advances() {
        let ctx = ContextId::new();
        let prin = PrincipalId::new();
        let mut store = BlockStore::new(ctx, prin);
        let player = PrincipalId::new();

        let first = store.reserve_block_id(player);
        assert_eq!(first.seq, 0, "first reserved seq in a virgin lane is 0");
        assert_eq!(first.principal_id, player);

        // Simulate a failed insert: we never insert `first`. The lane still
        // advances — a seq gap is benign (monotonic-unique, not dense).
        let second = store.reserve_block_id(player);
        assert_eq!(second.seq, 1, "subsequent reserve mints +1 even if the prior insert failed");

        // next_seq_for reflects the claimed lane.
        assert_eq!(store.next_seq_for(player), 2);
    }
}
