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
/// Writers (`index_context`) take `metadata` then `hnsw` — the metadata lock
/// is deliberately held across embed + insert so concurrent callers can't
/// embed the same context twice. Everyone else must **never hold `hnsw` while
/// acquiring `metadata`**: collect what you need from the graph, drop the
/// guard, then touch metadata (`neighbors`/`clusters` show the pattern).
/// Holding both in the opposite order inverts the writer order and deadlocks.
///
/// Lock poisoning is deliberately fatal: a panic while holding either lock
/// (OOM mid-embed, hnsw_rs panic) poisons it, and every later `.unwrap()`
/// propagates the panic instead of serving state of unknown integrity —
/// crash over corruption. The kernel treats a failed SemanticIndex as
/// "no index" and degrades gracefully; a restart rebuilds from disk.
pub struct SemanticIndex {
    embedder: Arc<dyn Embedder>,
    hnsw: RwLock<index::HnswIndex>,
    metadata: Mutex<metadata::MetadataStore>,
    config: IndexConfig,
    synthesis_cache: synthesis::SynthesisCache,
}

impl SemanticIndex {
    /// Create or load a semantic index.
    ///
    /// Before any other on-disk state is touched, this compares the
    /// embedder's `(model_name(), dimensions)` against every distinct pair
    /// recorded in `index_meta.db` (if one exists — a fresh data_dir is
    /// never mismatched). Vectors from two different embedding models live
    /// in incomparable spaces; mixing them into one HNSW graph produces
    /// meaningless similarity scores with no error. On any mismatch, the
    /// on-disk index (HNSW graph, SQLite metadata, and any in-flight
    /// atomic-dump files — a completed old-model dump must not be
    /// resurrected by `HnswIndex::new`'s crash recovery) is wiped and
    /// construction proceeds as a fresh, empty index. The index is a
    /// derived cache: contexts re-index lazily via the watcher / `kj synth
    /// all`, so this is never data loss.
    ///
    /// `model_name` derives from the model directory's basename (see
    /// `OnnxEmbedder::new`), so renaming the model directory — even with
    /// identical files inside — also triggers a wipe. That's correct: the
    /// config changed, and the cache follows.
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
        Self::wipe_on_model_mismatch(&config, embedder.model_name())?;

        let hnsw = index::HnswIndex::new(&config)?;
        let metadata = metadata::MetadataStore::open(&config.data_dir)?;

