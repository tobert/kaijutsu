//! Content-type-specific block creation tools.
//!
//! Thin convenience wrappers over `block_create` that select the correct
//! `ContentType`, validate where appropriate, and handle CAS for images.
//!
//! These are LLM-callable kernel tools — registered as `svg_block`,
//! `abc_block`, and `img_block` (plus variants).

use std::sync::Arc;

use kaijutsu_cas::{ContentHash, ContentStore, FileStore};
use kaijutsu_crdt::{BlockKind, ContentType, Role, Status};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::block_store::SharedBlockStore;
use crate::execution::{ExecContext, ExecResult};
use crate::kj::cas::mime_from_extension;

// ============================================================================
// Shared helper
// ============================================================================

/// Append a block at the end of the current context and return its key.
fn append_block(
    documents: &SharedBlockStore,
    ctx: &ExecContext,
    role: Role,
    content: &str,
    content_type: ContentType,
) -> Result<String, String> {
    let context_id = ctx.context_id;
    if !documents.contains(context_id) {
        return Err(format!("no document for context {}", context_id.short()));
    }

    let last_block_id = documents
        .get(context_id)
        .and_then(|doc| doc.doc.blocks_ordered().last().map(|b| b.id));

    documents
        .insert_block(
            context_id,
            None,
            last_block_id.as_ref(),
            role,
            BlockKind::Text,
            content,
            Status::Done,
            content_type,
        )
        .map(|id| id.to_key())
        .map_err(|e| e.to_string())
}

fn result_json(key: &str) -> String {
    serde_json::json!({ "block_id": key }).to_string()
}

// ============================================================================
// svg_block
// ============================================================================

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SvgBlockParams {
    /// SVG content (`<svg>...</svg>`).
    pub content: String,
}

/// Insert an SVG block at the end of the current context.
pub struct SvgBlockEngine {
    documents: SharedBlockStore,
}

impl SvgBlockEngine {
    pub fn new(documents: SharedBlockStore) -> Self {
        Self { documents }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "SVG markup (e.g. `<svg viewBox=...>...</svg>`)"
                }
            },
            "required": ["content"]
        })
    }
}


impl SvgBlockEngine {
    pub fn description(&self) -> &str {
        "Append an SVG block to the current context. Renders as vector graphics inline."
    }

    pub async fn execute(&self, params: &str, ctx: &ExecContext) -> anyhow::Result<ExecResult> {
        let params: SvgBlockParams = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid parameters: {e}"))),
        };
        match append_block(
            &self.documents,
            ctx,
            Role::Tool,
            &params.content,
            ContentType::Svg,
        ) {
            Ok(key) => {
                tracing::info!("svg_block inserted {} ({} bytes)", key, params.content.len());
                Ok(ExecResult::success(result_json(&key)))
            }
            Err(e) => Ok(ExecResult::failure(1, e)),
        }
    }

}

// ============================================================================
// abc_block
// ============================================================================

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AbcBlockParams {
    /// ABC music notation text.
    pub content: String,
}

/// Insert an ABC music notation block. Validates parse before inserting.
pub struct AbcBlockEngine {
    documents: SharedBlockStore,
}

impl AbcBlockEngine {
    pub fn new(documents: SharedBlockStore) -> Self {
        Self { documents }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "ABC notation (e.g. `X:1\\nT:Title\\nK:C\\nCDEF GABc`)"
                }
            },
            "required": ["content"]
        })
    }
}


impl AbcBlockEngine {
    pub fn description(&self) -> &str {
        "Append an ABC music notation block. Validates parse; renders as sheet music inline."
    }

    pub async fn execute(&self, params: &str, ctx: &ExecContext) -> anyhow::Result<ExecResult> {
        let params: AbcBlockParams = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid parameters: {e}"))),
        };

        // Validate ABC parses
        let parse = kaijutsu_abc::parse(&params.content);
        if parse.has_errors() {
            let errs: Vec<_> = parse.errors().map(|e| e.message.clone()).collect();
            return Ok(ExecResult::failure(
                1,
                format!("ABC parse error: {}", errs.join("; ")),
            ));
        }

        match append_block(
            &self.documents,
            ctx,
            Role::Tool,
            &params.content,
            ContentType::Abc,
        ) {
            Ok(key) => {
                tracing::info!("abc_block inserted {} ({} bytes)", key, params.content.len());
                Ok(ExecResult::success(result_json(&key)))
            }
            Err(e) => Ok(ExecResult::failure(1, e)),
        }
    }

}

// ============================================================================
// img_block (via existing CAS hash)
// ============================================================================

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImgBlockParams {
    /// Hex-encoded CAS hash of an image already stored in the CAS.
    pub hash: String,
}

/// Insert an image block referencing content already in the CAS.
pub struct ImgBlockEngine {
    documents: SharedBlockStore,
}

impl ImgBlockEngine {
    pub fn new(documents: SharedBlockStore) -> Self {
        Self { documents }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "hash": {
                    "type": "string",
                    "description": "Hex CAS hash (content must already be stored)"
                }
            },
            "required": ["hash"]
        })
    }
}


impl ImgBlockEngine {
    pub fn description(&self) -> &str {
        "Append an image block referencing content already in the CAS by hash."
    }

    pub async fn execute(&self, params: &str, ctx: &ExecContext) -> anyhow::Result<ExecResult> {
        let params: ImgBlockParams = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid parameters: {e}"))),
        };

