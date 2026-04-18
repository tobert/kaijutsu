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

pub mod cluster;
pub mod config;
pub mod content;
pub mod embedder;
pub mod index;
pub mod metadata;
pub mod synthesis;
pub mod watcher;

pub use config::IndexConfig;
pub use content::extract_context_content;
pub use embedder::{Embedder, OnnxEmbedder};

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use kaijutsu_types::{BlockSnapshot, ContextId, Status};
use std::sync::Mutex;
use std::sync::RwLock;

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
    embedder: Arc<dyn Embedder>,
    hnsw: RwLock<index::HnswIndex>,
    metadata: Mutex<metadata::MetadataStore>,
    config: IndexConfig,
    synthesis_cache: synthesis::SynthesisCache,
}

impl SemanticIndex {
    /// Create or load a semantic index.
    pub fn new(config: IndexConfig, embedder: Box<dyn Embedder>) -> Result<Self, IndexError> {
        std::fs::create_dir_all(&config.data_dir)?;

        let hnsw = index::HnswIndex::new(&config)?;
        let metadata = metadata::MetadataStore::open(&config.data_dir)?;

        Ok(Self {
            embedder: Arc::from(embedder),
            hnsw: RwLock::new(hnsw),
            metadata: Mutex::new(metadata),
            config,
            synthesis_cache: synthesis::SynthesisCache::new(),
        })
    }

    /// Index a context's blocks. Returns true if content was (re-)embedded.
    ///
    /// This is a blocking operation — call from `spawn_blocking`.
    /// ONNX inference, HNSW graph operations, and SQLite writes all happen synchronously.
    pub fn index_context(
        &self,
        ctx_id: ContextId,
        blocks: &[BlockSnapshot],
    ) -> Result<bool, IndexError> {
        let (text, hash) = extract_context_content(blocks, self.config.max_tokens * 4);

        if text.is_empty() {
            return Ok(false);
        }

        // Hold the metadata lock across hash-check + embed + assign_slot to prevent
        // another thread from embedding the same context concurrently.
        // Embedding is CPU-bound on a blocking thread, so holding std::sync::Mutex is fine.
        let mut meta = self.metadata.lock().unwrap();

        // Check if already indexed with same content
        if meta.get_content_hash(ctx_id)?.is_some_and(|h| h == hash) {
            return Ok(false);
        }

        // Embed — ONNX inference is CPU-bound, fine on a blocking thread
        let embedding = self.embedder.embed(&text)?;

        // Assign or get slot
        let slot = meta.assign_slot(
            ctx_id,
            &hash,
            self.embedder.model_name(),
            self.config.dimensions,
        )?;

        // Insert into HNSW
        {
            let mut hnsw = self.hnsw.write().unwrap();
            hnsw.insert(slot, &embedding)?;
            hnsw.save()?;
        }

        tracing::debug!(
            context = %ctx_id.short(),
            slot = slot,
            "indexed context"
        );

        // LRU eviction: if max_contexts is set and we've exceeded it, evict oldest
        if let Some(max) = self.config.max_contexts {
            let count = meta.count()?;
            if count > max {
                let to_evict = count - max;
                let evicted = meta.evict_oldest(to_evict)?;
                tracing::info!(
                    evicted = evicted,
                    max_contexts = max,
                    "evicted oldest contexts from index"
                );
            }
        }

        Ok(true)
    }

    /// Rebuild the HNSW index from scratch, reclaiming dead slots from eviction.
    ///
    /// HNSW does not support point deletion — `evict_oldest` removes metadata
    /// rows but leaves orphaned vectors in the graph. Call this periodically
    /// (e.g. on server startup or via admin command) to compact the index.
    ///
    /// Blocking — call from `spawn_blocking`.
    pub fn rebuild(&self) -> Result<(), IndexError> {
        // TODO: implement full rebuild
        //  1. Read all (slot, context_id) from metadata
        //  2. Collect embeddings from current HNSW for live slots
        //  3. Create a fresh HnswIndex
        //  4. Re-insert all live embeddings with compacted slot numbers
        //  5. Update metadata slot numbers to match
        //  6. Swap the new index into self.hnsw
        tracing::warn!("rebuild() is not yet implemented — dead HNSW slots persist until then");
        Ok(())
    }

