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
//! that whole rebuild. Each block's `collapsed` comes from the viewer's
//! [`ContextFolds`], not from the wire, so a fold survives streaming
//! elsewhere in the document.
//! `RenderBlockStore` keeps the methods the app calls — `blocks_ordered`,
//! `block_ids_ordered`, `get_block_snapshot`,
//! `version`, `set_version`, `principal_id`, `set_collapsed`,
//! `insert_block`, `rebuild` — over a `Vec` plus an id-to-index map.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use kaijutsu_present::fold::{self, AutoFold};
use kaijutsu_types::{
    BlockId, BlockKind, BlockSnapshot, BlockSnapshotBuilder, ContentType, ContextId, PrincipalId,
    Role, Status,
};

/// The viewer's folds for one context.
///
/// A fold is the viewer's own: it is never written to the kernel, and one
/// viewer folding a block does not fold it on another screen. The rule for
/// which blocks fold on their own is `kaijutsu_present::fold`, shared with
/// the terminal client so a conversation reads the same in both.
#[derive(Debug, Default)]
pub struct ContextFolds {
    /// Each block's fold as this viewer last saw it. A block absent here has
    /// not been shown yet.
    folded: HashMap<BlockId, bool>,
    auto: AutoFold,
}

impl ContextFolds {
    /// The fold `block` renders with. The first time a block is shown it
    /// takes the kernel's flag, or its kind's default
    /// (`fold::collapses_by_default`); after that, only this viewer moves it.
    fn resolve(&mut self, block: &BlockSnapshot) -> bool {
        *self
            .folded
            .entry(block.id)
            .or_insert_with(|| block.collapsed || fold::collapses_by_default(block.kind))
    }

    /// Record the viewer's fold for one block. The fold rule has no say
    /// over it afterwards.
    pub fn set(&mut self, id: BlockId, folded: bool) {
        self.folded.insert(id, folded);
        self.auto.decide(id);
    }

    /// Forget blocks that are no longer in the context.
    fn retain(&mut self, live: &HashSet<BlockId>) {
        self.folded.retain(|id, _| live.contains(id));
        self.auto.retain(live);
    }
}

