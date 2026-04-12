//! Gemini image generation via the gpal MCP server.
//!
//! Wraps `mcp__gpal__generate_image` as a single-chunk `ImageBackend`.

use std::sync::Arc;

use base64::Engine as _;
use futures::stream;
use tracing::info;

use crate::mcp_pool::McpServerPool;
use super::backend::{ImageBackend, ImageError, ImageGenOpts, ImageStream};

/// Image generation backend using Gemini via the gpal MCP server.
pub struct GeminiBackend {
    pool: Arc<McpServerPool>,
}

impl GeminiBackend {
    pub fn new(pool: Arc<McpServerPool>) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ImageBackend for GeminiBackend {
    fn name(&self) -> &str {
        "gemini"
    }

    async fn generate(
        &self,
        prompt: &str,
        opts: ImageGenOpts,
    ) -> Result<ImageStream, ImageError> {
        let mut args = serde_json::json!({
            "prompt": prompt,
        });

        if let Some((w, h)) = opts.size {
            args["width"] = serde_json::json!(w);
            args["height"] = serde_json::json!(h);
        }
        if let Some(ref model) = opts.model {
            args["model"] = serde_json::json!(model);
        }

        info!(prompt = %prompt, "Gemini image generation starting");

        let result = self
            .pool
            .call_tool("gpal", "generate_image", args)
            .await
            .map_err(|e| ImageError::Mcp(e.to_string()))?;

        // Extract image bytes from the MCP result.
        // gpal returns image data as base64-encoded content or raw bytes
        // in the CallToolResult content array.
        let mut image_bytes: Option<Vec<u8>> = None;
        let mut mime = "image/png".to_string();

        for content in &result.content {
            // Content is Annotated<RawContent> — access .raw for the enum
            match &content.raw {
                rmcp::model::RawContent::Image(img) => {
                    // img.data is base64-encoded
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(&img.data)
                        .map_err(|e| ImageError::Decode(format!("invalid base64: {}", e)))?;
                    image_bytes = Some(decoded);
                    mime = img.mime_type.clone();
                    break;
                }
                rmcp::model::RawContent::Text(text) => {
                    // Some MCP servers return base64 in a text content block
                    if let Ok(decoded) = base64::engine::general_purpose::STANDARD
                        .decode(&text.text)
                    {
                        image_bytes = Some(decoded);
                        break;
                    }
                }
                _ => {}
            }
        }

        let data = image_bytes.ok_or_else(|| {
            ImageError::GenerationFailed("no image content in MCP response".into())
        })?;

        info!(size = data.len(), mime = %mime, "Gemini image generation complete");

        // Emit as a single chunk
        let chunks = stream::once(async move { Ok(data) });

        Ok(ImageStream {
            mime,
            chunks: Box::pin(chunks),
            total_size_hint: None,
        })
    }
}

