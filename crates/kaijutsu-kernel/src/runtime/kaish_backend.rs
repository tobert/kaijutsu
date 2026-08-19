//! KaijutsuBackend: kaish KernelBackend implementation backed by kernel blocks.
//!
//! This backend maps kaish file operations to kaijutsu's kernel block store,
//! enabling collaborative editing through the shell interface.
//!
//! # Architecture
//!
//! ```text
//! kaish builtin (cat, ls, echo, etc.)
//!     ↓
//! ctx.backend.read() / write() / etc.
//!     ↓
//! KaijutsuBackend
//!     ├── File ops → BlockStore
//!     └── Tool calls → ToolRegistry (ExecutionEngines)
//! ```
//!
//! # Path Mapping
//!
//! VFS paths map to blocks as follows:
//!
//! - `/docs/{ctx_hex}` - List blocks in a document
//! - `/docs/{ctx_hex}/{block_key}` - Access a specific block's content
//! - `/docs/{ctx_hex}/_meta` - Document metadata (kind, language)
//!
//! Where `ctx_hex` is the 32-char hex representation of a ContextId.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value as JsonValue;

use kaijutsu_types::BlockId;
use crate::Kernel as KaijutsuKernel;
use crate::block_store::SharedBlockStore;
use crate::ExecResult;
use kaijutsu_types::DocKind;
use kaijutsu_types::{ContextId, PrincipalId, SessionId};

/// Minimal name/description tuple for converting a broker-visible tool into
/// kaish's `ToolInfo`. The full tool metadata lives on `KernelTool`; this
/// local shape is just what `convert_tool_info` consumes.
struct KaijutsuToolInfo {
    name: String,
    description: String,
}

impl KaijutsuToolInfo {
    fn new(name: impl Into<String>, description: impl Into<String>, _category: &str) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

use kaish_kernel::tools::{ParamSchema, ToolArgs, ToolCtx, ToolSchema};
use kaish_kernel::vfs::{DirEntry, MountInfo};
use kaish_kernel::{
    BackendError, BackendResult, KernelBackend, PatchOp, ReadRange, ToolInfo, ToolResult, WriteMode,
};

use super::context_engine::{SessionContextExt, SessionContextMap};

/// Backend that routes kaish operations to kaijutsu's kernel block store.
///
/// File operations become block operations:
/// - `cat /docs/{ctx_hex}/block-key` → read block content
/// - `echo "text" >> /docs/{ctx_hex}/block-key` → append to block
/// - `ls /docs/` → list documents
///
/// Tool calls route through kaijutsu's Kernel which includes:
/// - Block tools (block_create, block_edit, etc.)
/// - MCP tools (when McpServerPool is implemented)
pub struct KaijutsuBackend {
    /// kernel document/block storage.
    blocks: SharedBlockStore,
    /// The kaijutsu kernel for tool dispatch.
    kernel: Arc<KaijutsuKernel>,
    /// Identity fields for bridging kaish ExecContext → kaijutsu ToolContext.
    principal_id: PrincipalId,
    /// Shared mutable context tracking map.
    session_contexts: SessionContextMap,
    session_id: SessionId,
}

impl KaijutsuBackend {
    /// Create a new backend with block store, kernel, and identity fields.
    ///
    /// Reads context switches from the global `SessionContextMap` using `session_id`.
    pub fn new(
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        principal_id: PrincipalId,
        session_contexts: SessionContextMap,
        session_id: SessionId,
    ) -> Self {
        Self {
            blocks,
            kernel,
            principal_id,
            session_contexts,
            session_id,
        }
    }

    /// Resolve a VFS path to a ContextId and optional block ID.
    ///
    /// Path formats:
    /// - `/docs` → (None, None) - docs root
    /// - `/docs/{ctx_hex}` → (Some(ctx_id), None) - document directory
    /// - `/docs/{ctx_hex}/{block_key}` → (Some(ctx_id), Some(block_id))
    /// - `/docs/{ctx_hex}/_meta` → document metadata (special case)
    fn resolve_path(&self, path: &Path) -> PathResolution {
        let path_str = path.to_string_lossy();
        let components: Vec<&str> = path_str
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();

        match components.as_slice() {
            [] => PathResolution::Root,
            ["docs"] => PathResolution::DocsRoot,
            ["docs", ctx_hex] => match ContextId::parse(ctx_hex) {
                Ok(ctx_id) => PathResolution::Document(ctx_id),
                Err(_) => PathResolution::Invalid(format!("invalid context ID: {}", ctx_hex)),
            },
            ["docs", ctx_hex, "_meta"] => match ContextId::parse(ctx_hex) {
                Ok(ctx_id) => PathResolution::DocumentMeta(ctx_id),
                Err(_) => PathResolution::Invalid(format!("invalid context ID: {}", ctx_hex)),
            },
            ["docs", ctx_hex, block_key] => match ContextId::parse(ctx_hex) {
                Ok(ctx_id) => {
                    if let Some(block_id) = BlockId::from_key(block_key) {
                        PathResolution::Block(ctx_id, block_id)
                    } else {
                        PathResolution::Invalid(format!("invalid block key: {}", block_key))
                    }
                }
                Err(_) => PathResolution::Invalid(format!("invalid context ID: {}", ctx_hex)),
            },
            _ => PathResolution::Invalid(format!("unsupported path: {}", path_str)),
        }
    }

