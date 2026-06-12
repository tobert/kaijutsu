//! splice.rs — the cut-and-join layer for any keep-set over the block log.
//!
//! Every fork shape and every hydration window is an order-free set selection
//! over the block log (design: `docs/fork-filters.md`): `kept = (base ∩ ∪inc)
//! \ ∪exc`. That selection answers *membership* but is deliberately blind to
//! *order*. This module is where order comes back in — it takes a keep-set and
//! the ordered log and splices the kept runs into a coherent, replayable wire
//! plan, leaving a visible seam wherever it dropped material.
//!
//! Three hazards live at the cut edges of any keep-set, and the membership set
//! cannot see them because they are all about adjacency:
//!
//! 1. **Turn-boundary integrity.** A kept run must not begin on a `ToolResult`
//!    or a mid-`Model` continuation (the wire would lead with an orphaned
//!    tool_result or a bare assistant turn), nor end partway through an
//!    assistant exchange (a dangling `tool_use` with its result dropped).
//! 2. **Tool-pair integrity.** A `tool_use` and its matching `tool_result`
//!    must travel together — never split across a kept/dropped boundary, and
//!    never separated by an injected seam.
//! 3. **False continuity.** Where two non-adjacent runs are kept, the archived
//!    gap between them must be marked, or two unrelated `Model/Text` fragments
//!    merge into one apparent assistant turn.
//!
//! All three resolve with **one rule**: runs snap *outward* to turn-group
//! boundaries. A *turn group* runs from a `Role::User` block up to (but not
//! including) the next `Role::User` block — so an entire agentic exchange
//! (`User → Model/Thinking → Model/ToolCall → Tool/ToolResult → Model/Text`)
//! is a single indivisible group. Snapping a start *back* and an end *forward*
//! to these boundaries can only enlarge a run, so it never splits a tool-pair
//! and never begins on a continuation. After snapping, overlapping runs merge;
//! a surviving gap gets a synthetic `[N blocks archived]` **seam**.
//!
//! The seam is emitted as a `User`-role message by the consumer. That is safe
//! even when the block before it is itself a `tool_result` (a user message):
//! the Anthropic Messages API merges consecutive same-role messages into one
//! turn rather than rejecting them.
//!
//! Consumers: `mailbox::rehydrate_windowed` (now), fork selection and the
//! windowed-notation pull primitive (later slices). They share this module so
//! the cut-and-join semantics are defined exactly once.

use std::ops::Range;

use kaijutsu_types::{BlockSnapshot, Role as BlockRole};

/// One step in a splice plan over the source block slice.
///
/// A plan is a flat, ordered sequence: `Keep(i)` says "translate the source
/// block at index `i`", `Seam { archived }` says "inject a synthetic
/// `[archived blocks archived]` user-role seam here." The consumer walks the
/// plan in order; the splicer never touches `BlockSnapshot` content, only
/// indices — so it stays independent of the hydrator's translation rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpliceItem {
    /// Translate the source block at this index into the wire conversation.
    Keep(usize),
    /// Inject a synthetic user-role seam standing in for `archived` source
    /// blocks dropped between two kept runs.
    Seam { archived: usize },
}

/// Is this block a turn-group boundary — a point a kept run may legally begin
/// on without producing a leading assistant message or an orphaned
/// `tool_result`?
///
/// A boundary is a genuine user-side turn start (`User/Text`, or the
/// `User/ToolCall` of a user-initiated shell command). Everything else —
/// `Model` continuations, agent `Tool/ToolResult`s, `Thinking` — is a
/// *continuation* of the exchange begun by the preceding `User` block, and
/// snapping pulls it back into that group.
///
/// `Drift`/`Notification`/`Resource` blocks hydrate as standalone user
/// messages and would be safe starts too, but treating only `Role::User` as a
/// boundary is conservative (it keeps slightly more, never less) and keeps the
/// rule a single predicate. Revisit if a window that legitimately begins on a
/// drift block needs to *not* pull in the prior human turn.
fn is_turn_start(b: &BlockSnapshot) -> bool {
    b.role == BlockRole::User
}

/// Snap a run start back to the nearest turn boundary at or before `lo`.
/// Index 0 is always a valid start (there is nothing earlier to snap to).
fn snap_start(blocks: &[BlockSnapshot], lo: usize) -> usize {
    let mut i = lo;
    while i > 0 && !is_turn_start(&blocks[i]) {
        i -= 1;
    }
    i
}