        let this = Self {
            embedder: Arc::from(embedder),
            hnsw: RwLock::new(hnsw),
            metadata: Mutex::new(metadata),
            config,
            synthesis_cache: synthesis::SynthesisCache::new(),
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

    /// Wipe the on-disk index if it was built by a different embedding model
    /// (name or dimensions) than the one about to open it.
    ///
    /// No-op when `index_meta.db` doesn't exist yet — a fresh data_dir can't
    /// be mismatched. Opens a throwaway `MetadataStore` to read the recorded
    /// `(model_name, dimensions)` pairs and drops it *before* deleting any
    /// files: the SQLite connection must close first, or the delete of
    /// `index_meta.db` (and the WAL/SHM opened alongside it) would fight an
    /// open handle.
    fn wipe_on_model_mismatch(config: &IndexConfig, model_name: &str) -> Result<(), IndexError> {
        let meta_path = config.data_dir.join("index_meta.db");
        if !meta_path.exists() {
            return Ok(());
        }

        let mismatch = {
            let store = metadata::MetadataStore::open(&config.data_dir)?;
            store
                .distinct_models()?
                .into_iter()
                .find(|(name, dims)| name != model_name || *dims != config.dimensions)
            // `store` (and its SQLite connection) drops here, at the end of
            // this block — before any file deletion below.
        };

        let Some((old_name, old_dims)) = mismatch else {
            return Ok(());
        };

        tracing::warn!(
            old_model = %old_name,
            old_dimensions = old_dims,
            new_model = %model_name,
            new_dimensions = config.dimensions,
            "semantic index model mismatch — wiping on-disk index; \
             it will re-populate lazily from the watcher / `kj synth all`"
        );

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
                    let mut hnsw = self.hnsw.write().unwrap();
                    for (slot, _) in &evicted {
                        hnsw.clear_slot(*slot);
                    }
                    drop(hnsw);
                    for (_, ctx) in &evicted {
                        self.synthesis_cache.remove(*ctx);
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
    /// Blocking — call from `spawn_blocking`.
    pub fn search(&self, query: &str, k: usize) -> Result<Vec<SearchResult>, IndexError> {
        let embedding = self.embedder.embed(query)?;

        // Lock order: drop the hnsw guard before taking metadata (see struct docs).
        let neighbors = {
            let hnsw = self.hnsw.read().unwrap();
            hnsw.search(&embedding, k)?
        };

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

    #[test]
    fn cluster_label_picks_highest_summed_keyword() {
        // "rust" totals 0.6+0.5=1.1 across two members; "async" only 0.9; "gpu" 0.4.
        let m1 = kw(&[("rust", 0.6), ("gpu", 0.4)]);
        let m2 = kw(&[("rust", 0.5), ("async", 0.9)]);
        let label = pick_cluster_label([m1.as_slice(), m2.as_slice()]);
        assert_eq!(label.as_deref(), Some("rust"));
    }

    #[test]
    fn cluster_label_breaks_score_ties_alphabetically() {
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

    #[test]
    fn cluster_label_none_when_no_keywords() {
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

    impl Embedder for NamedMockEmbedder {
        fn model_name(&self) -> &str {
            &self.name
        }
        fn dimensions(&self) -> usize {
            self.inner.dimensions()
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, IndexError> {
            self.inner.embed_batch(texts)
        }
        fn embed(&self, text: &str) -> Result<Vec<f32>, IndexError> {
            self.inner.embed(text)
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

    /// Regression: `search()` used to hold the hnsw read guard while acquiring
    /// the metadata lock, while `index_context()` acquires metadata then the
    /// hnsw write lock — an ABBA deadlock under concurrency. One indexer plus
    /// two searchers hammering the same index trips the inversion within a few
    /// iterations; the channel timeout converts a hang into a test failure.
    #[test]
    fn test_concurrent_search_and_index_no_deadlock() {
        use std::time::Duration;

        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let idx =
            Arc::new(SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap());
        seed_filler(&idx, 10);

        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let mut handles = Vec::new();

        // Indexer thread: metadata → hnsw.write
        {
            let idx = idx.clone();
            let done = done_tx.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..200 {
                    let ctx = ContextId::new();
                    let blocks = make_blocks(ctx, &format!("stress indexer content {i}"));
                    idx.index_context(ctx, &blocks).unwrap();
                }
                let _ = done.send(());
            }));
        }

        // Searcher threads: hnsw.read → metadata (the inverted order pre-fix)
        for t in 0..2 {
            let idx = idx.clone();
            let done = done_tx.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..200 {
                    idx.search(&format!("stress query {t} {i}"), 3).unwrap();
                }
                let _ = done.send(());
            }));
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

    #[test]
    fn test_eviction_clears_embeddings_cache() {
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
        std::thread::sleep(std::time::Duration::from_millis(10));
        // Evicts ctx1; no rebuild has run yet, so the graph point for ctx1
        // is still physically present — only the cache entry should be gone.
        idx.index_context(ctx3, &make_blocks(ctx3, "gamma context third"))
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

    #[test]
    fn test_rebuild_reclaims_evicted_slots() {
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
        std::thread::sleep(std::time::Duration::from_millis(10));
        // Indexing a third evicts ctx1 (oldest) down to max_contexts = 2.
        idx.index_context(ctx3, &make_blocks(ctx3, "gamma context third"))
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

        let results = idx.search("context", 10).unwrap();
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

    #[test]
    fn test_rebuild_repairs_orphan_metadata_row() {
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

    #[test]
    fn test_startup_auto_rebuild_reclaims_dead_slots() {
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
            seed_filler(&idx, 30);
            idx.index_context(ctx_live, &make_blocks(ctx_live, "persistent live context"))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
            // Pushes count to 32 > 31, evicting the oldest filler — leaves a
            // dead slot in the graph that only a rebuild reclaims.
            let extra_ctx = ContextId::new();
            idx.index_context(
                extra_ctx,
                &make_blocks(extra_ctx, "one more filler to force eviction"),
            )
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

        let results = idx.search("persistent live context", 5).unwrap();
        assert!(
            !results.is_empty(),
            "search should still work after auto-rebuild"
        );
    }

    #[test]
    fn test_model_mismatch_wipes_index() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());

        {
            let idx =
                SemanticIndex::new(config.clone(), Box::new(MockEmbedder { dims: 32 })).unwrap();
            // See seed_filler's doc comment: enough points to guarantee
            // layer-0 connectivity so post-wipe search/index checks are stable.
            seed_filler(&idx, 30);
            let ctx = ContextId::new();
            idx.index_context(ctx, &make_blocks(ctx, "model mismatch test alpha"))
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
        let results = idx.search("model mismatch test alpha", 5).unwrap();
        assert!(results.is_empty(), "wiped index should return no results");

        // The index must still be usable after the wipe.
        let ctx2 = ContextId::new();
        idx.index_context(ctx2, &make_blocks(ctx2, "fresh content after wipe"))
            .unwrap();
        assert_eq!(idx.len(), 1);
        let results2 = idx.search("fresh content after wipe", 5).unwrap();
        assert!(!results2.is_empty(), "index should work after the wipe");
    }

    #[test]
    fn test_dimensions_mismatch_wipes_index() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());

        {
            let idx =
                SemanticIndex::new(config.clone(), Box::new(MockEmbedder { dims: 32 })).unwrap();
            seed_filler(&idx, 30);
            idx.save().unwrap();
            assert_eq!(idx.len(), 30);
        }

        // Reopen with the same model NAME but different dimensions — HNSW
        // vectors are dimension-bound, so this alone must trip the guard.
        let mut mismatched = test_config(dir.path());
        mismatched.dimensions = 16;
        let idx = SemanticIndex::new(mismatched, Box::new(MockEmbedder { dims: 16 })).unwrap();

        assert_eq!(idx.len(), 0, "dimension mismatch must wipe the index");
        let results = idx.search("anything", 5).unwrap();
        assert!(results.is_empty(), "wiped index should return no results");

        let ctx = ContextId::new();
        idx.index_context(ctx, &make_blocks(ctx, "content in new dims"))
            .unwrap();
        assert_eq!(idx.len(), 1, "index should work after the wipe");
    }

    #[test]
    fn test_matching_model_preserves_index() {
        // Mirrors test_persistence_round_trip: identical model name + dims
        // on reopen must NOT trigger the mismatch guard.
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let ctx1 = ContextId::new();

        {
            let idx =
                SemanticIndex::new(config.clone(), Box::new(MockEmbedder { dims: 32 })).unwrap();
            seed_filler(&idx, 30);
            idx.index_context(ctx1, &make_blocks(ctx1, "matching model preserved"))
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

        let results = idx.search("matching model preserved", 5).unwrap();
        assert!(
            !results.is_empty(),
            "search should still find indexed content after reopen"
        );
    }

    #[test]
    fn test_mismatch_wipe_removes_pending_dump() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());

        {
            let idx =
                SemanticIndex::new(config.clone(), Box::new(MockEmbedder { dims: 32 })).unwrap();
            seed_filler(&idx, 30);
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
        idx.index_context(ctx, &make_blocks(ctx, "works after dump cleanup"))
            .unwrap();
        assert_eq!(idx.len(), 1, "index should work after the wipe");
    }

    /// Eviction must clear the in-memory synthesis cache alongside the SQLite
    /// rows — otherwise get_any() serves an evicted context's gist/keywords
    /// until the next restart (deepseek review finding, 2026-07-12).
    #[test]
    fn test_eviction_clears_synthesis_cache_in_memory() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config(dir.path());
        config.max_contexts = Some(2);
        let idx = SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap();

        let ctx1 = ContextId::new();
        let ctx2 = ContextId::new();
        let ctx3 = ContextId::new();

        idx.index_context(ctx1, &make_blocks(ctx1, "alpha context first"))
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
        idx.index_context(ctx2, &make_blocks(ctx2, "beta context second"))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        // Evicts ctx1 (oldest).
        idx.index_context(ctx3, &make_blocks(ctx3, "gamma context third"))
            .unwrap();
        assert_eq!(idx.len(), 2);

        assert!(
            idx.synthesis_cache().get_any(ctx1).is_none(),
            "evicted ctx1's synthesis must leave the memory cache immediately, not at restart"
        );
    }

    #[test]
    fn test_synthesis_survives_reopen() {
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

    #[test]
    fn test_synthesis_hash_invalidation_still_works() {
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

    #[test]
    fn test_eviction_removes_persisted_synthesis() {
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

            idx.index_context(ctx1, &make_blocks(ctx1, "alpha context first"))
                .unwrap();
            idx.store_synthesis(ctx1, synth_for("alpha")).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));

            idx.index_context(ctx2, &make_blocks(ctx2, "beta context second"))
                .unwrap();
            idx.store_synthesis(ctx2, synth_for("beta")).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));

            // Indexing a third context evicts ctx1 (oldest) down to
            // max_contexts = 2 — evict_oldest must also drop ctx1's synthesis.
            idx.index_context(ctx3, &make_blocks(ctx3, "gamma context third"))
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
