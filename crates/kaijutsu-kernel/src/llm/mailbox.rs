//! ConversationMailbox — the live conversation session for one context.
//!
//! Holds a [`HydrationState`] and feeds blocks into it either as a
//! bootstrap batch (fork / new / cold start / attach) or one at a
//! time (incremental, from a `BlockFlow::Inserted` subscriber).
//!
//! [`snapshot`] returns the wire-history view by cloning the internal
//! state, running final flush + tool_use/result repair on the clone,
//! and returning the resulting `Vec<Message>`. The original state is
//! untouched — see invariant #2 in the
//! `architecture_context_invariants` design note (conversation is
//! append-only after commit).
//!
//! **Out of scope for this slice:** the tool_use/tool_result *gate*
//! that holds non-result writers while a tool_use is pending. That's
//! an insert-time concern (the durable block log must also stay
//! coherent), tracked as a follow-up in `docs/conversation-session.md`.
//!
//! [`snapshot`]: ConversationMailbox::snapshot
//! [`HydrationState`]: super::hydrate::HydrationState

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use kaijutsu_types::{BlockId, BlockSnapshot};
use tracing::warn;

use super::Message;
use super::hydrate::HydrationState;
use super::splice::{SpliceItem, plan_splice};

/// Live conversation session for one context.
///
/// Holds accumulating state across many block feeds; produces a
/// repaired `Vec<Message>` on demand without consuming itself.
pub struct ConversationMailbox {
    state: HydrationState,
    /// Block ids already folded into `state`. Lets `catch_up` and
    /// `feed` be idempotent so the LLM-stream path can call
    /// `catch_up(&block_log)` every turn without paying for blocks
    /// it has already processed.
    seen: HashSet<BlockId>,
    /// True once `bootstrap`, `feed`, or `catch_up` has been called
    /// at least once. Lets the LLM-stream path tell "I've hydrated
    /// this context (and it might genuinely be empty)" apart from
    /// "cache miss — need to hydrate."
    materialized: bool,
    /// True when the current `state`/`seen` were built by a windowed
    /// rebuild ([`rehydrate_windowed`]), so `seen` has a HOLE where the
    /// archived middle was. An incremental [`catch_up`] over that hole
    /// would fold the now-"unseen" middle and append it *after* the tail
    /// — a scrambled, out-of-order wire. The flag makes `catch_up` rebuild
    /// from scratch on the windowed→full transition instead.
    ///
    /// [`rehydrate_windowed`]: Self::rehydrate_windowed
    /// [`catch_up`]: Self::catch_up
    windowed: bool,
}

/// The keep-set a windowed conversation hydrates, as its maximal runs over the
/// log: the pinned prefix `[0, marker]` and the sliding tail (the last `window`
/// blocks). This is the order-free `window` selection — the cost guard for
/// endless composer logs (design: `docs/chameleon.md`, the hydration marker).
/// Cut hygiene (turn-boundary snapping, tool-pair integrity, archived-gap
/// seams) is applied separately by [`plan_splice`] when the runs are walked.
///
/// `marker == None` → **no windowing**, the whole log `[0, len)` (today's
/// behavior, and every non-composer context — they never set a marker). With a
/// marker the runs are `[0, marker+1)` and `[len-window, len)`; these may
/// overlap (a short log, or a window reaching into/behind the prefix) — that's
/// fine, `plan_splice` merges them into a seamless whole.
///
/// **Fail-safe on a stale marker:** a `marker` naming a block not in `blocks` is
/// a bug (markers point at durable blocks), but we return the whole log rather
/// than hide context behind a marker that went stale — never hydrate less
/// because the marker is wrong. The caller logs the anomaly.
fn hydration_keep_set(
    blocks: &[BlockSnapshot],
    marker: Option<BlockId>,
    window: usize,
) -> Vec<Range<usize>> {
    // Resolve the marker `BlockId` to its position, then defer to the single
    // definition of the `window` shape shared with the fork side. A `None` or
    // stale (not-found) marker resolves to no index → the whole log. The result
    // is a canonical (sorted, disjoint, merged) keep-set; `plan_splice` owns the
    // order-dependent cut hygiene (snapping, tool-pairs, archived-gap seams).
    let marker_idx = marker.and_then(|m| blocks.iter().position(|b| b.id == m));
    kaijutsu_crdt::window_base(blocks.len(), marker_idx, window).into_runs()
}

