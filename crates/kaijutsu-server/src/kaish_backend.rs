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
//! - `/docs/{doc_id}` - List blocks in a document
//! - `/docs/{doc_id}/{block_key}` - Access a specific block's content
//! - `/docs/{doc_id}/_meta` - Document metadata (kind, language)
//!
//! This allows kaish to navigate documents like directories and blocks like files.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::Value as JsonValue;

use kaijutsu_crdt::BlockId;
use kaijutsu_kernel::block_store::SharedBlockStore;
use kaijutsu_kernel::db::DocumentKind;
use kaijutsu_kernel::tools::{ExecResult, ToolInfo as KaijutsuToolInfo};
use kaijutsu_kernel::Kernel as KaijutsuKernel;

use kaish_kernel::{
    BackendError, BackendResult, EntryInfo, KernelBackend, PatchOp, ReadRange, ToolInfo,
    ToolResult, WriteMode,
};
use kaish_kernel::tools::{ExecContext, ToolArgs, ToolSchema};
use kaish_kernel::vfs::MountInfo;

/// Backend that routes kaish operations to kaijutsu's CRDT block store.
///
/// File operations become block operations:
/// - `cat /docs/conv-123/block-1` → read block content
/// - `echo "text" >> /docs/conv-123/block-1` → append to block
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
    /// Cached tool schemas for help (reserved for future use).
    #[allow(dead_code)]
    tool_schemas: RwLock<Vec<ToolSchema>>,
}

impl KaijutsuBackend {
    /// Create a new backend with block store and kaijutsu kernel.
    pub fn new(blocks: SharedBlockStore, kernel: Arc<KaijutsuKernel>) -> Self {
        Self {
            blocks,
            kernel,
            tool_schemas: RwLock::new(Vec::new()),
        }
    }

