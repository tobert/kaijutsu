//! Embedder for any OpenAI-compatible `POST {base_url}/embeddings` endpoint:
//! OpenRouter, OpenAI, Google's Gemini API through its OpenAI-compatible
//! base URL, or a local server that speaks the same request. See
//! `docs/synthesis.md`, "Embedding service".
use std::borrow::Cow;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::{Embedder, EmbeddingPurpose, IndexError};

/// The longest batch one request carries.
const MAX_BATCH: usize = 32;
/// How much of a provider's error body an error message keeps.
const MAX_ERROR_BODY: usize = 300;

/// How a model wants queries and documents prepared. A provider's endpoint
/// takes plain text; the model's training expects a prefix for one purpose or
/// both, so the prefix is the caller's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmbeddingProfile {
    /// Send the text as given.
    Plain,
    /// Qwen3-Embedding: queries carry an instruction, documents are plain.
    Qwen3,
    /// EmbeddingGemma: queries and documents each carry a task prefix.
    EmbeddingGemma,
}

impl EmbeddingProfile {
    const TOKENS: &'static str = "plain, qwen3, embeddinggemma";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Qwen3 => "qwen3",
            Self::EmbeddingGemma => "embeddinggemma",
        }
    }

    fn prepare(self, text: &str, purpose: EmbeddingPurpose) -> Cow<'_, str> {
        match (self, purpose) {
            (Self::Plain, _) | (Self::Qwen3, EmbeddingPurpose::Document) => Cow::Borrowed(text),
            (Self::Qwen3, EmbeddingPurpose::Query) => Cow::Owned(format!(
                "Instruct: Given a search query, retrieve relevant passages that answer the query\nQuery: {text}"
            )),
            (Self::EmbeddingGemma, EmbeddingPurpose::Query) => Cow::Owned(format!("task: search result | query: {text}")),
            (Self::EmbeddingGemma, EmbeddingPurpose::Document) => Cow::Owned(format!("title: none | text: {text}")),
        }
    }
}

impl FromStr for EmbeddingProfile {
    type Err = String;

    fn from_str(token: &str) -> Result<Self, String> {
        [Self::Plain, Self::Qwen3, Self::EmbeddingGemma]
            .into_iter()
            .find(|profile| profile.as_str() == token)
            .ok_or_else(|| format!("unknown embedding profile `{token}`; use one of: {}", Self::TOKENS))
    }
}

pub struct OpenAiEmbedderConfig {
    /// The API root that has an `/embeddings` route, for example
    /// `https://openrouter.ai/api/v1`. No query or fragment.
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// Sent as `dimensions` and checked against every response, so a model
    /// that ignores it fails loudly instead of filling the index with the
    /// wrong size.
    pub dimensions: usize,
    pub profile: EmbeddingProfile,
    pub timeout: Duration,
    pub max_in_flight: usize,
}

impl std::fmt::Debug for OpenAiEmbedderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiEmbedderConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("dimensions", &self.dimensions)
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Response {
    data: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    index: usize,
    embedding: Vec<f32>,
}

/// Pins the model, dimensions and profile for this client's lifetime. A
/// hosted provider can change a model's weights without notice and this
/// client cannot see it; `revision` names the profile, not the weights.
pub struct OpenAiEmbedder {
    client: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
    revision: String,
    dimensions: usize,
    profile: EmbeddingProfile,
    timeout: Duration,
    permits: tokio::sync::Semaphore,
}

fn failure(error: impl std::fmt::Display) -> IndexError {
    IndexError::Embedding(format!("embedding service: {error}"))
}

