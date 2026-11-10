//! The reader's block: the one block an act on the conversation applies to.
//!
//! Each client knows where its reader is — the terminal client by the line
//! the scrolled view rests on, the app by its focused block — and names it
//! as a block id, or as `None` when the reader is on the live tail. Every
//! act that needs a block (toggle a fold, open a diff) asks [`target`] for
//! it, so the same key lands on the same block in both clients
//! (`docs/tui.md`, "The reader's block"; `docs/input.md`).

use kaijutsu_types::{BlockId, BlockSnapshot, Status};

/// The block an act applies to: the last block that `fits`, at or above the
/// reader's block in document order.
///
/// `reader` is `None` on the live tail, where the search starts from the
/// newest block. A reader's block that is no longer in `blocks` is treated
/// the same way: the place is gone, and the tail is where the view falls
/// back to. `Draft` blocks are the compose line, never a target.
pub fn target<'a>(
    blocks: impl IntoIterator<Item = &'a BlockSnapshot>,
    reader: Option<BlockId>,
    fits: impl Fn(&BlockSnapshot) -> bool,
) -> Option<BlockId> {
    let blocks: Vec<&BlockSnapshot> = blocks
        .into_iter()
        .filter(|b| b.status != Status::Draft)
        .collect();
    let end = reader
        .and_then(|id| blocks.iter().position(|b| b.id == id))
        .map_or(blocks.len(), |at| at + 1);
    blocks[..end].iter().rev().find(|b| fits(b)).map(|b| b.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockKind, BlockSnapshotBuilder, ContextId, PrincipalId};

    fn block(ctx: ContextId, seq: u64, kind: BlockKind, status: Status) -> BlockSnapshot {
        BlockSnapshotBuilder::new(BlockId::new(ctx, PrincipalId::new(), seq), kind)
            .status(status)
            .content("x")
            .build()
    }

    fn thinking(b: &BlockSnapshot) -> bool {
        b.kind == BlockKind::Thinking
    }

    fn conversation() -> Vec<BlockSnapshot> {
        let ctx = ContextId::new();
        vec![
            block(ctx, 1, BlockKind::Thinking, Status::Done),
            block(ctx, 2, BlockKind::Text, Status::Done),
            block(ctx, 3, BlockKind::Thinking, Status::Done),
            block(ctx, 4, BlockKind::Text, Status::Done),
        ]
    }

    #[test]
    fn on_the_tail_the_target_is_the_newest_block_that_fits() {
        let blocks = conversation();
        assert_eq!(target(&blocks, None, thinking), Some(blocks[2].id));
    }

    #[test]
    fn the_readers_own_block_is_the_target_when_it_fits() {
        let blocks = conversation();
        assert_eq!(target(&blocks, Some(blocks[0].id), thinking), Some(blocks[0].id));
    }

    #[test]
    fn otherwise_the_nearest_block_above_the_reader_and_never_one_below() {
        let blocks = conversation();
        assert_eq!(target(&blocks, Some(blocks[1].id), thinking), Some(blocks[0].id));
        assert_eq!(target(&blocks, Some(blocks[3].id), thinking), Some(blocks[2].id));
    }

    #[test]
    fn nothing_fits_at_or_above_the_reader() {
        let blocks = conversation();
        let diff = |b: &BlockSnapshot| b.kind == BlockKind::File;
        assert_eq!(target(&blocks, None, diff), None);
        let text = |b: &BlockSnapshot| b.kind == BlockKind::Text;
        assert_eq!(target(&blocks, Some(blocks[0].id), text), None);
    }

    #[test]
    fn a_readers_block_that_left_the_conversation_reads_as_the_tail() {
        let blocks = conversation();
        let gone = BlockId::new(ContextId::new(), PrincipalId::new(), 9);
        assert_eq!(target(&blocks, Some(gone), thinking), Some(blocks[2].id));
    }

    #[test]
    fn a_draft_is_never_the_target() {
        let mut blocks = conversation();
        let ctx = blocks[0].id.context_id;
        blocks.push(block(ctx, 5, BlockKind::Thinking, Status::Draft));
        assert_eq!(target(&blocks, None, thinking), Some(blocks[2].id));
    }
}
