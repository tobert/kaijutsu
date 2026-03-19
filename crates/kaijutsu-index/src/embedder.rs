//! Embedding generation via ONNX models.
//!
//! The `Embedder` trait abstracts over embedding generation so the ONNX
//! implementation can be swapped for API-backed embedders in the future.

use std::path::Path;
use std::sync::Mutex;

use ort::value::Tensor;

use crate::IndexError;

/// Trait abstracting over embedding generation.
///
/// Implementations must be Send + Sync (the ONNX session is thread-safe).
/// Methods are sync — callers should use `spawn_blocking` for CPU-bound inference.
pub trait Embedder: Send + Sync {
    /// Human-readable model name (e.g. "bge-small-en-v1.5").
    fn model_name(&self) -> &str;

    /// Output embedding dimensions.
    fn dimensions(&self) -> usize;

    /// Embed a batch of texts. Returns one vector per input.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, IndexError>;

    /// Embed a single text. Default: batch of 1.
    fn embed(&self, text: &str) -> Result<Vec<f32>, IndexError> {
        let mut results = self.embed_batch(&[text])?;
        results.pop().ok_or_else(|| IndexError::Embedding("empty batch result".into()))
    }
}

/// ONNX-backed embedder using ort + HuggingFace tokenizer.
///
/// Loads from a directory containing `model.onnx` and `tokenizer.json`.
/// Inference: tokenize → run session → mean-pool → L2-normalize.
pub struct OnnxEmbedder {
    session: Mutex<ort::session::Session>,
    tokenizer: tokenizers::Tokenizer,
    dims: usize,
    #[allow(dead_code)] // Phase 2+: used in content extraction truncation
    max_tokens: usize,
    name: String,
}

impl OnnxEmbedder {
    /// Create a new embedder from a model directory.
    ///
    /// The directory must contain:
    /// - `model.onnx` — the ONNX model file
    /// - `tokenizer.json` — HuggingFace tokenizer config
    pub fn new(model_dir: &Path, dimensions: usize, max_tokens: usize) -> Result<Self, IndexError> {
        let model_path = model_dir.join("model.onnx");
        let tokenizer_path = model_dir.join("tokenizer.json");

        if !model_path.exists() {
            return Err(IndexError::ModelNotFound(model_path.display().to_string()));
        }
        if !tokenizer_path.exists() {
            return Err(IndexError::ModelNotFound(tokenizer_path.display().to_string()));
        }

        let session = ort::session::Session::builder()
            .map_err(|e| IndexError::Onnx(e.to_string()))?
            .with_intra_threads(1)
            .map_err(|e| IndexError::Onnx(e.to_string()))?
            .commit_from_file(&model_path)
            .map_err(|e| IndexError::Onnx(e.to_string()))?;

        let mut tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| IndexError::Tokenizer(e.to_string()))?;

        // Set truncation to max_tokens
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: max_tokens,
                ..Default::default()
            }))
            .map_err(|e| IndexError::Tokenizer(e.to_string()))?;

        // Set padding to pad to longest in batch
        tokenizer.with_padding(Some(tokenizers::PaddingParams {
            strategy: tokenizers::PaddingStrategy::BatchLongest,
            ..Default::default()
        }));

        let name = model_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        tracing::info!(
            model = %name,
            dims = dimensions,
            max_tokens = max_tokens,
            "Loaded ONNX embedding model"
        );

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            dims: dimensions,
            max_tokens,
            name,
        })
    }
}

impl Embedder for OnnxEmbedder {
    fn model_name(&self) -> &str {
        &self.name
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, IndexError> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        // Tokenize
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| IndexError::Tokenizer(e.to_string()))?;

        let batch_size = encodings.len();
        let seq_len = encodings[0].get_ids().len();

        // Build flat vectors for input tensors
        let mut input_ids: Vec<i64> = Vec::with_capacity(batch_size * seq_len);
        let mut attention_mask: Vec<i64> = Vec::with_capacity(batch_size * seq_len);
        let mut token_type_ids: Vec<i64> = Vec::with_capacity(batch_size * seq_len);

        for enc in &encodings {
            for &id in enc.get_ids() {
                input_ids.push(id as i64);
            }
            for &mask in enc.get_attention_mask() {
                attention_mask.push(mask as i64);
            }
            for &tt in enc.get_type_ids() {
                token_type_ids.push(tt as i64);
            }
        }

        // Create ort Tensor values via (shape, data) tuples
        let shape = [batch_size, seq_len];
        let input_ids_tensor = Tensor::from_array((shape, input_ids.clone()))
            .map_err(|e| IndexError::Onnx(e.to_string()))?;
        let attention_mask_tensor = Tensor::from_array((shape, attention_mask.clone()))
            .map_err(|e| IndexError::Onnx(e.to_string()))?;
        let token_type_ids_tensor = Tensor::from_array((shape, token_type_ids))
            .map_err(|e| IndexError::Onnx(e.to_string()))?;

        // Run inference with named inputs
        let mut session = self.session.lock()
            .map_err(|e| IndexError::Onnx(format!("session lock: {}", e)))?;
        let outputs = session
            .run(ort::inputs![
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
                "token_type_ids" => token_type_ids_tensor,
            ])
            .map_err(|e| IndexError::Onnx(e.to_string()))?;

        // Extract token embeddings: shape [batch_size, seq_len, dims]
        // First output is typically "last_hidden_state"
        let embeddings_value = &outputs[0];

        let (emb_shape, emb_data) = embeddings_value
            .try_extract_tensor::<f32>()
            .map_err(|e| IndexError::Onnx(e.to_string()))?;

        // emb_shape is [batch_size, seq_len, dims] (deref to &[i64])
        let dims = emb_shape.get(2).copied().unwrap_or(self.dims as i64) as usize;
        let actual_seq_len = emb_shape.get(1).copied().unwrap_or(seq_len as i64) as usize;
        assert!(actual_seq_len <= seq_len,
            "ONNX output seq_len ({actual_seq_len}) exceeds input seq_len ({seq_len})");

        // Mean-pool over sequence length, respecting attention mask
        let mut results = Vec::with_capacity(batch_size);

        for i in 0..batch_size {
            let mut pooled = vec![0.0f32; dims];
            let mut count = 0.0f32;

            for j in 0..actual_seq_len {
                let mask_val = attention_mask[i * seq_len + j] as f32;
                if mask_val > 0.0 {
                    let offset = i * actual_seq_len * dims + j * dims;
                    for k in 0..dims {
                        pooled[k] += emb_data[offset + k] * mask_val;
                    }
                    count += mask_val;
                }
            }

            // Average
            if count > 0.0 {
                for v in &mut pooled {
                    *v /= count;
                }
            }

            // L2 normalize
            let norm: f32 = pooled.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for v in &mut pooled {
                    *v /= norm;
                }
            }

            results.push(pooled);
        }

        Ok(results)
    }
}
