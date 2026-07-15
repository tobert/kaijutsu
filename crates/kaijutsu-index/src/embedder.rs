//! Embedding generation via ONNX models.
//!
//! The `Embedder` trait abstracts over embedding generation so the ONNX
//! implementation can be swapped for API-backed embedders in the future.

use std::path::Path;

use rten::{Model, NodeId, ValueOrView};
use rten_tensor::NdTensor;
use rten_tensor::prelude::*;

use crate::IndexError;

/// Trait abstracting over embedding generation.
///
/// Implementations must be Send + Sync (the ONNX model is thread-safe).
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
        results
            .pop()
            .ok_or_else(|| IndexError::Embedding("empty batch result".into()))
    }
}

/// ONNX-backed embedder using rten (pure-Rust inference) + HuggingFace tokenizer.
///
/// Loads from a directory containing `model.onnx` and `tokenizer.json`.
/// Inference: tokenize → run model → mean-pool → L2-normalize.
pub struct RtenEmbedder {
    model: Model,
    input_ids_node: NodeId,
    attention_mask_node: NodeId,
    token_type_ids_node: Option<NodeId>,
    output_node: NodeId,
    tokenizer: tokenizers::Tokenizer,
    dims: usize,
    #[allow(dead_code)] // Phase 2+: used in content extraction truncation
    max_tokens: usize,
    name: String,
}

impl RtenEmbedder {
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
            return Err(IndexError::ModelNotFound(
                tokenizer_path.display().to_string(),
            ));
        }

        let model =
            Model::load_file(&model_path).map_err(|e| IndexError::Inference(e.to_string()))?;

        let input_ids_node = model
            .node_id("input_ids")
            .map_err(|e| IndexError::Inference(e.to_string()))?;
        let attention_mask_node = model
            .node_id("attention_mask")
            .map_err(|e| IndexError::Inference(e.to_string()))?;
        // Not every model exports a token_type_ids input; only wire it up if present.
        let token_type_ids_node = model.find_node("token_type_ids");
        // Prefer the conventional "last_hidden_state" output name; fall back to
        // the model's first declared output (matches the old ort code, which
        // indexed outputs[0] positionally).
        let output_node = model.find_node("last_hidden_state").or_else(|| {
            model.output_ids().first().copied()
        }).ok_or_else(|| IndexError::Inference("model has no output nodes".into()))?;

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
            model,
            input_ids_node,
            attention_mask_node,
            token_type_ids_node,
            output_node,
            tokenizer,
            dims: dimensions,
            max_tokens,
            name,
        })
    }
}

/// Upper bound on texts per model run.
///
/// `embed_batch` pads every text to the batch's longest sequence
/// (`PaddingStrategy::BatchLongest`), so one unbounded batch — e.g. `kj synth
/// all` embedding every block of a large context at once — allocates tensors
/// of `batch × longest_seq × dims`. Chunking bounds that allocation to a few
/// dozen MB per run without changing results; callers keep passing whatever
/// batch size is natural for them.
const EMBED_CHUNK: usize = 32;

impl Embedder for RtenEmbedder {
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

        if texts.len() > EMBED_CHUNK {
            let mut results = Vec::with_capacity(texts.len());
            for chunk in texts.chunks(EMBED_CHUNK) {
                results.extend(self.embed_batch(chunk)?);
            }
            return Ok(results);
        }

        // Tokenize
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| IndexError::Tokenizer(e.to_string()))?;

        let batch_size = encodings.len();
        let seq_len = encodings[0].get_ids().len();

        // Build flat vectors for input tensors. rten's ONNX ops expect i32,
        // unlike ort which used i64.
        let mut input_ids: Vec<i32> = Vec::with_capacity(batch_size * seq_len);
        let mut attention_mask: Vec<i32> = Vec::with_capacity(batch_size * seq_len);
        let mut token_type_ids: Vec<i32> = Vec::with_capacity(batch_size * seq_len);

        for enc in &encodings {
            for &id in enc.get_ids() {
                input_ids.push(id as i32);
            }
            for &mask in enc.get_attention_mask() {
                attention_mask.push(mask as i32);
            }
            for &tt in enc.get_type_ids() {
                token_type_ids.push(tt as i32);
            }
        }

        let shape = [batch_size, seq_len];
        let input_ids_tensor = NdTensor::from_data(shape, input_ids);
        // attention_mask is cloned into the tensor: the original is still
        // needed below for mean-pooling.
        let attention_mask_tensor = NdTensor::from_data(shape, attention_mask.clone());

        let mut inputs: Vec<(NodeId, ValueOrView)> = vec![
            (self.input_ids_node, input_ids_tensor.into()),
            (self.attention_mask_node, attention_mask_tensor.into()),
        ];
        if let Some(token_type_ids_node) = self.token_type_ids_node {
            let token_type_ids_tensor = NdTensor::from_data(shape, token_type_ids);
            inputs.push((token_type_ids_node, token_type_ids_tensor.into()));
        }

        // Run inference
        let [output] = self
            .model
            .run_n(inputs, [self.output_node], None)
            .map_err(|e| IndexError::Inference(e.to_string()))?;

        // Extract token embeddings: shape [batch_size, seq_len, dims]
        let hidden: NdTensor<f32, 3> = output
            .try_into()
            .map_err(|e: rten::TryFromValueError| IndexError::Inference(e.to_string()))?;

        let [out_batch, actual_seq_len, dims] = hidden.shape();
        assert_eq!(
            out_batch, batch_size,
            "model output batch size ({out_batch}) does not match input batch size ({batch_size})"
        );
        assert!(
            actual_seq_len <= seq_len,
            "model output seq_len ({actual_seq_len}) exceeds input seq_len ({seq_len})"
        );

        let emb_data: Vec<f32> = hidden.to_vec();

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
