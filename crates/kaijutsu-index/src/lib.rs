//! Semantic vector indexing for kaijutsu contexts.
//!
//! Provides asynchronous service embeddings, HNSW nearest-neighbor search,
//! density-based clustering, and persistent synthesis caches.
//!
//! # Architecture
//!
//! ```text
//! kaijutsu-types  (leaf)
//!        │
//! kaijutsu-index  (this crate — no kernel dep)
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
pub use embedder::{Embedder, EmbeddingPurpose};
pub mod lfm2d;
pub use lfm2d::Lfm2dEmbedder;

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

    #[error("Inference error: {0}")]
    Inference(String),

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

/// Outcome of a `SemanticIndex::rebuild()` pass.
#[derive(Debug, Clone)]
pub struct RebuildStats {
    /// Live slots re-inserted into the fresh graph.
    pub kept: usize,
    /// Graph points dropped because they had no matching metadata row
    /// (evicted since the last rebuild — dead weight the old graph carried).
    pub dropped_dead: usize,
    /// Metadata rows deleted because they pointed at a slot with no graph
    /// point behind it — a crash artifact (e.g. process died between
    /// `assign_slot` and the HNSW insert). The watcher re-indexes the
    /// context on its next terminal event.
    pub repaired_orphan_rows: usize,
}

/// A cluster of related contexts.
#[derive(Debug, Clone)]
pub struct ClusterInfo {
    pub cluster_id: usize,
    pub context_ids: Vec<ContextId>,
    /// Kernel-synthesized label for the cluster (the top keyword shared across
    /// its members), or `None` when no member has synthesis keywords.
    pub label: Option<String>,
}

