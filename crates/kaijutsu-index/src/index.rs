//! HNSW vector index wrapper.
//!
//! Wraps `hnsw_rs` with mmap persistence.

use std::path::PathBuf;

use anndists::dist::DistCosine;
use hnsw_rs::prelude::*;

use crate::{IndexConfig, IndexError};

/// HNSW nearest-neighbor index with disk persistence.
pub struct HnswIndex {
    hnsw: Hnsw<'static, f32, DistCosine>,
    data_dir: PathBuf,
    dims: usize,
    /// Track embeddings by slot for retrieval and clustering.
    embeddings: Vec<Option<Vec<f32>>>,
    /// Keep HnswIo alive (leaked Box) so loaded Hnsw can borrow from it.
    /// Only set when loaded from disk; None when freshly created.
    _hnsw_io: Option<&'static HnswIo>,
}

impl HnswIndex {
    /// Create or load an HNSW index.
    pub fn new(config: &IndexConfig) -> Result<Self, IndexError> {
        let data_dir = config.data_dir.clone();

        // Finish or discard any dump left mid-flight by a prior crash before
        // touching the real `index.hnsw.*` files.
        Self::recover_atomic_dump(&data_dir)?;

        // Check if saved index exists (file_dump creates {basename}.hnsw.{graph,data})
        let graph_path = data_dir.join("index.hnsw.graph");
        let data_path = data_dir.join("index.hnsw.data");

        if graph_path.exists() && data_path.exists() {
            // Box the HnswIo first without leaking — only leak after successful load.
            let hnsw_io_box = Box::new(HnswIo::new(&data_dir, "index"));

            // SAFETY: We need a &'static reference for load_hnsw_with_dist, which
            // returns an Hnsw that borrows from HnswIo. We create a raw pointer to
            // get the reference, then leak only on success. On failure we reclaim the Box.
            let hnsw_io_ptr: *const HnswIo = &*hnsw_io_box;
            let hnsw_io_ref: &'static HnswIo = unsafe { &*hnsw_io_ptr };

            match hnsw_io_ref.load_hnsw_with_dist(DistCosine) {
                Ok(loaded) => {
                    // Load succeeded — the Hnsw borrows from HnswIo, so leak it now.
                    let hnsw_io: &'static HnswIo = Box::leak(hnsw_io_box);

                    // Rebuild embeddings cache from HNSW's persisted vectors.
                    // hnsw_rs stores each point in exactly one layer (its randomly
                    // assigned level), not in every layer ≤ level. Iterating layer 0
                    // alone silently drops any point that landed at level > 0, which
                    // corrupts get_embedding()/get_all_embeddings() after reload.
                    // PointIndexation::into_iter walks every layer.
                    let mut embeddings: Vec<Option<Vec<f32>>> = Vec::new();
                    let pi = loaded.get_point_indexation();
                    for point in pi.into_iter() {
                        let slot = point.get_origin_id();
                        let v = point.get_v();
                        if slot >= embeddings.len() {
                            embeddings.resize(slot + 1, None);
                        }
                        embeddings[slot] = Some(v.to_vec());
                    }

                    let count = embeddings.iter().filter(|e| e.is_some()).count();
                    tracing::info!(
                        path = %data_dir.display(),
                        points = count,
                        "loaded HNSW index from disk"
                    );
                    return Ok(Self {
                        hnsw: loaded,
                        data_dir,
                        dims: config.dimensions,
                        embeddings,
                        _hnsw_io: Some(hnsw_io),
                    });
                }
                Err(e) => {
                    // Load failed — drop the Box normally, no leak.
                    drop(hnsw_io_box);
                    tracing::warn!(
                        error = %e,
                        "failed to load HNSW index, creating new"
                    );
                }
            }
        }

        Ok(Self {
            hnsw: Self::create_new(config),
            data_dir,
            dims: config.dimensions,
            embeddings: Vec::new(),
            _hnsw_io: None,
        })
    }

