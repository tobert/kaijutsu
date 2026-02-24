//! Semantic vector indexing for kaijutsu contexts.
//!
//! Provides local ONNX embeddings, HNSW nearest-neighbor search, and
//! density-based clustering. No external API calls — fully offline.
//!
//! # Architecture
//!
//! ```text
//! kaijutsu-types  (leaf)
//!        │
//! kaijutsu-index  (this crate — no kernel/crdt dep)
//!        │
//! kaijutsu-server (implements BlockSource/StatusReceiver traits)
//! ```

pub mod config;
pub mod embedder;
pub mod content;
pub mod index;
pub mod metadata;
pub mod cluster;
pub mod watcher;

pub use config::IndexConfig;
pub use embedder::{Embedder, OnnxEmbedder};
pub use content::extract_context_content;

use std::pin::Pin;
use std::future::Future;

use kaijutsu_types::{BlockSnapshot, ContextId, Status};
use tokio::sync::RwLock;

// ============================================================================
// Error Types
// ============================================================================

/// Errors from the semantic index subsystem.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("Model not found: {0}")]
    ModelNotFound(String),

    #[error("ONNX error: {0}")]
    Onnx(String),

    #[error("Tokenizer error: {0}")]
    Tokenizer(String),

    #[error("Embedding error: {0}")]
    Embedding(String),

    #[error("Index error: {0}")]
    Index(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

// ============================================================================
// Trait Boundaries
// ============================================================================

/// Source of block data for a context.
///
/// The server crate implements this on SharedBlockStore.
pub trait BlockSource: Send + Sync {
    fn block_snapshots(&self, ctx: ContextId) -> Result<Vec<BlockSnapshot>, String>;
}

/// Notification when a block reaches terminal status.
pub struct StatusEvent {
    pub context_id: ContextId,
    pub status: Status,
}

/// Receiver for block status events.
///
/// The server crate implements this as a wrapper over FlowBus subscription.
pub trait StatusReceiver: Send {
    fn recv(&mut self) -> Pin<Box<dyn Future<Output = Option<StatusEvent>> + Send + '_>>;
}

// ============================================================================
// Search Results
// ============================================================================

/// A context returned by semantic search.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub context_id: ContextId,
    pub score: f32,
    pub label: Option<String>,
}

/// A cluster of related contexts.
#[derive(Debug, Clone)]
pub struct ClusterInfo {
    pub cluster_id: usize,
    pub context_ids: Vec<ContextId>,
}

// ============================================================================
// SemanticIndex
// ============================================================================

/// Main entry point for semantic indexing.
///
/// Combines an embedder, HNSW index, and SQLite metadata store.
/// Thread-safe — wrap in Arc for sharing.
pub struct SemanticIndex {
    embedder: Box<dyn Embedder>,
    hnsw: RwLock<index::HnswIndex>,
    metadata: tokio::sync::Mutex<metadata::MetadataStore>,
    config: IndexConfig,
}

impl SemanticIndex {
    /// Create or load a semantic index.
    pub fn new(config: IndexConfig, embedder: Box<dyn Embedder>) -> Result<Self, IndexError> {
        std::fs::create_dir_all(&config.data_dir)?;

        let hnsw = index::HnswIndex::new(&config)?;
        let metadata = metadata::MetadataStore::open(&config.data_dir)?;

        Ok(Self {
            embedder,
            hnsw: RwLock::new(hnsw),
            metadata: tokio::sync::Mutex::new(metadata),
            config,
        })
    }

