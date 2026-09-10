//! Configuration for the semantic index.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Configuration for the semantic index subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConfig {
    /// Embedding dimensions (must match model output).
    pub dimensions: usize,
    /// Directory for HNSW mmap + SQLite metadata.
    pub data_dir: PathBuf,
    /// HNSW max connections per node.
    pub hnsw_max_nb_connection: usize,
    /// HNSW construction-time search breadth.
    pub hnsw_ef_construction: usize,
    /// UTF-8 byte budget of the indexed context projection.
    pub max_context_bytes: usize,
    /// Maximum number of contexts to keep in the index.
    /// When exceeded, the oldest contexts (by `embedded_at`) are evicted.
    /// `None` means unbounded.
    #[serde(default)]
    pub max_contexts: Option<usize>,
}

impl IndexConfig {
    /// Create an index config from kernel embedding settings.
    ///
    /// Dimensions come from service discovery; the projection budget comes
    /// from the kernel DB's `embedding_config` row.
    /// `kernel_data_dir` is typically `~/.local/share/kaijutsu/kernels/{id}/`.
    pub fn new(
        dimensions: usize,
        max_context_bytes: usize,
        kernel_data_dir: &Path,
    ) -> Self {
        let data_dir = kernel_data_dir.join("index");
        Self {
            dimensions,
            data_dir,
            hnsw_max_nb_connection: 16,
            hnsw_ef_construction: 200,
            max_context_bytes,
            max_contexts: None,
        }
    }
}