    /// Convert kaijutsu ToolInfo to kaish ToolInfo format.
    ///
    /// When a JSON Schema is provided (from the engine), converts its properties
    /// to kaish `ParamSchema` entries so that positional→named mapping works.
    fn convert_tool_info(
        info: &KaijutsuToolInfo,
        json_schema: Option<serde_json::Value>,
    ) -> ToolInfo {
        let mut schema = ToolSchema::new(&info.name, &info.description);

        if let Some(js) = json_schema
            && let Some(props) = js.get("properties").and_then(|p| p.as_object())
        {
            let required: Vec<&str> = js
                .get("required")
                .and_then(|r| r.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();

            // Add required params first (in `required` array order) so positional
            // mapping assigns args to the right params regardless of JSON key order.
            for &req_name in &required {
                if let Some(prop) = props.get(req_name) {
                    let param_type = prop
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("string");
                    let desc = prop
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("");
                    schema = schema.param(ParamSchema::required(req_name, param_type, desc));
                }
            }

            // Then optional params
            for (name, prop) in props {
                if required.contains(&name.as_str()) {
                    continue; // Already added above
                }
                let param_type = prop
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("string");
                let desc = prop
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                let default = prop
                    .get("default")
                    .cloned()
                    .map(json_to_kaish_value)
                    .unwrap_or(kaish_kernel::ast::Value::Null);
                schema = schema.param(ParamSchema::optional(name, param_type, default, desc));
            }
        }

        ToolInfo {
            name: info.name.clone(),
            description: info.description.clone(),
            schema,
        }
    }

    /// Convert ExecResult to ToolResult.
    fn convert_exec_result(result: ExecResult) -> ToolResult {
        if result.success {
            ToolResult::success(result.stdout)
        } else {
            ToolResult::failure(result.exit_code, result.stderr)
        }
    }
}

/// Result of resolving a VFS path.
#[derive(Debug)]
enum PathResolution {
    /// Root of the VFS (`/`)
    Root,
    /// Documents root (`/docs`)
    DocsRoot,
    /// A specific document (`/docs/{ctx_hex}`)
    Document(ContextId),
    /// Document metadata (`/docs/{ctx_hex}/_meta`)
    DocumentMeta(ContextId),
    /// A specific block (`/docs/{ctx_hex}/{block_key}`)
    Block(ContextId, BlockId),
    /// Invalid or unsupported path
    Invalid(String),
}

#[async_trait]
impl KernelBackend for KaijutsuBackend {
    // =========================================================================
    // File Operations
    // =========================================================================

    async fn read(&self, path: &Path, range: Option<ReadRange>) -> BackendResult<Vec<u8>> {
        match self.resolve_path(path) {
            PathResolution::Root => {
                // List top-level directories
                Ok(b"docs/\n".to_vec())
            }
            PathResolution::DocsRoot => {
                // List all documents
                let ctx_ids = self.blocks.list_ids();
                let listing: String = ctx_ids
                    .iter()
                    .map(|id| format!("{}\n", id.to_hex()))
                    .collect();
                Ok(listing.into_bytes())
            }
            PathResolution::Document(ctx_id) => {
                // List blocks in document
                let entry = self.blocks.get(ctx_id).ok_or_else(|| {
                    BackendError::NotFound(format!("document not found: {}", ctx_id.to_hex()))
                })?;
                let blocks = entry.doc.blocks_ordered();
                let listing: Vec<String> = blocks.iter().map(|b| b.id.to_key()).collect();
                Ok((listing.join("\n") + "\n").into_bytes())
            }
            PathResolution::DocumentMeta(ctx_id) => {
                // Return document metadata as JSON
                let entry = self.blocks.get(ctx_id).ok_or_else(|| {
                    BackendError::NotFound(format!("document not found: {}", ctx_id.to_hex()))
                })?;
                let meta = serde_json::json!({
                    "id": ctx_id.to_hex(),
                    "kind": format!("{:?}", entry.kind),
                    "language": entry.language,
                    "version": entry.version(),
                });
                let json = serde_json::to_string_pretty(&meta)
                    .map_err(|e| BackendError::Io(e.to_string()))?;
                Ok(json.into_bytes())
            }
            PathResolution::Block(ctx_id, block_id) => {
                // Read block content
                let entry = self.blocks.get(ctx_id).ok_or_else(|| {
                    BackendError::NotFound(format!("document not found: {}", ctx_id.to_hex()))
                })?;

                // Find the block and get its content
                let blocks = entry.doc.blocks_ordered();
                let block = blocks.iter().find(|b| b.id == block_id).ok_or_else(|| {
                    BackendError::NotFound(format!("block not found: {}", block_id.to_key()))
                })?;

                let content = &block.content;

                // Apply range if specified
                let output = if let Some(range) = range {
                    apply_read_range(content, range)
                } else {
                    content.clone()
                };

                Ok(output.into_bytes())
            }
            PathResolution::Invalid(msg) => Err(BackendError::InvalidOperation(msg)),
        }
    }

    async fn write(&self, path: &Path, content: &[u8], mode: WriteMode) -> BackendResult<()> {
        let content_str =
            std::str::from_utf8(content).map_err(|e| BackendError::Io(e.to_string()))?;

        match self.resolve_path(path) {
            PathResolution::Document(ctx_id) => {
                // Create document if CreateNew or Overwrite
                match mode {
                    WriteMode::CreateNew => {
                        if self.blocks.contains(ctx_id) {
                            return Err(BackendError::AlreadyExists(ctx_id.to_hex()));
                        }
                        self.blocks
                            .create_document(ctx_id, DocKind::File, None)
                            .map_err(|e| BackendError::Io(e.to_string()))?;
                    }
                    WriteMode::UpdateOnly => {
                        if !self.blocks.contains(ctx_id) {
                            return Err(BackendError::NotFound(ctx_id.to_hex()));
                        }
                    }
                    WriteMode::Overwrite | WriteMode::Truncate => {
                        if !self.blocks.contains(ctx_id) {
                            self.blocks
                                .create_document(ctx_id, DocKind::File, None)
                                .map_err(|e| BackendError::Io(e.to_string()))?;
                        }
                    }
                    _ => {
                        return Err(BackendError::InvalidOperation(
                            "unsupported write mode".into(),
                        ));
                    }
                }
                let _ = content_str; // content unused for document-level writes
                Ok(())
            }
            PathResolution::Block(ctx_id, block_id) => {
                // Write to block content
                if !self.blocks.contains(ctx_id) {
                    return Err(BackendError::NotFound(format!(
                        "document not found: {}",
                        ctx_id.to_hex()
                    )));
                }

                // For blocks, we need to replace the content
                // First get current content length, then edit
                let current_len = {
                    let entry = self.blocks.get(ctx_id).ok_or_else(|| {
                        BackendError::NotFound(format!("document not found: {}", ctx_id.to_hex()))
                    })?;
                    let blocks = entry.doc.blocks_ordered();
                    blocks
                        .iter()
                        .find(|b| b.id == block_id)
                        .map(|b| b.content.len())
                        .ok_or_else(|| {
                            BackendError::NotFound(format!(
                                "block not found: {}",
                                block_id.to_key()
                            ))
                        })?
                };

                // Delete all content then insert new content
                self.blocks
                    .edit_text(ctx_id, &block_id, 0, content_str, current_len)
                    .map_err(|e| BackendError::Io(e.to_string()))?;

                Ok(())
            }
            PathResolution::DocsRoot | PathResolution::Root => Err(BackendError::IsDirectory(
                path.to_string_lossy().to_string(),
            )),
            PathResolution::DocumentMeta(_) => Err(BackendError::PermissionDenied(
                "cannot write to _meta".into(),
            )),
            PathResolution::Invalid(msg) => Err(BackendError::InvalidOperation(msg)),
        }
    }

