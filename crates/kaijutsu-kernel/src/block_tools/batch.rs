//! Append batching for streaming text.
//!
//! During streaming (e.g., LLM output), many small appends would create
//! excessive CRDT operations. This module batches appends and flushes
//! on natural boundaries (newlines) or timeouts.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kaijutsu_crdt::{BlockId, ContextId};
use parking_lot::Mutex;

use crate::block_store::SharedBlockStore;

/// Configuration for append batching behavior.
#[derive(Debug, Clone)]
pub struct BatchConfig {
    /// Maximum buffer size before forcing a flush.
    pub max_buffer_size: usize,
    /// Maximum time to hold buffered content.
    pub max_buffer_age: Duration,
    /// Flush immediately on newlines.
    pub flush_on_newline: bool,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_buffer_size: 50,
            max_buffer_age: Duration::from_millis(100),
            flush_on_newline: true,
        }
    }
}

/// Buffer for a single block's pending appends.
#[derive(Debug)]
struct AppendBuffer {
    /// The context containing this block.
    context_id: ContextId,
    /// The block being appended to.
    block_id: BlockId,
    /// Buffered text waiting to be flushed.
    buffer: String,
    /// When the buffer was created or last flushed.
    last_flush: Instant,
}

impl AppendBuffer {
    fn new(context_id: ContextId, block_id: BlockId) -> Self {
        Self {
            context_id,
            block_id,
            buffer: String::new(),
            last_flush: Instant::now(),
        }
    }

    /// Check if buffer should be flushed based on config.
    fn should_flush(&self, config: &BatchConfig) -> bool {
        // Flush on newline
        if config.flush_on_newline && self.buffer.contains('\n') {
            return true;
        }

        // Flush on size limit
        if self.buffer.len() >= config.max_buffer_size {
            return true;
        }

        // Flush on age limit
        if self.last_flush.elapsed() >= config.max_buffer_age {
            return true;
        }

        false
    }

    /// Take the buffer content and reset.
    fn take(&mut self) -> String {
        self.last_flush = Instant::now();
        std::mem::take(&mut self.buffer)
    }
}

/// Manages batched appends across multiple blocks.
pub struct AppendBatcher {
    documents: SharedBlockStore,
    config: BatchConfig,
    /// Buffers keyed by block key (document_id/agent_id/seq).
    buffers: Arc<Mutex<HashMap<String, AppendBuffer>>>,
}

impl AppendBatcher {
    /// Create a new batcher with default config.
    pub fn new(documents: SharedBlockStore) -> Self {
        Self::with_config(documents, BatchConfig::default())
    }