        if params.hash.parse::<ContentHash>().is_err() {
            return Ok(ExecResult::failure(1, format!("invalid hash: {}", params.hash)));
        }

        match append_block(
            &self.documents,
            ctx,
            Role::Asset,
            &params.hash,
            ContentType::Image,
        ) {
            Ok(key) => {
                tracing::info!("img_block inserted {} (hash={})", key, params.hash);
                Ok(ExecResult::success(result_json(&key)))
            }
            Err(e) => Ok(ExecResult::failure(1, e)),
        }
    }

}

// ============================================================================
// img_block_from_path (read file → CAS → block)
// ============================================================================

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImgBlockFromPathParams {
    /// Filesystem path to an image file.
    pub path: String,
}

/// Read an image from disk, store it in the CAS, and append an image block.
pub struct ImgBlockFromPathEngine {
    documents: SharedBlockStore,
    cas: Arc<FileStore>,
}

impl ImgBlockFromPathEngine {
    pub fn new(documents: SharedBlockStore, cas: Arc<FileStore>) -> Self {
        Self { documents, cas }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Filesystem path to image file (png/jpg/webp/gif/avif/svg)"
                }
            },
            "required": ["path"]
        })
    }
}


impl ImgBlockFromPathEngine {
    pub fn description(&self) -> &str {
        "Read an image file, store it in the CAS, and append an image block."
    }

    pub async fn execute(&self, params: &str, ctx: &ExecContext) -> anyhow::Result<ExecResult> {
        let params: ImgBlockFromPathParams = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid parameters: {e}"))),
        };

        let data = match std::fs::read(&params.path) {
            Ok(d) => d,
            Err(e) => {
                return Ok(ExecResult::failure(
                    1,
                    format!("read error {}: {}", params.path, e),
                ));
            }
        };

        let mime = mime_from_extension(&params.path);
        let hash = match self.cas.store(&data, mime) {
            Ok(h) => h,
            Err(e) => return Ok(ExecResult::failure(1, format!("CAS error: {e}"))),
        };
        let hash_str = hash.to_string();

        match append_block(
            &self.documents,
            ctx,
            Role::Asset,
            &hash_str,
            ContentType::Image,
        ) {
            Ok(key) => {
                tracing::info!("img_block_from_path inserted {} (hash={})", key, hash_str);
                Ok(ExecResult::success(result_json(&key)))
            }
            Err(e) => Ok(ExecResult::failure(1, e)),
        }
    }

}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store;
    use kaijutsu_types::{ContextId, KernelId, PrincipalId, SessionId};
    use std::path::PathBuf;

    fn test_ctx(context_id: ContextId) -> ExecContext {
        ExecContext {
            principal_id: PrincipalId::system(),
            context_id,
            cwd: PathBuf::from("/"),
            session_id: SessionId::new(),
            kernel_id: KernelId::new(),
        }
    }

    #[tokio::test]
    async fn test_svg_block_inserts_svg_content_type() {
        let blocks = shared_block_store(PrincipalId::system());
        let ctx_id = ContextId::new();
        // Create the document
        blocks
            .create_document(ctx_id, crate::block_store::DocumentKind::Conversation, None)
            .unwrap();

        let engine = SvgBlockEngine::new(blocks.clone());
        let tc = test_ctx(ctx_id);

        let params = serde_json::json!({
            "content": "<svg viewBox='0 0 10 10'><circle cx='5' cy='5' r='3'/></svg>"
        })
        .to_string();

        let result = engine.execute(&params, &tc).await.unwrap();
        assert!(result.success);

        // Verify the block exists with ContentType::Svg
        let doc = blocks.get(ctx_id).unwrap();
        let ordered = doc.doc.blocks_ordered();
        let last = ordered.last().unwrap();
        assert_eq!(last.content_type, ContentType::Svg);
        assert!(last.content.contains("<svg"));
    }

    #[tokio::test]
    async fn test_abc_block_validates_parse() {
        let blocks = shared_block_store(PrincipalId::system());
        let ctx_id = ContextId::new();
        blocks
            .create_document(ctx_id, crate::block_store::DocumentKind::Conversation, None)
            .unwrap();

        let engine = AbcBlockEngine::new(blocks.clone());
        let tc = test_ctx(ctx_id);

        // Valid ABC
        let params = serde_json::json!({
            "content": "X:1\nT:Test\nK:C\nCDEF GABc"
        })
        .to_string();
        let result = engine.execute(&params, &tc).await.unwrap();
        assert!(result.success, "expected success, got: {}", result.stderr);

        let doc = blocks.get(ctx_id).unwrap();
        let ordered = doc.doc.blocks_ordered();
        let last = ordered.last().unwrap();
        assert_eq!(last.content_type, ContentType::Abc);
    }

    #[tokio::test]
    async fn test_img_block_rejects_invalid_hash() {
        let blocks = shared_block_store(PrincipalId::system());
        let ctx_id = ContextId::new();
        blocks
            .create_document(ctx_id, crate::block_store::DocumentKind::Conversation, None)
            .unwrap();

        let engine = ImgBlockEngine::new(blocks);
        let tc = test_ctx(ctx_id);

        let params = serde_json::json!({ "hash": "not-a-hash" }).to_string();
        let result = engine.execute(&params, &tc).await.unwrap();
        assert!(!result.success);
        assert!(result.stderr.contains("invalid hash"));
    }
}
