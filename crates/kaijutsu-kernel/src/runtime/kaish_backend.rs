//! KaijutsuBackend: kaish KernelBackend implementation backed by CRDT blocks.
//!
//! This backend maps kaish file operations to kaijutsu's CRDT block store,
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
//!     ├── File ops → BlockStore (CRDT)
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

use kaijutsu_crdt::BlockId;
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

/// Backend that routes kaish operations to kaijutsu's CRDT block store.
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
    /// CRDT document/block storage.
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
                            .create_document(ctx_id, DocKind::Code, None)
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
                                .create_document(ctx_id, DocKind::Code, None)
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
                let content = {
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

                // Apply patch operations in order
                let mut current_content = content;
                for op in ops {
                    current_content =
                        apply_patch_op(&self.blocks, ctx_id, &block_id, op, &current_content)?;
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
                    .create_document(ctx_id, DocKind::Code, None)
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
        // doc rows derive their timing from CRDT ticks, not a settable mtime.
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
    // through MountBackend → MountTable → ConfigCrdtFs, which does support links
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
            // CRDT-backed mount; residency lives in the BlockStore, not tracked here.
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

/// Project a WIRE byte offset onto the char index the CRDT text layer
/// consumes. `PatchOp::Insert`/`Delete`/`Replace` offsets are BYTES by the
/// kaish-types contract ("Insert content at byte offset", "Delete bytes …"),
/// and this function is the single seam where that byte domain meets the
/// char-indexed `edit_text` (`BlockDocument::edit_text` bounds-checks against
/// `chars().count()` and splices at char positions).
///
/// A mid-char or out-of-range byte offset fails LOUD here, before any splice
/// — the old path spliced the CRDT at a bogus char position and then panicked
/// in the byte mirror's `replace_range`, leaving the durable block corrupted
/// behind the crash.
fn wire_byte_to_char(content: &str, byte: usize, what: &str) -> BackendResult<usize> {
    if byte > content.len() || !content.is_char_boundary(byte) {
        return Err(BackendError::Io(format!(
            "patch {what}: byte offset {byte} is not a char boundary in {}-byte content",
            content.len()
        )));
    }
    Ok(crate::block_tools::translate::byte_to_char_offset(
        content, byte,
    ))
}

/// Apply a single patch operation to a block.
///
/// Two coordinate domains, deliberately: the wire offsets and the local
/// `result` mirror ops (`insert_str`/`replace_range`) are BYTES per the
/// PatchOp contract; the `blocks.edit_text` calls are CHARS (the CRDT text
/// layer is char-indexed). Every CRDT call site converts through
/// [`wire_byte_to_char`] / `byte_to_char_offset` — never feed a byte offset
/// to `edit_text` directly (the multibyte-corruption bug class).
fn apply_patch_op(
    blocks: &SharedBlockStore,
    ctx_id: ContextId,
    block_id: &BlockId,
    op: &PatchOp,
    current_content: &str,
) -> BackendResult<String> {
    match op {
        PatchOp::Insert { offset, content } => {
            let pos = wire_byte_to_char(current_content, *offset, "insert")?;
            blocks
                .edit_text(ctx_id, block_id, pos, content, 0)
                .map_err(|e| BackendError::Io(e.to_string()))?;
            let mut result = current_content.to_string();
            result.insert_str(*offset, content);
            Ok(result)
        }
        PatchOp::Delete {
            offset,
            len,
            expected,
        } => {
            // Validate/convert BEFORE the CAS check: a bogus offset should
            // report as the boundary error it is, not as a misleading
            // conflict against the empty string `.get()` yields.
            let start = wire_byte_to_char(current_content, *offset, "delete")?;
            let end = wire_byte_to_char(current_content, *offset + *len, "delete")?;
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
            blocks
                .edit_text(ctx_id, block_id, start, "", end - start)
                .map_err(|e| BackendError::Io(e.to_string()))?;
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
            let start = wire_byte_to_char(current_content, *offset, "replace")?;
            let end = wire_byte_to_char(current_content, *offset + *len, "replace")?;
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
            blocks
                .edit_text(ctx_id, block_id, start, content, end - start)
                .map_err(|e| BackendError::Io(e.to_string()))?;
            let mut result = current_content.to_string();
            result.replace_range(*offset..*offset + *len, content);
            Ok(result)
        }
        PatchOp::InsertLine { line, content } => {
            let line_offset = line_to_byte_offset(current_content, *line);
            // Line starts always sit on char boundaries, so the projection is
            // infallible; the mirror insert below stays in the byte domain.
            let pos = crate::block_tools::translate::byte_to_char_offset(
                current_content,
                line_offset,
            );
            blocks
                .edit_text(ctx_id, block_id, pos, &format!("{}\n", content), 0)
                .map_err(|e| BackendError::Io(e.to_string()))?;
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

            let start_c =
                crate::block_tools::translate::byte_to_char_offset(current_content, start);
            let end_c = crate::block_tools::translate::byte_to_char_offset(current_content, end);
            blocks
                .edit_text(ctx_id, block_id, start_c, "", end_c - start_c)
                .map_err(|e| BackendError::Io(e.to_string()))?;
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
            let start_c =
                crate::block_tools::translate::byte_to_char_offset(current_content, start);
            let end_c = crate::block_tools::translate::byte_to_char_offset(current_content, end);
            blocks
                .edit_text(ctx_id, block_id, start_c, &replacement, end_c - start_c)
                .map_err(|e| BackendError::Io(e.to_string()))?;
            let mut result = current_content.to_string();
            result.replace_range(start..end, &replacement);
            Ok(result)
        }
        PatchOp::Append { content } => {
            blocks
                .append_text(ctx_id, block_id, content)
                .map_err(|e| BackendError::Io(e.to_string()))?;
            Ok(format!("{}{}", current_content, content))
        }
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
/// the kaish patch surface. Outputs are BYTE offsets, consumed by the local
/// byte-domain mirror ops; the CRDT call sites in `apply_patch_op` project
/// them through `byte_to_char_offset` (the shared conversion) before any
/// `edit_text` — that projection is the one source of truth for byte→char.
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

    // ── apply_patch_op × multibyte content (byte-vs-char offset regression) ──
    //
    // `blocks.edit_text` is CHAR-indexed (the CRDT layer bounds-checks against
    // `chars().count()` and splices at char positions). PatchOp's wire
    // contract is BYTES for Insert/Delete/Replace (kaish-types doc: "Insert
    // content at byte offset", "Delete bytes") and 1-indexed lines for the
    // *Line ops — so the byte→char projection must happen at the CRDT seam.
    // Nastiest failure: the byte MIRROR string ops were correct while the
    // CRDT splice was wrong, so the returned content and the durable block
    // silently diverged.

    /// A store + document + one block with the given content; returns the
    /// pieces `apply_patch_op` needs. CRDT-side content is read back through
    /// `crdt_content`.
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

    fn crdt_content(blocks: &SharedBlockStore, ctx_id: ContextId, block_id: &BlockId) -> String {
        blocks
            .get_block_snapshot(ctx_id, block_id)
            .unwrap()
            .unwrap()
            .content
    }

    #[test]
    fn patch_insert_line_after_multibyte_line() {
        let content = "改善 → done\nsecond";
        let (blocks, ctx_id, block_id) = patch_fixture(content);

        // 1-indexed: line 2 = before "second". Line 1 is 10 chars / 16 bytes;
        // whole content is 16 chars — the buggy byte offset (16) passed the
        // char bounds check and appended at the END of the CRDT text while the
        // byte mirror spliced correctly: silent CRDT/mirror divergence.
        let result = apply_patch_op(
            &blocks,
            ctx_id,
            &block_id,
            &PatchOp::InsertLine { line: 2, content: "INSERTED".into() },
            content,
        )
        .expect("insert line after a multibyte line must succeed");
        assert_eq!(result, "改善 → done\nINSERTED\nsecond");
        assert_eq!(
            crdt_content(&blocks, ctx_id, &block_id),
            result,
            "CRDT content must match the returned mirror"
        );
    }

    #[test]
    fn patch_delete_line_with_multibyte_before() {
        let content = "改善 → done\nDELETE ME\nkeep";
        let (blocks, ctx_id, block_id) = patch_fixture(content);

        // Buggy byte range 16..26 vs 24 chars → spurious PositionOutOfBounds.
        let result = apply_patch_op(
            &blocks,
            ctx_id,
            &block_id,
            &PatchOp::DeleteLine { line: 2, expected: Some("DELETE ME".into()) },
            content,
        )
        .expect("delete line after a multibyte line must not trip bounds");
        assert_eq!(result, "改善 → done\nkeep");
        assert_eq!(crdt_content(&blocks, ctx_id, &block_id), result);
    }

    #[test]
    fn patch_replace_line_with_multibyte_before() {
        let content = "→ arrows ✅\nold\ntail";
        let (blocks, ctx_id, block_id) = patch_fixture(content);

        // Buggy byte range 15..19 passed the char bounds check (len 19) but
        // deleted chars 15..19 — "tail" — in the CRDT while the mirror
        // replaced "old\n": divergence.
        let result = apply_patch_op(
            &blocks,
            ctx_id,
            &block_id,
            &PatchOp::ReplaceLine {
                line: 2,
                content: "new".into(),
                expected: Some("old".into()),
            },
            content,
        )
        .expect("replace line after a multibyte line must succeed");
        assert_eq!(result, "→ arrows ✅\nnew\ntail");
        assert_eq!(crdt_content(&blocks, ctx_id, &block_id), result);
    }

    /// Pins the byte-offset ruling for the positional ops: PatchOp::Replace's
    /// wire `offset`/`len` are BYTES (kaish-types contract; the CAS check
    /// byte-slices), converted to chars only at the CRDT seam.
    #[test]
    fn patch_byte_replace_with_multibyte_before() {
        let content = "改善X";
        let (blocks, ctx_id, block_id) = patch_fixture(content);

        // Bytes 6..7 = "X" (改善 = 6 bytes); chars 2..3. Buggy: edit_text(6,…)
        // vs 3 chars → spurious bounds error.
        let result = apply_patch_op(
            &blocks,
            ctx_id,
            &block_id,
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
        assert_eq!(crdt_content(&blocks, ctx_id, &block_id), result);
    }

    #[test]
    fn patch_byte_insert_with_multibyte_before() {
        let content = "改善X";
        let (blocks, ctx_id, block_id) = patch_fixture(content);

        let result = apply_patch_op(
            &blocks,
            ctx_id,
            &block_id,
            &PatchOp::Insert { offset: 6, content: "Q".into() },
            content,
        )
        .expect("byte-offset insert after multibyte prefix must succeed");
        assert_eq!(result, "改善QX");
        assert_eq!(crdt_content(&blocks, ctx_id, &block_id), result);
    }

    #[test]
    fn patch_byte_delete_with_multibyte_before() {
        let content = "改善AB";
        let (blocks, ctx_id, block_id) = patch_fixture(content);

        let result = apply_patch_op(
            &blocks,
            ctx_id,
            &block_id,
            &PatchOp::Delete { offset: 6, len: 1, expected: Some("A".into()) },
            content,
        )
        .expect("byte-offset delete after multibyte prefix must succeed");
        assert_eq!(result, "改善B");
        assert_eq!(crdt_content(&blocks, ctx_id, &block_id), result);
    }

    /// A wire byte offset that lands MID-CHAR is a loud error, not a panic —
    /// the old path spliced the CRDT at a bogus char position and then
    /// panicked in the byte mirror's `replace_range`, leaving the block
    /// corrupted behind the crash.
    #[test]
    fn patch_byte_offset_mid_char_fails_loud() {
        let content = "改善";
        let (blocks, ctx_id, block_id) = patch_fixture(content);

        let err = apply_patch_op(
            &blocks,
            ctx_id,
            &block_id,
            &PatchOp::Replace { offset: 1, len: 1, content: "z".into(), expected: None },
            content,
        )
        .expect_err("mid-char byte offset must be rejected");
        assert!(
            err.to_string().contains("char boundary"),
            "error should name the boundary problem: {err}"
        );
        // The CRDT block is untouched — the guard fired before any splice.
        assert_eq!(crdt_content(&blocks, ctx_id, &block_id), "改善");
    }
}
