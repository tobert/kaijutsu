//! Embedder for a council server's `POST /council/v2/embeddings` route
//! (gogi's `docs/council-api.md`, "Embeddings"). The server renders each
//! task's prompt, so this client sends plain text and the task name.
use std::time::Duration;

use async_trait::async_trait;
use gogi_proto::embeddings::EmbeddingsRequest;
use gogi_proto::identity::{Capability, ModelStatus};
use gogi_proto::message::Content;
use kaijutsu_mk::MkClient;
use sha2::{Digest, Sha256};

use crate::{Embedder, EmbeddingPurpose, IndexError};

/// The most inputs this client sends in one request, whatever the server allows.
const MAX_BATCH: usize = 32;
const QUERY: &str = "query";
const DOCUMENT: &str = "document";

pub struct CouncilEmbedderConfig {
    /// The council server's address, such as `http://spaceghost:8091`.
    pub base_url: String,
    /// The served model: its `model` or an alias.
    pub model: String,
    /// One of the served model's `embeddings.dimensions`.
    pub dimensions: usize,
    pub timeout: Duration,
    pub max_in_flight: usize,
}

/// Pins the server's identity for this client's lifetime. A response with any
/// other identity is refused: vectors of different identities do not compare,
/// so restart the index to adopt a changed server.
pub struct CouncilEmbedder {
    client: MkClient,
    model: String,
    /// A hash of the identity fields the vectors depend on.
    revision: String,
    /// The identity as the server wrote it, for the next response to match.
    identity: String,
    dimensions: usize,
    batch: usize,
    timeout: Duration,
    permits: tokio::sync::Semaphore,
}

fn failure(error: impl std::fmt::Display) -> IndexError {
    IndexError::Embedding(format!("embedding service: {error}"))
}

fn task(purpose: EmbeddingPurpose) -> &'static str {
    match purpose {
        EmbeddingPurpose::Query => QUERY,
        EmbeddingPurpose::Document => DOCUMENT,
    }
}

impl CouncilEmbedder {
    /// Read the server's identity, check that it serves the model for
    /// embedding at the configured size, then embed one probe string to learn
    /// the identity the vectors carry. Any refusal here leaves the index off.
    pub async fn connect(config: CouncilEmbedderConfig) -> Result<Self, IndexError> {
        if config.timeout.is_zero() || config.max_in_flight == 0 {
            return Err(failure("timeout and request concurrency must be positive"));
        }
        let dimensions = u32::try_from(config.dimensions).ok().filter(|d| *d > 0)
            .ok_or_else(|| failure("dimensions must be positive"))?;
        if config.model.trim().is_empty() {
            return Err(failure("the model name is empty"));
        }
        let client = MkClient::new(config.base_url.as_str(), config.timeout).map_err(failure)?;
        let identity = client.council().identity().await.map_err(failure)?;
        let served = identity.route(&config.model).map_err(|e| failure(e.message))?;
        let offered = match &served.embeddings {
            Some(offered) if served.has(Capability::Embed) => offered,
            _ => return Err(failure(format!(
                "model {} does not embed; its capabilities do not list `embed`", served.model
            ))),
        };
        if served.status != ModelStatus::Ready {
            return Err(failure(format!("model {} is not ready", served.model)));
        }
        for needed in [QUERY, DOCUMENT] {
            if !offered.tasks.contains(&needed.to_owned()) {
                return Err(failure(format!(
                    "model {} has no `{needed}` task; its tasks are {}", served.model, offered.tasks.join(", ")
                )));
            }
        }
        if !offered.dimensions.contains(&dimensions) {
            let list: Vec<String> = offered.dimensions.iter().map(u32::to_string).collect();
            return Err(failure(format!(
                "model {} has no {dimensions} dimensions; it offers {}", served.model, list.join(", ")
            )));
        }
        let mut embedder = Self {
            client,
            model: served.model.clone(),
            revision: String::new(),
            identity: String::new(),
            dimensions: config.dimensions,
            batch: MAX_BATCH.min(offered.inputs_per_request as usize),
            timeout: config.timeout,
            permits: tokio::sync::Semaphore::new(config.max_in_flight),
        };
        let probe = embedder.send(&["dimension probe"], EmbeddingPurpose::Document).await?;
        let written = serde_json::to_string(&probe.identity).map_err(failure)?;
        embedder.revision = format!("{:x}", Sha256::digest(written.as_bytes()));
        embedder.identity = written;
        Ok(embedder)
    }

    async fn send(&self, texts: &[&str], purpose: EmbeddingPurpose) -> Result<gogi_proto::embeddings::EmbeddingsResponse, IndexError> {
        let request = EmbeddingsRequest {
            model: self.model.clone(),
            task: task(purpose).to_owned(),
            inputs: texts.iter().map(|text| Content::Text((*text).to_owned())).collect(),
            dimensions: Some(self.dimensions as u32),
            dry_run: false,
            class: None,
            priority: None,
        };
        // The deadline includes queue time. Overload cannot leave a caller
        // waiting indefinitely before the request's own timeout starts.
        tokio::time::timeout(self.timeout, async {
            let _permit = self.permits.acquire().await.map_err(failure)?;
            self.client.council().embeddings(&request, self.timeout).await.map_err(failure)
        }).await.map_err(|_| failure("embedding request timed out (including queue time)"))?
    }

    async fn request(&self, texts: &[&str], purpose: EmbeddingPurpose) -> Result<Vec<Vec<f32>>, IndexError> {
        let response = self.send(texts, purpose).await?;
        if !self.identity.is_empty() {
            let written = serde_json::to_string(&response.identity).map_err(failure)?;
            if written != self.identity {
                return Err(failure("the server's identity changed; restart the semantic index"));
            }
        }
        if response.model != self.model || response.task != task(purpose) || response.dimensions as usize != self.dimensions {
            return Err(failure(format!(
                "response is for model {} task {} at {} dimensions, not {} {} at {}",
                response.model, response.task, response.dimensions, self.model, task(purpose), self.dimensions
            )));
        }
        if response.embeddings.len() != texts.len() {
            return Err(failure(format!("expected {} vectors, received {}", texts.len(), response.embeddings.len())));
        }
        let mut vectors = Vec::with_capacity(texts.len());
        for (position, embedding) in response.embeddings.into_iter().enumerate() {
            if embedding.index != position as u64 {
                return Err(failure(format!("vector {position} arrived with index {}", embedding.index)));
            }
            let mut vector = embedding.vector.ok_or_else(|| failure(format!("vector {position} is missing")))?;
            if vector.len() != self.dimensions {
                return Err(failure(format!("model {} returned {} dimensions, configured {}", self.model, vector.len(), self.dimensions)));
            }
            if vector.iter().any(|v| !v.is_finite()) {
                return Err(failure("response has non-finite values"));
            }
            let norm = vector.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>().sqrt();
            if norm == 0.0 {
                return Err(failure("response contains a zero vector"));
            }
            for value in &mut vector { *value = (f64::from(*value) / norm) as f32; }
            vectors.push(vector);
        }
        Ok(vectors)
    }
}

#[async_trait]
impl Embedder for CouncilEmbedder {
    fn model_name(&self) -> &str { &self.model }
    fn revision(&self) -> &str { &self.revision }
    fn dimensions(&self) -> usize { self.dimensions }
    async fn embed_batch(&self, texts: &[&str], purpose: EmbeddingPurpose) -> Result<Vec<Vec<f32>>, IndexError> {
        let mut vectors = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(self.batch) {
            vectors.extend(self.request(chunk, purpose).await?);
        }
        Ok(vectors)
    }
}
