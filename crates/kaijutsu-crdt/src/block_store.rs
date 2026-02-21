//! Block store — collection of per-block DTE instances.
//!
//! Replaces the old `BlockDocument` (single shared DTE Document) with a
//! collection of `BlockContent` instances, each owning its own DTE Document.
//! Metadata lives in `BlockHeader` (plain data), content in per-block CRDTs.

use std::collections::{BTreeMap, HashMap};

use diamond_types_extended::Frontier;

use crate::content::{order_midpoint, BlockContent};
use crate::{
    BlockHeader, BlockId, BlockKind, BlockSnapshot, ContextId, CrdtError, DriftKind,
    PrincipalId, Result, Role, Status, MAX_DAG_DEPTH,
};

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

    /// Principal ID for this agent instance.
    agent_id: PrincipalId,

    /// Blocks indexed by ID.
    blocks: BTreeMap<BlockId, BlockContent>,

    /// Next sequence number for block IDs (agent-local).
    next_seq: u64,

    /// Store version (bumped on any mutation).
    version: u64,

    /// Lamport clock for LWW conflict resolution on header fields.
    /// Monotonically increasing. Advanced on local mutations and on merge.
    lamport_clock: u64,
}

impl BlockStore {
    /// Create a new empty store.
    pub fn new(context_id: ContextId, agent_id: PrincipalId) -> Self {
        Self {
            context_id,
            agent_id,
            blocks: BTreeMap::new(),
            next_seq: 0,
            version: 0,
            lamport_clock: 0,
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

    /// Get the agent/principal ID.
    pub fn agent_id(&self) -> PrincipalId {
        self.agent_id
    }

    /// Get the current version.
    pub fn version(&self) -> u64 {
        self.version
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
        let mut current_id = self
            .blocks
            .get(id)
            .and_then(|b| b.header().parent_id);

        while let Some(pid) = current_id {
            if ancestors.len() >= MAX_DAG_DEPTH {
                tracing::warn!("get_ancestors() hit MAX_DAG_DEPTH ({MAX_DAG_DEPTH}), truncating");
                break;
            }
            ancestors.push(pid);
            current_id = self
                .blocks
                .get(&pid)
                .and_then(|b| b.header().parent_id);
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
        let id = BlockId::new(self.context_id, self.agent_id, self.next_seq);
        self.next_seq += 1;
        id
    }

    // =========================================================================
    // Order key calculation
    // =========================================================================

    /// 2-char base62 suffix derived from agent ID.
    ///
    /// Each agent gets a unique "lane" in the keyspace. Since the chars are
    /// valid base62, they participate normally in midpoint calculations — no
    /// stripping needed. Sequential inserts by the same agent naturally group
    /// because they all share the same suffix in their key lineage.
    fn agent_order_suffix(&self) -> String {
        let bytes = self.agent_id.as_bytes();
        // Use bytes 10-13 (random portion of UUIDv7, not the timestamp prefix)
        // to maximize entropy and minimize suffix collisions between agents.
        let c1 = crate::content::BASE62[(bytes[10] as usize) % 62] as char;
        let c2 = crate::content::BASE62[(bytes[11] as usize) % 62] as char;
        let c3 = crate::content::BASE62[(bytes[12] as usize) % 62] as char;
        format!("{c1}{c2}{c3}")
    }

    /// Compute an order_key for a new block inserted after `after`.
    ///
    /// Appends a 2-char agent suffix (base62-encoded from agent UUID) to
    /// the midpoint. Since the suffix chars are valid base62, they participate
    /// naturally in subsequent midpoint calculations — no stripping needed.
    /// Concurrent inserts at the same position get different keys (different
    /// agent suffix), and sequential inserts by the same agent group together
    /// (same suffix in the key lineage).
    // TODO: This is O(N log N) per insertion due to block_ids_ordered() full sort.
    // For high-frequency inserts (>1000 blocks), maintain a secondary sorted index.
    fn calc_order_key(&self, after: Option<&BlockId>) -> String {
        let ordered = self.block_ids_ordered();
        let suffix = self.agent_order_suffix();

        let base = match after {
            None => {
                if ordered.is_empty() {
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
                            format!("{after_key}V")
                        }
                    }
                    None => {
                        if let Some(last) = ordered.last() {
                            let last_key = self.blocks[last].order_key();
                            format!("{last_key}V")
                        } else {
                            "V".to_string()
                        }
                    }
                }
            }
        };

        format!("{base}{suffix}")
    }

    // =========================================================================
    // Block Operations
    // =========================================================================

    /// Insert a new block.
    ///
    /// Author is implicit — derived from `self.agent_id` via the BlockId.
    pub fn insert_block(
        &mut self,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        role: Role,
        kind: BlockKind,
        content: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let content_str = content.into();

        // Validate references
        if let Some(after_id) = after {
            if !self.blocks.contains_key(after_id) || self.blocks[after_id].is_deleted() {
                return Err(CrdtError::InvalidReference(*after_id));
            }
        }
        if let Some(pid) = parent_id {
            if !self.blocks.contains_key(pid) || self.blocks[pid].is_deleted() {
                return Err(CrdtError::InvalidReference(*pid));
            }
        }

        let order_key = self.calc_order_key(after);
        let ts = self.tick();
        let header = BlockHeader {
            id,
            parent_id: parent_id.copied(),
            role,
            kind,
            status: Status::Done,
            compacted: false,
            collapsed: false,
            created_at: now_millis(),
            updated_at: ts,
            tool_kind: None,
            exit_code: None,
            is_error: false,
        };

        let block = BlockContent::with_content(header, &content_str, self.agent_id, order_key);
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
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let input_json = serde_json::to_string_pretty(&tool_input)
            .map_err(|e| CrdtError::Serialization(e.to_string()))?;

        // Validate references
        if let Some(after_id) = after {
            if !self.blocks.contains_key(after_id) || self.blocks[after_id].is_deleted() {
                return Err(CrdtError::InvalidReference(*after_id));
            }
        }
        if let Some(pid) = parent_id {
            if !self.blocks.contains_key(pid) || self.blocks[pid].is_deleted() {
                return Err(CrdtError::InvalidReference(*pid));
            }
        }

        let order_key = self.calc_order_key(after);
        let now = now_millis();
        let ts = self.tick();
        let header = BlockHeader {
            id,
            parent_id: parent_id.copied(),
            role: Role::Model,
            kind: BlockKind::ToolCall,
            status: Status::Running,
            compacted: false,
            collapsed: false,
            created_at: now,
            updated_at: ts,
            tool_kind: None,
            exit_code: None,
            is_error: false,
        };

        let mut block = BlockContent::with_content(header, &input_json, self.agent_id, order_key);
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
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let after = after.or(Some(tool_call_id));

        // Validate tool call exists
        if !self.blocks.contains_key(tool_call_id) || self.blocks[tool_call_id].is_deleted() {
            return Err(CrdtError::InvalidReference(*tool_call_id));
        }
        if let Some(after_id) = after {
            if !self.blocks.contains_key(after_id) || self.blocks[after_id].is_deleted() {
                return Err(CrdtError::InvalidReference(*after_id));
            }
        }

        let order_key = self.calc_order_key(after);
        let now = now_millis();
        let ts = self.tick();
        let header = BlockHeader {
            id,
            parent_id: Some(*tool_call_id),
            role: Role::Tool,
            kind: BlockKind::ToolResult,
            status: if is_error { Status::Error } else { Status::Done },
            compacted: false,
            collapsed: false,
            created_at: now,
            updated_at: ts,
            tool_kind: None,
            exit_code,
            is_error,
        };

        let mut block = BlockContent::with_content(
            header,
            &content.into(),
            self.agent_id,
            order_key,
        );
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

        if let Some(after_id) = after {
            if !self.blocks.contains_key(after_id) || self.blocks[after_id].is_deleted() {
                return Err(CrdtError::InvalidReference(*after_id));
            }
        }
        if let Some(pid) = parent_id {
            if !self.blocks.contains_key(pid) || self.blocks[pid].is_deleted() {
                return Err(CrdtError::InvalidReference(*pid));
            }
        }

        let order_key = self.calc_order_key(after);

        let snap = BlockSnapshot::drift(id, parent_id.copied(), content, source_context, source_model, drift_kind);
        let block = BlockContent::from_snapshot(&snap, self.agent_id, order_key);
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

        if let Some(after_id) = after {
            if !self.blocks.contains_key(after_id) || self.blocks[after_id].is_deleted() {
                return Err(CrdtError::InvalidReference(*after_id));
            }
        }
        if let Some(pid) = parent_id {
            if !self.blocks.contains_key(pid) || self.blocks[pid].is_deleted() {
                return Err(CrdtError::InvalidReference(*pid));
            }
        }

        let order_key = self.calc_order_key(after);
        let snap = BlockSnapshot::file(id, parent_id.copied(), file_path, content);
        let block = BlockContent::from_snapshot(&snap, self.agent_id, order_key);
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
        let block_id = if snapshot.id.context_id.is_nil() {
            tracing::warn!(
                "insert_from_snapshot called with nil context_id — \
                 this is deprecated, use insert_drift_block() instead"
            );
            self.new_block_id()
        } else {
            if snapshot.id.agent_id == self.agent_id {
                self.next_seq = self.next_seq.max(snapshot.id.seq + 1);
            }
            snapshot.id
        };

        if self.blocks.contains_key(&block_id) {
            return Err(CrdtError::DuplicateBlock(block_id));
        }

        let order_key = self.calc_order_key(after);
        let block = BlockContent::from_snapshot(&snapshot, self.agent_id, order_key);
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

    /// Set the display hint.
    pub fn set_display_hint(&mut self, id: &BlockId, hint: Option<&str>) -> Result<()> {
        let block = self
            .blocks
            .get_mut(id)
            .filter(|b| !b.is_deleted())
            .ok_or(CrdtError::BlockNotFound(*id))?;
        block.set_display_hint(hint.map(|s| s.to_string()));
        self.version += 1;
        Ok(())
    }

    /// Move a block to a new position.
    pub fn move_block(&mut self, id: &BlockId, after: Option<&BlockId>) -> Result<()> {
        if !self.blocks.contains_key(id) || self.blocks[id].is_deleted() {
            return Err(CrdtError::BlockNotFound(*id));
        }
        if let Some(after_id) = after {
            if !self.blocks.contains_key(after_id) || self.blocks[after_id].is_deleted() {
                return Err(CrdtError::InvalidReference(*after_id));
            }
        }

        let order_key = self.calc_order_key(after);
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
                    // New block: send full snapshot for reconstruction
                    new_blocks.push(block.snapshot());
                    // Also include header so Lamport timestamp propagates
                    updated_headers.push(*block.header());
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

        // First, create blocks from full snapshots (new blocks)
        for snap in &payload.new_blocks {
            if !self.blocks.contains_key(&snap.id) {
                // Use the snapshot's order_key if present (preserves remote ordering)
                let fallback_key = format!("{:020}", self.blocks.len());
                let block = BlockContent::from_snapshot(snap, self.agent_id, fallback_key);
                // Track remote Lamport from the new block's header
                max_remote_ts = max_remote_ts.max(block.header().updated_at);
                self.blocks.insert(snap.id, block);
                if snap.id.agent_id == self.agent_id {
                    self.next_seq = self.next_seq.max(snap.id.seq + 1);
                }
            }
        }

        // Apply header updates (LWW merge)
        for header in &payload.updated_headers {
            max_remote_ts = max_remote_ts.max(header.updated_at);
            if let Some(block) = self.blocks.get_mut(&header.id) {
                block.merge_header(header);
            }
        }

        // Merge per-block incremental DTE ops
        for (id, ops) in payload.block_ops {
            if let Some(block) = self.blocks.get_mut(&id) {
                block.merge_ops(ops)?;
            } else {
                tracing::warn!("sync payload has ops for unknown block {id}, skipping");
            }
        }

        // Apply tombstone deletions
        for id in &payload.deleted_blocks {
            // Tick once per deletion to get a unique Lamport timestamp
            let ts = self.tick();
            if let Some(block) = self.blocks.get_mut(id) {
                if !block.is_deleted() {
                    block.mark_deleted(ts);
                }
            }
        }

        // Advance Lamport clock past any remote timestamp
        if max_remote_ts > 0 {
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
    pub fn fork(&self, new_context_id: ContextId, new_agent_id: PrincipalId) -> Self {
        let mut forked = Self::new(new_context_id, new_agent_id);

        for (_, block) in &self.blocks {
            if block.is_deleted() {
                continue;
            }
            let snap = block.snapshot();

            // Remap IDs: only context_id changes
            let new_id = BlockId::new(new_context_id, snap.id.agent_id, snap.id.seq);
            let new_parent_id = snap
                .parent_id
                .map(|pid| BlockId::new(new_context_id, pid.agent_id, pid.seq));
            let new_tool_call_id = snap
                .tool_call_id
                .map(|tcid| BlockId::new(new_context_id, tcid.agent_id, tcid.seq));

            if snap.id.agent_id == new_agent_id {
                forked.next_seq = forked.next_seq.max(snap.id.seq + 1);
            }

            let mut remapped = snap;
            remapped.id = new_id;
            remapped.parent_id = new_parent_id;
            remapped.tool_call_id = new_tool_call_id;

            let order_key = block.order_key().to_string();
            let content = BlockContent::from_snapshot(&remapped, new_agent_id, order_key);
            forked.blocks.insert(new_id, content);
        }

        forked.version = 1;
        forked
    }

    // =========================================================================
    // Snapshot / Restore
    // =========================================================================

    /// Create a snapshot of the entire store.
    pub fn snapshot(&self) -> StoreSnapshot {
        StoreSnapshot {
            context_id: self.context_id,
            blocks: self.blocks_ordered(),
        }
    }

    /// Restore from a snapshot.
    pub fn from_snapshot(snapshot: StoreSnapshot, agent_id: PrincipalId) -> Self {
        let mut store = Self::new(snapshot.context_id, agent_id);
        let mut order_seq = 0u64;

        for block_snap in &snapshot.blocks {
            if block_snap.id.agent_id == agent_id {
                store.next_seq = store.next_seq.max(block_snap.id.seq + 1);
            }

            let fallback_key = format!("{:020}", order_seq);
            order_seq += 1;

            let content = BlockContent::from_snapshot(block_snap, agent_id, fallback_key);
            store.blocks.insert(block_snap.id, content);
        }

        store
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
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StoreSnapshot {
    /// Context ID.
    pub context_id: ContextId,
    /// Blocks in order.
    pub blocks: Vec<BlockSnapshot>,
}

/// Per-block sync payload.
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

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> BlockStore {
        BlockStore::new(ContextId::new(), PrincipalId::new())
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
            .insert_block(None, None, Role::User, BlockKind::Text, "Hello!")
            .unwrap();
        let id2 = store
            .insert_block(Some(&id1), Some(&id1), Role::Model, BlockKind::Text, "Hi!")
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
            .insert_block(None, None, Role::User, BlockKind::Text, "Hello")
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
            .insert_block(None, None, Role::User, BlockKind::Text, "First")
            .unwrap();
        let id2 = store
            .insert_block(None, Some(&id1), Role::User, BlockKind::Text, "Second")
            .unwrap();
        let id3 = store
            .insert_block(None, Some(&id2), Role::User, BlockKind::Text, "Third")
            .unwrap();

        let order: Vec<_> = store.blocks_ordered().iter().map(|b| b.id).collect();
        assert_eq!(order, vec![id1, id2, id3]);
    }

    #[test]
    fn test_insert_at_beginning() {
        let mut store = test_store();

        let id1 = store
            .insert_block(None, None, Role::User, BlockKind::Text, "First")
            .unwrap();
        let id2 = store
            .insert_block(None, None, Role::User, BlockKind::Text, "Before First")
            .unwrap();

        let order: Vec<_> = store.blocks_ordered().iter().map(|b| b.id).collect();
        assert_eq!(order, vec![id2, id1]);
    }

    #[test]
    fn test_set_status() {
        let mut store = test_store();
        let id = store
            .insert_block(None, None, Role::Model, BlockKind::ToolCall, "{}")
            .unwrap();

        store.set_status(&id, Status::Running).unwrap();
        assert_eq!(
            store.get_block_snapshot(&id).unwrap().status,
            Status::Running
        );

        store.set_status(&id, Status::Error).unwrap();
        assert_eq!(
            store.get_block_snapshot(&id).unwrap().status,
            Status::Error
        );
    }

    #[test]
    fn test_set_collapsed() {
        let mut store = test_store();
        let id = store
            .insert_block(None, None, Role::Model, BlockKind::Thinking, "Thinking...")
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
            .insert_block(None, None, Role::Model, BlockKind::ToolCall, "{}")
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
            )
            .unwrap();

        let snap = store.get_block_snapshot(&call_id).unwrap();
        assert_eq!(snap.kind, BlockKind::ToolCall);
        assert_eq!(snap.tool_name, Some("read_file".to_string()));
        assert_eq!(snap.status, Status::Running);

        let result_id = store
            .insert_tool_result_block(&call_id, Some(&call_id), "127.0.0.1 localhost", false, Some(0))
            .unwrap();

        let snap = store.get_block_snapshot(&result_id).unwrap();
        assert_eq!(snap.kind, BlockKind::ToolResult);
        assert_eq!(snap.parent_id, Some(call_id));
        assert_eq!(snap.tool_call_id, Some(call_id));
        assert!(!snap.is_error);
        assert_eq!(snap.exit_code, Some(0));
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
            .insert_block(None, None, Role::User, BlockKind::Text, "First")
            .unwrap();
        let id2 = store
            .insert_block(None, Some(&id1), Role::User, BlockKind::Text, "Second")
            .unwrap();
        let _id3 = store
            .insert_block(None, Some(&id2), Role::User, BlockKind::Text, "Third")
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
            .insert_block(None, None, Role::User, BlockKind::Text, "A")
            .unwrap();
        let b = store
            .insert_block(None, Some(&a), Role::User, BlockKind::Text, "B")
            .unwrap();
        let c = store
            .insert_block(None, Some(&b), Role::User, BlockKind::Text, "C")
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
            .insert_block(None, None, Role::User, BlockKind::Text, "Question")
            .unwrap();
        let child1 = store
            .insert_block(
                Some(&parent),
                Some(&parent),
                Role::Model,
                BlockKind::Thinking,
                "Thinking...",
            )
            .unwrap();
        let child2 = store
            .insert_block(
                Some(&parent),
                Some(&child1),
                Role::Model,
                BlockKind::Text,
                "Answer",
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
            .insert_block(None, None, Role::Model, BlockKind::Thinking, "Thinking...")
            .unwrap();
        store
            .insert_block(None, None, Role::Model, BlockKind::Text, "Response")
            .unwrap();

        let snapshot = store.snapshot();
        let restored = BlockStore::from_snapshot(snapshot.clone(), PrincipalId::new());

        assert_eq!(restored.block_count(), store.block_count());
        assert_eq!(restored.full_text(), store.full_text());
    }

    #[test]
    fn test_fork() {
        let mut original = test_store();
        let original_agent = original.agent_id();

        let user_msg = original
            .insert_block(None, None, Role::User, BlockKind::Text, "Hello Claude!")
            .unwrap();
        let _model_response = original
            .insert_block(
                Some(&user_msg),
                Some(&user_msg),
                Role::Model,
                BlockKind::Text,
                "Hi Amy!",
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
        assert_eq!(blocks[0].id.agent_id, original_agent);
    }

    #[test]
    fn test_insert_from_snapshot() {
        let mut store = test_store();
        let source_ctx = ContextId::new();

        let drift_id = BlockId::new(store.context_id(), store.agent_id(), 0);
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

    #[test]
    fn test_ordering_stress_100_bisections() {
        let mut store = test_store();

        let first = store
            .insert_block(None, None, Role::User, BlockKind::Text, "First")
            .unwrap();
        let _last = store
            .insert_block(None, Some(&first), Role::User, BlockKind::Text, "Last")
            .unwrap();

        for i in 0..100 {
            store
                .insert_block(
                    None,
                    Some(&first),
                    Role::User,
                    BlockKind::Text,
                    &format!("Middle-{i}"),
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
            .insert_block(None, None, Role::User, BlockKind::Text, "Hello from store1")
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
            .insert_block(None, None, Role::User, BlockKind::Text, "Hello")
            .unwrap();

        // Initial sync
        let payload = store1.ops_since(&HashMap::new());
        store2.merge_ops(payload).unwrap();
        assert_eq!(store2.block_count(), 1);

        // Store1 adds a new block
        let id2 = store1
            .insert_block(Some(&id1), Some(&id1), Role::Model, BlockKind::Text, "World")
            .unwrap();

        // Incremental sync — new block arrives as full snapshot
        let frontiers = store2.frontier();
        let payload = store1.ops_since(&frontiers);
        assert_eq!(payload.new_blocks.len(), 1, "new block should be in new_blocks");
        store2.merge_ops(payload).unwrap();

        assert_eq!(store2.block_count(), 2);
        let snap = store2.get_block_snapshot(&id2).unwrap();
        assert_eq!(snap.content, "World");
        assert_eq!(snap.parent_id, Some(id1));
    }

    // ── Sync: header propagation ──────────────────────────────────────

    #[test]
    fn test_sync_propagates_status_change() {
        let ctx = ContextId::new();
        let mut store1 = BlockStore::new(ctx, PrincipalId::new());
        let mut store2 = BlockStore::new(ctx, PrincipalId::new());

        let id = store1
            .insert_block(None, None, Role::Model, BlockKind::ToolCall, "{}")
            .unwrap();

        // Initial sync
        let payload = store1.ops_since(&HashMap::new());
        store2.merge_ops(payload).unwrap();

        // Store1 changes status
        store1.set_status(&id, Status::Done).unwrap();

        // Incremental sync — header update should propagate
        let frontiers = store2.frontier();
        let payload = store1.ops_since(&frontiers);
        assert!(!payload.updated_headers.is_empty(), "should include updated header");
        store2.merge_ops(payload).unwrap();

        let snap = store2.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.status, Status::Done, "status change should propagate via sync");
    }

    #[test]
    fn test_sync_propagates_collapsed() {
        let ctx = ContextId::new();
        let mut store1 = BlockStore::new(ctx, PrincipalId::new());
        let mut store2 = BlockStore::new(ctx, PrincipalId::new());

        let id = store1
            .insert_block(None, None, Role::Model, BlockKind::Thinking, "Thinking...")
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
            .insert_block(None, None, Role::User, BlockKind::Text, "Keep")
            .unwrap();
        let id2 = store1
            .insert_block(None, Some(&id1), Role::User, BlockKind::Text, "Delete me")
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
        assert_eq!(payload.deleted_blocks.len(), 1, "should include deleted block ID");
        assert_eq!(payload.deleted_blocks[0], id2);
        store2.merge_ops(payload).unwrap();

        assert_eq!(store2.block_count(), 1, "deletion should propagate via sync");
        assert!(store2.get_block_snapshot(&id2).is_none());
    }

    // ── Sync: order_key propagation ───────────────────────────────────

    #[test]
    fn test_sync_preserves_order_key() {
        let ctx = ContextId::new();
        let mut store1 = BlockStore::new(ctx, PrincipalId::new());
        let mut store2 = BlockStore::new(ctx, PrincipalId::new());

        let id1 = store1
            .insert_block(None, None, Role::User, BlockKind::Text, "First")
            .unwrap();
        let id2 = store1
            .insert_block(None, Some(&id1), Role::User, BlockKind::Text, "Second")
            .unwrap();
        let _id3 = store1
            .insert_block(None, Some(&id2), Role::User, BlockKind::Text, "Third")
            .unwrap();

        // Sync to store2
        let payload = store1.ops_since(&HashMap::new());
        store2.merge_ops(payload).unwrap();

        // Ordering should match
        let order1: Vec<_> = store1.blocks_ordered().iter().map(|b| b.content.clone()).collect();
        let order2: Vec<_> = store2.blocks_ordered().iter().map(|b| b.content.clone()).collect();
        assert_eq!(order1, order2, "synced store should preserve document order");
    }

    // ── Lamport clock ─────────────────────────────────────────────────

    #[test]
    fn test_lamport_clock_advances() {
        let mut store = test_store();

        let id = store
            .insert_block(None, None, Role::User, BlockKind::Text, "Hello")
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
            .insert_block(None, None, Role::User, BlockKind::Text, "Hello")
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
            .insert_block(None, None, Role::User, BlockKind::Text, "Hello")
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
            .insert_block(None, None, Role::User, BlockKind::Text, "A")
            .unwrap();
        let _b = store1
            .insert_block(None, Some(&a), Role::User, BlockKind::Text, "B")
            .unwrap();

        // Store2 inserts C, D (independently)
        let c = store2
            .insert_block(None, None, Role::User, BlockKind::Text, "C")
            .unwrap();
        let _d = store2
            .insert_block(None, Some(&c), Role::User, BlockKind::Text, "D")
            .unwrap();

        // Sync both ways
        let payload1 = store1.ops_since(&HashMap::new());
        let payload2 = store2.ops_since(&HashMap::new());
        store1.merge_ops(payload2).unwrap();
        store2.merge_ops(payload1).unwrap();

        // Both stores should see 4 blocks in the same order
        let order1: Vec<_> = store1.blocks_ordered().iter().map(|b| b.content.clone()).collect();
        let order2: Vec<_> = store2.blocks_ordered().iter().map(|b| b.content.clone()).collect();
        assert_eq!(order1, order2, "both stores should converge to same order");

        // A and B should be adjacent, C and D should be adjacent (no interleaving)
        let a_pos = order1.iter().position(|c| c == "A").unwrap();
        let b_pos = order1.iter().position(|c| c == "B").unwrap();
        let c_pos = order1.iter().position(|c| c == "C").unwrap();
        let d_pos = order1.iter().position(|c| c == "D").unwrap();

        assert_eq!(
            (b_pos as isize - a_pos as isize).abs(),
            1,
            "A and B should be adjacent, got positions A={}, B={}",
            a_pos,
            b_pos
        );
        assert_eq!(
            (d_pos as isize - c_pos as isize).abs(),
            1,
            "C and D should be adjacent, got positions C={}, D={}",
            c_pos,
            d_pos
        );
    }
}