    /// Search for contexts similar to a text query.
    ///
    /// Blocking — call from `spawn_blocking`.
    pub fn search(&self, query: &str, k: usize) -> Result<Vec<SearchResult>, IndexError> {
        let embedding = self.embedder.embed(query)?;

        let hnsw = self.hnsw.read().unwrap();
        let neighbors = hnsw.search(&embedding, k)?;

        let meta = self.metadata.lock().unwrap();
        let mut results = Vec::with_capacity(neighbors.len());
        for (slot, distance) in neighbors {
            if let Some(ctx_id) = meta.get_context_id(slot)? {
                results.push(SearchResult {
                    context_id: ctx_id,
                    score: (1.0 - distance).clamp(0.0, 1.0),
                    label: None,
                });
            }
        }

        Ok(results)
    }

    /// Find contexts similar to a given context.
    ///
    /// Blocking — call from `spawn_blocking`.
    pub fn neighbors(&self, ctx_id: ContextId, k: usize) -> Result<Vec<SearchResult>, IndexError> {
        let meta = self.metadata.lock().unwrap();
        let slot = match meta.get_slot(ctx_id)? {
            Some(s) => s,
            None => return Ok(vec![]),
        };
        drop(meta);

        let hnsw = self.hnsw.read().unwrap();
        let embedding = hnsw.get_embedding(slot)?;
        let neighbors = hnsw.search(&embedding, k + 1)?; // +1 to exclude self
        drop(hnsw);

        let meta = self.metadata.lock().unwrap();
        let mut results = Vec::with_capacity(neighbors.len());
        for (neighbor_slot, distance) in neighbors {
            if neighbor_slot == slot {
                continue; // skip self
            }
            if let Some(neighbor_ctx) = meta.get_context_id(neighbor_slot)? {
                results.push(SearchResult {
                    context_id: neighbor_ctx,
                    score: (1.0 - distance).clamp(0.0, 1.0),
                    label: None,
                });
            }
        }

        Ok(results)
    }

