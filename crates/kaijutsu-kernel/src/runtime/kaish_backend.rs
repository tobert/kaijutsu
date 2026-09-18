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
//!     └── Tool calls → MCP broker
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

use kaijutsu_types::{BlockId, Status};
use crate::Kernel as KaijutsuKernel;
use crate::block_store::SharedBlockStore;
use crate::ExecResult;
use kaijutsu_types::DocKind;
use kaijutsu_types::ContextId;

/// Minimal name/description tuple for converting a broker-visible tool into
/// kaish's `ToolInfo`. The full tool metadata lives on `KernelTool`; this
/// local shape is just what `convert_tool_info` consumes.
struct KaijutsuToolInfo {
    name: String,
    description: String,
}

impl KaijutsuToolInfo {
    fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
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

use super::context_shell::ShellIdentity;
use super::context_engine::{SessionContextExt, SessionContextMap};

/// Backend that routes kaish operations to kaijutsu's kernel block store.
///
/// File operations become block operations:
/// - `cat /docs/{ctx_hex}/block-key` → read block content
/// - `echo "text" >> /docs/{ctx_hex}/block-key` → append to block
/// - `ls /docs/` → list documents
///
/// Tool calls preserve requester, performer, reviewer, and session through the
/// MCP broker. Each call resolves the current context from the invocation's map.
pub struct KaijutsuBackend {
    /// kernel document/block storage.
    blocks: SharedBlockStore,
    /// The kaijutsu kernel for tool dispatch.
    kernel: Arc<KaijutsuKernel>,
    /// Identity fields for bridging kaish ExecContext → kaijutsu ToolContext.
    identity: ShellIdentity,
    /// Shared mutable context tracking map.
    session_contexts: SessionContextMap,
}

impl KaijutsuBackend {
    /// Create a new backend with block store, kernel, and identity fields.
    ///
    /// Read context switches from the invocation's shared session map.
    pub fn new(
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        identity: ShellIdentity,
        session_contexts: SessionContextMap,
    ) -> Self {
        Self {
            blocks,
            kernel,
            identity,
            session_contexts,
        }
    }