impl ConversationMailbox {
    /// Create an empty, un-materialized mailbox.
    pub fn new() -> Self {
        Self {
            state: HydrationState::new(),
            seen: HashSet::new(),
            materialized: false,
            windowed: false,
        }
    }

    /// Has anything been fed in yet? Becomes `true` on the first
    /// `bootstrap`, `feed`, or `catch_up` call, regardless of whether
    /// the input was empty.
    pub fn is_materialized(&self) -> bool {
        self.materialized
    }

    /// How many blocks have been folded in so far. Useful for
    /// diagnostics ("hydrated N of M") in the LLM-stream path.
    pub fn block_count(&self) -> usize {
        self.seen.len()
    }

    /// Fold a full block batch into the session. Use at boundary
    /// events — fork, new context, cold start, peer attach — when
    /// the durable log is the source of truth and the mailbox is
    /// known to be empty. Idempotent: blocks already seen are
    /// skipped.
    pub fn bootstrap(&mut self, blocks: &[BlockSnapshot]) {
        self.catch_up(blocks);
    }

    /// Fold one block into the session — incremental path.
    ///
    /// Idempotent: a block already folded in is silently skipped, so
    /// a push subscriber that delivers the same event twice is
    /// harmless.
    ///
    /// `parent` is consulted only for `BlockKind::Error` blocks (for
    /// fold-into-parent semantics). Callers that don't have the
    /// parent on hand can pass `None`; the Error block then falls
    /// back to a standalone user message.
    pub fn feed(&mut self, block: &BlockSnapshot, parent: Option<&BlockSnapshot>) {
        if self.seen.contains(&block.id) {
            return;
        }
        self.state.translate_block(block, parent);
        self.seen.insert(block.id);
        self.materialized = true;
    }

    /// Catch up against the full current block log — fold any blocks
    /// not yet seen, in their committed order. Returns the number
    /// of new blocks folded in.
    ///
    /// This is the pull-based incremental path: the LLM-stream
    /// caller hands over the current `block_snapshots(context_id)`
    /// result every turn, the mailbox reconciles against its own
    /// high-water mark, and only the delta is processed. Cheaper
    /// than re-hydrating the entire conversation, and works without
    /// a separate BlockFlow subscriber.
    pub fn catch_up(&mut self, blocks: &[BlockSnapshot]) -> usize {
        // Windowed→full transition: the prior windowed rebuild left a HOLE in
        // `seen` (the archived middle). An incremental fold would translate
        // those now-"unseen" middle blocks and append them *after* the tail —
        // a scrambled, out-of-order wire. Discard the windowed state and rebuild
        // the whole log in chronological order. (Reached via `kj context hydrate
        // --clear` or a fail-safe-to-None policy read.)
        if self.windowed {
            self.state = HydrationState::new();
            self.seen.clear();
            self.windowed = false;
        }
        let by_id: HashMap<BlockId, &BlockSnapshot> =
            blocks.iter().map(|b| (b.id, b)).collect();
        let mut new_blocks = 0usize;
        for block in blocks {
            if self.seen.contains(&block.id) {
                continue;
            }
            let parent = block.parent_id.and_then(|pid| by_id.get(&pid).copied());
            self.state.translate_block(block, parent);
            self.seen.insert(block.id);
            new_blocks += 1;
        }
        self.materialized = true;
        new_blocks
    }