/// Pick a cluster label from its members' synthesis keywords.
///
/// Tallies each keyword's summed score across all members and returns the
/// highest-scoring term. Score ties break alphabetically (smaller term wins) so
/// the label is deterministic regardless of member iteration order. Returns
/// `None` when no member contributed any keyword.
fn pick_cluster_label<'a>(
    member_keywords: impl IntoIterator<Item = &'a [(String, f32)]>,
) -> Option<String> {
    let mut totals: std::collections::HashMap<&str, f32> = std::collections::HashMap::new();
    for kws in member_keywords {
        for (term, score) in kws {
            *totals.entry(term.as_str()).or_insert(0.0) += *score;
        }
    }
    totals
        .into_iter()
        .max_by(|(a_term, a_score), (b_term, b_score)| {
            a_score
                .partial_cmp(b_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                // On a score tie, the alphabetically smaller term should win, so
                // it must compare as "greater" for max_by: compare b's term to a's.
                .then_with(|| b_term.cmp(a_term))
        })
        .map(|(term, _)| term.to_string())
}

// ============================================================================
// SemanticIndex
// ============================================================================

/// Main entry point for semantic indexing.
///
/// Combines an embedder, HNSW index, and SQLite metadata store.
/// Thread-safe — wrap in Arc for sharing.
///
/// # Lock order
///
/// Writers take `metadata` then `hnsw` during storage. Inference awaits
/// happen outside both locks. Readers must drop `hnsw` before taking
/// `metadata` to avoid inverting the writer order. Blocking graph and SQLite
/// work runs on blocking tasks. Poisoned locks are fatal: serving state of
/// unknown integrity is worse than refusing the operation.
pub struct SemanticIndex {
    embedder: Arc<dyn Embedder>,
    hnsw: Arc<RwLock<index::HnswIndex>>,
    metadata: Arc<Mutex<metadata::MetadataStore>>,
    config: IndexConfig,
    synthesis_cache: Arc<synthesis::SynthesisCache>,
    synthesis_refresh: tokio::sync::Mutex<()>,
}

impl SemanticIndex {
    /// Create or load a semantic index.
    ///
    /// The persisted embedding profile covers model name, revision,
    /// dimensions, and the query/document normalization contract. A changed
    /// or missing profile clears the derived graph and synthesis caches,
    /// including synthesis-only databases. Context blocks remain intact.
    ///
    /// If the loaded graph carries more points than metadata has live rows —
    /// dead slots survived from eviction before the process last stopped —
    /// this runs a `rebuild()` before returning so callers never observe a
    /// stale, bloated graph. See `rebuild()` for what that entails.
    pub fn new(config: IndexConfig, embedder: Box<dyn Embedder>) -> Result<Self, IndexError> {
        std::fs::create_dir_all(&config.data_dir)?;

        // Must run before HnswIndex::new/MetadataStore::open construct
        // anything real, so a mismatch leaves a genuinely empty data_dir for
        // the normal fresh-index path below to build on.
        if config.dimensions != embedder.dimensions() || config.dimensions == 0 {
            return Err(IndexError::Embedding("index dimensions do not match the embedding profile".into()));
        }
        Self::wipe_on_model_mismatch(&config, &embedder.cache_identity())?;

        let hnsw = index::HnswIndex::new(&config)?;
        let metadata = metadata::MetadataStore::open(&config.data_dir)?;
        metadata.set_embedding_profile(&embedder.cache_identity())?;

        let this = Self {
            embedder: Arc::from(embedder),
            hnsw: Arc::new(RwLock::new(hnsw)),
            metadata: Arc::new(Mutex::new(metadata)),
            config,
            synthesis_cache: Arc::new(synthesis::SynthesisCache::new()),
            synthesis_refresh: tokio::sync::Mutex::new(()),
        };

        // Lock order: each guard here is a standalone temporary, dropped at
        // the end of its own statement, so hnsw and metadata are never held
        // simultaneously (see struct-level lock order docs).
        //
        // Any disagreement triggers the rebuild: graph > meta means dead
        // points from eviction; meta > graph means orphan rows (crash between
        // the metadata commit and the graph save) that rebuild() repairs so
        // those contexts get re-indexed instead of erroring in neighbors().
        let graph_count = this.hnsw.read().unwrap().graph_point_count();
        let meta_count = this.metadata.lock().unwrap().count()?;
        if graph_count != meta_count {
            let stats = this.rebuild()?;
            tracing::info!(
                graph_count,
                meta_count,
                kept = stats.kept,
                dropped_dead = stats.dropped_dead,
                repaired_orphan_rows = stats.repaired_orphan_rows,
                "startup auto-rebuild reclaimed dead HNSW slots"
            );
        }

        // Hydrate the in-memory synthesis cache from SQLite so app well cards
        // (gist/keywords) aren't blank after a restart — the watcher's
        // on_indexed callback only fires on content *change*, so an unchanged
        // context would otherwise never re-populate the memory-only cache.
        let persisted = this.metadata.lock().unwrap().load_all_synthesis()?;
        let persisted_count = persisted.len();
        for (ctx, result) in persisted {
            this.synthesis_cache.insert(ctx, result);
        }
        if persisted_count > 0 {
            tracing::info!(
                count = persisted_count,
                "hydrated synthesis cache from index_meta.db"
            );
        }

        Ok(this)
    }

    /// Compare the global profile before loading the graph. Close SQLite
    /// before deleting mismatched derived files, including atomic dump remnants.
    fn wipe_on_model_mismatch(config: &IndexConfig, model_name: &str) -> Result<(), IndexError> {
        let meta_path = config.data_dir.join("index_meta.db");
        if !meta_path.exists() {
            return Ok(());
        }

        let old_profile = metadata::MetadataStore::open(&config.data_dir)?.embedding_profile()?;
        if old_profile.as_deref() == Some(model_name) { return Ok(()); }
        tracing::warn!(old_profile = ?old_profile, new_profile = %model_name,
            "embedding profile changed or unrecorded; clearing derived index and synthesis caches");

        // Real index files plus any in-flight atomic-dump leftovers
        // (index.new.*) — a completed old-model dump must be deleted
        // outright here, not left for HnswIndex::new's recover_atomic_dump
        // to resurrect onto the (about to be absent) real files.
        for name in [
            "index.hnsw.graph",
            "index.hnsw.data",
            "index_meta.db",
            "index_meta.db-wal",
            "index_meta.db-shm",
            "index.new.hnsw.graph",
            "index.new.hnsw.data",
            "index.new.ready",
        ] {
            let path = config.data_dir.join(name);
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
        }

        Ok(())
    }

    /// Index a context's blocks. Returns true if content was (re-)embedded.
    ///
    /// Service inference holds no storage locks. An optimistic hash check
    /// refuses to overwrite a concurrent refresh with superseded work.
    pub async fn index_context(
        &self,
        ctx_id: ContextId,
        blocks: &[BlockSnapshot],
    ) -> Result<bool, IndexError> {
        let (text, hash) = extract_context_content(blocks, self.config.max_context_bytes);
        if text.is_empty() { return Ok(false); }
        let metadata = self.metadata.clone();
        let before = tokio::task::spawn_blocking(move || metadata.lock().unwrap().get_content_hash(ctx_id))
            .await.map_err(|e| IndexError::Index(format!("read index metadata: {e}")))??;
        if before.as_deref() == Some(hash.as_str()) { return Ok(false); }

        let embedding = self.embedder.embed(&text, EmbeddingPurpose::Document).await?;
        let metadata = self.metadata.clone();
        let hnsw_store = self.hnsw.clone();
        let synthesis_cache = self.synthesis_cache.clone();
        let model_name = self.embedder.model_name().to_owned();
        let dimensions = self.config.dimensions;
        let max_contexts = self.config.max_contexts;
        tokio::task::spawn_blocking(move || {
            let mut meta = metadata.lock().unwrap();
            let current = meta.get_content_hash(ctx_id)?;
            if current.as_deref() == Some(hash.as_str()) { return Ok(false); }
            if current != before {
                return Err(IndexError::Index("context was indexed by another refresh; retry with current blocks".into()));
            }
            // Assign or get slot
            let slot = meta.assign_slot(
                ctx_id,
                &hash,
                &model_name,
                dimensions,
            )?;

            // Insert into HNSW
            {
                let mut hnsw = hnsw_store.write().unwrap();
                hnsw.insert(slot, &embedding)?;
                hnsw.save()?;
            }

            tracing::debug!(
                context = %ctx_id.short(),
                slot = slot,
                "indexed context"
            );

            // LRU eviction: if max_contexts is set and we've exceeded it, evict oldest
            if let Some(max) = max_contexts {
                let count = meta.count()?;
                if count > max {
                    let to_evict = count - max;
                    let evicted = meta.evict_oldest(to_evict)?;

                    // The graph points for evicted slots remain in HNSW — it has
                    // no delete — but clear_slot at least stops the embeddings
                    // cache from continuing to serve them. meta is still held
                    // here, so lock order (metadata -> hnsw) matches every other
                    // writer. The graph points themselves are only reclaimed by
                    // the next rebuild() (startup auto-rebuild or `kj synth
                    // rebuild`). The in-memory synthesis cache must be cleared
                    // too — evict_oldest already deleted the SQLite rows, and a
                    // leftover memory entry would serve the evicted context's
                    // gist until the next restart.
                    if !evicted.is_empty() {
                        let mut hnsw = hnsw_store.write().unwrap();
                        for (slot, _) in &evicted {
                            hnsw.clear_slot(*slot);
                        }
                        drop(hnsw);
                        for (_, ctx) in &evicted {
                            synthesis_cache.remove(*ctx);
                        }
                    }

                    tracing::info!(
                        evicted = evicted.len(),
                        max_contexts = max,
                        "evicted oldest contexts from index"
                    );
                }
            }

            Ok(true)
        }).await.map_err(|e| IndexError::Index(format!("store index: {e}")))?
    }

    /// Rebuild the HNSW index from scratch, reclaiming dead slots from eviction.
    ///
    /// HNSW does not support point deletion — `evict_oldest` removes metadata
    /// rows but leaves orphaned vectors in the graph. Call this periodically
    /// (e.g. on server startup or via `kj synth rebuild`) to compact the index.
    ///
    /// Slot numbers are **never renumbered**: the fresh graph re-inserts each
    /// live slot at its existing number, so a normal rebuild never writes to
    /// metadata — the graph is simply swapped for a smaller one holding the
    /// same slots. This makes crash-consistency trivial: if the process dies
    /// mid-rebuild, metadata and the old (still-present, still valid)
    /// `index.hnsw.*` files never disagree. The only metadata write is orphan
    /// repair (see below), which is itself limited to deleting rows that were
    /// already unusable.
    ///
    /// Blocking — call from `spawn_blocking`.
    pub fn rebuild(&self) -> Result<RebuildStats, IndexError> {
        // Lock order: metadata then hnsw, matching index_context. This blocks
        // concurrent index_context for the duration of the rebuild, which is
        // correct — we're about to swap the graph out from under it.
        let mut meta = self.metadata.lock().unwrap();
        let slots = meta.all_slots()?;

        let mut hnsw = self.hnsw.write().unwrap();
        let old_point_count = hnsw.graph_point_count();

        let mut entries = Vec::with_capacity(slots.len());
        let mut repaired_orphan_rows = 0usize;

        for (slot, ctx_id) in &slots {
            match hnsw.get_embedding(*slot) {
                Ok(embedding) => entries.push((*slot, embedding)),
                Err(_) => {
                    // Metadata row survived without a matching graph point —
                    // a crash artifact, not a normal eviction (eviction
                    // deletes the metadata row too, via evict_oldest). Drop
                    // the row; the watcher re-indexes this context on its
                    // next terminal event.
                    tracing::warn!(
                        context = %ctx_id.short(),
                        slot = slot,
                        "rebuild: metadata row has no graph point, repairing"
                    );
                    meta.remove(*ctx_id)?;
                    repaired_orphan_rows += 1;
                }
            }
        }

        let kept = entries.len();
        let dropped_dead = old_point_count.saturating_sub(kept);

        let new_index = index::HnswIndex::from_entries(&self.config, &entries)?;
        new_index.save()?;

        *hnsw = new_index;

        let stats = RebuildStats {
            kept,
            dropped_dead,
            repaired_orphan_rows,
        };
        tracing::info!(
            kept = stats.kept,
            dropped_dead = stats.dropped_dead,
            repaired_orphan_rows = stats.repaired_orphan_rows,
            "rebuilt HNSW index"
        );
        Ok(stats)
    }

    /// Search for contexts similar to a text query.
    ///
    /// Embeds with query purpose, then searches on a blocking task.
    pub async fn search(&self, query: &str, k: usize) -> Result<Vec<SearchResult>, IndexError> {
        let embedding = self.embedder.embed(query, EmbeddingPurpose::Query).await?;
        let hnsw_store = self.hnsw.clone();
        let metadata = self.metadata.clone();
        tokio::task::spawn_blocking(move || {
            // Lock order: drop the hnsw guard before taking metadata (see struct docs).
            let neighbors = {
                let hnsw = hnsw_store.read().unwrap();
                hnsw.search(&embedding, k)?
            };

            let meta = metadata.lock().unwrap();
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
        }).await.map_err(|e| IndexError::Index(format!("search index: {e}")))?
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
                // Synthesize a label from members' keywords (kernel-side, so the
                // client just renders it — see thin-client/smart-kernel rule).
                let synth = self.synthesis_cache();
                let kw_lists: Vec<Vec<(String, f32)>> = context_ids
                    .iter()
                    .filter_map(|id| synth.get_any(*id).map(|s| s.keywords))
                    .collect();
                let label = pick_cluster_label(kw_lists.iter().map(|v| v.as_slice()));
                clusters.push(ClusterInfo {
                    cluster_id,
                    context_ids,
                    label,
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

    /// Serialize synthesis refreshes so concurrent requests can reuse the
    /// first result. This guard is independent of all storage locks.
    pub async fn synthesis_refresh(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.synthesis_refresh.lock().await
    }

    /// Persist a synthesis result and update the in-memory cache.
    ///
    /// DB-first: writes to `index_meta.db` before touching the memory cache,
    /// so a persistence failure returns `Err` without the cache and the DB
    /// disagreeing (see the "observable write failures" convention — no
    /// swallowed warn on a write-through failure). The metadata lock is held
    /// only for the DB write, then dropped before the memory-cache insert —
    /// `SynthesisCache` has its own internal lock and is deliberately outside
    /// the struct's hnsw/metadata lock order.
    pub fn store_synthesis(
        &self,
        ctx: ContextId,
        result: synthesis::SynthesisResult,
    ) -> Result<(), IndexError> {
        {
            let mut meta = self.metadata.lock().unwrap();
            meta.save_synthesis(ctx, &result)?;
        }
        self.synthesis_cache.insert(ctx, result);
        Ok(())
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

    fn kw(pairs: &[(&str, f32)]) -> Vec<(String, f32)> {
        pairs.iter().map(|(t, s)| (t.to_string(), *s)).collect()
    }

    struct PausingEmbedder {
        started: Arc<tokio::sync::Notify>, release: Arc<tokio::sync::Notify>,
        purposes: Arc<Mutex<Vec<EmbeddingPurpose>>>,
    }
    #[async_trait::async_trait]
    impl Embedder for PausingEmbedder {
        fn model_name(&self) -> &str { "mock" }
        fn revision(&self) -> &str { "mock-v1" }
        fn dimensions(&self) -> usize { 32 }
        async fn embed_batch(&self, texts: &[&str], purpose: EmbeddingPurpose) -> Result<Vec<Vec<f32>>, IndexError> {
            self.purposes.lock().unwrap().push(purpose);
            if texts.iter().any(|text| text.contains("old snapshot")) {
                self.started.notify_one();
                self.release.notified().await;
            }
            MockEmbedder { dims: 32 }.embed_batch(texts, purpose).await
        }
    }

    #[tokio::test]
    async fn inference_releases_storage_and_superseded_index_write_is_refused() {
        let dir = TempDir::new().unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let purposes = Arc::new(Mutex::new(Vec::new()));
        let idx = Arc::new(SemanticIndex::new(test_config(dir.path()), Box::new(PausingEmbedder {
            started: started.clone(), release: release.clone(), purposes: purposes.clone(),
        })).unwrap());
        let ctx = ContextId::new();
        let old = { let idx = idx.clone(); tokio::spawn(async move {
            idx.index_context(ctx, &make_blocks(ctx, "old snapshot waiting on the service")).await
        }) };
        started.notified().await;
        let new = make_blocks(ctx, "new snapshot with the latest text");
        tokio::time::timeout(std::time::Duration::from_secs(2), idx.index_context(ctx, &new))
            .await.expect("inference must not hold the storage lock").unwrap();
        release.notify_one();
        assert!(old.await.unwrap().is_err(), "old refresh must not overwrite the newer commit");
        assert!(!idx.index_context(ctx, &new).await.unwrap());
        idx.search("query about the latest text", 1).await.unwrap();
        assert_eq!(*purposes.lock().unwrap(), vec![EmbeddingPurpose::Document, EmbeddingPurpose::Document, EmbeddingPurpose::Query]);
    }

    #[tokio::test]
    async fn profile_change_discards_synthesis_without_index_entries() {
        let dir = TempDir::new().unwrap();
        let ctx = ContextId::new();
        {
            let idx = SemanticIndex::new(test_config(dir.path()), Box::new(MockEmbedder { dims: 32 })).unwrap();
            idx.store_synthesis(ctx, synthesis::SynthesisResult {
                content_hash: "old-profile".into(), gist: Some("old gist".into()),
                keywords: vec![], top_blocks: vec![],
            }).unwrap();
        }
        let idx = SemanticIndex::new(test_config(dir.path()), Box::new(NamedMockEmbedder {
            inner: MockEmbedder { dims: 32 }, name: "changed-model".into(),
        })).unwrap();
        assert!(idx.synthesis_cache().get_any(ctx).is_none());
    }

    #[tokio::test]
    async fn cluster_label_picks_highest_summed_keyword() {
        // "rust" totals 0.6+0.5=1.1 across two members; "async" only 0.9; "gpu" 0.4.
        let m1 = kw(&[("rust", 0.6), ("gpu", 0.4)]);
        let m2 = kw(&[("rust", 0.5), ("async", 0.9)]);
        let label = pick_cluster_label([m1.as_slice(), m2.as_slice()]);
        assert_eq!(label.as_deref(), Some("rust"));
    }

    #[tokio::test]
    async fn cluster_label_breaks_score_ties_alphabetically() {
        // Both terms total 1.0; the alphabetically smaller ("alpha") wins,
        // regardless of member order.
        let m1 = kw(&[("zeta", 1.0)]);
        let m2 = kw(&[("alpha", 1.0)]);
        assert_eq!(
            pick_cluster_label([m1.as_slice(), m2.as_slice()]).as_deref(),
            Some("alpha")
        );
        assert_eq!(
            pick_cluster_label([m2.as_slice(), m1.as_slice()]).as_deref(),
            Some("alpha")
        );
    }

    #[tokio::test]
    async fn cluster_label_none_when_no_keywords() {
        let empty: Vec<Vec<(String, f32)>> = vec![vec![], vec![]];
        assert_eq!(
            pick_cluster_label(empty.iter().map(|v| v.as_slice())),
            None
        );
    }

    /// Deterministic mock embedder for testing.
    ///
    /// Produces L2-normalized vectors by hashing text bytes into components.
    struct MockEmbedder {
        dims: usize,
    }

    #[async_trait::async_trait]
    impl Embedder for MockEmbedder {
        fn model_name(&self) -> &str {
            "mock"
        }

        fn revision(&self) -> &str { "mock-v1" }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn embed_batch(&self, texts: &[&str], purpose: EmbeddingPurpose) -> Result<Vec<Vec<f32>>, IndexError> {
            let mut vectors = Vec::new();
            for text in texts { vectors.push(self.embed(text, purpose).await?); }
            Ok(vectors)
        }

        async fn embed(&self, text: &str, _purpose: EmbeddingPurpose) -> Result<Vec<f32>, IndexError> {
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

    /// `MockEmbedder` with a caller-chosen model identity.
    ///
    /// `MockEmbedder` itself hardcodes `model_name() = "mock"`, which is fine
    /// for every test except the model-mismatch guard, which needs to
    /// present a *different* name (or dims) on reopen. Delegates embedding
    /// to an inner `MockEmbedder` so the hash-based vectors stay identical —
    /// only the reported identity differs.
    struct NamedMockEmbedder {
        inner: MockEmbedder,
        name: String,
    }

    #[async_trait::async_trait]
    impl Embedder for NamedMockEmbedder {
        fn model_name(&self) -> &str {
            &self.name
        }
        fn revision(&self) -> &str { "mock-v1" }

        fn dimensions(&self) -> usize {
            self.inner.dimensions()
        }
        async fn embed_batch(&self, texts: &[&str], purpose: EmbeddingPurpose) -> Result<Vec<Vec<f32>>, IndexError> {
            self.inner.embed_batch(texts, purpose).await
        }
        async fn embed(&self, text: &str, _purpose: EmbeddingPurpose) -> Result<Vec<f32>, IndexError> {
            self.inner.embed(text, _purpose).await
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

    #[async_trait::async_trait]
    impl Embedder for KeyedEmbedder {
        fn model_name(&self) -> &str {
            "keyed"
        }
        fn revision(&self) -> &str { "mock-v1" }

        fn dimensions(&self) -> usize {
            self.dims
        }
        async fn embed_batch(&self, texts: &[&str], purpose: EmbeddingPurpose) -> Result<Vec<Vec<f32>>, IndexError> {
            let mut vectors = Vec::new();
            for text in texts { vectors.push(self.embed(text, purpose).await?); }
            Ok(vectors)
        }
        async fn embed(&self, text: &str, _purpose: EmbeddingPurpose) -> Result<Vec<f32>, IndexError> {
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
            dimensions: 32,
            data_dir: dir.to_path_buf(),
            hnsw_max_nb_connection: 8,
            hnsw_ef_construction: 50,
            max_context_bytes: 512,
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
    async fn seed_filler(idx: &SemanticIndex, n: usize) {
        for i in 0..n {
            let ctx = ContextId::new();
            let filler = format!("FILLER context number {i}");
            idx.index_context(ctx, &make_blocks(ctx, &filler)).await.unwrap();
        }
    }

    #[tokio::test]
    async fn test_index_and_search_round_trip() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        let ctx = ContextId::new();
        let blocks = make_blocks(ctx, "the quick brown fox jumps over the lazy dog");

        let indexed = idx.index_context(ctx, &blocks).await.unwrap();
        assert!(indexed, "first indexing should embed");

        let results = idx.search("quick brown fox", 5).await.unwrap();
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

    #[tokio::test]
    async fn test_dedup_same_content() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        let ctx = ContextId::new();
        let blocks = make_blocks(ctx, "identical content for dedup test");

        let first = idx.index_context(ctx, &blocks).await.unwrap();
        assert!(first, "first call should index");

        let second = idx.index_context(ctx, &blocks).await.unwrap();
        assert!(!second, "second call with same content should skip");
    }

    #[tokio::test]
    async fn test_neighbors() {
        // This test covers the `neighbors()` API — metadata lookup, self-
        // exclusion, score clamping. It does NOT assert on HNSW approximate-
        // nearest-neighbor ordering: hnsw_rs's reverse_update writes reverse
        // edges at the neighbour's own level (not the current search layer),
        // so points inserted after a random-higher-layer point may not appear
        // in its layer-0 neighbour list. Semantic ordering quality belongs in
        // integration tests with a real embedding service + a realistic corpus.
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let idx = SemanticIndex::new(config, Box::new(KeyedEmbedder { dims: 32 })).unwrap();
        seed_filler(&idx, 30).await;

        let ctx1 = ContextId::new();
        let ctx2 = ContextId::new();
        idx.index_context(ctx1, &make_blocks(ctx1, "PAIR alpha content")).await
            .unwrap();
        idx.index_context(ctx2, &make_blocks(ctx2, "PAIR beta content")).await
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

    #[tokio::test]
    async fn test_persistence_round_trip() {
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
            seed_filler(&idx, 30).await;
            idx.index_context(ctx1, &make_blocks(ctx1, "persistence test alpha")).await
                .unwrap();
            idx.index_context(ctx2, &make_blocks(ctx2, "persistence test beta")).await
                .unwrap();
            idx.save().unwrap();
        }

        // Reload and verify
        {
            let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

            let results = idx.search("persistence test", 5).await.unwrap();
            assert!(results.len() >= 2, "should find both contexts after reload");

            let neighbors = idx.neighbors(ctx1, 5).unwrap();
            assert!(!neighbors.is_empty(), "neighbors should work after reload");
        }
    }

    /// Regression: `search()` used to hold the hnsw read guard while acquiring
    /// the metadata lock, while `index_context()` acquires metadata then the
    /// hnsw write lock — an ABBA deadlock under concurrency. One indexer plus
    /// two searchers hammering the same index trips the inversion within a few
    /// iterations; the channel timeout converts a hang into a test failure.
    #[tokio::test]
    async fn test_concurrent_search_and_index_no_deadlock() {
        use std::time::Duration;

        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let idx =
            Arc::new(SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap());
        seed_filler(&idx, 10).await;

        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let mut handles = Vec::new();

        // Indexer thread: metadata → hnsw.write
        {
            let idx = idx.clone();
            let done = done_tx.clone();
            handles.push(std::thread::spawn(move || tokio::runtime::Runtime::new().unwrap().block_on(async move {
                for i in 0..200 {
                    let ctx = ContextId::new();
                    let blocks = make_blocks(ctx, &format!("stress indexer content {i}"));
                    idx.index_context(ctx, &blocks).await.unwrap();
                }
                let _ = done.send(());
            })));
        }

        // Searcher threads: hnsw.read → metadata (the inverted order pre-fix)
        for t in 0..2 {
            let idx = idx.clone();
            let done = done_tx.clone();
            handles.push(std::thread::spawn(move || tokio::runtime::Runtime::new().unwrap().block_on(async move {
                for i in 0..200 {
                    idx.search(&format!("stress query {t} {i}"), 3).await.unwrap();
                }
                let _ = done.send(());
            })));
        }
        drop(done_tx);

        for _ in 0..3 {
            done_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("deadlock: a worker thread did not finish within 30s");
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[tokio::test]
    async fn test_empty_index_search() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        let results = idx.search("anything", 5).await.unwrap();
        assert!(
            results.is_empty(),
            "empty index should return empty results"
        );
    }

    #[tokio::test]
    async fn test_max_contexts_eviction() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config(dir.path());
        config.max_contexts = Some(2);
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        let ctx1 = ContextId::new();
        let ctx2 = ContextId::new();
        let ctx3 = ContextId::new();

        idx.index_context(ctx1, &make_blocks(ctx1, "alpha context first")).await
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        idx.index_context(ctx2, &make_blocks(ctx2, "beta context second")).await
            .unwrap();
        assert_eq!(idx.len(), 2);

        // Indexing a third should evict the oldest (ctx1)
        std::thread::sleep(std::time::Duration::from_millis(10));
        idx.index_context(ctx3, &make_blocks(ctx3, "gamma context third")).await
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

    #[tokio::test]
    async fn test_empty_content_not_indexed() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        let ctx = ContextId::new();
        let indexed = idx.index_context(ctx, &[]).await.unwrap();
        assert!(!indexed, "empty blocks should not be indexed");
        assert!(idx.is_empty());
    }

    #[tokio::test]
    async fn test_eviction_clears_embeddings_cache() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config(dir.path());
        config.max_contexts = Some(2);
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        let ctx1 = ContextId::new();
        let ctx2 = ContextId::new();
        let ctx3 = ContextId::new();

        idx.index_context(ctx1, &make_blocks(ctx1, "alpha context first")).await
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        idx.index_context(ctx2, &make_blocks(ctx2, "beta context second")).await
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        // Evicts ctx1; no rebuild has run yet, so the graph point for ctx1
        // is still physically present — only the cache entry should be gone.
        idx.index_context(ctx3, &make_blocks(ctx3, "gamma context third")).await
            .unwrap();
        assert_eq!(idx.len(), 2);

        let hnsw = idx.hnsw.read().unwrap();
        let all = hnsw.get_all_embeddings().unwrap();
        assert_eq!(
            all.len(),
            2,
            "embeddings cache should reflect only live entries after eviction, before rebuild"
        );
    }

    #[tokio::test]
    async fn test_rebuild_reclaims_evicted_slots() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config(dir.path());
        config.max_contexts = Some(2);
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        let ctx1 = ContextId::new();
        let ctx2 = ContextId::new();
        let ctx3 = ContextId::new();

        idx.index_context(ctx1, &make_blocks(ctx1, "alpha context first")).await
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        idx.index_context(ctx2, &make_blocks(ctx2, "beta context second")).await
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        // Indexing a third evicts ctx1 (oldest) down to max_contexts = 2.
        idx.index_context(ctx3, &make_blocks(ctx3, "gamma context third")).await
            .unwrap();
        assert_eq!(idx.len(), 2);

        // Pre-rebuild: the evicted slot is still dead weight in the graph.
        let graph_count_before = idx.hnsw.read().unwrap().graph_point_count();
        assert_eq!(graph_count_before, 3);

        let stats = idx.rebuild().unwrap();
        assert_eq!(stats.kept, 2, "kept must equal live metadata count");
        assert_eq!(
            stats.dropped_dead, 1,
            "the one evicted slot should be dropped"
        );
        assert_eq!(stats.repaired_orphan_rows, 0);

        let graph_count_after = idx.hnsw.read().unwrap().graph_point_count();
        assert_eq!(
            graph_count_after,
            idx.len(),
            "graph must match metadata after rebuild"
        );

        let results = idx.search("context", 10).await.unwrap();
        let ids: Vec<ContextId> = results.iter().map(|r| r.context_id).collect();
        assert!(
            !ids.contains(&ctx1),
            "evicted ctx1 must not appear in search results"
        );
        assert!(
            ids.contains(&ctx2) || ids.contains(&ctx3),
            "a live context should still be findable"
        );
    }

    #[tokio::test]
    async fn test_rebuild_repairs_orphan_metadata_row() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        // Simulate a crash between assign_slot and the HNSW insert: a
        // metadata row exists with no matching graph point behind it.
        let orphan_ctx = ContextId::new();
        {
            let mut meta = idx.metadata.lock().unwrap();
            meta.assign_slot(orphan_ctx, "orphan-hash", "mock", 32)
                .unwrap();
        }
        assert_eq!(idx.len(), 1);

        let stats = idx.rebuild().unwrap();
        assert_eq!(stats.repaired_orphan_rows, 1);
        assert_eq!(stats.kept, 0);

        assert_eq!(idx.len(), 0, "orphan row should be removed from metadata");
    }

    #[tokio::test]
    async fn test_startup_auto_rebuild_reclaims_dead_slots() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config(dir.path());
        // Generous enough that seeding fillers doesn't evict any of them, but
        // tight enough that indexing one more context forces an eviction —
        // leaves plenty of live points behind for post-rebuild search
        // connectivity (see seed_filler's doc comment on tiny-graph flakiness).
        config.max_contexts = Some(31);

        let ctx_live = ContextId::new();

        {
            let idx =
                SemanticIndex::new(config.clone(), Box::new(MockEmbedder { dims: 32 })).unwrap();
            seed_filler(&idx, 30).await;
            idx.index_context(ctx_live, &make_blocks(ctx_live, "persistent live context")).await
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
            // Pushes count to 32 > 31, evicting the oldest filler — leaves a
            // dead slot in the graph that only a rebuild reclaims.
            let extra_ctx = ContextId::new();
            idx.index_context(
                extra_ctx,
                &make_blocks(extra_ctx, "one more filler to force eviction"),
            ).await
            .unwrap();

            let meta_count = idx.len();
            let graph_count = idx.hnsw.read().unwrap().graph_point_count();
            assert!(
                graph_count > meta_count,
                "pre-save: graph should carry a dead slot from eviction"
            );

            idx.save().unwrap();
        }

        // Reopen with the same config — auto-rebuild should run because the
        // saved graph has more points than metadata rows.
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        let meta_count = idx.len();
        let graph_count = idx.hnsw.read().unwrap().graph_point_count();
        assert_eq!(
            graph_count, meta_count,
            "auto-rebuild on startup should have reclaimed the dead slot"
        );

        let results = idx.search("persistent live context", 5).await.unwrap();
        assert!(
            !results.is_empty(),
            "search should still work after auto-rebuild"
        );
    }

    #[tokio::test]
    async fn test_model_mismatch_wipes_index() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());

        {
            let idx =
                SemanticIndex::new(config.clone(), Box::new(MockEmbedder { dims: 32 })).unwrap();
            // See seed_filler's doc comment: enough points to guarantee
            // layer-0 connectivity so post-wipe search/index checks are stable.
            seed_filler(&idx, 30).await;
            let ctx = ContextId::new();
            idx.index_context(ctx, &make_blocks(ctx, "model mismatch test alpha")).await
                .unwrap();
            idx.save().unwrap();
            assert_eq!(idx.len(), 31);
        }

        // Reopen with a different model NAME, same dimensions.
        let idx = SemanticIndex::new(
            config,
            Box::new(NamedMockEmbedder {
                inner: MockEmbedder { dims: 32 },
                name: "other-model".to_string(),
            }),
        )
        .unwrap();

        assert_eq!(idx.len(), 0, "model name mismatch must wipe the index");
        let results = idx.search("model mismatch test alpha", 5).await.unwrap();
        assert!(results.is_empty(), "wiped index should return no results");

        // The index must still be usable after the wipe.
        let ctx2 = ContextId::new();
        idx.index_context(ctx2, &make_blocks(ctx2, "fresh content after wipe")).await
            .unwrap();
        assert_eq!(idx.len(), 1);
        let results2 = idx.search("fresh content after wipe", 5).await.unwrap();
        assert!(!results2.is_empty(), "index should work after the wipe");
    }

    #[tokio::test]
    async fn test_dimensions_mismatch_wipes_index() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());

        {
            let idx =
                SemanticIndex::new(config.clone(), Box::new(MockEmbedder { dims: 32 })).unwrap();
            seed_filler(&idx, 30).await;
            idx.save().unwrap();
            assert_eq!(idx.len(), 30);
        }

        // Reopen with the same model NAME but different dimensions — HNSW
        // vectors are dimension-bound, so this alone must trip the guard.
        let mut mismatched = test_config(dir.path());
        mismatched.dimensions = 16;
        let idx = SemanticIndex::new(mismatched, Box::new(MockEmbedder { dims: 16 })).unwrap();

        assert_eq!(idx.len(), 0, "dimension mismatch must wipe the index");
        let results = idx.search("anything", 5).await.unwrap();
        assert!(results.is_empty(), "wiped index should return no results");

        let ctx = ContextId::new();
        idx.index_context(ctx, &make_blocks(ctx, "content in new dims")).await
            .unwrap();
        assert_eq!(idx.len(), 1, "index should work after the wipe");
    }

    #[tokio::test]
    async fn test_matching_model_preserves_index() {
        // Mirrors test_persistence_round_trip: identical model name + dims
        // on reopen must NOT trigger the mismatch guard.
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let ctx1 = ContextId::new();

        {
            let idx =
                SemanticIndex::new(config.clone(), Box::new(MockEmbedder { dims: 32 })).unwrap();
            seed_filler(&idx, 30).await;
            idx.index_context(ctx1, &make_blocks(ctx1, "matching model preserved")).await
                .unwrap();
            idx.save().unwrap();
            assert_eq!(idx.len(), 31);
        }

        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();
        assert_eq!(
            idx.len(),
            31,
            "identical model name + dimensions must preserve the index"
        );

        let results = idx.search("matching model preserved", 5).await.unwrap();
        assert!(
            !results.is_empty(),
            "search should still find indexed content after reopen"
        );
    }

    #[tokio::test]
    async fn test_mismatch_wipe_removes_pending_dump() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());

        {
            let idx =
                SemanticIndex::new(config.clone(), Box::new(MockEmbedder { dims: 32 })).unwrap();
            seed_filler(&idx, 30).await;
            idx.save().unwrap();
        }

        // Simulate a completed-but-unrecovered atomic dump left behind by a
        // prior process (see HnswIndex::dump_atomic / recover_atomic_dump):
        // copies of the real files under the "index.new.*" names, plus the
        // ready marker that says the dump finished.
        std::fs::copy(
            dir.path().join("index.hnsw.graph"),
            dir.path().join("index.new.hnsw.graph"),
        )
        .unwrap();
        std::fs::copy(
            dir.path().join("index.hnsw.data"),
            dir.path().join("index.new.hnsw.data"),
        )
        .unwrap();
        std::fs::write(dir.path().join("index.new.ready"), b"").unwrap();

        // Reopen with a mismatched model — the wipe must run BEFORE
        // HnswIndex::new's recover_atomic_dump, so the old-model dump is
        // deleted outright rather than resurrected by recovery.
        let idx = SemanticIndex::new(
            config,
            Box::new(NamedMockEmbedder {
                inner: MockEmbedder { dims: 32 },
                name: "different-model".to_string(),
            }),
        )
        .unwrap();

        // These stay gone: nothing in the fresh-construction path that
        // follows the wipe recreates them until the caller indexes+saves
        // (below). `index_meta.db` itself is excluded from this check —
        // `MetadataStore::open` always creates a fresh one as part of
        // normal construction, mismatch or not; its emptiness is what
        // `idx.len() == 0` below actually verifies.
        for name in [
            "index.hnsw.graph",
            "index.hnsw.data",
            "index.new.hnsw.graph",
            "index.new.hnsw.data",
            "index.new.ready",
        ] {
            assert!(
                !dir.path().join(name).exists(),
                "{name} should be removed by the mismatch wipe, not resurrected by recovery"
            );
        }

        assert_eq!(idx.len(), 0, "old-model metadata rows must be gone");
        let ctx = ContextId::new();
        idx.index_context(ctx, &make_blocks(ctx, "works after dump cleanup")).await
            .unwrap();
        assert_eq!(idx.len(), 1, "index should work after the wipe");
    }

    /// Eviction must clear the in-memory synthesis cache alongside the SQLite
    /// rows — otherwise get_any() serves an evicted context's gist/keywords
    /// until the next restart (deepseek review finding, 2026-07-12).
    #[tokio::test]
    async fn test_eviction_clears_synthesis_cache_in_memory() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config(dir.path());
        config.max_contexts = Some(2);
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        let ctx1 = ContextId::new();
        let ctx2 = ContextId::new();
        let ctx3 = ContextId::new();

        idx.index_context(ctx1, &make_blocks(ctx1, "alpha context first")).await
            .unwrap();
        idx.store_synthesis(
            ctx1,
            synthesis::SynthesisResult {
                keywords: vec![("alpha".to_string(), 0.5)],
                top_blocks: vec![],
                gist: Some("alpha gist".to_string()),
                content_hash: "h1".to_string(),
            },
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        idx.index_context(ctx2, &make_blocks(ctx2, "beta context second")).await
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        // Evicts ctx1 (oldest).
        idx.index_context(ctx3, &make_blocks(ctx3, "gamma context third")).await
            .unwrap();
        assert_eq!(idx.len(), 2);

        assert!(
            idx.synthesis_cache().get_any(ctx1).is_none(),
            "evicted ctx1's synthesis must leave the memory cache immediately, not at restart"
        );
    }

    #[tokio::test]
    async fn test_synthesis_survives_reopen() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let ctx = ContextId::new();

        {
            let idx =
                SemanticIndex::new(config.clone(), Box::new(MockEmbedder { dims: 32 })).unwrap();
            let result = synthesis::SynthesisResult {
                keywords: vec![("rust".to_string(), 0.9), ("async".to_string(), 0.7)],
                top_blocks: vec![("blk1".to_string(), 0.95, "preview text".to_string())],
                gist: Some("a representative sentence".to_string()),
                content_hash: "hash-xyz".to_string(),
            };
            idx.store_synthesis(ctx, result).unwrap();
        }

        // Reopen the same dir with the same model — no re-synthesis happens
        // (no index_context/embed call between construction and the check
        // below), so a non-empty result here can only have come from hydration.
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();
        let cached = idx
            .synthesis_cache()
            .get_any(ctx)
            .expect("synthesis should survive reopen via hydration");
        assert_eq!(cached.content_hash, "hash-xyz");
        assert_eq!(cached.gist.as_deref(), Some("a representative sentence"));
        assert_eq!(
            cached.keywords,
            vec![("rust".to_string(), 0.9), ("async".to_string(), 0.7)]
        );
        assert_eq!(
            cached.top_blocks,
            vec![("blk1".to_string(), 0.95, "preview text".to_string())]
        );
    }

    #[tokio::test]
    async fn test_synthesis_hash_invalidation_still_works() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let ctx = ContextId::new();

        {
            let idx =
                SemanticIndex::new(config.clone(), Box::new(MockEmbedder { dims: 32 })).unwrap();
            let result = synthesis::SynthesisResult {
                keywords: vec![],
                top_blocks: vec![],
                gist: None,
                content_hash: "hash-abc".to_string(),
            };
            idx.store_synthesis(ctx, result).unwrap();
        }

        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();
        // Existing SynthesisCache::get semantics, now exercised over hydrated
        // (not freshly-inserted) data: a hash mismatch is a miss, get_any isn't.
        assert!(
            idx.synthesis_cache().get(ctx, Some("different-hash")).is_none(),
            "hash mismatch must still be a cache miss after hydration"
        );
        assert!(idx.synthesis_cache().get_any(ctx).is_some());
    }

    #[tokio::test]
    async fn test_eviction_removes_persisted_synthesis() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config(dir.path());
        config.max_contexts = Some(2);

        let ctx1 = ContextId::new();
        let ctx2 = ContextId::new();
        let ctx3 = ContextId::new();

        let synth_for = |tag: &str| synthesis::SynthesisResult {
            keywords: vec![(tag.to_string(), 0.5)],
            top_blocks: vec![],
            gist: Some(format!("{tag} gist")),
            content_hash: tag.to_string(),
        };

        {
            let idx =
                SemanticIndex::new(config.clone(), Box::new(MockEmbedder { dims: 32 })).unwrap();

            idx.index_context(ctx1, &make_blocks(ctx1, "alpha context first")).await
                .unwrap();
            idx.store_synthesis(ctx1, synth_for("alpha")).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));

            idx.index_context(ctx2, &make_blocks(ctx2, "beta context second")).await
                .unwrap();
            idx.store_synthesis(ctx2, synth_for("beta")).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));

            // Indexing a third context evicts ctx1 (oldest) down to
            // max_contexts = 2 — evict_oldest must also drop ctx1's synthesis.
            idx.index_context(ctx3, &make_blocks(ctx3, "gamma context third")).await
                .unwrap();
            idx.store_synthesis(ctx3, synth_for("gamma")).unwrap();

            assert_eq!(idx.len(), 2);
        }

        // Reopen: hydration must only pick up the survivors.
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();
        assert!(
            idx.synthesis_cache().get_any(ctx1).is_none(),
            "evicted ctx1's synthesis must not survive reopen"
        );
        assert!(idx.synthesis_cache().get_any(ctx2).is_some());
        assert!(idx.synthesis_cache().get_any(ctx3).is_some());
    }
}
