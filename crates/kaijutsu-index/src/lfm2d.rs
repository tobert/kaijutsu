//! lfm2d embedding service adapter.
use crate::{Embedder, EmbeddingPurpose, IndexError};
use async_trait::async_trait;
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize)]
struct Model {
    id: String,
    kind: String,
    weight_hash: Option<String>,
    hidden_size: Option<usize>,
}

/// Pins the discovered model for this client's lifetime. Restart the index to
/// adopt a changed model; mixing vector spaces would corrupt similarity scores.
pub struct Lfm2dEmbedder {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    model: String,
    revision: String,
    dimensions: usize,
    timeout: Duration,
    permits: tokio::sync::Semaphore,
}

fn failure(error: impl std::fmt::Display) -> IndexError {
    IndexError::Embedding(format!("lfm2d: {error}"))
}

impl Lfm2dEmbedder {
    pub async fn connect(endpoint: &str, timeout: Duration, max_in_flight: usize) -> Result<Self, IndexError> {
        if timeout.is_zero() || max_in_flight == 0 {
            return Err(failure("timeout and request concurrency must be positive"));
        }
        let mut builder = reqwest::Client::builder().timeout(timeout);
        let endpoint = if let Some(path) = endpoint.strip_prefix("unix://") {
            if !std::path::Path::new(path).is_absolute() {
                return Err(failure("unix socket path must be absolute"));
            }
            builder = builder.unix_socket(path.to_owned());
            reqwest::Url::parse("http://localhost/").map_err(failure)?
        } else {
            let url = reqwest::Url::parse(endpoint).map_err(failure)?;
            if !matches!(url.scheme(), "http" | "https") || url.query().is_some() || url.fragment().is_some() {
                return Err(failure("endpoint must be an HTTP(S) URL without query or fragment, or unix:///absolute/path"));
            }
            url
        };
        let client = builder.build().map_err(failure)?;
        let models: Vec<Model> = client.get(endpoint.join("/v1/models").map_err(failure)?)
            .send().await.map_err(failure)?.error_for_status().map_err(failure)?
            .json().await.map_err(failure)?;
        let mut embedders = models.into_iter().filter(|model| model.kind == "embedder");
        let model = embedders.next().ok_or_else(|| failure("discovery returned no embedding model"))?;
        if embedders.next().is_some() {
            return Err(failure("discovery returned multiple embedding models"));
        }
        let revision = model.weight_hash.ok_or_else(|| failure("discovery omitted weight_hash"))?;
        let dimensions = model.hidden_size.ok_or_else(|| failure("discovery omitted hidden_size"))?;
        if model.id.is_empty() || dimensions == 0 || revision.len() != 64
            || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(failure("discovery returned an invalid embedding profile"));
        }
        Ok(Self { client, endpoint, model: model.id, revision, dimensions, timeout,
            permits: tokio::sync::Semaphore::new(max_in_flight) })
    }

    async fn request(&self, texts: &[&str], purpose: EmbeddingPurpose) -> Result<Vec<Vec<f32>>, IndexError> {
        // The deadline includes queue time. Overload cannot leave a caller
        // waiting indefinitely before reqwest's request timeout starts.
        tokio::time::timeout(self.timeout, async {
            let _permit = self.permits.acquire().await.map_err(failure)?;
            let response = self.client.post(self.endpoint.join("/embed").map_err(failure)?)
                .json(&serde_json::json!({ "inputs": texts, "kind": purpose }))
                .send().await.map_err(failure)?.error_for_status().map_err(failure)?;
            for (header, expected) in [("x-model-id", self.model.as_str()), ("x-model-weight-hash", self.revision.as_str())] {
                let actual = response.headers().get(header).and_then(|v| v.to_str().ok());
                if actual != Some(expected) {
                    return Err(failure(format!("{header} changed or missing; reconnect the semantic index")));
                }
            }
            let mut vectors: Vec<Vec<f32>> = response.json().await.map_err(failure)?;
            if vectors.len() != texts.len() {
                return Err(failure(format!("expected {} vectors, received {}", texts.len(), vectors.len())));
            }
            for vector in &mut vectors {
                if vector.len() != self.dimensions || vector.iter().any(|v| !v.is_finite()) {
                    return Err(failure("response has invalid dimensions or non-finite values"));
                }
                let norm = vector.iter().map(|v| (*v as f64).powi(2)).sum::<f64>().sqrt();
                if norm == 0.0 {
                    return Err(failure("response contains a zero vector"));
                }
                for value in vector { *value = (*value as f64 / norm) as f32; }
            }
            Ok(vectors)
        }).await.map_err(|_| failure("embedding request timed out (including queue time)"))?
    }
}

#[async_trait]
impl Embedder for Lfm2dEmbedder {
    fn model_name(&self) -> &str { &self.model }
    fn revision(&self) -> &str { &self.revision }
    fn dimensions(&self) -> usize { self.dimensions }
    async fn embed_batch(&self, texts: &[&str], purpose: EmbeddingPurpose) -> Result<Vec<Vec<f32>>, IndexError> {
        let mut vectors = Vec::with_capacity(texts.len());
        // lfm2d embeds each input independently; chunking preserves that
        // contract and bounds each request's service occupancy.
        for chunk in texts.chunks(32) {
            vectors.extend(self.request(chunk, purpose).await?);
        }
        Ok(vectors)
    }
}