    async fn append(&self, path: &Path, content: &[u8]) -> BackendResult<()> {
        let content_str =
            std::str::from_utf8(content).map_err(|e| BackendError::Io(e.to_string()))?;

        match self.resolve_path(path) {
            PathResolution::Block(ctx_id, block_id) => {
                self.blocks
                    .append_text(ctx_id, &block_id, content_str)
                    .map_err(|e| BackendError::Io(e.to_string()))?;
                Ok(())
            }
            PathResolution::Document(_) | PathResolution::DocsRoot | PathResolution::Root => Err(
                BackendError::IsDirectory(path.to_string_lossy().to_string()),
            ),
            PathResolution::DocumentMeta(_) => Err(BackendError::PermissionDenied(
                "cannot append to _meta".into(),
            )),
            PathResolution::Invalid(msg) => Err(BackendError::InvalidOperation(msg)),
        }
    }

    async fn patch(&self, path: &Path, ops: &[PatchOp]) -> BackendResult<()> {
        match self.resolve_path(path) {
            PathResolution::Block(ctx_id, block_id) => {
                // Get current content for offset calculations
                let original_content = {
                    let entry = self.blocks.get(ctx_id).ok_or_else(|| {
                        BackendError::NotFound(format!("document not found: {}", ctx_id.to_hex()))
                    })?;
                    let blocks = entry.doc.blocks_ordered();
                    blocks
                        .iter()
                        .find(|b| b.id == block_id)
                        .map(|b| b.content.clone())
                        .ok_or_else(|| {
                            BackendError::NotFound(format!(
                                "block not found: {}",
                                block_id.to_key()
                            ))
                        })?
                };

                // Compute the WHOLE batch against an in-memory String first —
                // no storage call happens until every op (including every
                // CAS precondition) has validated. A failure on op N leaves
                // `original_content` completely unreferenced from here on;
                // nothing has been journaled.
                let mut new_content = original_content.clone();
                for op in ops {
                    new_content = compute_patch_op(op, &new_content)?;
                }

                // Commit once: a single char-indexed splice replacing the
                // whole block (block text positions are chars, not bytes —
                // same shape as `reload_block_from_disk` in
                // file_tools/cache.rs). Skipped when the batch is a true
                // no-op (empty `ops`, or ops whose net effect is identity) so
                // an unchanged block never gets a spurious journal entry.
                if new_content != original_content {
                    self.blocks
                        .edit_text(
                            ctx_id,
                            &block_id,
                            0,
                            &new_content,
                            original_content.chars().count(),
                        )
                        .map_err(|e| BackendError::Io(e.to_string()))?;
                }

                Ok(())
            }
            PathResolution::Document(_) | PathResolution::DocsRoot | PathResolution::Root => Err(
                BackendError::IsDirectory(path.to_string_lossy().to_string()),
            ),
            PathResolution::DocumentMeta(_) => {
                Err(BackendError::PermissionDenied("cannot patch _meta".into()))
            }
            PathResolution::Invalid(msg) => Err(BackendError::InvalidOperation(msg)),
        }
    }

    // =========================================================================
    // Directory Operations
    // =========================================================================

    async fn list(&self, path: &Path) -> BackendResult<Vec<DirEntry>> {
        match self.resolve_path(path) {
            PathResolution::Root => Ok(vec![DirEntry::directory("docs")]),
            PathResolution::DocsRoot => {
                let entries = self
                    .blocks
                    .list_ids()
                    .into_iter()
                    .map(|id| DirEntry::directory(id.to_hex()))
                    .collect();
                Ok(entries)
            }
            PathResolution::Document(ctx_id) => {
                let entry = self.blocks.get(ctx_id).ok_or_else(|| {
                    BackendError::NotFound(format!("document not found: {}", ctx_id.to_hex()))
                })?;
                let blocks = entry.doc.blocks_ordered();
                let mut entries: Vec<DirEntry> = blocks
                    .iter()
                    .map(|b| DirEntry::file(b.id.to_key(), b.content.len() as u64))
                    .collect();
                // Add _meta pseudo-file
                entries.push(DirEntry::file("_meta", 0));
                Ok(entries)
            }
            PathResolution::Block(_, _) | PathResolution::DocumentMeta(_) => Err(
                BackendError::NotDirectory(path.to_string_lossy().to_string()),
            ),
            PathResolution::Invalid(msg) => Err(BackendError::InvalidOperation(msg)),
        }
    }

    async fn stat(&self, path: &Path) -> BackendResult<DirEntry> {
        match self.resolve_path(path) {
            PathResolution::Root => Ok(DirEntry::directory("/")),
            PathResolution::DocsRoot => Ok(DirEntry::directory("docs")),
            PathResolution::Document(ctx_id) => {
                if self.blocks.contains(ctx_id) {
                    Ok(DirEntry::directory(ctx_id.to_hex()))
                } else {
                    Err(BackendError::NotFound(ctx_id.to_hex()))
                }
            }
            PathResolution::DocumentMeta(ctx_id) => {
                if self.blocks.contains(ctx_id) {
                    Ok(DirEntry::file("_meta", 0))
                } else {
                    Err(BackendError::NotFound(ctx_id.to_hex()))
                }
            }
            PathResolution::Block(ctx_id, block_id) => {
                let entry = self.blocks.get(ctx_id).ok_or_else(|| {
                    BackendError::NotFound(format!("document not found: {}", ctx_id.to_hex()))
                })?;
                let blocks = entry.doc.blocks_ordered();
                let block = blocks.iter().find(|b| b.id == block_id).ok_or_else(|| {
                    BackendError::NotFound(format!("block not found: {}", block_id.to_key()))
                })?;
                Ok(DirEntry::file(
                    block_id.to_key(),
                    block.content.len() as u64,
                ))
            }
            PathResolution::Invalid(msg) => Err(BackendError::InvalidOperation(msg)),
        }
    }

    async fn lstat(&self, path: &Path) -> BackendResult<DirEntry> {
        self.stat(path).await
    }

    async fn mkdir(&self, path: &Path) -> BackendResult<()> {
        match self.resolve_path(path) {
            PathResolution::Document(ctx_id) => {
                if self.blocks.contains(ctx_id) {
                    return Err(BackendError::AlreadyExists(ctx_id.to_hex()));
                }
                self.blocks
                    .create_document(ctx_id, DocKind::File, None)
                    .map_err(|e| BackendError::Io(e.to_string()))?;
                Ok(())
            }
            PathResolution::Root | PathResolution::DocsRoot => Err(BackendError::AlreadyExists(
                path.to_string_lossy().to_string(),
            )),
            PathResolution::Block(_, _) | PathResolution::DocumentMeta(_) => Err(
                BackendError::InvalidOperation("cannot mkdir on block or meta".into()),
            ),
            PathResolution::Invalid(msg) => Err(BackendError::InvalidOperation(msg)),
        }
    }

