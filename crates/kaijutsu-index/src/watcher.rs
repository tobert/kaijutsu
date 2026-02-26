//! Background task that re-indexes contexts on block status changes.
//!
//! Uses trait objects so kaijutsu-index has no dependency on kaijutsu-kernel.
//! Debounces rapid events (1s window) and runs indexing on `spawn_blocking`
//! to keep the tokio runtime free.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::{BlockSource, SemanticIndex, StatusReceiver};

/// Spawn a background task that re-indexes contexts when blocks complete.
///
/// Receives `StatusEvent`s via the trait. On terminal status, collects a batch
/// (1s debounce window), then indexes each context on a blocking thread.
/// Content hash comparison naturally deduplicates across batches.
pub fn spawn_index_watcher(
    index: Arc<SemanticIndex>,
    blocks: Arc<dyn BlockSource>,
    mut events: Box<dyn StatusReceiver>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("semantic index watcher started");

        loop {
            // Wait for first terminal event
            let event = match events.recv().await {
                Some(e) => e,
                None => {
                    tracing::info!("semantic index watcher: event stream closed, saving");
                    let idx = index.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Err(e) = idx.save() {
                            tracing::warn!(error = %e, "failed to save HNSW on shutdown");
                        }
                    }).await;
                    break;
                }
            };

            if !event.status.is_terminal() {
                continue;
            }

            let mut pending: HashSet<kaijutsu_types::ContextId> = HashSet::new();
            pending.insert(event.context_id);

            // Collect more events for 1 second (debounce window)
            let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
            loop {
                match tokio::time::timeout_at(deadline, events.recv()).await {
                    Ok(Some(e)) if e.status.is_terminal() => {
                        pending.insert(e.context_id);
                    }
                    Ok(Some(_)) => {} // non-terminal, discard
                    _ => break,       // timeout or stream closed
                }
            }

            // Index each pending context on a blocking thread
            for ctx_id in pending {
                let idx = index.clone();
                let src = blocks.clone();
                match tokio::task::spawn_blocking(move || {
                    let snaps = src.block_snapshots(ctx_id)
                        .map_err(|e| crate::IndexError::Index(e))?;
                    idx.index_context(ctx_id, &snaps)
                }).await {
                    Ok(Ok(true)) => {
                        tracing::debug!(context = %ctx_id.short(), "indexed context");
                    }
                    Ok(Ok(false)) => {} // content unchanged
                    Ok(Err(e)) => {
                        tracing::warn!(
                            context = %ctx_id.short(),
                            error = %e,
                            "failed to index context"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "index watcher spawn_blocking failed");
                    }
                }
            }
        }
    })
}