impl OpenAiEmbedder {
    /// Validate the configuration, then embed one probe string so a wrong
    /// key, model or dimension count is found at boot and the semantic index
    /// stays off, rather than at the first block that needs a vector.
    pub async fn connect(config: OpenAiEmbedderConfig) -> Result<Self, IndexError> {
        if config.timeout.is_zero() || config.max_in_flight == 0 {
            return Err(failure("timeout and request concurrency must be positive"));
        }
        if config.dimensions == 0 {
            return Err(failure("dimensions must be positive"));
        }
        if config.model.trim().is_empty() {
            return Err(failure("the model name is empty"));
        }
        if config.api_key.trim().is_empty() {
            return Err(failure("the API key is empty"));
        }
        let base = reqwest::Url::parse(&config.base_url).map_err(failure)?;
        if !matches!(base.scheme(), "http" | "https") || base.query().is_some() || base.fragment().is_some() {
            return Err(failure("base_url must be an HTTP(S) URL without query or fragment"));
        }
        let client = reqwest::Client::builder().timeout(config.timeout).build().map_err(failure)?;
        let embedder = Self {
            client,
            url: format!("{}/embeddings", config.base_url.trim_end_matches('/')),
            api_key: config.api_key.trim().to_owned(),
            revision: format!("{}-hosted", config.profile.as_str()),
            model: config.model,
            dimensions: config.dimensions,
            profile: config.profile,
            timeout: config.timeout,
            permits: tokio::sync::Semaphore::new(config.max_in_flight),
        };
        embedder.request(&["dimension probe"], EmbeddingPurpose::Document).await?;
        Ok(embedder)
    }

    async fn request(&self, texts: &[&str], purpose: EmbeddingPurpose) -> Result<Vec<Vec<f32>>, IndexError> {
        let input: Vec<Cow<'_, str>> = texts.iter().map(|text| self.profile.prepare(text, purpose)).collect();
        // The deadline includes queue time. Overload cannot leave a caller
        // waiting indefinitely before reqwest's request timeout starts.
        tokio::time::timeout(self.timeout, async {
            let _permit = self.permits.acquire().await.map_err(failure)?;
            let response = self.client.post(&self.url)
                .bearer_auth(&self.api_key)
                .json(&serde_json::json!({
                    "model": self.model,
                    "input": input,
                    "dimensions": self.dimensions,
                    "encoding_format": "float",
                }))
                .send().await.map_err(failure)?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                let body: String = body.chars().take(MAX_ERROR_BODY).collect();
                return Err(failure(format!("{status}: {body}")));
            }
            let Response { data } = response.json().await.map_err(failure)?;
            self.ordered(data, texts.len())
        }).await.map_err(|_| failure("embedding request timed out (including queue time)"))?
    }

    /// Put a response's vectors in input order and normalize them, refusing
    /// anything that is not exactly one finite, nonzero vector per input.
    fn ordered(&self, data: Vec<Item>, expected: usize) -> Result<Vec<Vec<f32>>, IndexError> {
        if data.len() != expected {
            return Err(failure(format!("expected {expected} vectors, received {}", data.len())));
        }
        let mut slots: Vec<Option<Vec<f32>>> = vec![None; expected];
        for Item { index, embedding } in data {
            let slot = slots.get_mut(index)
                .ok_or_else(|| failure(format!("response index {index} is outside the {expected} inputs")))?;
            if slot.replace(embedding).is_some() {
                return Err(failure(format!("response repeats index {index}")));
            }
        }
        let mut vectors = Vec::with_capacity(expected);
        for slot in slots {
            let mut vector = slot.expect("a full response with unique in-range indexes fills every slot");
            if vector.len() != self.dimensions {
                return Err(failure(format!(
                    "model {} returned {} dimensions, configured {}", self.model, vector.len(), self.dimensions
                )));
            }
            if vector.iter().any(|v| !v.is_finite()) {
                return Err(failure("response has non-finite values"));
            }
            let norm = vector.iter().map(|v| (*v as f64).powi(2)).sum::<f64>().sqrt();
            if norm == 0.0 {
                return Err(failure("response contains a zero vector"));
            }
            for value in &mut vector { *value = (*value as f64 / norm) as f32; }
            vectors.push(vector);
        }
        Ok(vectors)
    }
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    fn model_name(&self) -> &str { &self.model }
    fn revision(&self) -> &str { &self.revision }
    fn dimensions(&self) -> usize { self.dimensions }
    async fn embed_batch(&self, texts: &[&str], purpose: EmbeddingPurpose) -> Result<Vec<Vec<f32>>, IndexError> {
        let mut vectors = Vec::with_capacity(texts.len());
        // Chunking bounds each request's size and occupancy.
        for chunk in texts.chunks(MAX_BATCH) {
            vectors.extend(self.request(chunk, purpose).await?);
        }
        Ok(vectors)
    }
}