    /// Rebuild the session against a **windowed** view of the log: discard the
    /// accumulated state and re-fold exactly `[0, marker] ∪ last-window`
    /// ([`hydration_keep_set`]), spliced clean. The windowed counterpart to
    /// [`catch_up`].
    ///
    /// Why a rebuild and not an incremental fold: a sliding tail means a block
    /// can fall *out* of the window as new turns arrive, which the append-only
    /// `catch_up` can't express. So a windowed context rebuilds every turn —
    /// bounded work (prefix + window), not O(history). The `[0, marker]` prefix
    /// is byte-identical on every rebuild, so the wire prompt cache still aligns
    /// on it; only the sliding tail re-streams.
    ///
    /// Parents are resolved against the **full** log (not just the window) so an
    /// `Error` block whose parent is archived still folds correctly.
    ///
    /// [`catch_up`]: Self::catch_up
    pub fn rehydrate_windowed(
        &mut self,
        blocks: &[BlockSnapshot],
        marker: BlockId,
        window: usize,
    ) {
        // A marker that doesn't resolve fails safe to the whole log (never hide
        // context behind a stale marker) — but that silently turns the cost
        // guard OFF on a context driving at tempo, the exact failure this
        // feature exists to prevent. Warn loudly and recurringly (this runs
        // every turn) so the anomaly is observable above the info-level numbers.
        if !blocks.iter().any(|b| b.id == marker) {
            warn!(
                marker = %marker,
                blocks = blocks.len(),
                "hydration marker not in block log; windowing bypassed — \
                 hydrating FULL history (cost guard OFF). Re-set with `kj context hydrate`."
            );
        }
        self.state = HydrationState::new();
        self.seen.clear();
        let by_id: HashMap<BlockId, &BlockSnapshot> =
            blocks.iter().map(|b| (b.id, b)).collect();
        // The order-free `window` keep-set, spliced clean: turn-boundary snaps,
        // tool-pairs kept whole, archived gaps marked with a user-role seam.
        let keep_set = hydration_keep_set(blocks, Some(marker), window);
        for item in plan_splice(blocks, &keep_set) {
            match item {
                SpliceItem::Keep(i) => {
                    let block = &blocks[i];
                    let parent = block.parent_id.and_then(|pid| by_id.get(&pid).copied());
                    self.state.translate_block(block, parent);
                    self.seen.insert(block.id);
                }
                SpliceItem::Seam { archived } => {
                    self.state.push_seam(archived);
                }
            }
        }
        self.materialized = true;
        self.windowed = true;
    }

    /// Return the current wire-history view.
    ///
    /// Clones the internal state, runs final flush +
    /// tool_use/tool_result repair on the clone, returns the
    /// resulting `Vec<Message>`. The mailbox itself is unchanged, so
    /// subsequent `feed` calls continue against the same accumulator.
    pub fn snapshot(&self) -> Vec<Message> {
        self.state.clone().into_messages()
    }
}