    fn create_new(config: &IndexConfig) -> Hnsw<'static, f32, DistCosine> {
        // Hnsw::new(max_nb_connection, max_elements, max_layer, ef_construction, dist)
        Hnsw::new(
            config.hnsw_max_nb_connection,
            10_000, // initial capacity, grows automatically
            16,     // max_layer
            config.hnsw_ef_construction,
            DistCosine {},
        )
    }

    /// Build a fresh graph from an explicit set of (slot, embedding) pairs.
    ///
    /// Used by `SemanticIndex::rebuild()`: rebuild never renumbers slots, so
    /// the caller passes the surviving (slot, embedding) pairs read out of the
    /// old graph and gets back a compacted graph that keeps those exact slot
    /// numbers — dead points (evicted, no longer in metadata) simply aren't
    /// re-inserted. The embeddings cache is built from `entries` alone, and
    /// `_hnsw_io` is `None`: a freshly built graph borrows from no `HnswIo`,
    /// so there's nothing to keep alive.
    pub fn from_entries(
        config: &IndexConfig,
        entries: &[(u32, Vec<f32>)],
    ) -> Result<HnswIndex, IndexError> {
        let hnsw = Self::create_new(config);
        let mut embeddings: Vec<Option<Vec<f32>>> = Vec::new();

        for (slot, embedding) in entries {
            if embedding.len() != config.dimensions {
                return Err(IndexError::Embedding(format!(
                    "dimension mismatch: expected {}, got {}",
                    config.dimensions,
                    embedding.len()
                )));
            }

            hnsw.insert((embedding.as_slice(), *slot as usize));

            let idx = *slot as usize;
            if idx >= embeddings.len() {
                embeddings.resize(idx + 1, None);
            }
            embeddings[idx] = Some(embedding.clone());
        }

        Ok(Self {
            hnsw,
            data_dir: config.data_dir.clone(),
            dims: config.dimensions,
            embeddings,
            _hnsw_io: None,
        })
    }

    /// Finish or discard an interrupted `dump_atomic` from a previous run.
    ///
    /// - Marker present: the dump completed (file_dump + fsync of both `.new`
    ///   files already happened) — finish the publish by renaming whichever
    ///   `index.new.hnsw.*` files still exist over the real names, then remove
    ///   the marker. Idempotent: whether we crashed before either rename,
    ///   between the two renames, or after both renames but before the marker
    ///   was removed, re-running this converges on the same end state (real
    ///   files present, `.new` files and marker gone).
    /// - Marker absent but `index.new.hnsw.*` files exist: an incomplete dump
    ///   (crashed before the marker was written) — discard them; the real
    ///   files were never touched and remain authoritative.
    fn recover_atomic_dump(data_dir: &std::path::Path) -> Result<(), IndexError> {
        let marker = data_dir.join("index.new.ready");
        let new_graph = data_dir.join("index.new.hnsw.graph");
        let new_data = data_dir.join("index.new.hnsw.data");
        let real_graph = data_dir.join("index.hnsw.graph");
        let real_data = data_dir.join("index.hnsw.data");

        if marker.exists() {
            if new_graph.exists() {
                std::fs::rename(&new_graph, &real_graph)?;
            }
            if new_data.exists() {
                std::fs::rename(&new_data, &real_data)?;
            }
            std::fs::remove_file(&marker)?;
            Self::sync_dir(data_dir);
            tracing::info!(
                path = %data_dir.display(),
                "recovered interrupted HNSW index dump"
            );
        } else {
            let mut cleaned = false;
            if new_graph.exists() {
                std::fs::remove_file(&new_graph)?;
                cleaned = true;
            }
            if new_data.exists() {
                std::fs::remove_file(&new_data)?;
                cleaned = true;
            }
            if cleaned {
                tracing::warn!(
                    path = %data_dir.display(),
                    "discarded incomplete HNSW index dump"
                );
            }
        }

        Ok(())
    }

    /// Insert or update an embedding at a given slot.
    pub fn insert(&mut self, slot: u32, embedding: &[f32]) -> Result<(), IndexError> {
        if embedding.len() != self.dims {
            return Err(IndexError::Embedding(format!(
                "dimension mismatch: expected {}, got {}",
                self.dims,
                embedding.len()
            )));
        }

        // hnsw_rs insert takes (&[T], DataId=usize)
        self.hnsw.insert((embedding, slot as usize));

        // Store embedding for retrieval
        let idx = slot as usize;
        if idx >= self.embeddings.len() {
            self.embeddings.resize(idx + 1, None);
        }
        self.embeddings[idx] = Some(embedding.to_vec());

        Ok(())
    }

    /// Search for k nearest neighbors. Returns (slot, distance) pairs.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>, IndexError> {
        let ef_search = k.max(16);
        let neighbors = self.hnsw.search(query, k, ef_search);

        Ok(neighbors
            .into_iter()
            .map(|n| (n.d_id as u32, n.distance))
            .collect())
    }

    /// Get the embedding for a given slot.
    pub fn get_embedding(&self, slot: u32) -> Result<Vec<f32>, IndexError> {
        let idx = slot as usize;
        self.embeddings
            .get(idx)
            .and_then(|e| e.clone())
            .ok_or_else(|| IndexError::Index(format!("no embedding at slot {}", slot)))
    }

    /// Get all stored embeddings as (slot, embedding) pairs.
    pub fn get_all_embeddings(&self) -> Result<Vec<(u32, Vec<f32>)>, IndexError> {
        let mut result = Vec::new();
        for (idx, emb) in self.embeddings.iter().enumerate() {
            if let Some(embedding) = emb {
                result.push((idx as u32, embedding.clone()));
            }
        }
        Ok(result)
    }

    /// Count points actually present in the HNSW graph.
    ///
    /// This is the source of truth for dead-slot detection — NOT the
    /// embeddings cache, which `clear_slot` can shrink out from under a graph
    /// point that hnsw_rs has no way to actually delete.
    ///
    /// Deliberately `Hnsw::get_nb_point()` (an atomically-maintained counter,
    /// correctly reconstructed on reload — see hnsw_rs's own
    /// `check_graph_equality` test) rather than
    /// `get_point_indexation().into_iter().count()`: that iterator's `next()`
    /// unconditionally unwraps the graph's entry point once it walks past an
    /// empty layer 0, and a freshly created graph (0 points, no entry point)
    /// has none — every `SemanticIndex::new()` on a brand-new index hit that
    /// panic via the startup auto-rebuild check. `get_nb_point()` doesn't
    /// iterate at all, so it can't trip over that.
    pub fn graph_point_count(&self) -> usize {
        self.hnsw.get_nb_point()
    }

    /// Drop the cached embedding for an evicted slot, if it's in range.
    ///
    /// The point itself stays in the HNSW graph until `rebuild()` runs —
    /// hnsw_rs has no delete. This only stops `get_embedding` /
    /// `get_all_embeddings` / `clusters` from continuing to serve a vector
    /// whose metadata row is already gone.
    pub fn clear_slot(&mut self, slot: u32) {
        if let Some(entry) = self.embeddings.get_mut(slot as usize) {
            *entry = None;
        }
    }

    /// Dump the graph to disk, publishing it atomically.
    ///
    /// Protocol (write-marker-rename): dump under a temp basename, fsync both
    /// files, write+fsync a marker, rename both temp files over the real
    /// names, then remove the marker. A crash before the marker exists leaves
    /// `index.hnsw.*` completely untouched; a crash after leaves recoverable
    /// state that `HnswIndex::new`'s startup recovery finishes or discards.
    /// This replaces the old dump-in-place `save()`, where a crash mid-write
    /// could leave both on-disk files corrupt with nothing to fall back to.
    ///
    /// An empty graph (0 points — e.g. a rebuild that dropped every slot) is
    /// a special case: hnsw_rs's own dump refuses to serialize a graph with
    /// no entry point (`PointIndexation::dump` errors "entry point not
    /// initialized" — there's no format for "empty" it can write). Since
    /// there's nothing to persist, just delete any stale `index.hnsw.*`
    /// files instead of dumping; deleting is inherently safe here (no
    /// marker/rename dance needed) because `HnswIndex::new`'s existence check
    /// (`graph_path.exists() && data_path.exists()`) already treats a
    /// missing pair as "create fresh, empty" — exactly the state we're in.
    fn dump_atomic(&self) -> Result<(), IndexError> {
        if self.graph_point_count() == 0 {
            for suffix in ["hnsw.graph", "hnsw.data"] {
                let path = self.data_dir.join(format!("index.{suffix}"));
                if path.exists() {
                    std::fs::remove_file(&path)?;
                }
            }
            tracing::debug!(
                path = %self.data_dir.display(),
                "HNSW graph is empty, cleared on-disk index instead of dumping"
            );
            return Ok(());
        }

        let new_basename = "index.new";
        // file_dump(path: &Path, basename: &str) creates {path}/{basename}.hnsw.{graph,data}
        self.hnsw
            .file_dump(&self.data_dir, new_basename)
            .map_err(|e| IndexError::Index(format!("save hnsw: {}", e)))?;

        let new_graph = self.data_dir.join("index.new.hnsw.graph");
        let new_data = self.data_dir.join("index.new.hnsw.data");

        // hnsw_rs flushes BufWriters but doesn't fsync. Under heavy parallel IO
        // the file metadata may not be stable for a subsequent reload. Sync both.
        for path in [&new_graph, &new_data] {
            if let Ok(f) = std::fs::File::open(path) {
                let _ = f.sync_all();
            }
        }

        let marker = self.data_dir.join("index.new.ready");
        {
            let f = std::fs::File::create(&marker)?;
            let _ = f.sync_all();
        }
        // The marker's directory entry must be durable before the renames —
        // and the renames durable before the marker's removal — or a power
        // loss can reorder them on disk (file fsync doesn't cover the
        // directory). gemini review catch, 2026-07-12.
        Self::sync_dir(&self.data_dir);

        let real_graph = self.data_dir.join("index.hnsw.graph");
        let real_data = self.data_dir.join("index.hnsw.data");
        std::fs::rename(&new_graph, &real_graph)?;
        std::fs::rename(&new_data, &real_data)?;
        Self::sync_dir(&self.data_dir);

        std::fs::remove_file(&marker)?;

        tracing::debug!(path = %self.data_dir.display(), "saved HNSW index to disk");
        Ok(())
    }

    /// fsync a directory so renames/creates/removes inside it are durable.
    /// Best-effort: not every filesystem supports opening a directory for
    /// sync, and recovery converges from any ordering anyway.
    fn sync_dir(dir: &std::path::Path) {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }

    /// Save the index to disk. Atomic — see `dump_atomic`.
    pub fn save(&self) -> Result<(), IndexError> {
        self.dump_atomic()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(dir: &std::path::Path) -> IndexConfig {
        IndexConfig {
            model_dir: dir.to_path_buf(),
            dimensions: 4,
            data_dir: dir.to_path_buf(),
            hnsw_max_nb_connection: 8,
            hnsw_ef_construction: 50,
            max_tokens: 512,
            max_contexts: None,
        }
    }

    #[test]
    fn test_insert_and_search() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let mut index = HnswIndex::new(&config).unwrap();

        // Insert 5 vectors
        index.insert(0, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        index.insert(1, &[0.0, 1.0, 0.0, 0.0]).unwrap();
        index.insert(2, &[0.9, 0.1, 0.0, 0.0]).unwrap();
        index.insert(3, &[0.0, 0.0, 1.0, 0.0]).unwrap();
        index.insert(4, &[0.0, 0.0, 0.0, 1.0]).unwrap();

        // Search for vector close to [1, 0, 0, 0]
        let results = index.search(&[0.95, 0.05, 0.0, 0.0], 2).unwrap();
        assert!(!results.is_empty());

        // Nearest should be slot 0 or 2 (both close to query)
        let nearest_slots: Vec<u32> = results.iter().map(|(s, _)| *s).collect();
        assert!(nearest_slots.contains(&0) || nearest_slots.contains(&2));
    }

    #[test]
    fn test_save_and_load() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());

        // Create and populate
        {
            let mut index = HnswIndex::new(&config).unwrap();
            index.insert(0, &[1.0, 0.0, 0.0, 0.0]).unwrap();
            index.insert(1, &[0.0, 1.0, 0.0, 0.0]).unwrap();
            index.save().unwrap();
        }

        // Reload — the HNSW graph should be loadable
        let index = HnswIndex::new(&config).unwrap();
        let results = index.search(&[1.0, 0.0, 0.0, 0.0], 1).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 0);

        // Embeddings cache must be rebuilt from HNSW on reload
        let emb0 = index
            .get_embedding(0)
            .expect("embedding 0 should exist after reload");
        assert_eq!(emb0.len(), 4);
        assert!((emb0[0] - 1.0).abs() < 1e-6);

        let emb1 = index
            .get_embedding(1)
            .expect("embedding 1 should exist after reload");
        assert_eq!(emb1.len(), 4);
        assert!((emb1[1] - 1.0).abs() < 1e-6);

        // get_all_embeddings should also work
        let all = index.get_all_embeddings().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_dimension_mismatch() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let mut index = HnswIndex::new(&config).unwrap();

        let result = index.insert(0, &[1.0, 0.0]); // wrong dims
        assert!(result.is_err());
    }

    #[test]
    fn test_get_all_embeddings() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let mut index = HnswIndex::new(&config).unwrap();

        index.insert(0, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        index.insert(2, &[0.0, 1.0, 0.0, 0.0]).unwrap();

        let all = index.get_all_embeddings().unwrap();
        assert_eq!(all.len(), 2);
    }

    /// Regression: hnsw_rs assigns each point to exactly one layer via a random
    /// exponential distribution. With max_nb_connection=8, P(level > 0) per point
    /// is 1/8. With 100 points, P(every point stays at level 0) ≈ 1.5e-6, so at
    /// least one higher-layer point is effectively guaranteed. The reload path
    /// must iterate all layers to rebuild the cache — iterating layer 0 alone
    /// drops any slot that landed at level > 0.
    #[test]
    fn test_reload_preserves_all_slots_across_layers() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());

        const N: u32 = 100;

        {
            let mut index = HnswIndex::new(&config).unwrap();
            for slot in 0..N {
                let mut v = vec![0.0f32; 4];
                v[(slot as usize) % 4] = 1.0 + (slot as f32) * 0.01;
                let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                for x in &mut v {
                    *x /= norm;
                }
                index.insert(slot, &v).unwrap();
            }
            index.save().unwrap();
        }

        let index = HnswIndex::new(&config).unwrap();
        let all = index.get_all_embeddings().unwrap();
        assert_eq!(
            all.len() as u32,
            N,
            "reload must preserve every slot regardless of assigned layer"
        );
        for slot in 0..N {
            index
                .get_embedding(slot)
                .unwrap_or_else(|_| panic!("slot {} missing after reload", slot));
        }
    }

    /// Regression: hnsw_rs's dump refuses to serialize a graph with no
    /// entry point (0 points). `save()`/`dump_atomic` must special-case
    /// that rather than propagate the "entry point not initialized" error —
    /// `from_entries` with an empty slice (e.g. a rebuild that drops every
    /// live slot) produces exactly this graph.
    #[test]
    fn test_save_empty_index_does_not_error() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());

        // First persist a non-empty index so there's something on disk to
        // confirm gets cleared, not left stale.
        {
            let mut index = HnswIndex::new(&config).unwrap();
            index.insert(0, &[1.0, 0.0, 0.0, 0.0]).unwrap();
            index.save().unwrap();
        }
        assert!(dir.path().join("index.hnsw.graph").exists());

        let empty = HnswIndex::from_entries(&config, &[]).unwrap();
        empty.save().unwrap();

        assert!(!dir.path().join("index.hnsw.graph").exists());
        assert!(!dir.path().join("index.hnsw.data").exists());

        // Reload: no saved files means a fresh, empty graph, not an error.
        let reloaded = HnswIndex::new(&config).unwrap();
        assert_eq!(reloaded.graph_point_count(), 0);
    }

    #[test]
    fn test_from_entries_sparse_slots() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());

        // Sparse slots (not contiguous from 0) must work — rebuild never
        // renumbers, so gaps are expected.
        let entries = vec![
            (3u32, vec![1.0, 0.0, 0.0, 0.0]),
            (7u32, vec![0.0, 1.0, 0.0, 0.0]),
        ];
        let index = HnswIndex::from_entries(&config, &entries).unwrap();

        let results = index.search(&[1.0, 0.0, 0.0, 0.0], 1).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 3);

        let all = index.get_all_embeddings().unwrap();
        assert_eq!(all.len(), 2);
        let slots: std::collections::HashSet<u32> = all.iter().map(|(s, _)| *s).collect();
        assert_eq!(slots, [3u32, 7u32].into_iter().collect());
    }

    #[test]
    fn test_from_entries_dimension_mismatch() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let entries = vec![(0u32, vec![1.0, 0.0])]; // wrong dims
        let result = HnswIndex::from_entries(&config, &entries);
        assert!(result.is_err());
    }

    /// Regression: hnsw_rs's `PointIndexation` iterator panics
    /// (`entry_point.unwrap()` on `None`) if you iterate an empty graph.
    /// `graph_point_count()` must not use it for that reason — a fresh index
    /// with zero points is the default startup state, not a corner case.
    #[test]
    fn test_graph_point_count_on_empty_graph_does_not_panic() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let index = HnswIndex::new(&config).unwrap();
        assert_eq!(index.graph_point_count(), 0);
    }

    #[test]
    fn test_graph_point_count_and_clear_slot() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let mut index = HnswIndex::new(&config).unwrap();

        index.insert(0, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        index.insert(1, &[0.0, 1.0, 0.0, 0.0]).unwrap();
        assert_eq!(index.graph_point_count(), 2);

        // clear_slot only drops the cache entry — the graph point stays put
        // until rebuild(), so graph_point_count is unaffected.
        index.clear_slot(0);
        assert_eq!(index.graph_point_count(), 2);
        assert!(index.get_embedding(0).is_err());
        assert!(index.get_embedding(1).is_ok());

        let all = index.get_all_embeddings().unwrap();
        assert_eq!(all.len(), 1);

        // Out-of-range slot is a no-op, not a panic.
        index.clear_slot(999);
    }

    /// Simulates a crash *after* `dump_atomic` finished writing + fsyncing
    /// the `.new` files and the marker, but before the renames landed. On
    /// reopen, recovery must finish the publish: rename the `.new` files over
    /// the real names and remove the marker.
    #[test]
    fn test_atomic_dump_recovers_completed_dump() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());

        {
            let mut index = HnswIndex::new(&config).unwrap();
            index.insert(0, &[1.0, 0.0, 0.0, 0.0]).unwrap();
            index.save().unwrap();
        }

        let dir_path = dir.path();
        std::fs::copy(
            dir_path.join("index.hnsw.graph"),
            dir_path.join("index.new.hnsw.graph"),
        )
        .unwrap();
        std::fs::copy(
            dir_path.join("index.hnsw.data"),
            dir_path.join("index.new.hnsw.data"),
        )
        .unwrap();
        std::fs::File::create(dir_path.join("index.new.ready")).unwrap();

        let index = HnswIndex::new(&config).unwrap();

        assert!(!dir_path.join("index.new.ready").exists());
        assert!(!dir_path.join("index.new.hnsw.graph").exists());
        assert!(!dir_path.join("index.new.hnsw.data").exists());
        assert!(dir_path.join("index.hnsw.graph").exists());
        assert!(dir_path.join("index.hnsw.data").exists());

        let results = index.search(&[1.0, 0.0, 0.0, 0.0], 1).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 0);
    }

    /// Simulates a crash *during* `dump_atomic`, before the marker was ever
    /// written. On reopen, recovery must discard the partial `.new` files and
    /// load from the untouched real files.
    #[test]
    fn test_atomic_dump_discards_incomplete_dump() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());

        {
            let mut index = HnswIndex::new(&config).unwrap();
            index.insert(0, &[1.0, 0.0, 0.0, 0.0]).unwrap();
            index.save().unwrap();
        }

        let dir_path = dir.path();
        std::fs::write(dir_path.join("index.new.hnsw.graph"), b"garbage").unwrap();
        std::fs::write(dir_path.join("index.new.hnsw.data"), b"garbage").unwrap();

        let index = HnswIndex::new(&config).unwrap();

        assert!(!dir_path.join("index.new.hnsw.graph").exists());
        assert!(!dir_path.join("index.new.hnsw.data").exists());

        // The untouched real index is still intact and loadable.
        let results = index.search(&[1.0, 0.0, 0.0, 0.0], 1).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 0);
    }

    /// Simulates a crash right after the marker was written but before any
    /// `.new` file existed (shouldn't happen given the real write order, but
    /// the recovery logic must handle it defensively): marker present, no
    /// `.new` files. Recovery should just clean up the stray marker and fall
    /// through to a normal load.
    #[test]
    fn test_atomic_dump_marker_without_new_files_is_cleaned() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());

        {
            let mut index = HnswIndex::new(&config).unwrap();
            index.insert(0, &[1.0, 0.0, 0.0, 0.0]).unwrap();
            index.save().unwrap();
        }

        let dir_path = dir.path();
        std::fs::File::create(dir_path.join("index.new.ready")).unwrap();

        let index = HnswIndex::new(&config).unwrap();

        assert!(!dir_path.join("index.new.ready").exists());
        let results = index.search(&[1.0, 0.0, 0.0, 0.0], 1).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 0);
    }
}