    /// Create a new batcher with custom config.
    pub fn with_config(documents: SharedBlockStore, config: BatchConfig) -> Self {
        Self {
            documents,
            config,
            buffers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Append text to a block, potentially batching it.
    ///
    /// Returns `true` if text was flushed to the CRDT, `false` if buffered.
    pub async fn append(&self, context_id: ContextId, block_id: &BlockId, text: &str) -> bool {
        let key = block_id.to_key();
        let should_flush;

        {
            let mut buffers = self.buffers.lock();
            let buffer = buffers
                .entry(key.clone())
                .or_insert_with(|| AppendBuffer::new(context_id, block_id.clone()));

            buffer.buffer.push_str(text);
            should_flush = buffer.should_flush(&self.config);
        }

        if should_flush {
            self.flush_block(&key).await;
            true
        } else {
            false
        }
    }

    /// Force flush a specific block's buffer.
    pub async fn flush_block(&self, block_key: &str) {
        let buffer_content = {
            let mut buffers = self.buffers.lock();
            if let Some(buffer) = buffers.get_mut(block_key) {
                let content = buffer.take();
                let context_id = buffer.context_id;
                let block_id = buffer.block_id.clone();
                if content.is_empty() {
                    None
                } else {
                    Some((context_id, block_id, content))
                }
            } else {
                None
            }
        };

        if let Some((context_id, block_id, content)) = buffer_content {
            // Append to the CRDT
            let _ = self.documents.append_text(context_id, &block_id, &content);
        }
    }

    /// Flush all pending buffers.
    pub async fn flush_all(&self) {
        let keys: Vec<String> = {
            let buffers = self.buffers.lock();
            buffers.keys().cloned().collect()
        };

        for key in keys {
            self.flush_block(&key).await;
        }
    }

    /// Flush and remove a block's buffer (call when block is finalized).
    pub async fn finalize_block(&self, block_key: &str) {
        self.flush_block(block_key).await;

        let mut buffers = self.buffers.lock();
        buffers.remove(block_key);
    }

    /// Get current buffer stats for debugging.
    pub fn stats(&self) -> BatcherStats {
        let buffers = self.buffers.lock();
        let mut total_buffered = 0;
        let mut oldest_age = Duration::ZERO;

        for buffer in buffers.values() {
            total_buffered += buffer.buffer.len();
            let age = buffer.last_flush.elapsed();
            if age > oldest_age {
                oldest_age = age;
            }
        }

        BatcherStats {
            active_buffers: buffers.len(),
            total_buffered_bytes: total_buffered,
            oldest_buffer_age: oldest_age,
        }
    }
}

/// Statistics about the batcher's current state.
#[derive(Debug, Clone)]
pub struct BatcherStats {
    /// Number of blocks with active buffers.
    pub active_buffers: usize,
    /// Total bytes buffered across all blocks.
    pub total_buffered_bytes: usize,
    /// Age of the oldest unflushed buffer.
    pub oldest_buffer_age: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store;
    use crate::db::DocumentKind;
    use kaijutsu_crdt::{BlockKind, PrincipalId, Role};

    fn test_context_id() -> ContextId {
        let uuid = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            b"kaijutsu:test:batch",
        );
        ContextId::from_bytes(*uuid.as_bytes())
    }

    fn setup_test_store() -> (SharedBlockStore, ContextId) {
        let store = shared_block_store(PrincipalId::system());
        let ctx_id = test_context_id();
        store.create_document(ctx_id, DocumentKind::Code, Some("rust".into())).unwrap();
        (store, ctx_id)
    }

    #[tokio::test]
    async fn test_batch_on_newline() {
        let (cells, ctx_id) = setup_test_store();

        // Create a block
        let block_id = cells.insert_block(ctx_id, None, None, Role::Model, BlockKind::Text, "").unwrap();

        let batcher = AppendBatcher::new(cells.clone());

        // Append without newline - should buffer
        let flushed = batcher.append(ctx_id, &block_id, "hello ").await;
        assert!(!flushed, "should buffer without newline");

        // Append with newline - should flush
        let flushed = batcher.append(ctx_id, &block_id, "world\n").await;
        assert!(flushed, "should flush on newline");

        // Check content was written
        let entry = cells.get(ctx_id).unwrap();
        let snapshot = entry.doc.get_block_snapshot(&block_id).unwrap();
        assert_eq!(snapshot.content, "hello world\n");
    }

    #[tokio::test]
    async fn test_batch_on_size() {
        let (cells, ctx_id) = setup_test_store();

        let block_id = cells.insert_block(ctx_id, None, None, Role::Model, BlockKind::Text, "").unwrap();

        // Config with small buffer, no newline flush
        let config = BatchConfig {
            max_buffer_size: 10,
            max_buffer_age: Duration::from_secs(60),
            flush_on_newline: false,
        };
        let batcher = AppendBatcher::with_config(cells.clone(), config);

        // Append small text - should buffer
        let flushed = batcher.append(ctx_id, &block_id, "12345").await;
        assert!(!flushed);

        // Append more to exceed limit - should flush
        let flushed = batcher.append(ctx_id, &block_id, "67890").await;
        assert!(flushed);

        let entry = cells.get(ctx_id).unwrap();
        let snapshot = entry.doc.get_block_snapshot(&block_id).unwrap();
        assert_eq!(snapshot.content, "1234567890");
    }

    #[tokio::test]
    async fn test_flush_all() {
        let (cells, ctx_id) = setup_test_store();

        let block1 = cells.insert_block(ctx_id, None, None, Role::Model, BlockKind::Text, "").unwrap();
        let block2 = cells.insert_block(ctx_id, None, None, Role::Model, BlockKind::Text, "").unwrap();

        // Config that never auto-flushes
        let config = BatchConfig {
            max_buffer_size: 1000,
            max_buffer_age: Duration::from_secs(60),
            flush_on_newline: false,
        };
        let batcher = AppendBatcher::with_config(cells.clone(), config);

        // Buffer to both blocks
        batcher.append(ctx_id, &block1, "block1 content").await;
        batcher.append(ctx_id, &block2, "block2 content").await;

        // Check nothing written yet
        {
            let entry = cells.get(ctx_id).unwrap();
            let snap1 = entry.doc.get_block_snapshot(&block1).unwrap();
            let snap2 = entry.doc.get_block_snapshot(&block2).unwrap();
            assert_eq!(snap1.content, "");
            assert_eq!(snap2.content, "");
        }

        // Flush all
        batcher.flush_all().await;

        // Now content should be there
        {
            let entry = cells.get(ctx_id).unwrap();
            let snap1 = entry.doc.get_block_snapshot(&block1).unwrap();
            let snap2 = entry.doc.get_block_snapshot(&block2).unwrap();
            assert_eq!(snap1.content, "block1 content");
            assert_eq!(snap2.content, "block2 content");
        }
    }

    #[tokio::test]
    async fn test_finalize_removes_buffer() {
        let (cells, ctx_id) = setup_test_store();

        let block_id = cells.insert_block(ctx_id, None, None, Role::Model, BlockKind::Text, "").unwrap();

        let batcher = AppendBatcher::new(cells.clone());

        // Buffer some content
        batcher.append(ctx_id, &block_id, "content").await;
        assert_eq!(batcher.stats().active_buffers, 1);

        // Finalize
        batcher.finalize_block(&block_id.to_key()).await;
        assert_eq!(batcher.stats().active_buffers, 0);

        // Content should be written
        let entry = cells.get(ctx_id).unwrap();
        let snapshot = entry.doc.get_block_snapshot(&block_id).unwrap();
        assert_eq!(snapshot.content, "content");
    }

    #[test]
    fn test_stats() {
        let cells = shared_block_store(PrincipalId::system());
        let batcher = AppendBatcher::new(cells);

        let stats = batcher.stats();
        assert_eq!(stats.active_buffers, 0);
        assert_eq!(stats.total_buffered_bytes, 0);
    }
}
