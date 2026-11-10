//! Which blocks fold, when, and what a folded block says.
//!
//! A fold is the viewer's own: each client keeps its fold state per block
//! and never writes it to the kernel. This module is the rule both clients
//! apply, so a conversation reads the same in a terminal and in the app
//! (`docs/tui.md`, "Folds").
//!
//! A block that [`folds_when_followed`] renders whole while it is the
//! newest thing in its context — reasoning is read as it streams — and
//! folds to one line when the next block arrives. [`AutoFold`] makes that
//! decision once per block; after it, only the viewer's toggle moves the
//! fold.

use std::collections::HashSet;

use kaijutsu_types::{BlockId, BlockKind, BlockSnapshot, Status};

/// A settled tool result longer than this many lines folds once something
/// follows it. A shorter result is cheaper to read than to open.
pub const TOOL_RESULT_FOLD_LINES: usize = 12;

/// Whether a block of this kind renders folded the first time it is seen,
/// before anything follows it. Only `Error`.
pub fn collapses_by_default(kind: BlockKind) -> bool {
    matches!(kind, BlockKind::Error)
}

/// Whether `block` folds on its own once a later block follows it: settled
/// reasoning, and a settled tool result longer than
/// [`TOOL_RESULT_FOLD_LINES`].
///
/// A block still streaming never folds: its text is what the viewer is
/// reading.
pub fn folds_when_followed(block: &BlockSnapshot) -> bool {
    if !matches!(block.status, Status::Done | Status::Error) {
        return false;
    }
    match block.kind {
        BlockKind::Thinking => true,
        BlockKind::ToolResult => block.content.lines().count() > TOOL_RESULT_FOLD_LINES,
        _ => false,
    }
}

/// Whether the viewer's toggle applies to `block` at all: the kinds that
/// fold on their own, plus `Error`, whatever their length or status.
pub fn foldable(block: &BlockSnapshot) -> bool {
    matches!(block.kind, BlockKind::Thinking | BlockKind::ToolResult | BlockKind::Error)
}

/// The once-per-block fold decision for one context.
#[derive(Debug, Default, Clone)]
pub struct AutoFold {
    decided: HashSet<BlockId>,
}

impl AutoFold {
    /// The blocks to fold now, given the context's blocks in document order.
    ///
    /// A block is decided the first time it both [`folds_when_followed`] and
    /// has a later block; `Draft` blocks follow nothing. `held` is the block
    /// the viewer is reading, if they have scrolled into one: it is decided
    /// like any other and left open, so a fold never moves the text under
    /// the reader. A decided block is never returned again.
    pub fn sweep<'a>(
        &mut self,
        blocks: impl IntoIterator<Item = &'a BlockSnapshot>,
        held: Option<BlockId>,
    ) -> Vec<BlockId> {
        let blocks: Vec<&BlockSnapshot> = blocks
            .into_iter()
            .filter(|b| b.status != Status::Draft)
            .collect();
        let mut fold = Vec::new();
        for block in blocks.iter().take(blocks.len().saturating_sub(1)) {
            if !folds_when_followed(block) || !self.decided.insert(block.id) {
                continue;
            }
            if held != Some(block.id) {
                fold.push(block.id);
            }
        }
        fold
    }

    /// Forget blocks that are no longer in the context, so the set does not
    /// outgrow the mirror it describes.
    pub fn retain(&mut self, live: &HashSet<BlockId>) {
        self.decided.retain(|id| live.contains(id));
    }
}

/// What folded reasoning says, without a fold mark:
/// `thinking · 14 lines · <detail>`. The detail is the kernel's summary, or
/// the text's first line when the block has none.
pub fn thinking_stub(summary: Option<&str>, text: &str) -> String {
    let text = text.trim();
    let detail = summary
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| text.lines().next().map(str::trim_end));
    match detail {
        // A provider can return a thinking block with no text to show.
        None | Some("") => "thinking · nothing to show".to_string(),
        Some(detail) => format!("thinking · {} · {detail}", line_count(text)),
    }
}

/// What a folded tool result says, without a fold mark:
/// `312 lines · <last line>`. The last line is where a build or a test run
/// puts its verdict.
pub fn tool_result_stub(text: &str) -> String {
    let text = text.trim_end();
    match text.lines().rev().map(str::trim).find(|line| !line.is_empty()) {
        None => "no output".to_string(),
        Some(last) => format!("{} · {last}", line_count(text)),
    }
}

