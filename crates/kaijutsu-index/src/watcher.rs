//! Background task that re-indexes contexts on block status changes.
//!
//! Uses trait objects so kaijutsu-index has no dependency on kaijutsu-kernel.
//! Debounces rapid events (1s window). Storage uses blocking tasks;
//! embedding service requests await without holding storage locks.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::{BlockSource, SemanticIndex, StatusReceiver};

/// How long one batch collects terminal events before it is indexed.
const DEBOUNCE_WINDOW: Duration = Duration::from_secs(1);

/// Callback invoked after a context is successfully (re-)indexed.
///
/// Runs on the async task — implementations should spawn_blocking if needed.
pub type OnIndexed = Arc<dyn Fn(kaijutsu_types::ContextId) + Send + Sync>;

/// Spawn a background task that re-indexes contexts when blocks complete.
///
/// Receives `StatusEvent`s via the trait. On terminal status, collects a batch
/// (1s debounce window), then indexes each context asynchronously.
/// Content hash comparison naturally deduplicates across batches.
///
/// If `on_indexed` is provided, it is called for each context that was
/// successfully (re-)indexed — use this to trigger synthesis.
///
/// When the stream closes, the batch already collected is indexed first, then
/// the index is saved once and the task returns.
pub fn spawn_index_watcher(
    index: Arc<SemanticIndex>,
    blocks: Arc<dyn BlockSource>,
    events: Box<dyn StatusReceiver>,
    on_indexed: Option<OnIndexed>,
) -> JoinHandle<()> {
    spawn_index_watcher_with_debounce(index, blocks, events, on_indexed, DEBOUNCE_WINDOW)
}