    async fn set_mtime(&self, path: &Path, _mtime: std::time::SystemTime) -> BackendResult<()> {
        // The kaijutsu:// document namespace is purely virtual — context/block/
        // doc rows derive their timing from block ticks, not a settable mtime.
        // Per the KernelBackend contract a virtual mount rejects rather than
        // silently succeeding, so `touch` never quietly no-ops here.
        Err(BackendError::InvalidOperation(format!(
            "set_mtime: {} is a virtual kaijutsu document; mtime is not settable",
            path.display()
        )))
    }

    async fn remove(&self, path: &Path, recursive: bool) -> BackendResult<()> {
        match self.resolve_path(path) {
            PathResolution::Document(ctx_id) => {
                if !self.blocks.contains(ctx_id) {
                    return Err(BackendError::NotFound(ctx_id.to_hex()));
                }
                // Check if document has blocks and recursive is false
                if !recursive {
                    let entry = self.blocks.get(ctx_id).ok_or_else(|| {
                        BackendError::NotFound(format!("document not found: {}", ctx_id.to_hex()))
                    })?;
                    if !entry.doc.blocks_ordered().is_empty() {
                        return Err(BackendError::InvalidOperation(
                            "document not empty, use recursive=true".into(),
                        ));
                    }
                }
                self.blocks
                    .delete_document(ctx_id)
                    .map_err(|e| BackendError::Io(e.to_string()))?;
                Ok(())
            }
            PathResolution::Block(ctx_id, block_id) => {
                self.blocks
                    .delete_block(ctx_id, &block_id)
                    .map_err(|e| BackendError::Io(e.to_string()))?;
                Ok(())
            }
            PathResolution::Root | PathResolution::DocsRoot => Err(BackendError::PermissionDenied(
                "cannot remove root directories".into(),
            )),
            PathResolution::DocumentMeta(_) => {
                Err(BackendError::PermissionDenied("cannot remove _meta".into()))
            }
            PathResolution::Invalid(msg) => Err(BackendError::InvalidOperation(msg)),
        }
    }

    async fn exists(&self, path: &Path) -> bool {
        match self.resolve_path(path) {
            PathResolution::Root | PathResolution::DocsRoot => true,
            PathResolution::Document(ctx_id) => self.blocks.contains(ctx_id),
            PathResolution::DocumentMeta(ctx_id) => self.blocks.contains(ctx_id),
            PathResolution::Block(ctx_id, block_id) => {
                if let Some(entry) = self.blocks.get(ctx_id) {
                    entry.doc.blocks_ordered().iter().any(|b| b.id == block_id)
                } else {
                    false
                }
            }
            PathResolution::Invalid(_) => false,
        }
    }

    async fn rename(&self, from: &Path, to: &Path) -> BackendResult<()> {
        match (self.resolve_path(from), self.resolve_path(to)) {
            (PathResolution::Block(from_ctx, _from_id), PathResolution::Block(to_ctx, _to_id))
                if from_ctx == to_ctx =>
            {
                Err(BackendError::InvalidOperation(
                    "block rename not supported - use block_create + block_delete".into(),
                ))
            }
            _ => Err(BackendError::InvalidOperation(
                "rename only supported within same document".into(),
            )),
        }
    }

    // Symlinks are unsupported in *this* backend on purpose: it serves the
    // `/docs/{ctx_hex}/{block}` conversation-block scheme, where a link between
    // blocks has no meaning. This is NOT the rc path — `ln -s /etc/rc/…` routes
    // through MountBackend → MountTable → ConfigDocFs, which does support links
    // (init.d-style rc composition). Failing loud here keeps the two schemes
    // from quietly conflating.
    async fn read_link(&self, _path: &Path) -> BackendResult<std::path::PathBuf> {
        Err(BackendError::InvalidOperation(
            "symlinks not supported on conversation blocks (/docs); use /etc/rc for rc composition".into(),
        ))
    }

    async fn symlink(&self, _target: &Path, _link: &Path) -> BackendResult<()> {
        Err(BackendError::InvalidOperation(
            "symlinks not supported on conversation blocks (/docs); use /etc/rc for rc composition".into(),
        ))
    }

    fn resolve_real_path(&self, _path: &Path) -> Option<std::path::PathBuf> {
        None
    }

    // =========================================================================
    // Tool Dispatch
    // =========================================================================

    async fn call_tool(
        &self,
        name: &str,
        args: ToolArgs,
        ctx: &mut dyn ToolCtx,
    ) -> BackendResult<ToolResult> {
        let params_json = tool_args_to_json(&args);
        let params_str =
            serde_json::to_string(&params_json).map_err(|e| BackendError::Io(e.to_string()))?;

        // Bridge kaish ExecContext → kaijutsu ToolContext.
        // Uses kaish's cwd so file-relative operations (glob, grep) scope correctly.
        // context_id is read from the session map so context switches propagate.
        let context_id = self
            .session_contexts
            .current(&self.session_id)
            .ok_or_else(|| BackendError::Io("no active context joined".to_string()))?;
        let tool_ctx = crate::ExecContext::new(
            self.principal_id,
            context_id,
            ctx.cwd().to_path_buf(),
            self.session_id,
            self.kernel.id(),
        );

        // Phase 1 M4: dispatch through the MCP broker.
        let result = self
            .kernel
            .dispatch_tool_via_broker(name, &params_str, &tool_ctx)
            .await
            .map_err(|e| match e {
                crate::mcp::McpError::ToolNotFound { tool, .. } => {
                    BackendError::ToolNotFound(tool)
                }
                // Central dispatch's never-resolved-anywhere case (typo or
                // hallucinated name) — same "no such tool" outcome from this
                // caller's point of view as `ToolNotFound` above, just
                // carrying the visible-tool-list detail `McpError`'s Display
                // already renders into `other.to_string()` for the message.
                crate::mcp::McpError::UnknownToolName { tool, .. } => {
                    BackendError::ToolNotFound(tool)
                }
                other => BackendError::Io(other.to_string()),
            })?;

        Ok(Self::convert_exec_result(result))
    }

