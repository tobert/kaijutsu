//! In-place context compaction (M1-A5).
//!
//! When a context's non-compacted block count exceeds a threshold, summarize
//! the older half into a Drift block placed at the boundary, then mark the
//! originals `compacted=true`. The hydrator skips compacted blocks (see
//! `llm/mod.rs:1022`), so on the next turn the model sees
//! `[drift summary, ...recent blocks...]` instead of the full history.

use kaijutsu_crdt::DriftKind;
use kaijutsu_types::{BlockId, BlockSnapshot, ContextId};

use crate::kj::KjDispatcher;

/// Default block-count threshold above which auto-compaction kicks in.
///
/// Tuned conservatively: most conversations stay well below this. Configurable
/// via `auto_compact_with_threshold` for tests and tuning experiments.
pub const DEFAULT_COMPACT_THRESHOLD: usize = 200;

/// Plan output of [`select_compaction_targets`] — the IO-free decision step.
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    /// Blocks to mark `compacted=true`.
    pub target_ids: Vec<BlockId>,
    /// Insertion point for the Drift summary block (after the last
    /// compacted target). `None` means there were no targets so the Drift
    /// would land at the document root, which we never want — callers
    /// should treat `target_ids.is_empty()` as a no-op.
    pub after_id: Option<BlockId>,
}

/// Pure decision: given a context's blocks, which should we compact?
///
/// Returns `None` when the live (non-compacted) block count is below
/// `threshold`. Otherwise compacts the older half — half feels like the
/// right ratio because the recent half stays cheap to read while the older
/// half collapses to a single summary, which is exactly the
/// `summarize-then-skip` pattern the hydrator was already designed for.
pub fn select_compaction_targets(
    blocks: &[BlockSnapshot],
    threshold: usize,
) -> Option<CompactionPlan> {
    let live: Vec<&BlockSnapshot> = blocks.iter().filter(|b| !b.compacted).collect();
    if live.len() < threshold {
        return None;
    }
    let target_count = live.len() / 2;
    if target_count == 0 {
        return None;
    }
    let target_ids: Vec<BlockId> = live.iter().take(target_count).map(|b| b.id).collect();
    let after_id = target_ids.last().copied();
    Some(CompactionPlan {
        target_ids,
        after_id,
    })
}

impl KjDispatcher {
    /// Auto-compact the context if it's over the block-count threshold.
    /// Returns `Ok(true)` when compaction ran, `Ok(false)` when below threshold.
    /// Errors propagate from `summarize` (LLM call) and block-store mutations.
    pub async fn auto_compact_if_needed(&self, ctx_id: ContextId) -> Result<bool, String> {
        self.auto_compact_with_threshold(ctx_id, DEFAULT_COMPACT_THRESHOLD)
            .await
    }

    /// Threshold-parameterized variant — used by tests to exercise the
    /// compaction path without needing 200+ block fixtures.
    pub async fn auto_compact_with_threshold(
        &self,
        ctx_id: ContextId,
        threshold: usize,
    ) -> Result<bool, String> {
        let blocks = self
            .block_store()
            .block_snapshots(ctx_id)
            .map_err(|e| e.to_string())?;
        let plan = match select_compaction_targets(&blocks, threshold) {
            Some(p) => p,
            None => return Ok(false),
        };

        // Summarize via the existing distillation primitive (LLM call).
        let summary = self.summarize(ctx_id, None).await?;

        // Insert Drift block at the boundary so the hydrator sees
        // `[drift, ...recent...]`. Source = self for in-place compaction.
        let source_model = {
            let drift = self.drift_router().read().await;
            drift.get(ctx_id).and_then(|h| h.model.clone())
        };
        self.block_store()
            .insert_drift_block(
                ctx_id,
                None,
                plan.after_id.as_ref(),
                summary,
                ctx_id,
                source_model,
                DriftKind::Distill,
            )
            .map_err(|e| format!("failed to insert drift summary: {e}"))?;

        // Mark the older half as compacted so hydration skips them.
        for id in &plan.target_ids {
            self.block_store()
                .set_compacted(ctx_id, id, true)
                .map_err(|e| format!("failed to mark {id} compacted: {e}"))?;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockId, BlockSnapshot, BlockSnapshotBuilder, BlockKind, ContextId, PrincipalId, Role};

    fn live_block(ctx: ContextId, agent: PrincipalId, seq: u64, content: &str) -> BlockSnapshot {
        BlockSnapshotBuilder::new(BlockId::new(ctx, agent, seq), BlockKind::Text)
            .role(Role::User)
            .content(content)
            .build()
    }

    #[test]
    fn below_threshold_returns_none() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let blocks: Vec<_> = (0..5).map(|i| live_block(ctx, agent, i, "x")).collect();
        assert!(select_compaction_targets(&blocks, 10).is_none());
    }

    #[test]
    fn at_threshold_compacts_older_half() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let blocks: Vec<_> = (0..10).map(|i| live_block(ctx, agent, i, "x")).collect();
        let plan = select_compaction_targets(&blocks, 10).expect("plan");
        assert_eq!(plan.target_ids.len(), 5);
        assert_eq!(plan.after_id, Some(plan.target_ids[4]));
        // The first 5 (older) blocks are targets.
        for (i, id) in plan.target_ids.iter().enumerate() {
            assert_eq!(*id, blocks[i].id);
        }
    }

    #[test]
    fn already_compacted_blocks_are_excluded_from_count() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let mut blocks: Vec<_> = (0..15).map(|i| live_block(ctx, agent, i, "x")).collect();
        // Pre-mark first 10 compacted; only 5 are live → below threshold of 10.
        for b in blocks.iter_mut().take(10) {
            b.compacted = true;
        }
        assert!(select_compaction_targets(&blocks, 10).is_none());
    }

    #[test]
    fn empty_blocks_returns_none() {
        assert!(select_compaction_targets(&[], 10).is_none());
    }
}
