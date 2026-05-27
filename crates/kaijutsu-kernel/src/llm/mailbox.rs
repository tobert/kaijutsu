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

use kaijutsu_types::{BlockId, BlockSnapshot};

use super::Message;
use super::hydrate::HydrationState;

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
}

impl ConversationMailbox {
    /// Create an empty, un-materialized mailbox.
    pub fn new() -> Self {
        Self {
            state: HydrationState::new(),
            seen: HashSet::new(),
            materialized: false,
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
        BlockId, BlockKind, BlockSnapshot, BlockSnapshotBuilder, ContextId, PrincipalId,
        Role as BlockRole,
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
}
