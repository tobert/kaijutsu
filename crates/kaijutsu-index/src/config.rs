//! Configuration for the semantic index.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Configuration for the semantic index subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConfig {
    /// Directory containing model.onnx + tokenizer.json.
    pub model_dir: PathBuf,
    /// Embedding dimensions (must match model output).
    pub dimensions: usize,
    /// Directory for HNSW mmap + SQLite metadata.
    pub data_dir: PathBuf,
    /// HNSW max connections per node.
    pub hnsw_max_nb_connection: usize,
    /// HNSW construction-time search breadth.
    pub hnsw_ef_construction: usize,
    /// Maximum input tokens for the embedding model.
    pub max_tokens: usize,
    /// Maximum number of contexts to keep in the index.
    /// When exceeded, the oldest contexts (by `embedded_at`) are evicted.
    /// `None` means unbounded.
    #[serde(default)]
    pub max_contexts: Option<usize>,
}

impl IndexConfig {
    /// Create an index config from kernel embedding settings.
    ///
    /// `model_dir` and `dimensions`/`max_tokens` come from models.rhai.
    /// `kernel_data_dir` is typically `~/.local/share/kaijutsu/kernels/{id}/`.
    pub fn new(model_dir: PathBuf, dimensions: usize, max_tokens: usize, kernel_data_dir: &Path) -> Self {
        let data_dir = kernel_data_dir.join("index");
        Self {
            model_dir,
            dimensions,
            data_dir,
            hnsw_max_nb_connection: 16,
            hnsw_ef_construction: 200,
            max_tokens,
            max_contexts: None,
        }
    }
}