    /// Resolve a VFS path to a document ID and optional block ID.
    ///
    /// Path formats:
    /// - `/docs` → (None, None) - docs root
    /// - `/docs/{doc_id}` → (Some(doc_id), None) - document directory
    /// - `/docs/{doc_id}/{block_key}` → (Some(doc_id), Some(block_id))
    /// - `/docs/{doc_id}/_meta` → document metadata (special case)
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
            ["docs", doc_id] => PathResolution::Document(doc_id.to_string()),
            ["docs", doc_id, "_meta"] => PathResolution::DocumentMeta(doc_id.to_string()),
            ["docs", doc_id, block_key] => {
                if let Some(block_id) = BlockId::from_key(block_key) {
                    PathResolution::Block(doc_id.to_string(), block_id)
                } else {
                    PathResolution::Invalid(format!("invalid block key: {}", block_key))
                }
            }
            _ => PathResolution::Invalid(format!("unsupported path: {}", path_str)),
        }
    }

    /// Find a block by ID across all documents.
    #[allow(dead_code)]
    fn find_block(&self, block_id: &BlockId) -> Option<(String, BlockId)> {
        for doc_id in self.blocks.list_ids() {
            if let Some(entry) = self.blocks.get(&doc_id) {
                for snapshot in entry.doc.blocks_ordered() {
                    if &snapshot.id == block_id {
                        return Some((doc_id, block_id.clone()));
                    }
                }
            }
        }
        None
    }

    /// Convert kaijutsu ToolInfo to kaish ToolInfo format.
    fn convert_tool_info(info: &KaijutsuToolInfo) -> ToolInfo {
        // Build a basic schema - engines don't expose full JSON schemas
        let schema = ToolSchema::new(&info.name, &info.description);

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
    /// A specific document (`/docs/{doc_id}`)
    Document(String),
    /// Document metadata (`/docs/{doc_id}/_meta`)
    DocumentMeta(String),
    /// A specific block (`/docs/{doc_id}/{block_key}`)
    Block(String, BlockId),
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
                let doc_ids = self.blocks.list_ids();
                let listing = doc_ids.join("\n") + "\n";
                Ok(listing.into_bytes())
            }
            PathResolution::Document(doc_id) => {
                // List blocks in document
                let entry = self.blocks.get(&doc_id).ok_or_else(|| {
                    BackendError::NotFound(format!("document not found: {}", doc_id))
                })?;
                let blocks = entry.doc.blocks_ordered();
                let listing: Vec<String> = blocks.iter().map(|b| b.id.to_key()).collect();
                Ok((listing.join("\n") + "\n").into_bytes())
            }
            PathResolution::DocumentMeta(doc_id) => {
                // Return document metadata as JSON
                let entry = self.blocks.get(&doc_id).ok_or_else(|| {
                    BackendError::NotFound(format!("document not found: {}", doc_id))
                })?;
                let meta = serde_json::json!({
                    "id": doc_id,
                    "kind": format!("{:?}", entry.kind),
                    "language": entry.language,
                    "version": entry.version(),
                });
                let json = serde_json::to_string_pretty(&meta)
                    .map_err(|e| BackendError::Io(e.to_string()))?;
                Ok(json.into_bytes())
            }
            PathResolution::Block(doc_id, block_id) => {
                // Read block content
                let entry = self.blocks.get(&doc_id).ok_or_else(|| {
                    BackendError::NotFound(format!("document not found: {}", doc_id))
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
            PathResolution::Document(doc_id) => {
                // Create document if CreateNew or Overwrite
                match mode {
                    WriteMode::CreateNew => {
                        if self.blocks.contains(&doc_id) {
                            return Err(BackendError::AlreadyExists(doc_id));
                        }
                        self.blocks
                            .create_document(doc_id.clone(), DocumentKind::Code, None)
                            .map_err(|e| BackendError::Io(e))?;
                    }
                    WriteMode::UpdateOnly => {
                        if !self.blocks.contains(&doc_id) {
                            return Err(BackendError::NotFound(doc_id));
                        }
                    }
                    WriteMode::Overwrite | WriteMode::Truncate => {
                        if !self.blocks.contains(&doc_id) {
                            self.blocks
                                .create_document(doc_id.clone(), DocumentKind::Code, None)
                                .map_err(|e| BackendError::Io(e))?;
                        }
                        // For truncate/overwrite, we'd need to clear existing blocks
                        // For now, just ensure the document exists
                    }
                }
                Ok(())
            }
            PathResolution::Block(doc_id, block_id) => {
                // Write to block content
                if !self.blocks.contains(&doc_id) {
                    return Err(BackendError::NotFound(format!(
                        "document not found: {}",
                        doc_id
                    )));
                }

                // For blocks, we need to replace the content
                // First get current content length, then edit
                let current_len = {
                    let entry = self.blocks.get(&doc_id).ok_or_else(|| {
                        BackendError::NotFound(format!("document not found: {}", doc_id))
                    })?;
                    let blocks = entry.doc.blocks_ordered();
                    blocks
                        .iter()
                        .find(|b| b.id == block_id)
                        .map(|b| b.content.len())
                        .ok_or_else(|| {
                            BackendError::NotFound(format!("block not found: {}", block_id.to_key()))
                        })?
                };

                // Delete all content then insert new content
                self.blocks
                    .edit_text(&doc_id, &block_id, 0, content_str, current_len)
                    .map_err(|e| BackendError::Io(e))?;

                Ok(())
            }
            PathResolution::DocsRoot | PathResolution::Root => {
                Err(BackendError::IsDirectory(path.to_string_lossy().to_string()))
            }
            PathResolution::DocumentMeta(_) => {
                Err(BackendError::PermissionDenied("cannot write to _meta".into()))
            }
            PathResolution::Invalid(msg) => Err(BackendError::InvalidOperation(msg)),
        }
    }

    async fn append(&self, path: &Path, content: &[u8]) -> BackendResult<()> {
        let content_str =
            std::str::from_utf8(content).map_err(|e| BackendError::Io(e.to_string()))?;

        match self.resolve_path(path) {
            PathResolution::Block(doc_id, block_id) => {
                self.blocks
                    .append_text(&doc_id, &block_id, content_str)
                    .map_err(|e| BackendError::Io(e))?;
                Ok(())
            }
            PathResolution::Document(_) | PathResolution::DocsRoot | PathResolution::Root => {
                Err(BackendError::IsDirectory(path.to_string_lossy().to_string()))
            }
            PathResolution::DocumentMeta(_) => {
                Err(BackendError::PermissionDenied("cannot append to _meta".into()))
            }
            PathResolution::Invalid(msg) => Err(BackendError::InvalidOperation(msg)),
        }
    }

    async fn patch(&self, path: &Path, ops: &[PatchOp]) -> BackendResult<()> {
        match self.resolve_path(path) {
            PathResolution::Block(doc_id, block_id) => {
                // Get current content for offset calculations
                let content = {
                    let entry = self.blocks.get(&doc_id).ok_or_else(|| {
                        BackendError::NotFound(format!("document not found: {}", doc_id))
                    })?;
                    let blocks = entry.doc.blocks_ordered();
                    blocks
                        .iter()
                        .find(|b| b.id == block_id)
                        .map(|b| b.content.clone())
                        .ok_or_else(|| {
                            BackendError::NotFound(format!("block not found: {}", block_id.to_key()))
                        })?
                };

                // Apply patch operations in order
                // Note: We apply each op sequentially, adjusting for prior changes
                let mut current_content = content;
                for op in ops {
                    current_content = apply_patch_op(&self.blocks, &doc_id, &block_id, op, &current_content)?;
                }

                Ok(())
            }
            PathResolution::Document(_) | PathResolution::DocsRoot | PathResolution::Root => {
                Err(BackendError::IsDirectory(path.to_string_lossy().to_string()))
            }
            PathResolution::DocumentMeta(_) => {
                Err(BackendError::PermissionDenied("cannot patch _meta".into()))
            }
            PathResolution::Invalid(msg) => Err(BackendError::InvalidOperation(msg)),
        }
    }

    // =========================================================================
    // Directory Operations
    // =========================================================================

    async fn list(&self, path: &Path) -> BackendResult<Vec<EntryInfo>> {
        match self.resolve_path(path) {
            PathResolution::Root => {
                Ok(vec![EntryInfo::directory("docs")])
            }
            PathResolution::DocsRoot => {
                let entries = self
                    .blocks
                    .list_ids()
                    .into_iter()
                    .map(|id| EntryInfo::directory(id))
                    .collect();
                Ok(entries)
            }
            PathResolution::Document(doc_id) => {
                let entry = self.blocks.get(&doc_id).ok_or_else(|| {
                    BackendError::NotFound(format!("document not found: {}", doc_id))
                })?;
                let blocks = entry.doc.blocks_ordered();
                let mut entries: Vec<EntryInfo> = blocks
                    .iter()
                    .map(|b| EntryInfo::file(b.id.to_key(), b.content.len() as u64))
                    .collect();
                // Add _meta pseudo-file
                entries.push(EntryInfo::file("_meta", 0));
                Ok(entries)
            }
            PathResolution::Block(_, _) | PathResolution::DocumentMeta(_) => {
                Err(BackendError::NotDirectory(path.to_string_lossy().to_string()))
            }
            PathResolution::Invalid(msg) => Err(BackendError::InvalidOperation(msg)),
        }
    }

    async fn stat(&self, path: &Path) -> BackendResult<EntryInfo> {
        match self.resolve_path(path) {
            PathResolution::Root => Ok(EntryInfo::directory("/")),
            PathResolution::DocsRoot => Ok(EntryInfo::directory("docs")),
            PathResolution::Document(doc_id) => {
                if self.blocks.contains(&doc_id) {
                    Ok(EntryInfo::directory(&doc_id))
                } else {
                    Err(BackendError::NotFound(doc_id))
                }
            }
            PathResolution::DocumentMeta(doc_id) => {
                if self.blocks.contains(&doc_id) {
                    Ok(EntryInfo::file("_meta", 0))
                } else {
                    Err(BackendError::NotFound(doc_id))
                }
            }
            PathResolution::Block(doc_id, block_id) => {
                let entry = self.blocks.get(&doc_id).ok_or_else(|| {
                    BackendError::NotFound(format!("document not found: {}", doc_id))
                })?;
                let blocks = entry.doc.blocks_ordered();
                let block = blocks.iter().find(|b| b.id == block_id).ok_or_else(|| {
                    BackendError::NotFound(format!("block not found: {}", block_id.to_key()))
                })?;
                Ok(EntryInfo::file(block_id.to_key(), block.content.len() as u64))
            }
            PathResolution::Invalid(msg) => Err(BackendError::InvalidOperation(msg)),
        }
    }

    async fn mkdir(&self, path: &Path) -> BackendResult<()> {
        match self.resolve_path(path) {
            PathResolution::Document(doc_id) => {
                if self.blocks.contains(&doc_id) {
                    return Err(BackendError::AlreadyExists(doc_id));
                }
                self.blocks
                    .create_document(doc_id, DocumentKind::Code, None)
                    .map_err(|e| BackendError::Io(e))?;
                Ok(())
            }
            PathResolution::Root | PathResolution::DocsRoot => {
                Err(BackendError::AlreadyExists(path.to_string_lossy().to_string()))
            }
            PathResolution::Block(_, _) | PathResolution::DocumentMeta(_) => {
                Err(BackendError::InvalidOperation("cannot mkdir on block or meta".into()))
            }
            PathResolution::Invalid(msg) => Err(BackendError::InvalidOperation(msg)),
        }
    }

    async fn remove(&self, path: &Path, recursive: bool) -> BackendResult<()> {
        match self.resolve_path(path) {
            PathResolution::Document(doc_id) => {
                if !self.blocks.contains(&doc_id) {
                    return Err(BackendError::NotFound(doc_id));
                }
                // Check if document has blocks and recursive is false
                if !recursive {
                    let entry = self.blocks.get(&doc_id).ok_or_else(|| {
                        BackendError::NotFound(format!("document not found: {}", doc_id))
                    })?;
                    if !entry.doc.blocks_ordered().is_empty() {
                        return Err(BackendError::InvalidOperation(
                            "document not empty, use recursive=true".into(),
                        ));
                    }
                }
                self.blocks
                    .delete_document(&doc_id)
                    .map_err(|e| BackendError::Io(e))?;
                Ok(())
            }
            PathResolution::Block(doc_id, block_id) => {
                self.blocks
                    .delete_block(&doc_id, &block_id)
                    .map_err(|e| BackendError::Io(e))?;
                Ok(())
            }
            PathResolution::Root | PathResolution::DocsRoot => {
                Err(BackendError::PermissionDenied("cannot remove root directories".into()))
            }
            PathResolution::DocumentMeta(_) => {
                Err(BackendError::PermissionDenied("cannot remove _meta".into()))
            }
            PathResolution::Invalid(msg) => Err(BackendError::InvalidOperation(msg)),
        }
    }

    async fn exists(&self, path: &Path) -> bool {
        match self.resolve_path(path) {
            PathResolution::Root | PathResolution::DocsRoot => true,
            PathResolution::Document(doc_id) => self.blocks.contains(&doc_id),
            PathResolution::DocumentMeta(doc_id) => self.blocks.contains(&doc_id),
            PathResolution::Block(doc_id, block_id) => {
                if let Some(entry) = self.blocks.get(&doc_id) {
                    entry.doc.blocks_ordered().iter().any(|b| b.id == block_id)
                } else {
                    false
                }
            }
            PathResolution::Invalid(_) => false,
        }
    }

    // =========================================================================
    // Tool Dispatch
    // =========================================================================

    async fn call_tool(
        &self,
        name: &str,
        args: ToolArgs,
        _ctx: &mut ExecContext,
    ) -> BackendResult<ToolResult> {
        // Convert ToolArgs to JSON for the execution engine
        let params_json = tool_args_to_json(&args);
        let params_str = serde_json::to_string(&params_json)
            .map_err(|e| BackendError::Io(e.to_string()))?;

        // Look up the engine from the kaijutsu kernel's tool registry
        let engine = self
            .kernel
            .get_engine(name)
            .await
            .ok_or_else(|| BackendError::ToolNotFound(name.to_string()))?;

        // Execute the tool
        let result = engine
            .execute(&params_str)
            .await
            .map_err(|e| BackendError::Io(e.to_string()))?;

        Ok(Self::convert_exec_result(result))
    }

    async fn list_tools(&self) -> BackendResult<Vec<ToolInfo>> {
        let tools = self.kernel.list_equipped().await;
        let infos: Vec<ToolInfo> = tools
            .iter()
            .map(|t| Self::convert_tool_info(t))
            .collect();
        Ok(infos)
    }

    async fn get_tool(&self, name: &str) -> BackendResult<Option<ToolInfo>> {
        let tool = self.kernel.get_tool(name).await;
        Ok(tool.map(|t| Self::convert_tool_info(&t)))
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
        // Report the docs namespace as a single mount
        vec![MountInfo {
            path: std::path::PathBuf::from("/docs"),
            read_only: false,
        }]
    }
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Apply a read range to content, returning the subset.
fn apply_read_range(content: &str, range: ReadRange) -> String {
    // Handle line-based ranges
    if range.start_line.is_some() || range.end_line.is_some() {
        let lines: Vec<&str> = content.lines().collect();
        let start = range.start_line.unwrap_or(1).saturating_sub(1);
        let end = range.end_line.unwrap_or(lines.len()).min(lines.len());
        return lines
            .get(start..end)
            .map(|slice| slice.join("\n"))
            .unwrap_or_default();
    }

    // Handle byte-based ranges
    if range.offset.is_some() || range.limit.is_some() {
        let offset = range.offset.unwrap_or(0) as usize;
        let limit = range.limit.unwrap_or(content.len() as u64) as usize;
        let end = (offset + limit).min(content.len());
        return content.get(offset..end).unwrap_or("").to_string();
    }

    content.to_string()
}

/// Apply a single patch operation to a block.
fn apply_patch_op(
    blocks: &SharedBlockStore,
    doc_id: &str,
    block_id: &BlockId,
    op: &PatchOp,
    current_content: &str,
) -> BackendResult<String> {
    match op {
        PatchOp::Insert { offset, content } => {
            blocks
                .edit_text(doc_id, block_id, *offset, content, 0)
                .map_err(|e| BackendError::Io(e))?;
            // Reconstruct content after edit
            let mut result = current_content.to_string();
            result.insert_str(*offset, content);
            Ok(result)
        }
        PatchOp::Delete { offset, len, expected } => {
            // CAS check if expected is provided
            if let Some(exp) = expected {
                let actual = current_content.get(*offset..*offset + *len).unwrap_or("");
                if actual != exp {
                    return Err(BackendError::Conflict(kaish_kernel::backend::ConflictError {
                        location: format!("offset {}", offset),
                        expected: exp.clone(),
                        actual: actual.to_string(),
                    }));
                }
            }
            blocks
                .edit_text(doc_id, block_id, *offset, "", *len)
                .map_err(|e| BackendError::Io(e))?;
            let mut result = current_content.to_string();
            result.replace_range(*offset..*offset + *len, "");
            Ok(result)
        }
        PatchOp::Replace { offset, len, content, expected } => {
            // CAS check if expected is provided
            if let Some(exp) = expected {
                let actual = current_content.get(*offset..*offset + *len).unwrap_or("");
                if actual != exp {
                    return Err(BackendError::Conflict(kaish_kernel::backend::ConflictError {
                        location: format!("offset {}", offset),
                        expected: exp.clone(),
                        actual: actual.to_string(),
                    }));
                }
            }
            blocks
                .edit_text(doc_id, block_id, *offset, content, *len)
                .map_err(|e| BackendError::Io(e))?;
            let mut result = current_content.to_string();
            result.replace_range(*offset..*offset + *len, content);
            Ok(result)
        }
        PatchOp::InsertLine { line, content } => {
            let line_offset = line_to_byte_offset(current_content, *line);
            blocks
                .edit_text(doc_id, block_id, line_offset, &format!("{}\n", content), 0)
                .map_err(|e| BackendError::Io(e))?;
            let mut result = current_content.to_string();
            result.insert_str(line_offset, &format!("{}\n", content));
            Ok(result)
        }
        PatchOp::DeleteLine { line, expected } => {
            let (start, end) = line_range(current_content, *line);
            let actual_line = current_content.get(start..end).unwrap_or("");

            // CAS check
            if let Some(exp) = expected {
                if actual_line.trim_end_matches('\n') != exp.trim_end_matches('\n') {
                    return Err(BackendError::Conflict(kaish_kernel::backend::ConflictError {
                        location: format!("line {}", line),
                        expected: exp.clone(),
                        actual: actual_line.to_string(),
                    }));
                }
            }

            blocks
                .edit_text(doc_id, block_id, start, "", end - start)
                .map_err(|e| BackendError::Io(e))?;
            let mut result = current_content.to_string();
            result.replace_range(start..end, "");
            Ok(result)
        }
        PatchOp::ReplaceLine { line, content, expected } => {
            let (start, end) = line_range(current_content, *line);
            let actual_line = current_content.get(start..end).unwrap_or("");

            // CAS check
            if let Some(exp) = expected {
                if actual_line.trim_end_matches('\n') != exp.trim_end_matches('\n') {
                    return Err(BackendError::Conflict(kaish_kernel::backend::ConflictError {
                        location: format!("line {}", line),
                        expected: exp.clone(),
                        actual: actual_line.to_string(),
                    }));
                }
            }

            let replacement = format!("{}\n", content);
            blocks
                .edit_text(doc_id, block_id, start, &replacement, end - start)
                .map_err(|e| BackendError::Io(e))?;
            let mut result = current_content.to_string();
            result.replace_range(start..end, &replacement);
            Ok(result)
        }
        PatchOp::Append { content } => {
            blocks
                .append_text(doc_id, block_id, content)
                .map_err(|e| BackendError::Io(e))?;
            Ok(format!("{}{}", current_content, content))
        }
    }
}

/// Get byte offset for a 1-indexed line number.
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

    // Add positional args as "_positional"
    if !args.positional.is_empty() {
        let positional: Vec<JsonValue> = args
            .positional
            .iter()
            .map(kaish_value_to_json)
            .collect();
        obj.insert("_positional".to_string(), JsonValue::Array(positional));
    }

    // Add named args
    for (key, value) in &args.named {
        obj.insert(key.clone(), kaish_value_to_json(value));
    }

    // Add flags as booleans
    for flag in &args.flags {
        obj.insert(flag.clone(), JsonValue::Bool(true));
    }

    JsonValue::Object(obj)
}

