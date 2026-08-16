//! `CellEditor`'s render buffer — an ordered list of `BlockSnapshot`s, not a
//! block store.
//!
//! `sync_main_cell_to_conversation` (`view/sync.rs`) rebuilds this from
//! `ContextMirror::blocks()` on every sync — never wire-fed, never persisted.
//! It used to be `kaijutsu_kernel::blocks::BlockStore` (formerly the
//! standalone CRDT crate), which carries a Lamport clock, fractional-index
//! ordering, and DAG-reference validation for
//! multi-writer merge. None of that applies here: this store has exactly one
//! writer (the sync system), and it is thrown away and rebuilt on every
//! version bump. `RenderBlockStore` keeps the twelve methods the app calls —
//! `blocks_ordered`, `block_ids_ordered`, `get_block_snapshot`, `full_text`,
//! `is_empty`, `version`, `set_version`, `principal_id`, `set_collapsed`,
//! `move_block`, `insert_block`, `insert_from_snapshot` — over a `Vec` plus
//! an id-to-index map.

use std::collections::HashMap;

use kaijutsu_types::{
    BlockId, BlockKind, BlockSnapshot, BlockSnapshotBuilder, ContentType, ContextId, PrincipalId,
    Role, Status,
};

/// A reference to a block this store does not hold.
///
/// `insert_from_snapshot` and `move_block` fail loud on an unresolvable
/// `after`/target id rather than silently falling back to append-at-end —
/// a caller passing a stale id is a bug in the caller, and the render
/// buffer rebuilds from scratch on the next sync regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderStoreError {
    /// `insert_from_snapshot` was given an id it already holds.
    DuplicateBlock(BlockId),
    /// `move_block` or `set_collapsed` targets an id not in the store.
    BlockNotFound(BlockId),
    /// `after` does not name a block in the store.
    UnknownAfter(BlockId),
}

impl std::fmt::Display for RenderStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateBlock(id) => write!(f, "render store already holds block {id}"),
            Self::BlockNotFound(id) => write!(f, "render store has no block {id}"),
            Self::UnknownAfter(id) => write!(f, "render store has no block {id} to insert after"),
        }
    }
}

impl std::error::Error for RenderStoreError {}

/// Ordered render buffer for one `CellEditor` — see the module doc.
pub struct RenderBlockStore {
    context_id: ContextId,
    principal_id: PrincipalId,
    blocks: Vec<BlockSnapshot>,
    index: HashMap<BlockId, usize>,
    /// Next per-principal seq for ids minted by `insert_block`. This store
    /// never observes foreign seq lanes (`insert_from_snapshot` carries its
    /// own id) — there is one writer, so one lane.
    next_seq: u64,
    version: u64,
}

impl RenderBlockStore {
    /// Create an empty store.
    pub fn new(context_id: ContextId, principal_id: PrincipalId) -> Self {
        Self {
            context_id,
            principal_id,
            blocks: Vec::new(),
            index: HashMap::new(),
            next_seq: 0,
            version: 0,
        }
    }

    /// Rebuild the `index` map for `blocks[start..]` — every insert/remove
    /// shifts the positions after it, so this runs after each one.
    fn reindex_from(&mut self, start: usize) {
        for (i, b) in self.blocks.iter().enumerate().skip(start) {
            self.index.insert(b.id, i);
        }
    }

    /// Resolve `after` to an insertion position: `None` appends at the end,
    /// `Some(id)` inserts right after it. Errors on an id the store does not
    /// hold instead of guessing a position (see `RenderStoreError`).
    fn position_after(&self, after: Option<&BlockId>) -> Result<usize, RenderStoreError> {
        match after {
            None => Ok(self.blocks.len()),
            Some(id) => self
                .index
                .get(id)
                .map(|&i| i + 1)
                .ok_or(RenderStoreError::UnknownAfter(*id)),
        }
    }

