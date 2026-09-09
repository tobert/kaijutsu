//! Embedding contract shared by indexing, search, and synthesis.

use async_trait::async_trait;
use serde::Serialize;

use crate::IndexError;

/// Asymmetric encoders use different preprocessing for these two purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingPurpose {
    Document,
    Query,
}

/// A fixed embedding profile. Implementations return L2-normalized vectors
/// in input order and reject incomplete, malformed, or mismatched responses.
/// Model identity stays fixed for the client's lifetime; a service changing
/// models must not inject vectors from another space into an existing index.
#[async_trait]
pub trait Embedder: Send + Sync {
    fn model_name(&self) -> &str;
    fn revision(&self) -> &str;
    fn dimensions(&self) -> usize;

    /// Stable identity of the vector space and its preprocessing contract.
    fn cache_identity(&self) -> String {
        serde_json::to_string(&(
            "embedding-query-document-l2-v1", self.model_name(), self.revision(), self.dimensions(),
        )).expect("embedding identity consists of strings and an integer")
    }

    async fn embed_batch(&self, texts: &[&str], purpose: EmbeddingPurpose) -> Result<Vec<Vec<f32>>, IndexError>;

    async fn embed(&self, text: &str, purpose: EmbeddingPurpose) -> Result<Vec<f32>, IndexError> {
        let mut results = self.embed_batch(&[text], purpose).await?;
        if results.len() != 1 {
            return Err(IndexError::Embedding(format!("expected one embedding, received {}", results.len())));
        }
        Ok(results.remove(0))
    }
}