/// Convert a kaish Value to JSON.
fn kaish_value_to_json(value: &kaish_kernel::ast::Value) -> JsonValue {
    use kaish_kernel::ast::Value;
    match value {
        Value::String(s) => JsonValue::String(s.clone()),
        Value::Int(i) => JsonValue::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        Value::Bool(b) => JsonValue::Bool(*b),
        Value::Null => JsonValue::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_kernel::block_store::shared_block_store;

    #[tokio::test]
    async fn test_path_resolution() {
        let blocks = shared_block_store("test");
        let kernel = Arc::new(KaijutsuKernel::new("test").await);
        let backend = KaijutsuBackend::new(blocks, kernel);

        // Test root paths
        assert!(matches!(
            backend.resolve_path(Path::new("/")),
            PathResolution::Root
        ));
        assert!(matches!(
            backend.resolve_path(Path::new("/docs")),
            PathResolution::DocsRoot
        ));

        // Test document paths
        match backend.resolve_path(Path::new("/docs/my-doc")) {
            PathResolution::Document(id) => assert_eq!(id, "my-doc"),
            _ => panic!("Expected Document"),
        }

        // Test meta paths
        match backend.resolve_path(Path::new("/docs/my-doc/_meta")) {
            PathResolution::DocumentMeta(id) => assert_eq!(id, "my-doc"),
            _ => panic!("Expected DocumentMeta"),
        }
    }

    #[test]
    fn test_line_to_byte_offset() {
        let content = "line 1\nline 2\nline 3";

        assert_eq!(line_to_byte_offset(content, 1), 0);
        assert_eq!(line_to_byte_offset(content, 2), 7);  // "line 1\n" = 7 bytes
        assert_eq!(line_to_byte_offset(content, 3), 14); // "line 1\nline 2\n" = 14 bytes
    }

    #[test]
    fn test_line_range() {
        let content = "line 1\nline 2\nline 3";

        assert_eq!(line_range(content, 1), (0, 7));   // "line 1\n"
        assert_eq!(line_range(content, 2), (7, 14));  // "line 2\n"
        assert_eq!(line_range(content, 3), (14, 20)); // "line 3" (no trailing newline)
    }

    #[test]
    fn test_apply_read_range_lines() {
        let content = "line 1\nline 2\nline 3\nline 4";

        // end_line is inclusive (per kaish ReadRange documentation)
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
}