    /// Resolve the tool surface for the shell's active context. Discovery and
    /// invocation must use the same broker-visible names: a binding can hide a
    /// registered tool, and a collision can qualify its visible name.
    async fn visible_tools(&self) -> BackendResult<Vec<(String, crate::mcp::KernelTool)>> {
        let context_id = self
            .session_contexts
            .current(&self.identity.session)
            .ok_or_else(|| BackendError::Io("no active context joined".to_string()))?;
        let call_context = crate::mcp::CallContext::new(
            self.identity.requester,
            context_id,
            self.identity.session,
            self.kernel.id(),
        )
        .with_actor(self.identity.performer, self.identity.reviewer);
        let broker = self.kernel.broker();
        broker
            .binding_checked(&context_id)
            .await
            .map_err(|error| BackendError::Io(error.to_string()))?;
        broker
            .list_visible_tools(context_id, &call_context)
            .await
            .map_err(|error| BackendError::Io(error.to_string()))
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
    /// When a JSON Schema is provided by the broker, converts its properties
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
        let ExecResult { stdout, stderr, exit_code, output, .. } = result;
        let mut kaish_result = kaish_kernel::interpreter::ExecResult::from_output(
            i64::from(exit_code),
            stdout,
            stderr,
        );
        kaish_result.set_output(output);
        ToolResult::from(kaish_result)
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
                let blocks: Vec<_> = entry.doc.blocks_ordered().into_iter()
                    .filter(|b| b.status != Status::Draft).collect();
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
                let blocks: Vec<_> = entry.doc.blocks_ordered().into_iter()
                    .filter(|b| b.status != Status::Draft).collect();
                let block = blocks.iter().find(|b| b.id == block_id).ok_or_else(|| {
                    BackendError::NotFound(format!("block not found: {}", block_id.to_key()))
                })?;

                // Ranged reads are unreachable here: `/v/docs` is the only live
                // mount over this backend (`docs_filesystem.rs`), and its
                // `Filesystem::read` always calls `KaijutsuBackend::read` with
                // `range: None` — kaish's own range-windowed reads go through
                // `MountBackend::apply_range` on the real-file path instead.
                // Refuse loudly rather than keep a second, untested windowing
                // implementation alive for a caller that does not exist.
                if range.is_some() {
                    return Err(BackendError::InvalidOperation(
                        "ranged reads not supported on conversation blocks (/docs)".into(),
                    ));
                }

                Ok(block.content.clone().into_bytes())
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
                    let blocks: Vec<_> = entry.doc.blocks_ordered().into_iter()
                    .filter(|b| b.status != Status::Draft).collect();
                    blocks
                        .iter()
                        .find(|b| b.id == block_id)
                        .map(|b| b.content.chars().count())
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

    // `append` and `patch` are unreachable through any live caller: `/v/docs`
    // (`docs_filesystem.rs`) mounts this backend as a `Filesystem`, whose
    // trait has no append/patch methods, and `MountBackend` (the mount kaish
    // actually scripts against) never forwards to `docs_tools` for file ops —
    // only `call_tool`/`list_tools`/`get_tool`/`mounts` (`mount_backend.rs`).
    // Refused structurally, the same way the symlink methods below already
    // are, rather than kept alive with no caller and no coverage. The
    // hardened byte-domain patch logic these used to run
    // (`compute_patch_op`/`check_byte_boundary`) moved to `mount_backend.rs`,
    // which *is* the live path — see `docs/audits/2026-08-20-kaish-glue.md`
    // B2/B3.
    async fn append(&self, _path: &Path, _content: &[u8]) -> BackendResult<()> {
        Err(BackendError::InvalidOperation(
            "append not supported on conversation blocks (/docs); unreachable through any live mount".into(),
        ))
    }

    async fn patch(&self, _path: &Path, _ops: &[PatchOp]) -> BackendResult<()> {
        Err(BackendError::InvalidOperation(
            "patch not supported on conversation blocks (/docs); unreachable through any live mount".into(),
        ))
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
                let blocks: Vec<_> = entry.doc.blocks_ordered().into_iter()
                    .filter(|b| b.status != Status::Draft).collect();
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
                let blocks: Vec<_> = entry.doc.blocks_ordered().into_iter()
                    .filter(|b| b.status != Status::Draft).collect();
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
                self.blocks.get_non_draft_snapshot(ctx_id, &block_id)
                    .map_err(|e| BackendError::Io(e.to_string()))?
                    .ok_or_else(|| BackendError::NotFound(block_id.to_key()))?;
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
                    entry.doc.blocks_ordered().iter().any(|b| b.id == block_id && b.status != Status::Draft)
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
    // blocks has no meaning. This is NOT the rc path — `ln -s /config/rc/…` routes
    // through MountBackend → MountTable → `LocalBackend`, which does support
    // real host links (init.d-style rc composition). Failing loud here keeps
    // the two schemes from quietly conflating.
    async fn read_link(&self, _path: &Path) -> BackendResult<std::path::PathBuf> {
        Err(BackendError::InvalidOperation(
            "symlinks not supported on conversation blocks (/docs); use /config/rc for rc composition".into(),
        ))
    }

    async fn symlink(&self, _target: &Path, _link: &Path) -> BackendResult<()> {
        Err(BackendError::InvalidOperation(
            "symlinks not supported on conversation blocks (/docs); use /config/rc for rc composition".into(),
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
            .current(&self.identity.session)
            .ok_or_else(|| BackendError::Io("no active context joined".to_string()))?;
        let tool_ctx = crate::ExecContext::new(
            self.identity.requester,
            context_id,
            ctx.cwd().to_path_buf(),
            self.identity.session,
            self.kernel.id(),
        ).with_actor(self.identity.performer, self.identity.reviewer);

        // Carry kaish's invocation cancellation through the broker. Its
        // watchdog signals this token; replacing it would strand pending tools
        // after the shell's caller cancels or the kernel shuts down.
        let cancel = ctx.as_any_mut().downcast_mut::<kaish_kernel::tools::ExecContext>()
            .ok_or_else(|| BackendError::Io("MCP dispatch requires a kaish execution context".into()))?
            .cancel.clone();
        let result = self
            .kernel
            .dispatch_tool_via_broker_with_cancel(name, &params_str, &tool_ctx, cancel)
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
        let tools = self.visible_tools().await?;
        let mut infos = Vec::with_capacity(tools.len());
        for (name, tool) in tools {
            let info = KaijutsuToolInfo::new(
                &name,
                tool.description.unwrap_or_default(),
            );
            infos.push(Self::convert_tool_info(&info, Some(tool.input_schema)));
        }
        Ok(infos)
    }

    async fn get_tool(&self, name: &str) -> BackendResult<Option<ToolInfo>> {
        let tools = self.visible_tools().await?;
        for (tool_name, tool) in tools {
            if tool_name == name {
                let info = KaijutsuToolInfo::new(
                    &tool_name,
                    tool.description.unwrap_or_default(),
                );
                return Ok(Some(Self::convert_tool_info(&info, Some(tool.input_schema))));
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
//
// The byte-domain `PatchOp` helpers (`compute_patch_op`, `check_byte_boundary`,
// `line_to_byte_offset`, `line_range`) and the read-range windower
// (`apply_read_range`) used to live here, backing this file's own
// `append`/`patch`/ranged-`read` — all now stubbed above as unreachable (see
// their doc comments). The hardened patch helpers moved to
// `mount_backend.rs`, which *is* the live path kaish scripts patch through;
// `apply_read_range` had no live equivalent to move to (`MountBackend`
// already had its own working `apply_range`) and was deleted outright. See
// `docs/audits/2026-08-20-kaish-glue.md` B2/B3/B5.

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
        // Delegate inline binary to the canonical converter so we emit
        // kaish's exact base64 envelope ({_type:"bytes",encoding:"base64",
        // data,len}) rather than re-deriving it.
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
    use crate::mcp::{ContextToolBinding, GlobPattern, HookAction, HookEntry, HookId, InstanceId, InstancePolicy, KernelCallParams, KernelTool, KernelToolResult, McpResult, McpServerLike, ServerNotification};
    use kaijutsu_types::{PrincipalId, SessionId};
    use tokio::sync::broadcast;

    #[test]
    fn convert_exec_result_preserves_both_streams_exit_status_and_output() {
        let output = kaijutsu_types::OutputData::text("structured error body");
        let converted = KaijutsuBackend::convert_exec_result(ExecResult {
            stdout: "{\"error\":\"structured error body\"}".into(),
            stderr: "diagnostic\n".into(),
            exit_code: 17,
            success: false,
            output: Some(output.clone()),
        });

        assert_eq!(converted.code, 17);
        assert_eq!(converted.stdout, "{\"error\":\"structured error body\"}");
        assert_eq!(converted.stderr, "diagnostic\n");
        assert_eq!(converted.output, Some(output));

        let converted = KaijutsuBackend::convert_exec_result(ExecResult {
            stdout: "normal output\n".into(),
            stderr: "warning\n".into(),
            exit_code: 0,
            success: true,
            output: None,
        });
        assert_eq!(converted.code, 0);
        assert_eq!(converted.stdout, "normal output\n");
        assert_eq!(converted.stderr, "warning\n");
    }

    struct DiscoveryServer {
        id: InstanceId,
        tool: String,
        notifications: broadcast::Sender<ServerNotification>,
        list_contexts: Arc<std::sync::Mutex<Vec<crate::mcp::CallContext>>>,
    }

    impl DiscoveryServer {
        fn new(instance: &str, tool: &str) -> Self {
            let (notifications, _) = broadcast::channel(1);
            Self { id: InstanceId::new(instance), tool: tool.into(), notifications,
                list_contexts: Arc::new(std::sync::Mutex::new(Vec::new())) }
        }

        fn list_contexts(&self) -> Vec<crate::mcp::CallContext> { self.list_contexts.lock().unwrap().clone() }

        fn clear_list_contexts(&self) { self.list_contexts.lock().unwrap().clear(); }
    }

    #[async_trait]
    impl McpServerLike for DiscoveryServer {
        fn instance_id(&self) -> &InstanceId { &self.id }

        async fn list_tools(&self, context: &crate::mcp::CallContext) -> McpResult<Vec<KernelTool>> {
            self.list_contexts.lock().unwrap().push(context.clone());
            Ok(vec![KernelTool {
                instance: self.id.clone(), name: self.tool.clone(), description: None,
                input_schema: serde_json::json!({"type": "object"}),
            }])
        }

        async fn call_tool(
            &self, _: KernelCallParams, _: &crate::mcp::CallContext, _: tokio_util::sync::CancellationToken,
        ) -> McpResult<KernelToolResult> {
            Ok(KernelToolResult::text(self.id.as_str()))
        }

        fn notifications(&self) -> broadcast::Receiver<ServerNotification> { self.notifications.subscribe() }

        async fn shutdown(&self) -> McpResult<()> { Ok(()) }
    }

    async fn discovery_backend_as(identity: ShellIdentity) -> (KaijutsuBackend, Arc<KaijutsuKernel>, SessionContextMap, SessionId) {
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("backend-discovery").await);
        let context = identity.context;
        let session = identity.session;
        let contexts = crate::runtime::context_engine::session_context_map();
        contexts.insert(session, context);
        let backend = KaijutsuBackend::new(
            shared_block_store(identity.requester), kernel.clone(), identity,
            contexts.clone(),
        );
        (backend, kernel, contexts, session)
    }

    async fn discovery_backend(context: ContextId) -> (KaijutsuBackend, Arc<KaijutsuKernel>, SessionContextMap, SessionId) {
        discovery_backend_as(ShellIdentity {
            requester: PrincipalId::system(), performer: PrincipalId::system(), reviewer: None,
            context, session: SessionId::new(),
        }).await
    }

    #[tokio::test]
    async fn tool_discovery_follows_the_active_context_binding() {
        let first = ContextId::new();
        let second = ContextId::new();
        let (backend, kernel, contexts, session) = discovery_backend(first).await;
        let alpha = Arc::new(DiscoveryServer::new("alpha", "alpha_tool"));
        let beta = Arc::new(DiscoveryServer::new("beta", "beta_tool"));
        kernel.broker().register_silently(alpha, InstancePolicy::default()).await.unwrap();
        kernel.broker().register_silently(beta, InstancePolicy::default()).await.unwrap();
        kernel.broker().set_binding(first, ContextToolBinding::with_instances(vec![InstanceId::new("alpha")])).await.unwrap();
        kernel.broker().set_binding(second, ContextToolBinding::with_instances(vec![InstanceId::new("beta")])).await.unwrap();

        let names: Vec<_> = backend.list_tools().await.unwrap().into_iter().map(|tool| tool.name).collect();
        assert_eq!(names, ["alpha_tool"]);
        assert!(backend.get_tool("alpha_tool").await.unwrap().is_some());
        assert!(backend.get_tool("beta_tool").await.unwrap().is_none());

        contexts.insert(session, second);
        let names: Vec<_> = backend.list_tools().await.unwrap().into_iter().map(|tool| tool.name).collect();
        assert_eq!(names, ["beta_tool"], "discovery must follow the same active session context as dispatch");
        assert!(backend.get_tool("alpha_tool").await.unwrap().is_none());
        assert!(backend.get_tool("beta_tool").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn tool_discovery_uses_broker_qualified_collision_names() {
        let context = ContextId::new();
        let (backend, kernel, _, _) = discovery_backend(context).await;
        let first = Arc::new(DiscoveryServer::new("one.tools", "inspect"));
        let second = Arc::new(DiscoveryServer::new("two.tools", "inspect"));
        kernel.broker().register_silently(first, InstancePolicy::default()).await.unwrap();
        kernel.broker().register_silently(second, InstancePolicy::default()).await.unwrap();
        kernel.broker().set_binding(context, ContextToolBinding::with_instances(vec![
            InstanceId::new("one.tools"), InstanceId::new("two.tools"),
        ])).await.unwrap();

        let mut names: Vec<_> = backend.list_tools().await.unwrap().into_iter().map(|tool| tool.name).collect();
        names.sort();
        assert_eq!(names, ["one_tools__inspect", "two_tools__inspect"]);
        assert!(backend.get_tool("inspect").await.unwrap().is_none());
        for name in names {
            assert!(backend.get_tool(&name).await.unwrap().is_some(), "resolved tool must be discoverable: {name}");
        }
    }

    #[tokio::test]
    async fn tool_discovery_preserves_shell_identity_and_list_tools_filtering() {
        let requester = PrincipalId::new();
        let performer = PrincipalId::new();
        let reviewer = PrincipalId::new();
        let context = ContextId::new();
        let session = SessionId::new();
        let (backend, kernel, _, _) = discovery_backend_as(ShellIdentity {
            requester, performer, reviewer: Some(reviewer), context, session,
        }).await;
        let kept = Arc::new(DiscoveryServer::new("kept", "read"));
        let hidden = Arc::new(DiscoveryServer::new("hidden", "write"));
        kernel.broker().register_silently(kept.clone(), InstancePolicy::default()).await.unwrap();
        kernel.broker().register_silently(hidden, InstancePolicy::default()).await.unwrap();
        kernel.broker().set_binding(context, ContextToolBinding::with_instances(vec![
            InstanceId::new("kept"), InstanceId::new("hidden"),
        ])).await.unwrap();
        kernel.broker().hooks().write().await.list_tools.entries.push(HookEntry {
            id: HookId("hide-writes-for-performer".into()),
            match_instance: Some(GlobPattern("hidden".into())),
            match_tool: Some(GlobPattern("write".into())),
            match_context: Some(context), match_principal: Some(performer),
            action: HookAction::Deny("fixture filter".into()), priority: 0, kaish_script_id: None,
        });
        kept.clear_list_contexts();

        let names: Vec<_> = backend.list_tools().await.unwrap().into_iter().map(|tool| tool.name).collect();
        assert_eq!(names, ["read"]);
        assert!(backend.get_tool("write").await.unwrap().is_none());
        let listed = kept.list_contexts();
        assert!(!listed.is_empty(), "visible server must receive the shell discovery context");
        assert!(listed.iter().all(|seen| seen.principal_id == requester
            && seen.actor_id == performer && seen.reviewer_id == Some(reviewer)
            && seen.context_id == context && seen.session_id == session));
    }

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
            crate::runtime::context_shell::ShellIdentity { requester: PrincipalId::system(), performer: PrincipalId::system(), reviewer: None, context: crate::runtime::context_engine::SessionContextExt::current(&session_contexts, &sid).expect("fixture context"), session: sid }, session_contexts);


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

    #[tokio::test]
    async fn virtual_document_removal_retains_context_history() {
        use crate::kj::test_helpers::{test_dispatcher_persistent, register_context};
        let dispatcher = test_dispatcher_persistent().await;
        let context = register_context(&dispatcher, Some("retained"), None, PrincipalId::system());
        dispatcher.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
        let session = SessionId::new();
        let contexts = crate::runtime::context_engine::session_context_map();
        contexts.insert(session, context);
        let backend = KaijutsuBackend::new(dispatcher.block_store().clone(), dispatcher.kernel().clone(),
            ShellIdentity { requester: PrincipalId::system(), performer: PrincipalId::system(), reviewer: None, context, session }, contexts);
        let path = format!("/docs/{}", context.to_hex());
        for archived in [false, true] {
            if archived { dispatcher.kernel_db().lock().archive_context(context).unwrap(); }
            let error = backend.remove(Path::new(&path), true).await.unwrap_err();
            assert!(error.to_string().contains("archive"), "{error}");
            assert!(dispatcher.block_store().contains(context));
            assert!(dispatcher.kernel_db().lock().get_context(context).unwrap().is_some());
        }
    }

    /// Fresh `KaijutsuBackend` over an empty block store, joined to `ctx_id`
    /// via the session map — enough identity for the stub methods below,
    /// which never touch storage.
    async fn stub_backend_fixture(ctx_id: ContextId) -> KaijutsuBackend {
        let blocks = shared_block_store(PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-stubs").await);
        let sid = SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        session_contexts.insert(sid, ctx_id);
        KaijutsuBackend::new(blocks, kernel, crate::runtime::context_shell::ShellIdentity { requester: PrincipalId::system(), performer: PrincipalId::system(), reviewer: None, context: crate::runtime::context_engine::SessionContextExt::current(&session_contexts, &sid).expect("fixture context"), session: sid }, session_contexts)
    }

    #[tokio::test]
    async fn docs_listing_and_stat_hide_drafts_until_submit() {
        let ctx = ContextId::new();
        let backend = stub_backend_fixture(ctx).await;
        backend.blocks.create_document(ctx, DocKind::Conversation, None).unwrap();
        let principal = PrincipalId::system();
        let draft = backend.blocks.edit_draft(ctx, principal, 0, "unfinished", 0).unwrap();
        let dir = format!("/docs/{}", ctx.to_hex());
        let path = format!("{dir}/{}", draft.to_key());
        assert!(!backend.exists(Path::new(&path)).await);
        assert!(backend.stat(Path::new(&path)).await.is_err());
        assert_eq!(backend.list(Path::new(&dir)).await.unwrap().len(), 1);
        assert!(!String::from_utf8(backend.read(Path::new(&dir), None).await.unwrap()).unwrap().contains(&draft.to_key()));
        backend.blocks.submit_draft(ctx, principal, None).unwrap();
        assert!(backend.exists(Path::new(&path)).await);
        assert!(backend.stat(Path::new(&path)).await.is_ok());
        assert_eq!(backend.list(Path::new(&dir)).await.unwrap().len(), 2);
        assert_eq!(backend.read(Path::new(&path), None).await.unwrap(), b"unfinished");
    }

    /// `append`/`patch` are unreachable through any live mount (see the doc
    /// comment on both methods) — pins that they refuse rather than silently
    /// no-op or panic now that their storage-backed bodies are gone.
    #[tokio::test]
    async fn append_and_patch_are_stubbed_unreachable() {
        let ctx_id = ContextId::new();
        let backend = stub_backend_fixture(ctx_id).await;
        let path = std::path::PathBuf::from(format!("/docs/{}/anything", ctx_id.to_hex()));

        let err = backend
            .append(&path, b"x")
            .await
            .expect_err("append must refuse, not silently succeed");
        assert!(matches!(err, BackendError::InvalidOperation(_)));

        let err = backend
            .patch(&path, &[PatchOp::Append { content: "x".into() }])
            .await
            .expect_err("patch must refuse, not silently succeed");
        assert!(matches!(err, BackendError::InvalidOperation(_)));
    }

    /// Overwriting a block replaces every character, not every byte: the
    /// delete count handed to `edit_text` is in chars, so a block holding
    /// multi-byte text must not trip the position guard or leave a tail.
    #[tokio::test]
    async fn overwrite_of_a_multibyte_block_replaces_the_whole_block() {
        let ctx_id = ContextId::new();
        let blocks = shared_block_store(PrincipalId::system());
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
                "改善 — kaizen",
                kaijutsu_types::Status::Done,
                kaijutsu_types::ContentType::Plain,
            )
            .unwrap();
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-multibyte-overwrite").await);
        let sid = SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        session_contexts.insert(sid, ctx_id);
        let backend = KaijutsuBackend::new(
            blocks.clone(),
            kernel,
            crate::runtime::context_shell::ShellIdentity { requester: PrincipalId::system(), performer: PrincipalId::system(), reviewer: None, context: crate::runtime::context_engine::SessionContextExt::current(&session_contexts, &sid).expect("fixture context"), session: sid }, session_contexts,
        );
        let path = std::path::PathBuf::from(format!(
            "/docs/{}/{}",
            ctx_id.to_hex(),
            block_id.to_key()
        ));

        backend
            .write(&path, b"plain", WriteMode::Overwrite)
            .await
            .expect("overwrite of a multi-byte block must succeed");

        let entry = blocks.get(ctx_id).unwrap();
        let content = entry
            .doc
            .blocks_ordered()
            .iter()
            .find(|b| b.id == block_id)
            .map(|b| b.content.clone())
            .unwrap();
        assert_eq!(content, "plain");
    }

    /// A ranged read is likewise unreachable (`/v/docs` always passes
    /// `None`) — pins that `Some(range)` refuses rather than silently
    /// serving the whole block back.
    #[tokio::test]
    async fn ranged_read_is_stubbed_unreachable() {
        let ctx_id = ContextId::new();
        let blocks = shared_block_store(PrincipalId::system());
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
                "hello world",
                kaijutsu_types::Status::Done,
                kaijutsu_types::ContentType::Plain,
            )
            .unwrap();
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-ranged-read").await);
        let sid = SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        session_contexts.insert(sid, ctx_id);
        let backend = KaijutsuBackend::new(blocks, kernel, crate::runtime::context_shell::ShellIdentity { requester: PrincipalId::system(), performer: PrincipalId::system(), reviewer: None, context: crate::runtime::context_engine::SessionContextExt::current(&session_contexts, &sid).expect("fixture context"), session: sid }, session_contexts);
        let path = std::path::PathBuf::from(format!(
            "/docs/{}/{}",
            ctx_id.to_hex(),
            block_id.to_key()
        ));

        let range = ReadRange {
            start_line: None,
            end_line: None,
            offset: Some(0),
            limit: Some(5),
        };
        let err = backend
            .read(&path, Some(range))
            .await
            .expect_err("a ranged read must refuse, not silently serve the whole block");
        assert!(matches!(err, BackendError::InvalidOperation(_)));

        // The reachable path (range: None) still works.
        let data = backend.read(&path, None).await.unwrap();
        assert_eq!(data, b"hello world");
    }
}