    async fn list_tools(&self) -> BackendResult<Vec<ToolInfo>> {
        // Phase 1 M4: enumerate through the broker's registered servers.
        let tools = self.kernel.list_all_registered_tools().await;
        let mut infos = Vec::with_capacity(tools.len());
        for (name, _instance, schema, description) in tools {
            let info = KaijutsuToolInfo::new(
                &name,
                description.clone().unwrap_or_default(),
                "mcp",
            );
            infos.push(Self::convert_tool_info(&info, Some(schema)));
        }
        Ok(infos)
    }

    async fn get_tool(&self, name: &str) -> BackendResult<Option<ToolInfo>> {
        let tools = self.kernel.list_all_registered_tools().await;
        for (tool_name, _instance, schema, description) in tools {
            if tool_name == name {
                let info = KaijutsuToolInfo::new(
                    &tool_name,
                    description.unwrap_or_default(),
                    "mcp",
                );
                return Ok(Some(Self::convert_tool_info(&info, Some(schema))));
            }
        }
        Ok(None)
    }

    // =========================================================================
    // Backend Information
    // =========================================================================

    fn read_only(&self) -> bool {
        false
    }

    fn backend_type(&self) -> &str {
        "kaijutsu"
    }

    fn mounts(&self) -> Vec<MountInfo> {
        vec![MountInfo {
            path: std::path::PathBuf::from("/docs"),
            read_only: false,
            // kernel-owned mount; residency lives in the BlockStore, not tracked here.
            resident_bytes: None,
        }]
    }
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Apply a read range to content, returning the subset.
fn apply_read_range(content: &str, range: ReadRange) -> String {
    if range.start_line.is_some() || range.end_line.is_some() {
        let lines: Vec<&str> = content.lines().collect();
        let start = range.start_line.unwrap_or(1).saturating_sub(1);
        let end = range.end_line.unwrap_or(lines.len()).min(lines.len());
        return lines
            .get(start..end)
            .map(|slice| slice.join("\n"))
            .unwrap_or_default();
    }

    if range.offset.is_some() || range.limit.is_some() {
        let offset = range.offset.unwrap_or(0) as usize;
        let limit = range.limit.unwrap_or(content.len() as u64) as usize;
        let end = (offset + limit).min(content.len());
        return content.get(offset..end).unwrap_or("").to_string();
    }

    content.to_string()
}

/// Ensure a wire BYTE offset lands on a UTF-8 char boundary in `content`.
///
/// `PatchOp::Insert`/`Delete`/`Replace` offsets are BYTES by the kaish-types
/// contract ("Insert content at byte offset", "Delete bytes …"), and the
/// in-memory mirror ops in [`compute_patch_op`] (`insert_str`,
/// `replace_range`) panic — rather than failing gracefully — on a
/// non-boundary index. Every offset is checked here before it reaches them,
/// so a mid-char offset is a loud `BackendError`, never a panic partway
/// through a batch: the old path spliced block text at a bogus char position
/// and then panicked in the byte mirror's `replace_range`, leaving the
/// durable block corrupted behind the crash.
fn check_byte_boundary(content: &str, byte: usize, what: &str) -> BackendResult<()> {
    if byte > content.len() || !content.is_char_boundary(byte) {
        return Err(BackendError::Io(format!(
            "patch {what}: byte offset {byte} is not a char boundary in {}-byte content",
            content.len()
        )));
    }
    Ok(())
}

/// Compute the result of applying one patch op to `current_content` — pure,
/// no storage access, no side effects. `patch()` folds every op in a batch
/// through this over an in-memory `String`, so every op (including every
/// `expected` CAS precondition) is validated before anything is committed.
///
/// This mirrors kaish's own `LocalBackend::apply_patch_op` shape (mutate a
/// local buffer, the backend writes once) rather than the old per-op-commits
/// shape this function replaced, where every op reached
/// `BlockStore::edit_text`/`append_text` directly and journaled to the
/// durable oplog on the spot — so a CAS failure on op N of a batch left ops
/// 1..N-1 durably committed with nothing to roll them back.
///
/// Byte-domain throughout: offsets are wire BYTES (kaish-types contract) and
/// the mirror ops below operate on that domain directly. There is no
/// char-indexed storage call in here at all — `patch()` performs the one
/// required byte→char projection when it commits the whole batch's result as
/// a single `edit_text` splice (block text positions are chars, not bytes).
fn compute_patch_op(op: &PatchOp, current_content: &str) -> BackendResult<String> {
    match op {
        PatchOp::Insert { offset, content } => {
            check_byte_boundary(current_content, *offset, "insert")?;
            let mut result = current_content.to_string();
            result.insert_str(*offset, content);
            Ok(result)
        }
        PatchOp::Delete {
            offset,
            len,
            expected,
        } => {
            // Validate boundaries BEFORE the CAS check: a bogus offset
            // should report as the boundary error it is, not as a
            // misleading conflict against the empty string `.get()` yields.
            check_byte_boundary(current_content, *offset, "delete")?;
            check_byte_boundary(current_content, offset + len, "delete")?;
            if let Some(exp) = expected {
                let actual = current_content.get(*offset..*offset + *len).unwrap_or("");
                if actual != exp {
                    return Err(BackendError::Conflict(
                        kaish_kernel::backend::ConflictError {
                            location: format!("offset {}", offset),
                            expected: exp.clone(),
                            actual: actual.to_string(),
                        },
                    ));
                }
            }
            let mut result = current_content.to_string();
            result.replace_range(*offset..*offset + *len, "");
            Ok(result)
        }
        PatchOp::Replace {
            offset,
            len,
            content,
            expected,
        } => {
            check_byte_boundary(current_content, *offset, "replace")?;
            check_byte_boundary(current_content, offset + len, "replace")?;
            if let Some(exp) = expected {
                let actual = current_content.get(*offset..*offset + *len).unwrap_or("");
                if actual != exp {
                    return Err(BackendError::Conflict(
                        kaish_kernel::backend::ConflictError {
                            location: format!("offset {}", offset),
                            expected: exp.clone(),
                            actual: actual.to_string(),
                        },
                    ));
                }
            }
            let mut result = current_content.to_string();
            result.replace_range(*offset..*offset + *len, content);
            Ok(result)
        }
        PatchOp::InsertLine { line, content } => {
            // Line starts always sit on char boundaries (see line_to_byte_offset),
            // so no boundary check is needed here.
            let line_offset = line_to_byte_offset(current_content, *line);
            let mut result = current_content.to_string();
            result.insert_str(line_offset, &format!("{}\n", content));
            Ok(result)
        }
        PatchOp::DeleteLine { line, expected } => {
            let (start, end) = line_range(current_content, *line);
            let actual_line = current_content.get(start..end).unwrap_or("");

            if let Some(exp) = expected
                && actual_line.trim_end_matches('\n') != exp.trim_end_matches('\n')
            {
                return Err(BackendError::Conflict(
                    kaish_kernel::backend::ConflictError {
                        location: format!("line {}", line),
                        expected: exp.clone(),
                        actual: actual_line.to_string(),
                    },
                ));
            }

            let mut result = current_content.to_string();
            result.replace_range(start..end, "");
            Ok(result)
        }
        PatchOp::ReplaceLine {
            line,
            content,
            expected,
        } => {
            let (start, end) = line_range(current_content, *line);
            let actual_line = current_content.get(start..end).unwrap_or("");

            if let Some(exp) = expected
                && actual_line.trim_end_matches('\n') != exp.trim_end_matches('\n')
            {
                return Err(BackendError::Conflict(
                    kaish_kernel::backend::ConflictError {
                        location: format!("line {}", line),
                        expected: exp.clone(),
                        actual: actual_line.to_string(),
                    },
                ));
            }

            let replacement = format!("{}\n", content);
            let mut result = current_content.to_string();
            result.replace_range(start..end, &replacement);
            Ok(result)
        }
        PatchOp::Append { content } => Ok(format!("{}{}", current_content, content)),
    }
}

/// Get byte offset for a 1-indexed line number.
///
/// Kept LOCAL — deliberately NOT replaced by the shared
/// `block_tools/translate` helpers — because the semantics differ in two
/// load-bearing ways that are part of the kaish `PatchOp` line contract:
/// this is **1-indexed** (kaish-types: "line number (1-indexed)") where
/// translate.rs is 0-indexed, and this **clamps** a beyond-EOF line to
/// end-of-content where translate.rs errors. Swapping would silently change
/// the kaish patch surface. Outputs are BYTE offsets, consumed directly by
/// `compute_patch_op`'s byte-domain mirror ops — no char projection happens
/// here; `patch()` is the one place that projects byte to char, once, when
/// it commits the whole batch's result as a single `edit_text` splice.
fn line_to_byte_offset(content: &str, line: usize) -> usize {
    if line <= 1 {
        return 0;
    }

    let mut offset = 0;
    let mut current_line = 1;
    for (i, c) in content.char_indices() {
        if current_line >= line {
            return i;
        }
        if c == '\n' {
            current_line += 1;
        }
        offset = i + c.len_utf8();
    }
    offset
}

/// Get byte range for a 1-indexed line (includes newline if present).
fn line_range(content: &str, line: usize) -> (usize, usize) {
    let start = line_to_byte_offset(content, line);
    let mut end = start;

    for (i, c) in content[start..].char_indices() {
        end = start + i + c.len_utf8();
        if c == '\n' {
            return (start, end);
        }
    }

    (start, end)
}

/// Convert kaish ToolArgs to JSON for passing to execution engines.
fn tool_args_to_json(args: &ToolArgs) -> JsonValue {
    let mut obj = serde_json::Map::new();

    if !args.positional.is_empty() {
        let positional: Vec<JsonValue> = args.positional.iter().map(kaish_value_to_json).collect();
        obj.insert("_positional".to_string(), JsonValue::Array(positional));
    }

    for (key, value) in &args.named {
        obj.insert(key.clone(), kaish_value_to_json(value));
    }

    for flag in &args.flags {
        obj.insert(flag.clone(), JsonValue::Bool(true));
    }

    JsonValue::Object(obj)
}

/// Convert a kaish Value to JSON.
pub fn kaish_value_to_json(value: &kaish_kernel::ast::Value) -> JsonValue {
    use kaish_kernel::ast::Value;
    match value {
        Value::String(s) => JsonValue::String(s.clone()),
        Value::Int(i) => JsonValue::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        Value::Bool(b) => JsonValue::Bool(*b),
        Value::Null => JsonValue::Null,
        Value::Json(json) => json.clone(),
        // kaish 0.9: Value::Blob → Value::Bytes (inline binary). Delegate to the
        // canonical converter so we emit kaish's exact base64 envelope
        // ({_type:"bytes",encoding:"base64",data,len}) rather than re-deriving it.
        Value::Bytes(_) => kaish_kernel::interpreter::value_to_json(value),
    }
}

/// Convert a JSON value to a kaish Value (for schema defaults).
fn json_to_kaish_value(json: JsonValue) -> kaish_kernel::ast::Value {
    use kaish_kernel::ast::Value;
    match json {
        JsonValue::String(s) => Value::String(s),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        JsonValue::Bool(b) => Value::Bool(b),
        JsonValue::Null => Value::Null,
        other => Value::Json(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store;
    use kaijutsu_types::PrincipalId;

    #[tokio::test]
    async fn test_path_resolution() {
        let ctx_id = ContextId::new();
        let blocks = shared_block_store(PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test").await);
        let sid = SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        session_contexts.insert(sid, ctx_id);
        let backend = KaijutsuBackend::new(
            blocks,
            kernel,
            PrincipalId::system(),
            session_contexts,
            sid);


        // Test root paths
        assert!(matches!(
            backend.resolve_path(Path::new("/")),
            PathResolution::Root
        ));
        assert!(matches!(
            backend.resolve_path(Path::new("/docs")),
            PathResolution::DocsRoot
        ));

        // Test document paths with a valid ContextId hex
        let ctx_id = ContextId::new();
        let path_str = format!("/docs/{}", ctx_id.to_hex());
        match backend.resolve_path(Path::new(&path_str)) {
            PathResolution::Document(id) => assert_eq!(id, ctx_id),
            other => panic!("Expected Document, got {:?}", other),
        }

        // Test meta paths
        let meta_path = format!("/docs/{}/_meta", ctx_id.to_hex());
        match backend.resolve_path(Path::new(&meta_path)) {
            PathResolution::DocumentMeta(id) => assert_eq!(id, ctx_id),
            other => panic!("Expected DocumentMeta, got {:?}", other),
        }
    }

    #[test]
    fn test_line_to_byte_offset() {
        let content = "line 1\nline 2\nline 3";

        assert_eq!(line_to_byte_offset(content, 1), 0);
        assert_eq!(line_to_byte_offset(content, 2), 7); // "line 1\n" = 7 bytes
        assert_eq!(line_to_byte_offset(content, 3), 14); // "line 1\nline 2\n" = 14 bytes
    }

    #[test]
    fn test_line_range() {
        let content = "line 1\nline 2\nline 3";

        assert_eq!(line_range(content, 1), (0, 7)); // "line 1\n"
        assert_eq!(line_range(content, 2), (7, 14)); // "line 2\n"
        assert_eq!(line_range(content, 3), (14, 20)); // "line 3" (no trailing newline)
    }

    #[test]
    fn test_apply_read_range_lines() {
        let content = "line 1\nline 2\nline 3\nline 4";

        let range = ReadRange {
            start_line: Some(2),
            end_line: Some(3),
            offset: None,
            limit: None,
        };

        assert_eq!(apply_read_range(content, range), "line 2\nline 3");
    }

    #[test]
    fn test_apply_read_range_bytes() {
        let content = "hello world";

        let range = ReadRange {
            start_line: None,
            end_line: None,
            offset: Some(6),
            limit: Some(5),
        };

        assert_eq!(apply_read_range(content, range), "world");
    }

    // ── compute_patch_op × multibyte content (byte-vs-char offset regression) ──
    //
    // `blocks.edit_text` is CHAR-indexed (it bounds-checks against
    // `chars().count()` and splices at char positions). PatchOp's wire
    // contract is BYTES for Insert/Delete/Replace (kaish-types doc: "Insert
    // content at byte offset", "Delete bytes") and 1-indexed lines for the
    // *Line ops. `compute_patch_op` itself never touches `edit_text` at all —
    // it is pure, byte-domain, no storage — so these pin its byte-domain
    // correctness directly; the byte→char projection lives solely in
    // `patch()`'s single post-batch commit, covered by the `patch_*` tests
    // below that go through `backend.patch()`.

    /// A store + document + one block with the given content, for
    /// `patch_backend_fixture` to wrap in a real backend. Stored content is
    /// read back through `block_content`.
    fn patch_fixture(content: &str) -> (SharedBlockStore, ContextId, BlockId) {
        let blocks = shared_block_store(PrincipalId::system());
        let ctx_id = ContextId::new();
        blocks
            .create_document(ctx_id, DocKind::Conversation, None)
            .unwrap();
        let block_id = blocks
            .insert_block(
                ctx_id,
                None,
                None,
                kaijutsu_types::Role::User,
                kaijutsu_types::BlockKind::Text,
                content,
                kaijutsu_types::Status::Done,
                kaijutsu_types::ContentType::Plain,
            )
            .unwrap();
        (blocks, ctx_id, block_id)
    }

    fn block_content(blocks: &SharedBlockStore, ctx_id: ContextId, block_id: &BlockId) -> String {
        blocks
            .get_block_snapshot(ctx_id, block_id)
            .unwrap()
            .unwrap()
            .content
    }

    /// Full-stack fixture for exercising `KernelBackend::patch()` itself —
    /// the level batch atomicity is a property AT ALL: a real multi-op batch
    /// only ever reaches storage through `patch()`, never through
    /// `compute_patch_op` (pure, no storage) on its own. Reuses
    /// `patch_fixture`'s block, wrapped in a real `KaijutsuBackend` so
    /// `backend.patch()` runs the whole path — resolve, compute every op,
    /// commit once — exactly like the shell surface does.
    async fn patch_backend_fixture(
        content: &str,
    ) -> (
        KaijutsuBackend,
        SharedBlockStore,
        ContextId,
        BlockId,
        std::path::PathBuf,
    ) {
        let (blocks, ctx_id, block_id) = patch_fixture(content);
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test").await);
        let sid = SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        session_contexts.insert(sid, ctx_id);
        let backend = KaijutsuBackend::new(
            blocks.clone(),
            kernel,
            PrincipalId::system(),
            session_contexts,
            sid,
        );
        let path = std::path::PathBuf::from(format!(
            "/docs/{}/{}",
            ctx_id.to_hex(),
            block_id.to_key()
        ));
        (backend, blocks, ctx_id, block_id, path)
    }

    // ── batch atomicity through the real `KernelBackend::patch()` path ──
    //
    // `patch()` computes every op in a batch against an in-memory `String`
    // via `compute_patch_op` (pure, no storage) and commits once, only after
    // the whole batch has succeeded. These tests exercise `backend.patch()`
    // directly, not `compute_patch_op`, because atomicity across ops is a
    // property of `patch()`'s commit — a single op always "commits
    // atomically" trivially, so the bug this guards against (a CAS failure
    // on op N leaving ops 1..N-1 durably committed with no rollback) is only
    // observable at this level.

    #[tokio::test]
    async fn patch_batch_cas_failure_leaves_content_untouched() {
        let original = "abcdef";
        let (backend, blocks, ctx_id, block_id, path) = patch_backend_fixture(original).await;
        let version_before = blocks.version(ctx_id).unwrap();

        // op1 would succeed alone (its CAS matches "abc"). op2's `expected`
        // does not match what op1 actually produces at that offset ("def",
        // not "MISMATCH") — the whole batch must fail, and op1 must not have
        // been left durably applied.
        let ops = vec![
            PatchOp::Replace {
                offset: 0,
                len: 3,
                content: "XXX".into(),
                expected: Some("abc".into()),
            },
            PatchOp::Replace {
                offset: 3,
                len: 3,
                content: "YYY".into(),
                expected: Some("MISMATCH".into()),
            },
        ];

        let err = backend
            .patch(&path, &ops)
            .await
            .expect_err("mid-batch CAS mismatch must fail the whole patch");
        assert!(
            matches!(err, BackendError::Conflict(_)),
            "expected a Conflict error, got {err:?}"
        );

        assert_eq!(
            block_content(&blocks, ctx_id, &block_id),
            original,
            "a failed batch must leave the block byte-identical — op1 must not have been journaled"
        );
        assert_eq!(
            blocks.version(ctx_id).unwrap(),
            version_before,
            "no commit happened — version must not have advanced"
        );
    }

    #[tokio::test]
    async fn patch_successful_multi_op_batch_uses_progressive_offsets() {
        let original = "abcdefghij";
        let (backend, blocks, ctx_id, block_id, path) = patch_backend_fixture(original).await;

        // op2 and op3's offsets are against the content AFTER the prior op,
        // not the original — that is the existing (pre-fix) semantics and
        // must be preserved.
        //   "abcdefghij"
        // → "XYZdefghij"      (op1: replace [0..3) "abc" -> "XYZ")
        // → "XYZ123defghij"   (op2: insert "123" at byte 3)
        // → "XYZ123ghij"      (op3: delete [6..9) "def")
        let ops = vec![
            PatchOp::Replace {
                offset: 0,
                len: 3,
                content: "XYZ".into(),
                expected: Some("abc".into()),
            },
            PatchOp::Insert {
                offset: 3,
                content: "123".into(),
            },
            PatchOp::Delete {
                offset: 6,
                len: 3,
                expected: Some("def".into()),
            },
        ];

        backend
            .patch(&path, &ops)
            .await
            .expect("a fully valid multi-op batch must apply every op");

        assert_eq!(block_content(&blocks, ctx_id, &block_id), "XYZ123ghij");
    }

    #[tokio::test]
    async fn patch_multibyte_multi_op_batch_char_vs_byte_splice() {
        let original = "改善 → done";
        let (backend, blocks, ctx_id, block_id, path) = patch_backend_fixture(original).await;

        // op1 replaces the arrow (multibyte) itself; op2's offset is
        // computed against the content AFTER op1, not the original — pins
        // the char-vs-byte splice through a batch that both starts and ends
        // with multibyte content, not just a multibyte prefix.
        let arrow_offset = original.find('→').unwrap();
        let arrow_len = '→'.len_utf8();
        let after_op1 = {
            let mut s = original.to_string();
            s.replace_range(arrow_offset..arrow_offset + arrow_len, "=>");
            s
        };
        let append_offset = after_op1.len();

        let ops = vec![
            PatchOp::Replace {
                offset: arrow_offset,
                len: arrow_len,
                content: "=>".into(),
                expected: Some("→".into()),
            },
            PatchOp::Insert {
                offset: append_offset,
                content: " 改善".into(),
            },
        ];

        backend
            .patch(&path, &ops)
            .await
            .expect("multibyte multi-op batch must apply cleanly");

        let expected = format!("{} 改善", after_op1);
        assert_eq!(block_content(&blocks, ctx_id, &block_id), expected);
    }

    #[tokio::test]
    async fn patch_single_op_batch_behaves_as_before() {
        let original = "hello world";
        let (backend, blocks, ctx_id, block_id, path) = patch_backend_fixture(original).await;

        // Single-op `Replace` batches are the only shape production sends
        // today (kaish's `patch`/`sed` builtins) — this must not regress.
        let ops = vec![PatchOp::Replace {
            offset: 6,
            len: 5,
            content: "kaish".into(),
            expected: Some("world".into()),
        }];

        backend
            .patch(&path, &ops)
            .await
            .expect("single-op batch must still succeed");

        assert_eq!(block_content(&blocks, ctx_id, &block_id), "hello kaish");
    }

    #[test]
    fn patch_insert_line_after_multibyte_line() {
        let content = "改善 → done\nsecond";

        // 1-indexed: line 2 = before "second". Line 1 is 10 chars / 16 bytes;
        // whole content is 16 chars — the buggy byte offset (16) used to pass
        // a char bounds check meant for a DIFFERENT (char-indexed) splice and
        // land in the wrong place. `compute_patch_op` never touches char
        // indices at all — it stays in the byte domain throughout — so this
        // now just pins the correct line-split result.
        let result = compute_patch_op(
            &PatchOp::InsertLine {
                line: 2,
                content: "INSERTED".into(),
            },
            content,
        )
        .expect("insert line after a multibyte line must succeed");
        assert_eq!(result, "改善 → done\nINSERTED\nsecond");
    }

    #[test]
    fn patch_delete_line_with_multibyte_before() {
        let content = "改善 → done\nDELETE ME\nkeep";

        let result = compute_patch_op(
            &PatchOp::DeleteLine {
                line: 2,
                expected: Some("DELETE ME".into()),
            },
            content,
        )
        .expect("delete line after a multibyte line must not trip bounds");
        assert_eq!(result, "改善 → done\nkeep");
    }

    #[test]
    fn patch_replace_line_with_multibyte_before() {
        let content = "→ arrows ✅\nold\ntail";

        let result = compute_patch_op(
            &PatchOp::ReplaceLine {
                line: 2,
                content: "new".into(),
                expected: Some("old".into()),
            },
            content,
        )
        .expect("replace line after a multibyte line must succeed");
        assert_eq!(result, "→ arrows ✅\nnew\ntail");
    }

    /// Pins the byte-offset ruling for the positional ops: PatchOp::Replace's
    /// wire `offset`/`len` are BYTES (kaish-types contract; the CAS check
    /// byte-slices). `compute_patch_op` stays in that byte domain end to end
    /// — the char projection only happens once, in `patch()`, when the whole
    /// batch's result is committed as a single splice.
    #[test]
    fn patch_byte_replace_with_multibyte_before() {
        let content = "改善X";

        // Bytes 6..7 = "X" (改善 = 6 bytes); chars 2..3. The old per-op path
        // converted this to a char position (2) and spliced at char 2 of 3 —
        // this pins that the byte offset itself, not a char projection, is
        // what's checked and used.
        let result = compute_patch_op(
            &PatchOp::Replace {
                offset: 6,
                len: 1,
                content: "Y".into(),
                expected: Some("X".into()),
            },
            content,
        )
        .expect("byte-offset replace after multibyte prefix must succeed");
        assert_eq!(result, "改善Y");
    }

    #[test]
    fn patch_byte_insert_with_multibyte_before() {
        let content = "改善X";

        let result = compute_patch_op(
            &PatchOp::Insert {
                offset: 6,
                content: "Q".into(),
            },
            content,
        )
        .expect("byte-offset insert after multibyte prefix must succeed");
        assert_eq!(result, "改善QX");
    }

    #[test]
    fn patch_byte_delete_with_multibyte_before() {
        let content = "改善AB";

        let result = compute_patch_op(
            &PatchOp::Delete {
                offset: 6,
                len: 1,
                expected: Some("A".into()),
            },
            content,
        )
        .expect("byte-offset delete after multibyte prefix must succeed");
        assert_eq!(result, "改善B");
    }

    /// A wire byte offset that lands MID-CHAR is a loud error, not a panic —
    /// `check_byte_boundary` rejects it before the mirror ops
    /// (`insert_str`/`replace_range`) ever see it, since those panic rather
    /// than error on a non-boundary index.
    #[test]
    fn patch_byte_offset_mid_char_fails_loud() {
        let content = "改善";

        let err = compute_patch_op(
            &PatchOp::Replace {
                offset: 1,
                len: 1,
                content: "z".into(),
                expected: None,
            },
            content,
        )
        .expect_err("mid-char byte offset must be rejected");
        assert!(
            err.to_string().contains("char boundary"),
            "error should name the boundary problem: {err}"
        );
    }
}
