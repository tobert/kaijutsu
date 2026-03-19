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
                    // Layer 0 contains all points — iterate to populate the cache
                    // so get_embedding() and get_all_embeddings() work after reload.
                    let mut embeddings: Vec<Option<Vec<f32>>> = Vec::new();
                    let pi = loaded.get_point_indexation();
                    for point in pi.get_layer_iterator(0) {
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

    /// Save the index to disk.
    pub fn save(&self) -> Result<(), IndexError> {
        // file_dump(path: &Path, basename: &str) creates {path}/{basename}.hnsw.{graph,data}
        self.hnsw
            .file_dump(&self.data_dir, "index")
            .map_err(|e| IndexError::Index(format!("save hnsw: {}", e)))?;

        tracing::debug!(path = %self.data_dir.display(), "saved HNSW index to disk");
        Ok(())
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
}
