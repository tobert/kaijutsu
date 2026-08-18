//! `CellEditor`'s render buffer — an ordered list of `BlockSnapshot`s, not a
//! block store.
//!
//! `sync_main_cell_to_conversation` (`view/sync.rs`) rebuilds this from
//! `ContextMirror::blocks()` on every sync — never wire-fed, never persisted.
//! It used to be `kaijutsu_kernel::blocks::BlockDocument`, which carries a
//! Lamport clock, fractional-index ordering, and DAG-reference validation for
//! multi-writer ordering. None of that applies here: this store has exactly one
//! writer (the sync system), and it is thrown away and rebuilt on every
//! version bump — during live streaming, nearly every frame (`ContextMirror::
//! apply` advances the version per event). [`RenderBlockStore::rebuild`] is
//! that whole rebuild, including carrying `collapsed` forward across it so a
//! local toggle survives streaming elsewhere in the document.
//! `RenderBlockStore` keeps the methods the app calls — `blocks_ordered`,
//! `block_ids_ordered`, `get_block_snapshot`, `full_text`, `is_empty`,
//! `version`, `set_version`, `principal_id`, `set_collapsed`, `move_block`,
//! `insert_block`, `insert_from_snapshot`, `rebuild` — over a `Vec` plus an
//! id-to-index map.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Source of [`RenderBlockStore::generation`]. Process-global and minted in
/// `new`, so no store can be constructed without a distinct id and no swap
/// site can forget to bump one. Starts at 1 so `0` stays available to
/// consumers as "no store seen yet".
static NEXT_STORE_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Ordered render buffer for one `CellEditor` — see the module doc.
pub struct RenderBlockStore {
    context_id: ContextId,
    principal_id: PrincipalId,
    blocks: Vec<BlockSnapshot>,
    index: HashMap<BlockId, usize>,
    /// Instance id — see [`RenderBlockStore::generation`].
    generation: u64,
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
            generation: NEXT_STORE_GENERATION.fetch_add(1, Ordering::Relaxed),
            next_seq: 0,
            version: 0,
        }
    }

    /// Instance id of this store, unique for the life of the process.
    ///
    /// `sync_main_cell_to_conversation` never mutates a store in place across
    /// a context switch — it builds a fresh one with [`rebuild`](Self::rebuild)
    /// and assigns it over `CellEditor.store`. The document version cannot see
    /// that: a hydrated context can land on the same number the welcome buffer
    /// was already at. This is the O(1) signal for "the store under me was
    /// replaced", so a consumer's expensive re-derivation (the geometry
    /// reconcile's document-order id walk) is paid only when it might matter.
    pub fn generation(&self) -> u64 {
        self.generation
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
    /// alone.
    ///
    /// This store is rebuilt wholesale on every mirror version bump — during
    /// live streaming that is nearly every frame (`ContextMirror::apply`
    /// advances the version per event, so a `TextAppended` anywhere in the
    /// document retriggers the rebuild), not just at hydration boundaries.
    /// Called directly, this default would fire on every rebuild, not just
    /// true first arrival, wiping a user's expand within a frame or two of
    /// it happening — callers that rebuild the whole store on a cadence like
    /// that should go through [`Self::rebuild`] instead, which carries
    /// `collapsed` forward for ids that already existed and only lets this
    /// default land on ids that didn't.
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

    /// Rebuild a fresh store from wire snapshots in document order —
    /// `sync_main_cell_to_conversation`'s (`view/sync.rs`) whole job, pulled
    /// out here so it is unit-testable without a Bevy `App`.
    ///
    /// `self` is the OUTGOING store: its `collapsed` per id is captured
    /// before any inserts, then reapplied to the new store for every id
    /// that already existed. That is what makes `insert_from_snapshot`'s
    /// Error-defaults-collapsed rule mean "first time this block has ever
    /// been seen" rather than "first time in *this* store" — see that
    /// method's doc for why this store's rebuild cadence (nearly every
    /// frame during live streaming) makes the distinction matter. A block
    /// id absent from `self` (true first arrival, or `self` was empty) gets
    /// no override, so `insert_from_snapshot`'s own default applies.
    ///
    /// `skip` filters snapshots that should never be materialized here (the
    /// local principal's own compose draft — already rendered in the
    /// compose box). Stops and returns the first error rather than
    /// continuing into an uncertain partial state; the caller should not
    /// swap it in on failure.
    pub fn rebuild<'a>(
        &self,
        context_id: ContextId,
        principal_id: PrincipalId,
        snapshots: impl IntoIterator<Item = &'a BlockSnapshot>,
        mut skip: impl FnMut(&BlockSnapshot) -> bool,
    ) -> Result<RenderBlockStore, RenderStoreError> {
        let prev_collapsed: HashMap<BlockId, bool> =
            self.blocks.iter().map(|b| (b.id, b.collapsed)).collect();

        let mut store = RenderBlockStore::new(context_id, principal_id);
        let mut after = None;
        for block in snapshots {
            if skip(block) {
                continue;
            }
            let id = store.insert_from_snapshot(block.clone(), after.as_ref())?;
            if let Some(&was_collapsed) = prev_collapsed.get(&id) {
                store.set_collapsed(&id, was_collapsed)?;
            }
            after = Some(id);
        }
        Ok(store)
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
    /// matching `kaijutsu_kernel::blocks::BlockDocument::move_block`'s
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

    // ------------------------------------------------------------------
    // rebuild — the sync.rs whole-document rebuild, carrying `collapsed`
    // forward across it. This runs nearly every frame during live
    // streaming (a `TextAppended` anywhere bumps the mirror version), so
    // "survives one rebuild" is really "survives streaming elsewhere in
    // the document," the bug a kaibo/gemini review caught.
    // ------------------------------------------------------------------

    #[test]
    fn rebuild_carries_forward_a_user_expanded_error_block() {
        let (ctx, agent) = ctx_and_principal();
        let id = BlockId::new(ctx, agent, 0);

        let mut old = RenderBlockStore::new(ctx, agent);
        // The wire snapshot never carries collapsed:true for this block —
        // `collapsed` is never round-tripped through the kernel — so the
        // SAME snapshot arrives on every rebuild regardless of what the
        // user did locally.
        let wire = BlockSnapshotBuilder::new(id, BlockKind::Error)
            .content("boom")
            .build();
        old.insert_from_snapshot(wire.clone(), None).unwrap();
        assert!(
            old.get_block_snapshot(&id).unwrap().collapsed,
            "sanity: defaults collapsed on first arrival"
        );
        old.set_collapsed(&id, false).unwrap(); // user expanded it

        let rebuilt = old
            .rebuild(ctx, agent, std::iter::once(&wire), |_| false)
            .unwrap();

        assert!(
            !rebuilt.get_block_snapshot(&id).unwrap().collapsed,
            "user's expand must survive a rebuild, not be re-defaulted to collapsed"
        );
    }

    #[test]
    fn rebuild_carries_forward_a_user_collapsed_thinking_block() {
        let (ctx, agent) = ctx_and_principal();
        let id = BlockId::new(ctx, agent, 0);

        let mut old = RenderBlockStore::new(ctx, agent);
        let wire = BlockSnapshotBuilder::new(id, BlockKind::Thinking)
            .content("pondering")
            .build();
        old.insert_from_snapshot(wire.clone(), None).unwrap();
        old.set_collapsed(&id, true).unwrap(); // user collapsed it

        let rebuilt = old
            .rebuild(ctx, agent, std::iter::once(&wire), |_| false)
            .unwrap();

        assert!(
            rebuilt.get_block_snapshot(&id).unwrap().collapsed,
            "user's collapse must survive a rebuild, not be silently re-expanded"
        );
    }

    #[test]
    fn rebuild_still_defaults_a_genuinely_new_error_block_to_collapsed() {
        let (ctx, agent) = ctx_and_principal();
        let old_id = BlockId::new(ctx, agent, 0);
        let new_id = BlockId::new(ctx, agent, 1);

        let mut old = RenderBlockStore::new(ctx, agent);
        let old_wire = BlockSnapshotBuilder::new(old_id, BlockKind::Text)
            .content("hi")
            .build();
        old.insert_from_snapshot(old_wire.clone(), None).unwrap();

        // A brand-new Error block arrives alongside the pre-existing one,
        // in the same rebuild — its id was never in `old`, so it gets the
        // real first-arrival default, not a carried-forward value.
        let new_wire = BlockSnapshotBuilder::new(new_id, BlockKind::Error)
            .content("boom")
            .build();
        let rebuilt = old
            .rebuild(ctx, agent, vec![&old_wire, &new_wire], |_| false)
            .unwrap();

        assert!(
            rebuilt.get_block_snapshot(&new_id).unwrap().collapsed,
            "a block never seen before should still default collapsed"
        );
    }

    #[test]
    fn rebuild_skips_blocks_the_predicate_rejects() {
        let (ctx, agent) = ctx_and_principal();
        let draft_id = BlockId::new(ctx, agent, 0);
        let keep_id = BlockId::new(ctx, agent, 1);

        let draft = BlockSnapshotBuilder::new(draft_id, BlockKind::Text)
            .status(Status::Draft)
            .build();
        let keep = BlockSnapshotBuilder::new(keep_id, BlockKind::Text)
            .status(Status::Done)
            .build();

        let old = RenderBlockStore::new(ctx, agent);
        let rebuilt = old
            .rebuild(ctx, agent, vec![&draft, &keep], |b| b.status == Status::Draft)
            .unwrap();

        assert!(rebuilt.get_block_snapshot(&draft_id).is_none());
        assert!(rebuilt.get_block_snapshot(&keep_id).is_some());
    }
}