/// `spawn_index_watcher` with an explicit debounce window, so tests exercise the
/// window without waiting a second per batch.
fn spawn_index_watcher_with_debounce(
    index: Arc<SemanticIndex>,
    blocks: Arc<dyn BlockSource>,
    mut events: Box<dyn StatusReceiver>,
    on_indexed: Option<OnIndexed>,
    debounce: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("semantic index watcher started");

        loop {
            // Wait for first terminal event
            let Some(event) = events.recv().await else {
                break;
            };

            if !event.status.is_terminal() {
                continue;
            }

            let mut pending: HashSet<kaijutsu_types::ContextId> = HashSet::new();
            pending.insert(event.context_id);

            // Collect more events for the debounce window. The window closing
            // ends the batch; the stream closing ends the watcher after it.
            let deadline = tokio::time::Instant::now() + debounce;
            let stream_closed = loop {
                match tokio::time::timeout_at(deadline, events.recv()).await {
                    Ok(Some(e)) if e.status.is_terminal() => {
                        pending.insert(e.context_id);
                    }
                    Ok(Some(_)) => {} // non-terminal, discard
                    Ok(None) => break true,
                    Err(_) => break false, // the debounce window elapsed
                }
            };

            // Fetch snapshots off the runtime, then await each refresh.
            for ctx_id in pending {
                let idx = index.clone();
                let src = blocks.clone();
                match tokio::task::spawn_blocking(move || {
                    let snaps = src
                        .block_snapshots(ctx_id)
                        .map_err(crate::IndexError::Index)?;
                    Ok::<_, crate::IndexError>(snaps)
                })
                .await
                {
                    Ok(Ok(snaps)) => match idx.index_context(ctx_id, &snaps).await {
                        Ok(true) => {
                            tracing::debug!(context = %ctx_id.short(), "indexed context");
                            if let Some(ref cb) = on_indexed {
                                cb(ctx_id);
                            }
                        }
                        Ok(false) => {} // content unchanged
                        Err(e) => {
                            tracing::warn!(
                                context = %ctx_id.short(),
                                error = %e,
                                "failed to index context"
                            );
                        }
                    },
                    Ok(Err(e)) => tracing::warn!(error = %e, "index watcher block read failed"),
                    Err(e) => tracing::warn!(error = %e, "index watcher spawn_blocking failed"),
                }
            }

            if stream_closed {
                break;
            }
        }

        // One exit, so the graph is saved exactly once however the stream ended.
        tracing::info!("semantic index watcher: event stream closed, saving");
        let idx = index.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = idx.save() {
                tracing::warn!(error = %e, "failed to save HNSW on shutdown");
            }
        })
        .await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use kaijutsu_types::{BlockId, BlockSnapshot, ContextId, PrincipalId, Role, Status};
    use tempfile::TempDir;

    use crate::{Embedder, EmbeddingPurpose, IndexConfig, IndexError, StatusEvent};

    /// A stand-in for the tests' `mock` profile: one fixed vector per text.
    struct FixedEmbedder;

    #[async_trait::async_trait]
    impl Embedder for FixedEmbedder {
        fn model_name(&self) -> &str {
            "watcher-test"
        }

        fn revision(&self) -> &str {
            "watcher-test-v1"
        }

        fn dimensions(&self) -> usize {
            32
        }

        async fn embed_batch(
            &self,
            texts: &[&str],
            _purpose: EmbeddingPurpose,
        ) -> Result<Vec<Vec<f32>>, IndexError> {
            Ok(texts
                .iter()
                .map(|_| {
                    let mut vector = vec![0.0f32; 32];
                    vector[0] = 1.0;
                    vector
                })
                .collect())
        }
    }

    /// Serves one text block per context and records which contexts were read.
    #[derive(Default)]
    struct RecordingBlocks {
        requested: Mutex<Vec<ContextId>>,
    }

    impl BlockSource for RecordingBlocks {
        fn block_snapshots(&self, ctx_id: ContextId) -> Result<Vec<BlockSnapshot>, String> {
            self.requested.lock().unwrap().push(ctx_id);
            let id = BlockId::new(ctx_id, PrincipalId::new(), 1);
            Ok(vec![BlockSnapshot::text(
                id,
                None,
                Role::Model,
                format!("watcher batch content for {ctx_id:?}"),
            )])
        }
    }

    fn test_config(dir: &std::path::Path) -> IndexConfig {
        IndexConfig {
            dimensions: 32,
            data_dir: dir.to_path_buf(),
            hnsw_max_nb_connection: 8,
            hnsw_ef_construction: 50,
            max_context_bytes: 512,
            max_contexts: None,
        }
    }

    fn terminal(ctx_id: ContextId) -> StatusEvent {
        StatusEvent {
            context_id: ctx_id,
            status: Status::Done,
        }
    }

    /// Yields the queued events, then reports the stream closed. Polls made
    /// after closure are counted: a stream that said it ended must not be read
    /// again.
    struct EndingReceiver {
        queued: VecDeque<StatusEvent>,
        closed: bool,
        polls_after_close: Arc<AtomicUsize>,
    }

    impl StatusReceiver for EndingReceiver {
        fn recv(
            &mut self,
        ) -> Pin<Box<dyn Future<Output = Option<StatusEvent>> + Send + '_>> {
            Box::pin(async move {
                if self.closed {
                    self.polls_after_close.fetch_add(1, Ordering::SeqCst);
                    return None;
                }
                match self.queued.pop_front() {
                    Some(event) => Some(event),
                    None => {
                        self.closed = true;
                        None
                    }
                }
            })
        }
    }

    /// A status stream the test drives event by event.
    struct ChannelReceiver(tokio::sync::mpsc::UnboundedReceiver<StatusEvent>);

    impl StatusReceiver for ChannelReceiver {
        fn recv(
            &mut self,
        ) -> Pin<Box<dyn Future<Output = Option<StatusEvent>> + Send + '_>> {
            Box::pin(async move { self.0.recv().await })
        }
    }

    type Indexed = tokio::sync::mpsc::UnboundedReceiver<ContextId>;

    async fn next_indexed(indexed: &mut Indexed) -> ContextId {
        tokio::time::timeout(Duration::from_secs(10), indexed.recv())
            .await
            .expect("indexed callback within the deadline")
            .expect("indexed channel stays open")
    }

    const GRAPH_FILES: [&str; 2] = ["index.hnsw.graph", "index.hnsw.data"];

    #[tokio::test]
    async fn closure_during_debounce_indexes_pending_batch_then_saves_and_stops() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().to_path_buf();
        let index = Arc::new(SemanticIndex::new(test_config(dir.path()), Box::new(FixedEmbedder)).unwrap());
        let blocks = Arc::new(RecordingBlocks::default());
        let polls_after_close = Arc::new(AtomicUsize::new(0));
        let ctx_id = ContextId::new();
        let events = EndingReceiver {
            queued: VecDeque::from([terminal(ctx_id)]),
            closed: false,
            polls_after_close: polls_after_close.clone(),
        };
        // Indexing dumps the graph itself, so the callback clears the files:
        // only the save on shutdown can put them back.
        let (tx, mut indexed) = tokio::sync::mpsc::unbounded_channel();
        let on_indexed: OnIndexed = Arc::new(move |indexed_ctx| {
            for name in GRAPH_FILES {
                let _ = std::fs::remove_file(data_dir.join(name));
            }
            let _ = tx.send(indexed_ctx);
        });

        let handle = spawn_index_watcher_with_debounce(
            index,
            blocks.clone(),
            Box::new(events),
            Some(on_indexed),
            Duration::from_millis(200),
        );

        assert_eq!(
            next_indexed(&mut indexed).await,
            ctx_id,
            "a closed stream must still drain the batch it already collected"
        );
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("the watcher stops once the stream is closed")
            .expect("the watcher task does not panic");

        assert_eq!(blocks.requested.lock().unwrap().len(), 1);
        assert_eq!(
            polls_after_close.load(Ordering::SeqCst),
            0,
            "the watcher read a stream that had already reported closure"
        );
        for name in GRAPH_FILES {
            assert!(
                dir.path().join(name).exists(),
                "the watcher saves the graph on shutdown, after the last batch"
            );
        }
    }

    #[tokio::test]
    async fn timeout_ends_the_batch_and_the_watcher_keeps_reading() {
        let dir = TempDir::new().unwrap();
        let index = Arc::new(SemanticIndex::new(test_config(dir.path()), Box::new(FixedEmbedder)).unwrap());
        let blocks = Arc::new(RecordingBlocks::default());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let (tx, mut indexed) = tokio::sync::mpsc::unbounded_channel();
        let on_indexed: OnIndexed = Arc::new(move |ctx_id| {
            let _ = tx.send(ctx_id);
        });
        let first = ContextId::new();
        let second = ContextId::new();

        let handle = spawn_index_watcher_with_debounce(
            index,
            blocks,
            Box::new(ChannelReceiver(receiver)),
            Some(on_indexed),
            Duration::from_millis(50),
        );

        sender.send(terminal(first)).unwrap();
        assert_eq!(
            next_indexed(&mut indexed).await,
            first,
            "the first batch is indexed when the window elapses"
        );

        sender.send(terminal(second)).unwrap();
        assert_eq!(
            next_indexed(&mut indexed).await,
            second,
            "an elapsed window ends the batch, not the watcher"
        );

        drop(sender);
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("the watcher stops after the stream closes")
            .expect("the watcher task does not panic");
    }
}