    /// Index a context's blocks. Returns true if content was (re-)embedded.
    ///
    /// Skips re-embedding if the content hash hasn't changed.
    pub async fn index_context(
        &self,
        ctx_id: ContextId,
        blocks: &[BlockSnapshot],
    ) -> Result<bool, IndexError> {
        let (text, hash) = extract_context_content(blocks, self.config.max_tokens * 4);

        if text.is_empty() {
            return Ok(false);
        }

        // Check if already indexed with same content
        {
            let meta = self.metadata.lock().await;
            if meta.get_content_hash(ctx_id)?.is_some_and(|h| h == hash) {
                return Ok(false);
            }
        }

        // Embed
        let embedding = self.embedder.embed(&text)?;

        // Assign or get slot
        let mut meta = self.metadata.lock().await;
        let slot = meta.assign_slot(
            ctx_id,
            &hash,
            self.embedder.model_name(),
            self.config.dimensions,
        )?;

        // Insert into HNSW
        {
            let mut hnsw = self.hnsw.write().await;
            hnsw.insert(slot, &embedding)?;
        }

        tracing::debug!(
            context = %ctx_id.short(),
            slot = slot,
            "indexed context"
        );

        Ok(true)
    }

    /// Search for contexts similar to a text query.
    pub async fn search(&self, query: &str, k: usize) -> Result<Vec<SearchResult>, IndexError> {
        let embedding = self.embedder.embed(query)?;

        let hnsw = self.hnsw.read().await;
        let neighbors = hnsw.search(&embedding, k)?;

        let meta = self.metadata.lock().await;
        let mut results = Vec::with_capacity(neighbors.len());
        for (slot, distance) in neighbors {
            if let Some(ctx_id) = meta.get_context_id(slot)? {
                results.push(SearchResult {
                    context_id: ctx_id,
                    score: 1.0 - distance, // cosine distance → similarity
                    label: None,
                });
            }
        }

        Ok(results)
    }

    /// Find contexts similar to a given context.
    pub async fn neighbors(
        &self,
        ctx_id: ContextId,
        k: usize,
    ) -> Result<Vec<SearchResult>, IndexError> {
        let meta = self.metadata.lock().await;
        let slot = match meta.get_slot(ctx_id)? {
            Some(s) => s,
            None => return Ok(vec![]),
        };
        drop(meta);

        let hnsw = self.hnsw.read().await;
        let embedding = hnsw.get_embedding(slot)?;
        let neighbors = hnsw.search(&embedding, k + 1)?; // +1 to exclude self
        drop(hnsw);

        let meta = self.metadata.lock().await;
        let mut results = Vec::with_capacity(neighbors.len());
        for (neighbor_slot, distance) in neighbors {
            if neighbor_slot == slot {
                continue; // skip self
            }
            if let Some(neighbor_ctx) = meta.get_context_id(neighbor_slot)? {
                results.push(SearchResult {
                    context_id: neighbor_ctx,
                    score: 1.0 - distance,
                    label: None,
                });
            }
        }

        Ok(results)
    }

    /// Compute clusters of related contexts.
    pub async fn clusters(
        &self,
        min_cluster_size: usize,
    ) -> Result<Vec<ClusterInfo>, IndexError> {
        let hnsw = self.hnsw.read().await;
        let all_embeddings = hnsw.get_all_embeddings()?;
        drop(hnsw);

        if all_embeddings.is_empty() {
            return Ok(vec![]);
        }

        let raw_clusters = cluster::compute_clusters(&all_embeddings, min_cluster_size)?;

        let meta = self.metadata.lock().await;
        let mut clusters = Vec::with_capacity(raw_clusters.len());
        for (cluster_id, slots) in raw_clusters {
            let mut context_ids = Vec::with_capacity(slots.len());
            for slot in slots {
                if let Some(ctx_id) = meta.get_context_id(slot)? {
                    context_ids.push(ctx_id);
                }
            }
            if !context_ids.is_empty() {
                clusters.push(ClusterInfo {
                    cluster_id,
                    context_ids,
                });
            }
        }

        Ok(clusters)
    }

    /// Number of indexed contexts.
    pub async fn len(&self) -> usize {
        let meta = self.metadata.lock().await;
        meta.count().unwrap_or(0)
    }

    /// Whether the index is empty.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Save the HNSW index to disk.
    pub async fn save(&self) -> Result<(), IndexError> {
        let hnsw = self.hnsw.read().await;
        hnsw.save()
    }

    /// Access the embedder (for external use, e.g. reranking).
    pub fn embedder(&self) -> &dyn Embedder {
        &*self.embedder
    }
}