impl Default for ConversationMailbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ContentBlock, MessageContent, Role, hydrate_from_blocks};
    use kaijutsu_types::{
        BlockId, BlockKind, BlockSnapshot, BlockSnapshotBuilder, ContentType, ContextId,
        PrincipalId, Role as BlockRole, Tick,
    };
    use std::cell::Cell;

    // Per-thread ctx/principal/seq so block ids are unique across
    // blocks built in one test. Each cargo-test thread gets its own
    // fixed (ctx, principal); seq is monotonic across the thread.
    thread_local! {
        static TEST_CTX: ContextId = ContextId::new();
        static TEST_PRINCIPAL: PrincipalId = PrincipalId::new();
        static SEQ: Cell<u64> = const { Cell::new(0) };
    }

    fn next_seq() -> u64 {
        SEQ.with(|s| {
            let v = s.get();
            s.set(v + 1);
            v
        })
    }

    fn text_block(role: BlockRole, content: &str) -> BlockSnapshot {
        let c = TEST_CTX.with(|v| *v);
        let p = TEST_PRINCIPAL.with(|v| *v);
        BlockSnapshotBuilder::new(BlockId::new(c, p, next_seq()), BlockKind::Text)
            .role(role)
            .content(content)
            .build()
    }

    fn user_text(s: &str) -> BlockSnapshot {
        text_block(BlockRole::User, s)
    }

    fn model_text(s: &str) -> BlockSnapshot {
        text_block(BlockRole::Model, s)
    }

    fn assistant_text_of(msg: &Message) -> Option<&str> {
        if msg.role != Role::Assistant {
            return None;
        }
        match &msg.content {
            MessageContent::Text(t) => Some(t.as_str()),
            MessageContent::Blocks(blocks) => blocks.iter().find_map(|b| {
                if let ContentBlock::Text { text } = b {
                    Some(text.as_str())
                } else {
                    None
                }
            }),
        }
    }

    // ── hydration_keep_set: the order-free `window` runs ──────────────────
    //
    // These pin the keep-set *producer* — now `kaijutsu_crdt::window_base`, the
    // shape shared with the fork side. It returns a canonical (merged) set, so
    // an overlapping prefix/tail collapses here rather than in the splicer; the
    // wire result is identical either way. Cut hygiene — snapping, tool-pairs,
    // archived-gap seams — remains plan_splice's job and lives in `llm::splice`
    // tests; the windowed end-to-end behavior is exercised by the
    // `rehydrate_windowed_*` tests below.

    /// A run of `n` user blocks, so position in the log is observable by content
    /// ("b0".."b{n-1}"). Each gets a unique id via the per-thread seq.
    fn run_of(n: usize) -> Vec<BlockSnapshot> {
        (0..n).map(|i| user_text(&format!("b{i}"))).collect()
    }

    #[test]
    fn keep_set_none_marker_is_whole_log() {
        let blocks = run_of(5);
        assert_eq!(super::hydration_keep_set(&blocks, None, 2), vec![0..5]);
    }

    #[test]
    fn keep_set_prefix_and_tail() {
        // 10 blocks, marker = b2 (prefix [0,3)), window 3 (tail [7,10)). The
        // middle [3,7) is the archived gap.
        let blocks = run_of(10);
        let marker = blocks[2].id;
        assert_eq!(
            super::hydration_keep_set(&blocks, Some(marker), 3),
            vec![0..3, 7..10],
        );
    }

    #[test]
    fn keep_set_overlap_merges_to_whole_log() {
        // 5 blocks, marker = b2 (prefix [0,3)), window 4 → tail_start 1. The
        // prefix and tail overlap; the canonical producer merges them to the
        // whole log (with no gap, so the splicer emits no seam).
        let blocks = run_of(5);
        let marker = blocks[2].id;
        assert_eq!(
            super::hydration_keep_set(&blocks, Some(marker), 4),
            vec![0..5],
        );
    }

    #[test]
    fn keep_set_zero_window_is_prefix_only() {
        // window 0 → tail run is empty (`len..len`); the canonical producer
        // drops it, leaving just the prefix.
        let blocks = run_of(6);
        let marker = blocks[1].id;
        assert_eq!(
            super::hydration_keep_set(&blocks, Some(marker), 0),
            vec![0..2],
        );
    }

    #[test]
    fn keep_set_stale_marker_fails_safe_to_whole_log() {
        // A marker naming a block not in the log → the whole log, never less.
        let blocks = run_of(4);
        let absent = BlockId::new(TEST_CTX.with(|v| *v), TEST_PRINCIPAL.with(|v| *v), 9999);
        assert_eq!(
            super::hydration_keep_set(&blocks, Some(absent), 1),
            vec![0..4],
            "stale marker must not hide context"
        );
    }

    #[test]
    fn rehydrate_windowed_folds_prefix_and_tail_skips_middle() {
        // A 6-block conversation; marker = the 2nd block (prefix = first two),
        // window 2 (last two). The middle exchange is archived — its text must
        // not reach the wire.
        let blocks = vec![
            user_text("q0"),
            model_text("a0"),
            user_text("q1-ARCHIVED"),
            model_text("a1-ARCHIVED"),
            user_text("q2"),
            model_text("a2"),
        ];
        let marker = blocks[1].id;

        let mut mb = ConversationMailbox::new();
        mb.rehydrate_windowed(&blocks, marker, 2);
        let wire: String = mb
            .snapshot()
            .iter()
            .filter_map(|m| m.as_text().map(str::to_string))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(mb.is_materialized());
        for kept in ["q0", "a0", "q2", "a2"] {
            assert!(wire.contains(kept), "windowed wire must keep {kept}; got: {wire}");
        }
        assert!(!wire.contains("ARCHIVED"), "middle must be skipped; got: {wire}");
        // The archived middle (q1, a1 — 2 blocks) leaves a visible seam between
        // the pinned prefix and the sliding tail, so the cross-gap Model/Text
        // fragments can't merge into false continuity.
        assert!(
            wire.contains("2 blocks archived"),
            "a seam must mark the archived gap; got: {wire}"
        );
    }

    #[test]
    fn rehydrate_windowed_is_idempotent_and_resets_prior_state() {
        // Calling it twice yields the same wire (a windowed context rebuilds
        // each turn), and a prior full catch_up doesn't leak archived blocks.
        let blocks = vec![
            user_text("p0"),
            model_text("p1"),
            user_text("mid-GONE"),
            user_text("t0"),
            model_text("t1"),
        ];
        let marker = blocks[1].id;

        let mut mb = ConversationMailbox::new();
        mb.catch_up(&blocks); // full hydrate first (mid-GONE folded in)
        mb.rehydrate_windowed(&blocks, marker, 2); // then window it
        let first: String = mb
            .snapshot()
            .iter()
            .filter_map(|m| m.as_text().map(str::to_string))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!first.contains("GONE"), "rebuild must drop the previously-folded middle");

        mb.rehydrate_windowed(&blocks, marker, 2);
        let second: String = mb
            .snapshot()
            .iter()
            .filter_map(|m| m.as_text().map(str::to_string))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(first, second, "windowed rebuild is stable turn-to-turn");
    }

    #[test]
    fn catch_up_after_windowed_rebuilds_full_in_chronological_order() {
        // The CRITICAL transition: a mailbox windowed last turn (its `seen` set
        // has a HOLE where the archived middle was) then hydrated full this turn
        // (policy cleared, or a fail-safe-to-None on DB error / unparseable
        // marker). A naive incremental catch_up would fold the now-"unseen"
        // middle and APPEND it after the tail → [prefix, tail, …middle], a
        // scrambled out-of-order wire. catch_up must rebuild the full log in
        // chronological order instead.
        let blocks = vec![
            user_text("u0"),
            model_text("a0"),
            user_text("u1-MID"),
            model_text("a1-MID"),
            user_text("u2"),
            model_text("a2"),
        ];
        let marker = blocks[1].id; // prefix [u0, a0]; window 2 → tail [u2, a2]

        let mut mb = ConversationMailbox::new();
        mb.rehydrate_windowed(&blocks, marker, 2); // middle archived
        mb.catch_up(&blocks); // policy gone → full history

        let after_transition: Vec<(Role, Option<String>)> = mb
            .snapshot()
            .iter()
            .map(|m| (m.role, m.as_text().map(str::to_string)))
            .collect();

        // Ground truth: a mailbox that only ever did a full catch_up.
        let mut fresh = ConversationMailbox::new();
        fresh.catch_up(&blocks);
        let expected: Vec<(Role, Option<String>)> = fresh
            .snapshot()
            .iter()
            .map(|m| (m.role, m.as_text().map(str::to_string)))
            .collect();

        assert_eq!(
            after_transition, expected,
            "catch_up after a windowed turn must rebuild full chronological history, \
             not append the archived middle after the tail"
        );
    }

    #[test]
    fn new_mailbox_is_not_materialized_and_snapshot_is_empty() {
        let mb = ConversationMailbox::new();
        assert!(!mb.is_materialized());
        assert!(mb.snapshot().is_empty());
    }

    #[test]
    fn empty_bootstrap_still_marks_materialized() {
        let mut mb = ConversationMailbox::new();
        mb.bootstrap(&[]);
        assert!(mb.is_materialized());
        assert!(mb.snapshot().is_empty());
    }

    #[test]
    fn bootstrap_matches_hydrate_from_blocks_for_simple_exchange() {
        let blocks = vec![
            user_text("hello"),
            model_text("hi there"),
            user_text("how are you?"),
            model_text("doing well"),
        ];

        let direct = hydrate_from_blocks(&blocks);

        let mut mb = ConversationMailbox::new();
        mb.bootstrap(&blocks);
        let via_mailbox = mb.snapshot();

        assert_eq!(direct.len(), via_mailbox.len());
        for (a, b) in direct.iter().zip(via_mailbox.iter()) {
            assert_eq!(a.role, b.role);
            assert_eq!(a.as_text(), b.as_text());
        }
    }

    #[test]
    fn incremental_feed_matches_bootstrap_for_same_blocks() {
        let blocks = vec![
            user_text("first"),
            model_text("reply one"),
            user_text("second"),
            model_text("reply two"),
        ];

        let mut batched = ConversationMailbox::new();
        batched.bootstrap(&blocks);

        let mut streamed = ConversationMailbox::new();
        for b in &blocks {
            streamed.feed(b, None);
        }

        let a = batched.snapshot();
        let b = streamed.snapshot();
        assert_eq!(a.len(), b.len());
        for (m1, m2) in a.iter().zip(b.iter()) {
            assert_eq!(m1.role, m2.role);
            assert_eq!(m1.as_text(), m2.as_text());
        }
    }

    #[test]
    fn snapshot_is_non_destructive() {
        let blocks = vec![user_text("hi"), model_text("hello")];
        let mut mb = ConversationMailbox::new();
        mb.bootstrap(&blocks);

        let first = mb.snapshot();
        let second = mb.snapshot();

        assert_eq!(first.len(), second.len());
        for (a, b) in first.iter().zip(second.iter()) {
            assert_eq!(a.role, b.role);
            assert_eq!(a.as_text(), b.as_text());
        }
    }

    #[test]
    fn snapshot_after_partial_feed_then_more_feed_grows_correctly() {
        let first_half = vec![user_text("turn one"), model_text("reply one")];
        let second_half = vec![user_text("turn two"), model_text("reply two")];

        let mut mb = ConversationMailbox::new();
        for b in &first_half {
            mb.feed(b, None);
        }
        let after_first = mb.snapshot();
        assert_eq!(after_first.len(), 2);

        for b in &second_half {
            mb.feed(b, None);
        }
        let after_second = mb.snapshot();
        assert_eq!(after_second.len(), 4);

        // Verify the first turn's content didn't get rewritten by the second feed
        // (live conversation is write-only after commit).
        assert_eq!(after_first[0].as_text(), after_second[0].as_text());
        assert_eq!(
            assistant_text_of(&after_first[1]),
            assistant_text_of(&after_second[1]),
        );
    }

    #[test]
    fn many_model_text_blocks_concatenate_within_one_assistant_turn() {
        // Streaming assistant text arrives as multiple Model+Text blocks
        // before the next user prompt boundary. The translator joins
        // them into a single assistant message — verify the mailbox
        // preserves that behavior incrementally.
        let blocks = vec![
            user_text("explain it"),
            model_text("part one"),
            model_text("part two"),
            model_text("part three"),
        ];

        let mut mb = ConversationMailbox::new();
        for b in &blocks {
            mb.feed(b, None);
        }
        // No closing user turn yet — assistant text is still in the
        // buffer. snapshot() runs final flush + repair on a clone, so
        // the wire view sees the concatenated message.
        let msgs = mb.snapshot();
        assert_eq!(msgs.len(), 2, "user + assistant after flush");
        let assistant = assistant_text_of(&msgs[1]).expect("assistant turn present");
        assert!(assistant.contains("part one"));
        assert!(assistant.contains("part two"));
        assert!(assistant.contains("part three"));

        // Mailbox state is preserved across snapshot: feeding a follow-up
        // user block should commit the same assistant text, not double-flush.
        mb.feed(&user_text("follow-up"), None);
        let after = mb.snapshot();
        assert_eq!(
            after.len(),
            3,
            "user + assistant + follow-up user, no duplication"
        );
        assert_eq!(
            assistant_text_of(&after[1]),
            assistant_text_of(&msgs[1]),
            "assistant message text identical across snapshots"
        );
    }

    #[test]
    fn catch_up_only_processes_unseen_blocks() {
        let initial = vec![
            user_text("hello"),
            model_text("hi"),
        ];
        let extended = {
            let mut v = initial.clone();
            v.push(user_text("more"));
            v.push(model_text("more reply"));
            v
        };

        let mut mb = ConversationMailbox::new();
        let first = mb.catch_up(&initial);
        assert_eq!(first, 2, "first catch_up processes all blocks");
        assert_eq!(mb.block_count(), 2);

        let second = mb.catch_up(&extended);
        assert_eq!(second, 2, "second catch_up processes only the delta");
        assert_eq!(mb.block_count(), 4);
    }

    #[test]
    fn catch_up_is_idempotent_when_no_new_blocks() {
        let blocks = vec![user_text("once"), model_text("only")];
        let mut mb = ConversationMailbox::new();
        let first = mb.catch_up(&blocks);
        let second = mb.catch_up(&blocks);
        let third = mb.catch_up(&blocks);

        assert_eq!(first, 2);
        assert_eq!(second, 0);
        assert_eq!(third, 0);

        // Snapshot stays stable across repeated no-op catch_ups.
        let snap_a = mb.snapshot();
        let snap_b = mb.snapshot();
        assert_eq!(snap_a.len(), snap_b.len());
        for (a, b) in snap_a.iter().zip(snap_b.iter()) {
            assert_eq!(a.role, b.role);
            assert_eq!(a.as_text(), b.as_text());
        }
    }

    #[test]
    fn feed_is_idempotent_for_duplicate_delivery() {
        let block = user_text("only once please");
        let mut mb = ConversationMailbox::new();
        mb.feed(&block, None);
        mb.feed(&block, None);
        mb.feed(&block, None);

        let msgs = mb.snapshot();
        assert_eq!(
            msgs.len(),
            1,
            "duplicate feed for same block id must not double-emit"
        );
        assert_eq!(mb.block_count(), 1);
    }

    #[test]
    fn catch_up_matches_direct_hydrate_for_a_full_log() {
        let blocks = vec![
            user_text("first"),
            model_text("first reply"),
            user_text("second"),
            model_text("second reply"),
            user_text("third"),
            model_text("third reply"),
        ];

        let direct = hydrate_from_blocks(&blocks);

        // Walk the log in two chunks to exercise the incremental path.
        let mut mb = ConversationMailbox::new();
        mb.catch_up(&blocks[..3]);
        mb.catch_up(&blocks); // full set; should fold only the remaining 3
        let via_catch_up = mb.snapshot();

        assert_eq!(direct.len(), via_catch_up.len());
        for (a, b) in direct.iter().zip(via_catch_up.iter()) {
            assert_eq!(a.role, b.role);
            assert_eq!(a.as_text(), b.as_text());
        }
    }

    // ── T14 (F2 §8): materialized score blocks are hydration-silent ───────
    //
    // The materializer (hyoushigi/mod.rs materialize_committed) stamps both
    // the ABC source block (Role::Model + Text + ContentType::Abc) and the
    // derived MIDI sibling (Role::Asset + Text + parent_id + hash content)
    // `ephemeral = true`. Without that stamp the ABC source would hydrate as
    // assistant text once per materialized phrase — the flood §8 closes.
    // This pins the stamp through BOTH conversation entry points that fold
    // blocks via `translate_block`: bootstrap (`hydrate_from_blocks`) and
    // the live `ConversationMailbox::catch_up`.

    /// An ABC source block shaped exactly as the materializer emits it:
    /// Role::Model + Text + ContentType::Abc + a beat tick + `ephemeral`.
    fn score_abc_block(abc: &str, ephemeral: bool) -> BlockSnapshot {
        let c = TEST_CTX.with(|v| *v);
        let p = TEST_PRINCIPAL.with(|v| *v);
        BlockSnapshotBuilder::new(BlockId::new(c, p, next_seq()), BlockKind::Text)
            .role(BlockRole::Model)
            .content(abc)
            .content_type(ContentType::Abc)
            .tick(Tick::new(16))
            .ephemeral(ephemeral)
            .build()
    }

    /// The derived MIDI sibling shaped as the materializer emits it:
    /// Role::Asset + Text + parent_id (score↔render pairing) + 32-hex hash
    /// content + same tick + `ephemeral`.
    fn score_midi_sibling(parent: BlockId, hash: &str, ephemeral: bool) -> BlockSnapshot {
        let c = TEST_CTX.with(|v| *v);
        let p = TEST_PRINCIPAL.with(|v| *v);
        BlockSnapshotBuilder::new(BlockId::new(c, p, next_seq()), BlockKind::Text)
            .role(BlockRole::Asset)
            .content(hash)
            .content_type(ContentType::Plain)
            .tick(Tick::new(16))
            .parent_id(parent)
            .ephemeral(ephemeral)
            .build()
    }

    #[test]
    fn materialized_score_blocks_are_hydration_silent() {
        let abc = "X:1\nM:4/4\nL:1/8\nK:Bb dorian\nB2 d2 f2 a2 |\n";
        let hash = "0123456789abcdef0123456789abcdef";

        let source = score_abc_block(abc, /* ephemeral */ true);
        let sibling = score_midi_sibling(source.id, hash, /* ephemeral */ true);
        let score = vec![source, sibling];

        // Path 1: bootstrap hydration (HydrationState via hydrate_from_blocks).
        let via_bootstrap = hydrate_from_blocks(&score);
        assert!(
            via_bootstrap.is_empty(),
            "ephemeral score blocks must not hydrate via bootstrap; got {} messages",
            via_bootstrap.len()
        );

        // Path 2: live conversation feed (ConversationMailbox::catch_up).
        let mut mb = ConversationMailbox::new();
        mb.catch_up(&score);
        let via_catch_up = mb.snapshot();
        assert!(
            via_catch_up.is_empty(),
            "ephemeral score blocks must not hydrate via catch_up; got {} messages",
            via_catch_up.len()
        );

        // Non-vacuity guard: the SAME shape WITHOUT the ephemeral stamp DOES
        // produce a message — proving the assertions above can fail and so
        // pin the stamp, not an always-empty translation. The ABC source is
        // a Model/Text block, which hydrates as one assistant message.
        let leaky_source = score_abc_block(abc, /* ephemeral */ false);
        let leaky = vec![leaky_source];
        let leaked = hydrate_from_blocks(&leaky);
        assert_eq!(
            leaked.len(),
            1,
            "a non-ephemeral Model/Text score block floods as assistant text — \
             this is exactly what the ephemeral stamp suppresses"
        );
    }
}