    /// Compute clusters of related contexts.
    ///
    /// Blocking — call from `spawn_blocking`.
    pub fn clusters(&self, min_cluster_size: usize) -> Result<Vec<ClusterInfo>, IndexError> {
        let hnsw = self.hnsw.read().unwrap();
        let all_embeddings = hnsw.get_all_embeddings()?;
        drop(hnsw);

        if all_embeddings.is_empty() {
            return Ok(vec![]);
        }

        let raw_clusters = cluster::compute_clusters(&all_embeddings, min_cluster_size)?;

        let meta = self.metadata.lock().unwrap();
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
    pub fn len(&self) -> usize {
        let meta = self.metadata.lock().unwrap();
        meta.count().unwrap_or(0)
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Save the HNSW index to disk. Blocking.
    pub fn save(&self) -> Result<(), IndexError> {
        let hnsw = self.hnsw.read().unwrap();
        hnsw.save()
    }

    /// Access the embedder (for external use, e.g. reranking).
    pub fn embedder(&self) -> &dyn Embedder {
        &*self.embedder
    }

    /// Access the embedder as an Arc (for Rhai registration).
    pub fn embedder_arc(&self) -> Arc<dyn Embedder> {
        self.embedder.clone()
    }

    /// Access the synthesis cache.
    pub fn synthesis_cache(&self) -> &synthesis::SynthesisCache {
        &self.synthesis_cache
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockId, BlockKind, PrincipalId, Role};
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use tempfile::TempDir;

    /// Deterministic mock embedder for testing.
    ///
    /// Produces L2-normalized vectors by hashing text bytes into components.
    struct MockEmbedder {
        dims: usize,
    }

    impl Embedder for MockEmbedder {
        fn model_name(&self) -> &str {
            "mock"
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, IndexError> {
            texts.iter().map(|t| self.embed(t)).collect()
        }

        fn embed(&self, text: &str) -> Result<Vec<f32>, IndexError> {
            let mut v = vec![0.0f32; self.dims];
            // Hash text bytes into vector components
            for (i, byte) in text.bytes().enumerate() {
                let mut hasher = DefaultHasher::new();
                (i, byte).hash(&mut hasher);
                let h = hasher.finish();
                let idx = (h as usize) % self.dims;
                v[idx] += (h as f32) / u64::MAX as f32;
            }
            // L2 normalize
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in &mut v {
                    *x /= norm;
                }
            } else {
                // Fallback: point along first axis
                v[0] = 1.0;
            }
            Ok(v)
        }
    }

    /// Deterministic embedder for testing neighbour semantics.
    ///
    /// Texts containing `"PAIR"` land on the first axis (close to each other).
    /// Texts containing `"FILLER"` are spread along axes ≥ 2. This removes the
    /// hash-distance noise of `MockEmbedder` for tests that assert on ordering.
    struct KeyedEmbedder {
        dims: usize,
    }

    impl Embedder for KeyedEmbedder {
        fn model_name(&self) -> &str {
            "keyed"
        }
        fn dimensions(&self) -> usize {
            self.dims
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, IndexError> {
            texts.iter().map(|t| self.embed(t)).collect()
        }
        fn embed(&self, text: &str) -> Result<Vec<f32>, IndexError> {
            let mut v = vec![0.0f32; self.dims];
            if text.contains("PAIR") {
                // Both pair points live near axis 0; slight perturbation on
                // axis 1 so they aren't exactly equal (prevents dedup).
                v[0] = 1.0;
                let mut hasher = DefaultHasher::new();
                text.hash(&mut hasher);
                v[1] = 0.01 * ((hasher.finish() as f32) / u64::MAX as f32);
            } else {
                // Filler: pick an axis ≥ 2 deterministically from the text hash.
                let mut hasher = DefaultHasher::new();
                text.hash(&mut hasher);
                let axis = 2 + (hasher.finish() as usize) % (self.dims - 2);
                v[axis] = 1.0;
            }
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            for x in &mut v {
                *x /= norm;
            }
            Ok(v)
        }
    }

    fn test_config(dir: &std::path::Path) -> IndexConfig {
        IndexConfig {
            model_dir: dir.to_path_buf(),
            dimensions: 32,
            data_dir: dir.to_path_buf(),
            hnsw_max_nb_connection: 8,
            hnsw_ef_construction: 50,
            max_tokens: 512,
            max_contexts: None,
        }
    }

    fn make_blocks(ctx_id: ContextId, content: &str) -> Vec<BlockSnapshot> {
        let agent = PrincipalId::new();
        let id = BlockId::new(ctx_id, agent, 1);
        vec![BlockSnapshot {
            id,
            parent_id: None,
            role: Role::Model,
            kind: BlockKind::Text,
            status: kaijutsu_types::Status::Done,
            content: content.to_string(),
            ..BlockSnapshot::text(id, None, Role::Model, content)
        }]
    }

    /// Seed the index with `n` filler contexts of unrelated content.
    ///
    /// hnsw_rs assigns every point to a random layer (exponential distribution,
    /// P(level > 0) = 1/max_nb_connection). With only 2 points, there's a ~20%
    /// chance the graph ends up split across layers in a way that search can't
    /// traverse. Populating enough unrelated points guarantees layer-0
    /// connectivity so tests that assert on search/neighbor results are stable.
    /// The `FILLER` keyword keeps `KeyedEmbedder` fillers off the PAIR axis.
    fn seed_filler(idx: &SemanticIndex, n: usize) {
        for i in 0..n {
            let ctx = ContextId::new();
            let filler = format!("FILLER context number {i}");
            idx.index_context(ctx, &make_blocks(ctx, &filler)).unwrap();
        }
    }

    #[test]
    fn test_index_and_search_round_trip() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        let ctx = ContextId::new();
        let blocks = make_blocks(ctx, "the quick brown fox jumps over the lazy dog");

        let indexed = idx.index_context(ctx, &blocks).unwrap();
        assert!(indexed, "first indexing should embed");

        let results = idx.search("quick brown fox", 5).unwrap();
        assert!(!results.is_empty(), "search should return results");
        assert_eq!(results[0].context_id, ctx);

        // Scores must be in [0.0, 1.0]
        for r in &results {
            assert!(
                r.score >= 0.0 && r.score <= 1.0,
                "score {} out of range",
                r.score
            );
        }
    }

    #[test]
    fn test_dedup_same_content() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        let ctx = ContextId::new();
        let blocks = make_blocks(ctx, "identical content for dedup test");

        let first = idx.index_context(ctx, &blocks).unwrap();
        assert!(first, "first call should index");

        let second = idx.index_context(ctx, &blocks).unwrap();
        assert!(!second, "second call with same content should skip");
    }

    #[test]
    fn test_neighbors() {
        // This test covers the `neighbors()` API — metadata lookup, self-
        // exclusion, score clamping. It does NOT assert on HNSW approximate-
        // nearest-neighbor ordering: hnsw_rs's reverse_update writes reverse
        // edges at the neighbour's own level (not the current search layer),
        // so points inserted after a random-higher-layer point may not appear
        // in its layer-0 neighbour list. Semantic ordering quality belongs in
        // integration tests with the real ONNX embedder + a realistic corpus.
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let idx = SemanticIndex::new(config, Box::new(KeyedEmbedder { dims: 32 })).unwrap();
        seed_filler(&idx, 30);

        let ctx1 = ContextId::new();
        let ctx2 = ContextId::new();
        idx.index_context(ctx1, &make_blocks(ctx1, "PAIR alpha content"))
            .unwrap();
        idx.index_context(ctx2, &make_blocks(ctx2, "PAIR beta content"))
            .unwrap();

        let neighbors = idx.neighbors(ctx1, 5).unwrap();
        assert!(!neighbors.is_empty(), "should find at least one neighbor");
        for r in &neighbors {
            assert_ne!(r.context_id, ctx1, "self must be excluded");
            assert!(
                r.score >= 0.0 && r.score <= 1.0,
                "score {} out of range",
                r.score
            );
        }
    }

    #[test]
    fn test_persistence_round_trip() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());

