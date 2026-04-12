//! Core trait and types for image generation backends.

use std::pin::Pin;

use futures::Stream;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ImageError {
    #[error("backend not available: {0}")]
    NotAvailable(String),

    #[error("generation failed: {0}")]
    GenerationFailed(String),

    #[error("MCP call failed: {0}")]
    Mcp(String),

    #[error("image decode failed: {0}")]
    Decode(String),

    #[error("CAS storage failed: {0}")]
    Storage(String),

    #[error("cancelled")]
    Cancelled,
}

/// Options for image generation.
#[derive(Debug, Clone, Default)]
pub struct ImageGenOpts {
    /// Target image dimensions (width, height). Backend may ignore if unsupported.
    pub size: Option<(u32, u32)>,
    /// Backend name override (e.g. "gemini", "local_sd"). None = use default.
    pub backend: Option<String>,
    /// Model name override within the backend.
    pub model: Option<String>,
    /// Seed for reproducible generation (if the backend supports it).
    pub seed: Option<u64>,
}

/// A stream of image byte chunks plus the MIME type.
///
/// Backends that don't support progressive output emit a single chunk.
pub struct ImageStream {
    /// MIME type of the image (known up front, e.g. "image/png").
    pub mime: String,
    /// Async stream of byte chunks.
    pub chunks: Pin<Box<dyn Stream<Item = Result<Vec<u8>, ImageError>> + Send>>,
    /// Total size hint in bytes (for progress reporting, when known).
    pub total_size_hint: Option<usize>,
}

/// Trait for image generation backends.
#[async_trait::async_trait]
pub trait ImageBackend: Send + Sync {
    /// Human-readable backend name (e.g. "gemini", "local_sd").
    fn name(&self) -> &str;

    /// Generate an image from a text prompt.
    async fn generate(
        &self,
        prompt: &str,
        opts: ImageGenOpts,
    ) -> Result<ImageStream, ImageError>;
}
