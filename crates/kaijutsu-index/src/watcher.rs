//! Background task that re-indexes contexts on block status changes.
//!
//! Uses trait objects so kaijutsu-index has no dependency on kaijutsu-kernel.

use std::sync::Arc;

use tokio::task::JoinHandle;

use crate::{BlockSource, SemanticIndex, StatusReceiver};

/// Spawn a background task that re-indexes contexts when blocks complete.
///
/// Receives `StatusEvent`s via the trait. On terminal status, fetches blocks
/// from `BlockSource` and calls `index.index_context()`. Content hash
/// comparison naturally deduplicates rapid-fire events.
pub fn spawn_index_watcher(
    index: Arc<SemanticIndex>,
    blocks: Arc<dyn BlockSource>,
    mut events: Box<dyn StatusReceiver>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("semantic index watcher started");

        loop {
            let event = match events.recv().await {
                Some(e) => e,
                None => {
                    tracing::info!("semantic index watcher: event stream closed");
                    break;
                }
            };

            // Only re-index on terminal status
            if !event.status.is_terminal() {
                continue;
            }

            let ctx_id = event.context_id;

            // Fetch blocks for this context
            let snapshots = match blocks.block_snapshots(ctx_id) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        context = %ctx_id.short(),
                        error = %e,
                        "failed to fetch blocks for indexing"
                    );
                    continue;
                }
            };

            // Index (skips if content hash unchanged)
            match index.index_context(ctx_id, &snapshots).await {
                Ok(true) => {
                    tracing::debug!(context = %ctx_id.short(), "indexed context");
                }
                Ok(false) => {
                    // Content unchanged, no re-embedding needed
                }
                Err(e) => {
                    tracing::warn!(
                        context = %ctx_id.short(),
                        error = %e,
                        "failed to index context"
                    );
                }
            }
        }
    })
}