        let ctx1 = ContextId::new();
        let ctx2 = ContextId::new();

        // Index and save
        {
            let idx =
                SemanticIndex::new(config.clone(), Box::new(MockEmbedder { dims: 32 })).unwrap();
            // See note in test_neighbors: tiny HNSW graphs are probabilistically
            // disconnected, so seed enough points to guarantee reachability.
            seed_filler(&idx, 30);
            idx.index_context(ctx1, &make_blocks(ctx1, "persistence test alpha"))
                .unwrap();
            idx.index_context(ctx2, &make_blocks(ctx2, "persistence test beta"))
                .unwrap();
            idx.save().unwrap();
        }

        // Reload and verify
        {
            let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

            let results = idx.search("persistence test", 5).unwrap();
            assert!(results.len() >= 2, "should find both contexts after reload");

            let neighbors = idx.neighbors(ctx1, 5).unwrap();
            assert!(!neighbors.is_empty(), "neighbors should work after reload");
        }
    }

    #[test]
    fn test_empty_index_search() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        let results = idx.search("anything", 5).unwrap();
        assert!(
            results.is_empty(),
            "empty index should return empty results"
        );
    }

    #[test]
    fn test_max_contexts_eviction() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config(dir.path());
        config.max_contexts = Some(2);
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        let ctx1 = ContextId::new();
        let ctx2 = ContextId::new();
        let ctx3 = ContextId::new();

        idx.index_context(ctx1, &make_blocks(ctx1, "alpha context first"))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        idx.index_context(ctx2, &make_blocks(ctx2, "beta context second"))
            .unwrap();
        assert_eq!(idx.len(), 2);

        // Indexing a third should evict the oldest (ctx1)
        std::thread::sleep(std::time::Duration::from_millis(10));
        idx.index_context(ctx3, &make_blocks(ctx3, "gamma context third"))
            .unwrap();
        assert_eq!(idx.len(), 2, "should have evicted down to max_contexts");

        // ctx1 should be gone from metadata
        let meta = idx.metadata.lock().unwrap();
        assert!(
            meta.get_slot(ctx1).unwrap().is_none(),
            "ctx1 should be evicted"
        );
        assert!(meta.get_slot(ctx2).unwrap().is_some(), "ctx2 should remain");
        assert!(meta.get_slot(ctx3).unwrap().is_some(), "ctx3 should remain");
    }

    #[test]
    fn test_empty_content_not_indexed() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        let ctx = ContextId::new();
        let indexed = idx.index_context(ctx, &[]).unwrap();
        assert!(!indexed, "empty blocks should not be indexed");
        assert!(idx.is_empty());
    }
}