/// Snap a run end (exclusive) forward to the next turn boundary at or after
/// `hi`. If `blocks[hi]` already starts a new group (or `hi == len`) the cut is
/// clean and `hi` is returned unchanged; otherwise the end extends forward
/// until it lands on a boundary, so the kept set always ends on a complete turn
/// group — no dangling `tool_use`.
fn snap_end(blocks: &[BlockSnapshot], hi: usize) -> usize {
    let len = blocks.len();
    let mut i = hi.min(len);
    while i < len && !is_turn_start(&blocks[i]) {
        i += 1;
    }
    i
}

/// Build a splice plan for `keep_set` over `blocks`.
///
/// `keep_set` is the order-free selection expressed as its maximal runs —
/// half-open `[lo, hi)` index ranges into `blocks`. They need not be sorted,
/// disjoint, or boundary-aligned (the hydration window emits `[0,marker] ∪
/// tail`; a later `\ ∪exc` selection can punch arbitrary holes). The plan
/// returned is the ordered keep/seam sequence after: clamping to bounds,
/// dropping empties, snapping each run outward to turn-group boundaries,
/// sorting, and merging overlaps. A `Seam { archived }` is emitted only where a
/// real gap survives between two kept runs (`archived` = the count of dropped
/// source indices in that gap). An empty `keep_set` (the player-`spawn` shape)
/// yields an empty plan — keep nothing, no seam.
pub(crate) fn plan_splice(blocks: &[BlockSnapshot], keep_set: &[Range<usize>]) -> Vec<SpliceItem> {
    let len = blocks.len();

    // 1. Clamp to bounds, drop empties, snap each run outward to turn-group
    //    boundaries.
    let mut snapped: Vec<Range<usize>> = keep_set
        .iter()
        .filter_map(|r| {
            let lo = r.start.min(len);
            let hi = r.end.min(len);
            if lo >= hi {
                return None; // empty / zero-width / inverted
            }
            let lo = snap_start(blocks, lo);
            let hi = snap_end(blocks, hi);
            Some(lo..hi)
        })
        .collect();

    if snapped.is_empty() {
        return Vec::new();
    }

    // 2. Sort by start, then merge overlapping or touching runs. Snapping can
    //    make two originally-disjoint runs overlap (e.g. a prefix end extended
    //    forward past a tail start snapped back) — merging collapses those so
    //    no seam is emitted where the gap closed.
    snapped.sort_by_key(|r| (r.start, r.end));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(snapped.len());
    for r in snapped {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => {
                if r.end > last.end {
                    last.end = r.end;
                }
            }
            _ => merged.push(r),
        }
    }

    // 3. Flatten to a keep/seam plan. Between two merged runs there is always a
    //    real gap (touching ones merged in step 2), so each boundary gets
    //    exactly one seam carrying the dropped-block count.
    let mut plan = Vec::new();
    let mut prev_end: Option<usize> = None;
    for r in merged {
        if let Some(end) = prev_end {
            let archived = r.start - end;
            if archived > 0 {
                plan.push(SpliceItem::Seam { archived });
            }
        }
        plan.extend((r.start..r.end).map(SpliceItem::Keep));
        prev_end = Some(r.end);
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{
        BlockId, BlockKind, BlockSnapshot, BlockSnapshotBuilder, ContextId, PrincipalId,
        Role as BlockRole,
    };
    use std::cell::Cell;

    // Per-thread (ctx, principal) + monotonic seq so block ids are unique
    // within a test, mirroring the mailbox test harness.
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

    fn block(role: BlockRole, kind: BlockKind) -> BlockSnapshot {
        let c = TEST_CTX.with(|v| *v);
        let p = TEST_PRINCIPAL.with(|v| *v);
        BlockSnapshotBuilder::new(BlockId::new(c, p, next_seq()), kind)
            .role(role)
            .build()
    }

    fn user() -> BlockSnapshot {
        block(BlockRole::User, BlockKind::Text)
    }
    fn model() -> BlockSnapshot {
        block(BlockRole::Model, BlockKind::Text)
    }
    fn model_call() -> BlockSnapshot {
        block(BlockRole::Model, BlockKind::ToolCall)
    }
    fn tool_result() -> BlockSnapshot {
        block(BlockRole::Tool, BlockKind::ToolResult)
    }
    fn thinking() -> BlockSnapshot {
        block(BlockRole::Model, BlockKind::Thinking)
    }

    /// The kept source indices a plan names, in order.
    fn kept(plan: &[SpliceItem]) -> Vec<usize> {
        plan.iter()
            .filter_map(|it| match it {
                SpliceItem::Keep(i) => Some(*i),
                SpliceItem::Seam { .. } => None,
            })
            .collect()
    }

    /// The seam `archived` counts a plan carries, in order.
    fn seams(plan: &[SpliceItem]) -> Vec<usize> {
        plan.iter()
            .filter_map(|it| match it {
                SpliceItem::Seam { archived } => Some(*archived),
                SpliceItem::Keep(_) => None,
            })
            .collect()
    }

    // ── trivial shapes ────────────────────────────────────────────────────

    #[test]
    fn empty_keep_set_keeps_nothing() {
        // The player-`spawn` shape: keep ~nothing, no seam.
        let blocks = vec![user(), model(), user(), model()];
        let plan = plan_splice(&blocks, &[]);
        assert!(plan.is_empty());
    }

    #[test]
    fn whole_log_is_all_keep_no_seam() {
        // The `marker == None` / all-pass shape.
        let blocks = vec![user(), model(), user(), model()];
        let plan = plan_splice(&blocks, &[0..4]);
        assert_eq!(kept(&plan), vec![0, 1, 2, 3]);
        assert!(seams(&plan).is_empty());
    }

    #[test]
    fn zero_width_and_inverted_runs_drop() {
        let blocks = vec![user(), model(), user(), model()];
        // 2..2 is empty; 3..1 is inverted — both vanish, leaving only 0..2.
        let plan = plan_splice(&blocks, &[0..2, 2..2, 3..1]);
        assert_eq!(kept(&plan), vec![0, 1]);
        assert!(seams(&plan).is_empty());
    }

    #[test]
    fn out_of_bounds_end_clamps_to_len() {
        let blocks = vec![user(), model()];
        let plan = plan_splice(&blocks, &[0..999]);
        assert_eq!(kept(&plan), vec![0, 1]);
    }

    // ── turn-boundary snapping ────────────────────────────────────────────

    #[test]
    fn start_on_model_snaps_back_to_user() {
        //  0:user 1:model 2:user 3:model
        // A tail starting at index 3 (Model continuation) snaps back to its
        // User turn (index 2) so the wire never leads with a bare assistant.
        let blocks = vec![user(), model(), user(), model()];
        let plan = plan_splice(&blocks, &[3..4]);
        assert_eq!(kept(&plan), vec![2, 3], "start snapped back to the User turn");
    }

    #[test]
    fn start_on_tool_result_snaps_back_over_the_whole_exchange() {
        //  0:user 1:model 2:model_call 3:tool_result 4:model 5:user 6:model
        // A tail starting at the agent tool_result (index 3) must snap back to
        // the User prompt that began the exchange (index 0) — pulling the
        // tool_call (2) and its result (3) in together. This is the "tool-pair
        // tail snap" review P1.
        let blocks = vec![
            user(),
            model(),
            model_call(),
            tool_result(),
            model(),
            user(),
            model(),
        ];
        let plan = plan_splice(&blocks, &[3..7]);
        assert_eq!(
            kept(&plan),
            vec![0, 1, 2, 3, 4, 5, 6],
            "snapped back to index 0, keeping the tool_call/tool_result pair intact"
        );
    }

    #[test]
    fn start_on_thinking_snaps_back_to_user() {
        //  0:user 1:thinking 2:model 3:user 4:thinking 5:model
        let blocks = vec![user(), thinking(), model(), user(), thinking(), model()];
        let plan = plan_splice(&blocks, &[4..6]);
        assert_eq!(kept(&plan), vec![3, 4, 5], "Thinking is a continuation, snap to its User");
    }

    // ── tool-pair integrity (marker-on-tool_call) ─────────────────────────

    #[test]
    fn prefix_ending_on_tool_call_extends_to_include_its_result() {
        //  0:user 1:model 2:model_call 3:tool_result 4:model 5:user 6:model
        // A hydration marker landing on the tool_call (prefix [0..3) keeps
        // 0,1,2 — a dangling tool_use). The end snaps forward to the next User
        // (index 5), pulling in 3 (result) and 4 (answer) so no pair is split.
        let blocks = vec![
            user(),
            model(),
            model_call(),
            tool_result(),
            model(),
            user(),
            model(),
        ];
        let plan = plan_splice(&blocks, &[0..3]);
        assert_eq!(
            kept(&plan),
            vec![0, 1, 2, 3, 4],
            "end extended past the tool_call to include its tool_result and the answer"
        );
        assert!(seams(&plan).is_empty(), "single run, no gap to seam");
    }

    #[test]
    fn pair_never_split_across_a_kept_dropped_gap() {
        //  0:user 1:model 2:model_call 3:tool_result 4:model
        //  5:user 6:model 7:user 8:model
        // Prefix marker on the tool_call (0..3) and a tail (7..9). The prefix
        // extends forward to 5 (next User), absorbing the result; the gap to
        // the tail is [5,7) → one seam of 2 archived blocks. The tool_call (2)
        // and tool_result (3) stay in the same kept run, never seam-split.
        let blocks = vec![
            user(),
            model(),
            model_call(),
            tool_result(),
            model(),
            user(),
            model(),
            user(),
            model(),
        ];
        let plan = plan_splice(&blocks, &[0..3, 7..9]);
        assert_eq!(kept(&plan), vec![0, 1, 2, 3, 4, 7, 8]);
        assert_eq!(seams(&plan), vec![2], "blocks 5,6 archived between the two groups");
        // The pair (2,3) lands inside one contiguous Keep run with no seam
        // between them.
        let pos_call = plan.iter().position(|it| *it == SpliceItem::Keep(2)).unwrap();
        let pos_result = plan.iter().position(|it| *it == SpliceItem::Keep(3)).unwrap();
        assert_eq!(pos_result, pos_call + 1, "tool_call immediately followed by its result");
    }

    // ── cross-gap merge ───────────────────────────────────────────────────

    #[test]
    fn snapping_can_close_a_gap_and_merge_runs() {
        //  0:user 1:model 2:model 3:model 4:user 5:model
        // Prefix [0..2) and tail [3..6). The prefix end snaps forward over the
        // Model continuations 2,3 to the next User (index 4); the tail start
        // (index 3, a Model continuation) snaps back to index 0. They overlap
        // and merge into the whole log — no seam, no gap.
        let blocks = vec![user(), model(), model(), model(), user(), model()];
        let plan = plan_splice(&blocks, &[0..2, 3..6]);
        assert_eq!(kept(&plan), vec![0, 1, 2, 3, 4, 5]);
        assert!(seams(&plan).is_empty(), "gap closed by snapping, runs merged");
    }

    #[test]
    fn genuine_middle_keeps_prefix_and_tail_with_one_seam() {
        //  0:user 1:model 2:user 3:model 4:user 5:model 6:user 7:model
        // Prefix [0..2) and tail [6..8): a real archived middle [2,6).
        let blocks = vec![
            user(),
            model(),
            user(),
            model(),
            user(),
            model(),
            user(),
            model(),
        ];
        let plan = plan_splice(&blocks, &[0..2, 6..8]);
        assert_eq!(kept(&plan), vec![0, 1, 6, 7]);
        assert_eq!(seams(&plan), vec![4], "blocks 2,3,4,5 archived");
    }

    #[test]
    fn three_runs_yield_two_seams() {
        //  0:user 1:user 2:user 3:user 4:user 5:user (each its own group)
        let blocks = vec![user(), user(), user(), user(), user(), user()];
        let plan = plan_splice(&blocks, &[0..1, 2..3, 5..6]);
        assert_eq!(kept(&plan), vec![0, 2, 5]);
        assert_eq!(seams(&plan), vec![1, 2], "gap [1,2)=1 then gap [3,5)=2");
    }

    #[test]
    fn unsorted_overlapping_runs_normalize() {
        let blocks = vec![user(), user(), user(), user(), user()];
        // Given out of order and overlapping; result is the same as [0..1, 2..5).
        let plan = plan_splice(&blocks, &[3..5, 0..1, 2..4]);
        assert_eq!(kept(&plan), vec![0, 2, 3, 4]);
        assert_eq!(seams(&plan), vec![1]);
    }

    // ── degenerate: no user boundary to snap to ───────────────────────────

    #[test]
    fn no_preceding_user_snaps_start_to_zero() {
        //  0:model 1:model 2:user 3:model  (log opens mid-assistant)
        let blocks = vec![model(), model(), user(), model()];
        let plan = plan_splice(&blocks, &[1..2]);
        assert_eq!(kept(&plan), vec![0, 1], "no earlier User, snap to index 0");
    }

    #[test]
    fn no_following_user_extends_end_to_len() {
        //  0:user 1:model 2:model_call 3:tool_result  (exchange runs to EOF)
        let blocks = vec![user(), model(), model_call(), tool_result()];
        let plan = plan_splice(&blocks, &[0..2]);
        assert_eq!(kept(&plan), vec![0, 1, 2, 3], "no later User, extend to len");
    }
}