fn line_count(text: &str) -> String {
    match text.lines().count() {
        1 => "1 line".to_string(),
        n => format!("{n} lines"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockSnapshotBuilder, ContextId, PrincipalId};

    fn block(ctx: ContextId, seq: u64, kind: BlockKind, status: Status, content: &str) -> BlockSnapshot {
        BlockSnapshotBuilder::new(BlockId::new(ctx, PrincipalId::new(), seq), kind)
            .status(status)
            .content(content)
            .build()
    }

    fn lines(n: usize) -> String {
        (0..n).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn reasoning_stays_open_until_something_follows_it() {
        let ctx = ContextId::new();
        let thinking = block(ctx, 1, BlockKind::Thinking, Status::Done, "hmm");
        let mut auto = AutoFold::default();
        assert!(auto.sweep([&thinking], None).is_empty(), "nothing follows it yet");

        let answer = block(ctx, 2, BlockKind::Text, Status::Running, "The");
        assert_eq!(auto.sweep([&thinking, &answer], None), vec![thinking.id]);
    }

    #[test]
    fn a_block_is_decided_once_so_a_reopened_fold_stays_open() {
        let ctx = ContextId::new();
        let thinking = block(ctx, 1, BlockKind::Thinking, Status::Done, "hmm");
        let answer = block(ctx, 2, BlockKind::Text, Status::Done, "ok");
        let mut auto = AutoFold::default();
        assert_eq!(auto.sweep([&thinking, &answer], None).len(), 1);
        assert!(auto.sweep([&thinking, &answer], None).is_empty());
    }

    #[test]
    fn streaming_reasoning_does_not_fold_when_a_block_lands_after_it() {
        let ctx = ContextId::new();
        let thinking = block(ctx, 1, BlockKind::Thinking, Status::Running, "hmm");
        let note = block(ctx, 2, BlockKind::Text, Status::Done, "a mid-turn message");
        let mut auto = AutoFold::default();
        assert!(auto.sweep([&thinking, &note], None).is_empty());

        // It settles later, already followed: now it folds.
        let settled = BlockSnapshot { status: Status::Done, ..thinking.clone() };
        assert_eq!(auto.sweep([&settled, &note], None), vec![settled.id]);
    }

    #[test]
    fn a_draft_follows_nothing() {
        let ctx = ContextId::new();
        let thinking = block(ctx, 1, BlockKind::Thinking, Status::Done, "hmm");
        let draft = block(ctx, 2, BlockKind::Text, Status::Draft, "typing");
        let mut auto = AutoFold::default();
        assert!(auto.sweep([&thinking, &draft], None).is_empty());
    }

    #[test]
    fn the_block_under_the_reader_is_decided_but_left_open() {
        let ctx = ContextId::new();
        let thinking = block(ctx, 1, BlockKind::Thinking, Status::Done, "hmm");
        let answer = block(ctx, 2, BlockKind::Text, Status::Done, "ok");
        let mut auto = AutoFold::default();
        assert!(auto.sweep([&thinking, &answer], Some(thinking.id)).is_empty());
        assert!(
            auto.sweep([&thinking, &answer], None).is_empty(),
            "the reader leaving does not fold it behind them"
        );
    }

    #[test]
    fn only_a_long_settled_tool_result_folds() {
        let ctx = ContextId::new();
        let short = block(ctx, 1, BlockKind::ToolResult, Status::Done, &lines(TOOL_RESULT_FOLD_LINES));
        let long = block(ctx, 2, BlockKind::ToolResult, Status::Done, &lines(TOOL_RESULT_FOLD_LINES + 1));
        let running = block(ctx, 3, BlockKind::ToolResult, Status::Running, &lines(400));
        let text = block(ctx, 4, BlockKind::Text, Status::Done, &lines(400));
        let last = block(ctx, 5, BlockKind::Text, Status::Done, "end");
        let mut auto = AutoFold::default();
        assert_eq!(auto.sweep([&short, &long, &running, &text, &last], None), vec![long.id]);
    }

    #[test]
    fn retain_forgets_blocks_that_left_the_context() {
        let ctx = ContextId::new();
        let thinking = block(ctx, 1, BlockKind::Thinking, Status::Done, "hmm");
        let answer = block(ctx, 2, BlockKind::Text, Status::Done, "ok");
        let mut auto = AutoFold::default();
        auto.sweep([&thinking, &answer], None);
        auto.retain(&HashSet::new());
        assert_eq!(auto.sweep([&thinking, &answer], None), vec![thinking.id]);
    }

    #[test]
    fn the_toggle_reaches_errors_and_short_results_too() {
        let ctx = ContextId::new();
        assert!(foldable(&block(ctx, 1, BlockKind::Error, Status::Error, "boom")));
        assert!(foldable(&block(ctx, 2, BlockKind::ToolResult, Status::Running, "x")));
        assert!(!foldable(&block(ctx, 3, BlockKind::Text, Status::Done, "x")));
    }

    #[test]
    fn folded_reasoning_says_its_length_and_the_summary() {
        assert_eq!(
            thinking_stub(Some("Check resolve_nofollow first"), "a\nb\nc"),
            "thinking · 3 lines · Check resolve_nofollow first"
        );
        assert_eq!(thinking_stub(None, "considering\nmore"), "thinking · 2 lines · considering");
        assert_eq!(thinking_stub(Some("  "), "one"), "thinking · 1 line · one");
        assert_eq!(thinking_stub(None, "  \n"), "thinking · nothing to show");
    }

    #[test]
    fn a_folded_result_says_its_length_and_its_last_line() {
        assert_eq!(
            tool_result_stub("Compiling\nRunning\ntest result: ok. 478 passed\n\n"),
            "3 lines · test result: ok. 478 passed"
        );
        assert_eq!(tool_result_stub(" \n"), "no output");
    }
}