/// A reference to a block this store does not hold.
///
/// `insert_from_snapshot` fails loud on an unresolvable `after`/target id
/// rather than silently falling back to append-at-end — a caller passing a
/// stale id is a bug in the caller, and the render buffer rebuilds from
/// scratch on the next sync regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderStoreError {
    /// `insert_from_snapshot` was given an id it already holds.
    DuplicateBlock(BlockId),
    /// `set_collapsed` targets an id not in the store.
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
// TODO: Render live ContextMirror blocks over `ContextFolds` instead of
// rebuilding a second domain-block store on each version change. Preserve
// welcome/offline content and the layout caches that consume this view.
// See docs/issues.md, "Render the live mirror".
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
    /// `after` (`None` appends at the end), exactly as given.
    fn insert_from_snapshot(
        &mut self,
        snapshot: BlockSnapshot,
        after: Option<&BlockId>,
    ) -> Result<BlockId, RenderStoreError> {
        let id = snapshot.id;
        if self.index.contains_key(&id) {
            return Err(RenderStoreError::DuplicateBlock(id));
        }
        let position = self.position_after(after)?;
        self.insert_at(position, snapshot);
        Ok(id)
    }

    /// Build a fresh store from wire snapshots in document order —
    /// `sync_main_cell_to_conversation`'s (`view/sync.rs`) whole job, pulled
    /// out here so it is unit-testable without a Bevy `App`.
    ///
    /// This runs on every mirror version bump, nearly every frame while a
    /// block streams. Each block's `collapsed` is the viewer's fold from
    /// `folds`, so a fold survives every rebuild. Then the shared auto-fold
    /// rule (`kaijutsu_present::fold::AutoFold::sweep`) folds each block
    /// that has just been followed: settled reasoning, and a long settled
    /// tool result. `held` is the block the viewer is reading; it is left
    /// open.
    ///
    /// `skip` filters snapshots that should never be materialized here (the
    /// local principal's own compose draft — already rendered in the
    /// compose box). Stops and returns the first error rather than
    /// continuing into an uncertain partial state; the caller should not
    /// swap it in on failure.
    pub fn rebuild<'a>(
        context_id: ContextId,
        principal_id: PrincipalId,
        snapshots: impl IntoIterator<Item = &'a BlockSnapshot>,
        mut skip: impl FnMut(&BlockSnapshot) -> bool,
        folds: &mut ContextFolds,
        held: Option<BlockId>,
    ) -> Result<RenderBlockStore, RenderStoreError> {
        let mut store = RenderBlockStore::new(context_id, principal_id);
        let mut after = None;
        for block in snapshots {
            if skip(block) {
                continue;
            }
            let mut block = block.clone();
            block.collapsed = folds.resolve(&block);
            let id = store.insert_from_snapshot(block, after.as_ref())?;
            after = Some(id);
        }
        for id in folds.auto.sweep(&store.blocks, held) {
            folds.set(id, true);
            store.set_collapsed(&id, true)?;
        }
        let live: HashSet<BlockId> = store.index.keys().copied().collect();
        folds.retain(&live);
        Ok(store)
    }

    /// Set the collapsed flag on one block.
    pub fn set_collapsed(&mut self, id: &BlockId, collapsed: bool) -> Result<(), RenderStoreError> {
        let &i = self
            .index
            .get(id)
            .ok_or(RenderStoreError::BlockNotFound(*id))?;
        self.blocks[i].collapsed = collapsed;
        self.version += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_present::fold::TOOL_RESULT_FOLD_LINES;

    struct Fixture {
        ctx: ContextId,
        principal: PrincipalId,
        folds: ContextFolds,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                ctx: ContextId::new(),
                principal: PrincipalId::new(),
                folds: ContextFolds::default(),
            }
        }

        fn block(&self, seq: u64, kind: BlockKind, status: Status, content: &str) -> BlockSnapshot {
            BlockSnapshotBuilder::new(BlockId::new(self.ctx, self.principal, seq), kind)
                .status(status)
                .content(content)
                .build()
        }

        fn rebuild(&mut self, blocks: &[&BlockSnapshot], held: Option<BlockId>) -> RenderBlockStore {
            RenderBlockStore::rebuild(
                self.ctx,
                self.principal,
                blocks.iter().copied(),
                |_| false,
                &mut self.folds,
                held,
            )
            .expect("rebuild")
        }
    }

    fn folded(store: &RenderBlockStore, id: BlockId) -> bool {
        store.get_block_snapshot(&id).expect("block in store").collapsed
    }

    fn lines(n: usize) -> String {
        (0..n).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn reasoning_stays_open_while_newest_and_folds_once_followed() {
        let mut fx = Fixture::new();
        let thinking = fx.block(1, BlockKind::Thinking, Status::Running, "considering");
        let store = fx.rebuild(&[&thinking], None);
        assert!(!folded(&store, thinking.id), "streaming reasoning is read as it streams");

        let settled = BlockSnapshot { status: Status::Done, ..thinking.clone() };
        let store = fx.rebuild(&[&settled], None);
        assert!(!folded(&store, thinking.id), "nothing follows it yet");

        let answer = fx.block(2, BlockKind::Text, Status::Running, "The");
        let store = fx.rebuild(&[&settled, &answer], None);
        assert!(folded(&store, thinking.id), "the answer began, so the reasoning folds");
        assert!(!folded(&store, answer.id));
    }

    #[test]
    fn a_reopened_fold_stays_open_across_rebuilds() {
        let mut fx = Fixture::new();
        let thinking = fx.block(1, BlockKind::Thinking, Status::Done, "considering");
        let answer = fx.block(2, BlockKind::Text, Status::Done, "ok");
        let store = fx.rebuild(&[&thinking, &answer], None);
        assert!(folded(&store, thinking.id));

        fx.folds.set(thinking.id, false); // the viewer reopens it
        let more = fx.block(3, BlockKind::Text, Status::Running, "and");
        let store = fx.rebuild(&[&thinking, &answer, &more], None);
        assert!(!folded(&store, thinking.id), "a reopened block is never folded again");
    }

    #[test]
    fn a_fold_opened_by_hand_before_anything_follows_stays_open() {
        let mut fx = Fixture::new();
        let thinking = fx.block(1, BlockKind::Thinking, Status::Done, "considering");
        fx.rebuild(&[&thinking], None);
        fx.folds.set(thinking.id, true); // the viewer folds the newest block
        fx.folds.set(thinking.id, false); // and opens it again

        let answer = fx.block(2, BlockKind::Text, Status::Running, "The");
        let store = fx.rebuild(&[&thinking, &answer], None);
        assert!(!folded(&store, thinking.id), "the viewer's own fold is not decided again");
    }

    #[test]
    fn a_hydrated_context_shows_its_history_folded() {
        let mut fx = Fixture::new();
        let thinking = fx.block(1, BlockKind::Thinking, Status::Done, "considering");
        let answer = fx.block(2, BlockKind::Text, Status::Done, "ok");
        let result = fx.block(3, BlockKind::ToolResult, Status::Done, &lines(40));
        let last_thinking = fx.block(4, BlockKind::Thinking, Status::Done, "newest");

        let store = fx.rebuild(&[&thinking, &answer, &result, &last_thinking], None);
        assert!(folded(&store, thinking.id));
        assert!(folded(&store, result.id));
        assert!(!folded(&store, answer.id));
        assert!(!folded(&store, last_thinking.id), "the newest block stays open");
    }

    #[test]
    fn only_a_long_settled_tool_result_folds() {
        let mut fx = Fixture::new();
        let short = fx.block(1, BlockKind::ToolResult, Status::Done, &lines(TOOL_RESULT_FOLD_LINES));
        let long = fx.block(2, BlockKind::ToolResult, Status::Done, &lines(TOOL_RESULT_FOLD_LINES + 1));
        let running = fx.block(3, BlockKind::ToolResult, Status::Running, &lines(400));
        let last = fx.block(4, BlockKind::Text, Status::Done, "end");

        let store = fx.rebuild(&[&short, &long, &running, &last], None);
        assert!(!folded(&store, short.id), "a short result is cheaper to read than to open");
        assert!(folded(&store, long.id));
        assert!(!folded(&store, running.id), "a running result is still being read");
    }

    #[test]
    fn the_block_the_viewer_is_reading_is_left_open() {
        let mut fx = Fixture::new();
        let thinking = fx.block(1, BlockKind::Thinking, Status::Done, "considering");
        let answer = fx.block(2, BlockKind::Text, Status::Running, "The");

        let store = fx.rebuild(&[&thinking, &answer], Some(thinking.id));
        assert!(!folded(&store, thinking.id), "a fold never moves the text under the reader");

        let store = fx.rebuild(&[&thinking, &answer], None);
        assert!(!folded(&store, thinking.id), "the reader leaving does not fold it behind them");
    }

    #[test]
    fn an_error_block_is_folded_the_first_time_it_is_shown() {
        let mut fx = Fixture::new();
        let error = fx.block(1, BlockKind::Error, Status::Error, "boom");
        assert!(!error.collapsed, "sanity: wire default is unfolded");
        let store = fx.rebuild(&[&error], None);
        assert!(folded(&store, error.id));
    }

    #[test]
    fn the_viewers_expand_of_an_error_survives_a_rebuild() {
        let mut fx = Fixture::new();
        let error = fx.block(1, BlockKind::Error, Status::Error, "boom");
        fx.rebuild(&[&error], None);
        fx.folds.set(error.id, false);
        let store = fx.rebuild(&[&error], None);
        assert!(!folded(&store, error.id));
    }

    #[test]
    fn the_viewers_fold_of_a_newest_block_survives_a_rebuild() {
        let mut fx = Fixture::new();
        let thinking = fx.block(1, BlockKind::Thinking, Status::Running, "considering");
        fx.rebuild(&[&thinking], None);
        fx.folds.set(thinking.id, true);
        let store = fx.rebuild(&[&thinking], None);
        assert!(folded(&store, thinking.id));
    }

    #[test]
    fn a_kernel_folded_block_starts_folded() {
        let mut fx = Fixture::new();
        let mut text = fx.block(1, BlockKind::Text, Status::Done, "hi");
        text.collapsed = true;
        let store = fx.rebuild(&[&text], None);
        assert!(folded(&store, text.id));
    }

    #[test]
    fn rebuild_skips_blocks_the_predicate_rejects() {
        let mut fx = Fixture::new();
        let draft = fx.block(0, BlockKind::Text, Status::Draft, "");
        let keep = fx.block(1, BlockKind::Text, Status::Done, "");

        let store = RenderBlockStore::rebuild(
            fx.ctx,
            fx.principal,
            [&draft, &keep],
            |b| b.status == Status::Draft,
            &mut fx.folds,
            None,
        )
        .unwrap();

        assert!(store.get_block_snapshot(&draft.id).is_none());
        assert!(store.get_block_snapshot(&keep.id).is_some());
    }
}