    fn insert_at(&mut self, position: usize, snapshot: BlockSnapshot) {
        self.blocks.insert(position, snapshot);
        self.reindex_from(position);
        self.version += 1;
    }

    // Only `move_block` calls this; a bin crate's plain (non-test) build
    // does not compile `#[cfg(test)]` code, so this reads as dead there even
    // though `view/render.rs`'s `reorder_repairs_children_after_order_only_change`
    // calls `move_block` for real.
    #[allow(dead_code)]
    fn remove_at(&mut self, position: usize) -> BlockSnapshot {
        let snapshot = self.blocks.remove(position);
        self.index.remove(&snapshot.id);
        self.reindex_from(position);
        snapshot
    }

    pub fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Override the version counter — set after a full re-materialization to
    /// the mirror's version, so `last_sync_version` comparisons in `sync.rs`
    /// see the right number.
    pub fn set_version(&mut self, v: u64) {
        self.version = v;
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn get_block_snapshot(&self, id: &BlockId) -> Option<BlockSnapshot> {
        self.index.get(id).map(|&i| self.blocks[i].clone())
    }

    /// Blocks in document order. `ContextMirror::blocks()` is already in
    /// document order (see `view/sync.rs`), so insertion order here is
    /// document order too — no sort.
    pub fn blocks_ordered(&self) -> Vec<BlockSnapshot> {
        self.blocks.clone()
    }

    pub fn block_ids_ordered(&self) -> Vec<BlockId> {
        self.blocks.iter().map(|b| b.id).collect()
    }

    pub fn full_text(&self) -> String {
        self.blocks
            .iter()
            .map(|b| b.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Insert a new block authored by this store's principal, after `after`
    /// (`None` appends at the end).
    #[allow(clippy::too_many_arguments)]
    pub fn insert_block(
        &mut self,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        role: Role,
        kind: BlockKind,
        content: impl Into<String>,
        status: Status,
        content_type: ContentType,
    ) -> Result<BlockId, RenderStoreError> {
        let position = self.position_after(after)?;
        let id = BlockId::new(self.context_id, self.principal_id, self.next_seq);
        self.next_seq += 1;
        let mut builder = BlockSnapshotBuilder::new(id, kind)
            .role(role)
            .status(status)
            .content(content)
            .content_type(content_type);
        if let Some(pid) = parent_id {
            builder = builder.parent_id(*pid);
        }
        self.insert_at(position, builder.build());
        Ok(id)
    }

    /// Materialize a snapshot carried in from the change-feed mirror, after
    /// `after` (`None` appends at the end).
    ///
    /// `Error` blocks default to collapsed on arrival (`view/format.rs`'s
    /// stub rendering) — the kernel's `collapsed` field has no per-kind
    /// default and `collapsed_at` isn't on the wire (CRDT-internal only), so
    /// this is the one place to bake it in. It only applies the default when
    /// the incoming snapshot is still at the wire default (`collapsed ==
    /// false`); a caller that already resolved a snapshot to `true` is left
    /// alone. Because this store is rebuilt wholesale on every doc version
    /// bump (see module doc), a locally-toggled *expand* is not durable
    /// across an unrelated edit — the same fragility `Thinking`'s local
    /// toggle already has, not a new one introduced here.
    pub fn insert_from_snapshot(
        &mut self,
        mut snapshot: BlockSnapshot,
        after: Option<&BlockId>,
    ) -> Result<BlockId, RenderStoreError> {
        let id = snapshot.id;
        if self.index.contains_key(&id) {
            return Err(RenderStoreError::DuplicateBlock(id));
        }
        if snapshot.kind == BlockKind::Error && !snapshot.collapsed {
            snapshot.collapsed = true;
        }
        let position = self.position_after(after)?;
        self.insert_at(position, snapshot);
        Ok(id)
    }

    /// Toggle the collapsed flag on a `Thinking` or `Error` block.
    pub fn set_collapsed(&mut self, id: &BlockId, collapsed: bool) -> Result<(), RenderStoreError> {
        let &i = self
            .index
            .get(id)
            .ok_or(RenderStoreError::BlockNotFound(*id))?;
        self.blocks[i].collapsed = collapsed;
        self.version += 1;
        Ok(())
    }

    /// Reposition an existing block. `after: None` moves it to the front —
    /// matching `kaijutsu_kernel::blocks::BlockStore::move_block`'s
    /// legacy move-to-front convention for a `None` target, which
    /// `view/render.rs`'s `reorder_repairs_children_after_order_only_change`
    /// exercises directly.
    #[allow(dead_code)] // test-only caller; see `remove_at`'s note above it.
    pub fn move_block(
        &mut self,
        id: &BlockId,
        after: Option<&BlockId>,
    ) -> Result<(), RenderStoreError> {
        let &current = self
            .index
            .get(id)
            .ok_or(RenderStoreError::BlockNotFound(*id))?;
        let snapshot = self.remove_at(current);
        let position = match after {
            None => 0,
            Some(after_id) => {
                let Some(&i) = self.index.get(after_id) else {
                    // Put the removed block back before failing — a failed
                    // move must not lose it.
                    self.insert_at(current, snapshot);
                    return Err(RenderStoreError::UnknownAfter(*after_id));
                };
                i + 1
            }
        };
        self.insert_at(position, snapshot);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_and_principal() -> (ContextId, PrincipalId) {
        (ContextId::new(), PrincipalId::new())
    }

    #[test]
    fn insert_from_snapshot_defaults_a_fresh_error_block_to_collapsed() {
        let (ctx, agent) = ctx_and_principal();
        let mut store = RenderBlockStore::new(ctx, agent);
        let id = BlockId::new(ctx, agent, 0);
        let block = BlockSnapshotBuilder::new(id, BlockKind::Error)
            .content("boom")
            .build();
        assert!(!block.collapsed, "sanity: wire default is uncollapsed");

        store.insert_from_snapshot(block, None).unwrap();
        assert!(
            store.get_block_snapshot(&id).unwrap().collapsed,
            "Error blocks should default to collapsed on arrival"
        );
    }

    #[test]
    fn insert_from_snapshot_cannot_distinguish_explicit_false_from_wire_default() {
        // `collapsed_at` (which would disambiguate "explicitly set to
        // false" from "never set") is CRDT-internal and not on the wire, so
        // this is a known limitation, not a design goal: an Error snapshot
        // that already carries `collapsed: false` is defaulted the same as
        // one that never touched the field.
        let (ctx, agent) = ctx_and_principal();
        let mut store = RenderBlockStore::new(ctx, agent);
        let id = BlockId::new(ctx, agent, 0);
        let block = BlockSnapshotBuilder::new(id, BlockKind::Error)
            .content("boom")
            .collapsed(false)
            .build();
        store.insert_from_snapshot(block, None).unwrap();
        assert!(store.get_block_snapshot(&id).unwrap().collapsed);
    }

    #[test]
    fn insert_from_snapshot_does_not_collapse_non_error_kinds() {
        let (ctx, agent) = ctx_and_principal();
        let mut store = RenderBlockStore::new(ctx, agent);
        let id = BlockId::new(ctx, agent, 0);
        let block = BlockSnapshotBuilder::new(id, BlockKind::Thinking)
            .content("pondering")
            .build();
        store.insert_from_snapshot(block, None).unwrap();
        assert!(!store.get_block_snapshot(&id).unwrap().collapsed);
    }

    #[test]
    fn insert_from_snapshot_respects_an_already_collapsed_error_block() {
        let (ctx, agent) = ctx_and_principal();
        let mut store = RenderBlockStore::new(ctx, agent);
        let id = BlockId::new(ctx, agent, 0);
        let block = BlockSnapshotBuilder::new(id, BlockKind::Error)
            .content("boom")
            .collapsed(true)
            .build();
        store.insert_from_snapshot(block, None).unwrap();
        assert!(store.get_block_snapshot(&id).unwrap().collapsed);
    }
}
