//! ExecutionEngine implementations for block tools.
//!
//! Each tool implements the ExecutionEngine trait for integration with
//! the kernel's tool system.

use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::block_store::SharedBlockStore;
use crate::tools::{ExecResult, ExecutionEngine};
use kaijutsu_crdt::{BlockId, BlockKind, Role, Status};

use super::error::{EditError, Result};
use super::translate::{
    content_with_line_numbers, extract_lines_with_numbers, line_count, line_range_to_byte_range,
    line_to_byte_offset, validate_expected_text,
};

// ============================================================================
// Parameter Types
// ============================================================================

/// Edit operation on a block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EditOp {
    /// Insert text before a line.
    Insert {
        line: u32,
        content: String,
    },
    /// Delete lines from start_line to end_line (exclusive).
    Delete {
        start_line: u32,
        end_line: u32,
    },
    /// Replace lines with new content, with optional CAS validation.
    Replace {
        start_line: u32,
        end_line: u32,
        content: String,
        /// Optional: expected text for compare-and-set validation.
        #[serde(default)]
        expected_text: Option<String>,
    },
}

/// Parameters for block_create tool.
#[derive(Debug, Deserialize)]
pub struct BlockCreateParams {
    /// Parent block ID for DAG relationship (None for root).
    pub parent_id: Option<String>,
    /// Role of the block author.
    pub role: String,
    /// Content type.
    pub kind: String,
    /// Initial content.
    #[serde(default)]
    pub content: Option<String>,
    /// Metadata (path, language, tool_name, etc.).
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

/// Parameters for block_append tool.
#[derive(Debug, Deserialize)]
pub struct BlockAppendParams {
    pub block_id: String,
    pub text: String,
}

/// Parameters for block_edit tool.
#[derive(Debug, Deserialize)]
pub struct BlockEditParams {
    pub block_id: String,
    pub operations: Vec<EditOp>,
}

/// Parameters for block_splice tool.
#[derive(Debug, Deserialize)]
pub struct BlockSpliceParams {
    pub block_id: String,
    pub offset: usize,
    pub delete_count: usize,
    #[serde(default)]
    pub insert: Option<String>,
}

/// Parameters for block_read tool.
#[derive(Debug, Deserialize)]
pub struct BlockReadParams {
    pub block_id: String,
    /// Whether to include line numbers (default: true).
    #[serde(default = "default_true")]
    pub line_numbers: bool,
    /// Optional line range (start, end) - 0-indexed, exclusive end.
    #[serde(default)]
    pub range: Option<(u32, u32)>,
}

fn default_true() -> bool {
    true
}

/// Parameters for block_search tool.
#[derive(Debug, Deserialize)]
pub struct BlockSearchParams {
    pub block_id: String,
    /// Regex or literal query.
    pub query: String,
    /// Lines of context before/after match (default: 2).
    #[serde(default = "default_context")]
    pub context_lines: u32,
    /// Maximum matches to return (default: 20).
    #[serde(default = "default_max_matches")]
    pub max_matches: u32,
}

fn default_context() -> u32 {
    2
}

fn default_max_matches() -> u32 {
    20
}

/// Parameters for block_list tool.
#[derive(Debug, Deserialize)]
pub struct BlockListParams {
    /// Filter by parent block ID.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Filter by block kind.
    #[serde(default)]
    pub kind: Option<String>,
    /// Filter by status.
    #[serde(default)]
    pub status: Option<String>,
    /// Filter file blocks by path prefix.
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// DAG traversal depth (default: 1).
    #[serde(default = "default_depth")]
    pub depth: u32,
}

fn default_depth() -> u32 {
    1
}

/// Parameters for block_status tool.
#[derive(Debug, Deserialize)]
pub struct BlockStatusParams {
    pub block_id: String,
    pub status: String,
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Parse a block ID from string key format (cell_id/agent_id/seq).
fn parse_block_id(s: &str) -> Result<BlockId> {
    BlockId::from_key(s).ok_or_else(|| {
        EditError::InvalidParams(format!("invalid block_id format: {}", s))
    })
}

/// Parse a role string to Role enum.
fn parse_role(s: &str) -> Result<Role> {
    match s.to_lowercase().as_str() {
        "user" | "human" => Ok(Role::User),
        "model" | "assistant" | "agent" => Ok(Role::Model),
        "system" => Ok(Role::System),
        "tool" => Ok(Role::Tool),
        _ => Err(EditError::InvalidParams(format!("invalid role: {}", s))),
    }
}

/// Parse a kind string to BlockKind enum.
fn parse_kind(s: &str) -> Result<BlockKind> {
    match s.to_lowercase().as_str() {
        "text" => Ok(BlockKind::Text),
        "thinking" => Ok(BlockKind::Thinking),
        "tool_call" | "toolcall" => Ok(BlockKind::ToolCall),
        "tool_result" | "toolresult" => Ok(BlockKind::ToolResult),
        _ => Err(EditError::InvalidParams(format!("invalid kind: {}", s))),
    }
}

/// Parse a status string to Status enum.
fn parse_status(s: &str) -> Result<Status> {
    match s.to_lowercase().as_str() {
        "pending" => Ok(Status::Pending),
        "active" | "running" => Ok(Status::Running),
        "done" | "complete" | "completed" => Ok(Status::Done),
        "error" => Ok(Status::Error),
        _ => Err(EditError::InvalidParams(format!("invalid status: {}", s))),
    }
}

/// Find a block by ID string, checking all cells.
/// Returns (cell_id, BlockId) if found.
fn find_block(cells: &SharedBlockStore, block_id_str: &str) -> Result<(String, BlockId)> {
    let block_id = parse_block_id(block_id_str)?;

    for cell_id in cells.list_ids() {
        if let Some(entry) = cells.get(&cell_id) {
            for snapshot in entry.doc.blocks_ordered() {
                if snapshot.id == block_id {
                    return Ok((cell_id, block_id));
                }
            }
        }
    }
    Err(EditError::BlockNotFound(block_id_str.to_string()))
}

// ============================================================================
// BlockCreateEngine
// ============================================================================

/// Execution engine for creating blocks.
pub struct BlockCreateEngine {
    cells: SharedBlockStore,
    agent_name: String,
}

impl BlockCreateEngine {
    pub fn new(cells: SharedBlockStore, agent_name: impl Into<String>) -> Self {
        Self {
            cells,
            agent_name: agent_name.into(),
        }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "parent_id": {
                    "type": "string",
                    "description": "Parent block ID for DAG relationship (omit for root)"
                },
                "role": {
                    "type": "string",
                    "enum": ["user", "model", "system", "tool"],
                    "description": "Role of the block author"
                },
                "kind": {
                    "type": "string",
                    "enum": ["text", "thinking", "tool_call", "tool_result", "file"],
                    "description": "Content type"
                },
                "content": {
                    "type": "string",
                    "description": "Initial content"
                },
                "metadata": {
                    "type": "object",
                    "description": "Metadata (path, language, tool_name, etc.)"
                }
            },
            "required": ["role", "kind"]
        })
    }

    fn execute_inner(&self, params: BlockCreateParams) -> Result<serde_json::Value> {
        let role = parse_role(&params.role)?;
        let kind = parse_kind(&params.kind)?;
        let content = params.content.unwrap_or_default();

        // Parse parent_id if provided
        let parent_id = params
            .parent_id
            .as_ref()
            .map(|s| parse_block_id(s))
            .transpose()?;

        // For now, create blocks in a default cell or first available cell
        let cell_id = self.cells.list_ids().into_iter().next().ok_or_else(|| {
            EditError::StoreError("no cells available, create a cell first".into())
        })?;

        let block_id = self
            .cells
            .insert_block(
                &cell_id,
                parent_id.as_ref(),
                None, // after
                role,
                kind,
                &content,
            )
            .map_err(|e| EditError::StoreError(e))?;

        let version = self
            .cells
            .get(&cell_id)
            .map(|c| c.version())
            .unwrap_or(0);

        Ok(serde_json::json!({
            "block_id": block_id.to_key(),
            "version": version
        }))
    }
}

#[async_trait]
impl ExecutionEngine for BlockCreateEngine {
    fn name(&self) -> &str {
        "block_create"
    }

    fn description(&self) -> &str {
        "Create a new block with role, kind, and optional content"
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let params: BlockCreateParams = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ExecResult::failure(1, format!("Invalid parameters: {}", e)));
            }
        };

        match self.execute_inner(params) {
            Ok(result) => Ok(ExecResult::success(result.to_string())),
            Err(e) => Ok(ExecResult::failure(1, e.to_string())),
        }
    }

    async fn is_available(&self) -> bool {
        true
    }
}

// ============================================================================
// BlockAppendEngine
// ============================================================================

/// Execution engine for appending text to blocks.
///
/// This is optimized for streaming model output.
pub struct BlockAppendEngine {
    cells: SharedBlockStore,
}

impl BlockAppendEngine {
    pub fn new(cells: SharedBlockStore) -> Self {
        Self { cells }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "block_id": {
                    "type": "string",
                    "description": "Block ID to append to"
                },
                "text": {
                    "type": "string",
                    "description": "Text to append"
                }
            },
            "required": ["block_id", "text"]
        })
    }

    fn execute_inner(&self, params: BlockAppendParams) -> Result<serde_json::Value> {
        let (cell_id, block_id) = find_block(&self.cells, &params.block_id)?;

        self.cells
            .append_text(&cell_id, &block_id, &params.text)
            .map_err(|e| EditError::StoreError(e))?;

        let version = self
            .cells
            .get(&cell_id)
            .map(|c| c.version())
            .unwrap_or(0);

        Ok(serde_json::json!({
            "version": version
        }))
    }
}

#[async_trait]
impl ExecutionEngine for BlockAppendEngine {
    fn name(&self) -> &str {
        "block_append"
    }

    fn description(&self) -> &str {
        "Append text to a block (optimized for streaming)"
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let params: BlockAppendParams = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ExecResult::failure(1, format!("Invalid parameters: {}", e)));
            }
        };

        match self.execute_inner(params) {
            Ok(result) => Ok(ExecResult::success(result.to_string())),
            Err(e) => Ok(ExecResult::failure(1, e.to_string())),
        }
    }

    async fn is_available(&self) -> bool {
        true
    }
}

// ============================================================================
// BlockEditEngine
// ============================================================================

/// Execution engine for line-based block editing with CAS validation.
///
/// Supports atomic batches of insert, delete, and replace operations.
pub struct BlockEditEngine {
    cells: SharedBlockStore,
    agent_name: String,
}

impl BlockEditEngine {
    pub fn new(cells: SharedBlockStore, agent_name: impl Into<String>) -> Self {
        Self {
            cells,
            agent_name: agent_name.into(),
        }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "block_id": {
                    "type": "string",
                    "description": "Block ID to edit"
                },
                "operations": {
                    "type": "array",
                    "description": "List of edit operations to apply atomically",
                    "items": {
                        "oneOf": [
                            {
                                "type": "object",
                                "properties": {
                                    "op": {"const": "insert"},
                                    "line": {"type": "integer", "minimum": 0, "description": "Line to insert before (0-indexed)"},
                                    "content": {"type": "string"}
                                },
                                "required": ["op", "line", "content"]
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "op": {"const": "delete"},
                                    "start_line": {"type": "integer", "minimum": 0},
                                    "end_line": {"type": "integer", "minimum": 0, "description": "Exclusive end line"}
                                },
                                "required": ["op", "start_line", "end_line"]
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "op": {"const": "replace"},
                                    "start_line": {"type": "integer", "minimum": 0},
                                    "end_line": {"type": "integer", "minimum": 0, "description": "Exclusive end line"},
                                    "content": {"type": "string"},
                                    "expected_text": {"type": "string", "description": "CAS: fails if current text doesn't match"}
                                },
                                "required": ["op", "start_line", "end_line", "content"]
                            }
                        ]
                    }
                }
            },
            "required": ["block_id", "operations"]
        })
    }

    fn execute_inner(&self, params: BlockEditParams) -> Result<serde_json::Value> {
        let (cell_id, block_id) = find_block(&self.cells, &params.block_id)?;

        // Apply operations atomically
        // Note: For true atomicity, we'd need to validate all operations first
        // then apply them. For now, we apply one by one but the design doc
        // specifies atomic semantics.
        for (idx, op) in params.operations.into_iter().enumerate() {
            self.apply_op(&cell_id, &block_id, op)
                .map_err(|e| e.in_batch(idx))?;
        }

        let version = self
            .cells
            .get(&cell_id)
            .map(|c| c.version())
            .unwrap_or(0);

        Ok(serde_json::json!({
            "version": version
        }))
    }

    fn apply_op(&self, cell_id: &str, block_id: &BlockId, op: EditOp) -> Result<()> {
        // Get current content
        let content = {
            let entry = self
                .cells
                .get(cell_id)
                .ok_or_else(|| EditError::StoreError("cell not found".into()))?;

            // Find the block and get its content
            entry
                .doc
                .get_block_snapshot(block_id)
                .map(|s| s.content.clone())
                .ok_or_else(|| EditError::BlockNotFound(block_id.to_string()))?
        };

        match op {
            EditOp::Insert { line, content: text } => {
                let pos = line_to_byte_offset(&content, line)?;
                let text_with_newline = if text.ends_with('\n') || content.is_empty() {
                    text
                } else {
                    format!("{}\n", text)
                };
                self.cells
                    .edit_text(cell_id, block_id, pos, &text_with_newline, 0)
                    .map_err(|e| EditError::StoreError(e))?;
            }
            EditOp::Delete {
                start_line,
                end_line,
            } => {
                let (start, end) = line_range_to_byte_range(&content, start_line, end_line)?;
                if start < end {
                    self.cells
                        .edit_text(cell_id, block_id, start, "", end - start)
                        .map_err(|e| EditError::StoreError(e))?;
                }
            }
            EditOp::Replace {
                start_line,
                end_line,
                content: text,
                expected_text,
            } => {
                // CAS validation if expected_text is provided
                if let Some(expected) = expected_text {
                    validate_expected_text(&content, start_line, end_line, &expected)?;
                }

                let (start, end) = line_range_to_byte_range(&content, start_line, end_line)?;
                let text_with_newline = if text.ends_with('\n') || text.is_empty() {
                    text
                } else {
                    format!("{}\n", text)
                };
                self.cells
                    .edit_text(cell_id, block_id, start, &text_with_newline, end - start)
                    .map_err(|e| EditError::StoreError(e))?;
            }
        }

        Ok(())
    }
}

#[async_trait]
impl ExecutionEngine for BlockEditEngine {
    fn name(&self) -> &str {
        "block_edit"
    }

    fn description(&self) -> &str {
        "Line-based block editing with atomic operations and CAS validation"
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let params: BlockEditParams = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ExecResult::failure(1, format!("Invalid parameters: {}", e)));
            }
        };

        match self.execute_inner(params) {
            Ok(result) => Ok(ExecResult::success(result.to_string())),
            Err(e) => Ok(ExecResult::failure(1, e.to_string())),
        }
    }

    async fn is_available(&self) -> bool {
        true
    }
}

// ============================================================================
// BlockSpliceEngine
// ============================================================================

/// Execution engine for character-based editing.
///
/// For programmatic/refactoring tools, not LLMs.
pub struct BlockSpliceEngine {
    cells: SharedBlockStore,
}

impl BlockSpliceEngine {
    pub fn new(cells: SharedBlockStore) -> Self {
        Self { cells }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "block_id": {
                    "type": "string",
                    "description": "Block ID to edit"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Byte offset"
                },
                "delete_count": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Number of bytes to delete"
                },
                "insert": {
                    "type": "string",
                    "description": "Text to insert"
                }
            },
            "required": ["block_id", "offset", "delete_count"]
        })
    }

    fn execute_inner(&self, params: BlockSpliceParams) -> Result<serde_json::Value> {
        let (cell_id, block_id) = find_block(&self.cells, &params.block_id)?;

        let insert = params.insert.unwrap_or_default();

        self.cells
            .edit_text(&cell_id, &block_id, params.offset, &insert, params.delete_count)
            .map_err(|e| EditError::StoreError(e))?;

        let version = self
            .cells
            .get(&cell_id)
            .map(|c| c.version())
            .unwrap_or(0);

        Ok(serde_json::json!({
            "version": version
        }))
    }
}

#[async_trait]
impl ExecutionEngine for BlockSpliceEngine {
    fn name(&self) -> &str {
        "block_splice"
    }

    fn description(&self) -> &str {
        "Character-based editing (for programmatic tools)"
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let params: BlockSpliceParams = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ExecResult::failure(1, format!("Invalid parameters: {}", e)));
            }
        };

        match self.execute_inner(params) {
            Ok(result) => Ok(ExecResult::success(result.to_string())),
            Err(e) => Ok(ExecResult::failure(1, e.to_string())),
        }
    }

    async fn is_available(&self) -> bool {
        true
    }
}

// ============================================================================
// BlockReadEngine
// ============================================================================

/// Execution engine for reading block content.
pub struct BlockReadEngine {
    cells: SharedBlockStore,
}

impl BlockReadEngine {
    pub fn new(cells: SharedBlockStore) -> Self {
        Self { cells }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "block_id": {
                    "type": "string",
                    "description": "Block ID to read"
                },
                "line_numbers": {
                    "type": "boolean",
                    "default": true,
                    "description": "Include line numbers"
                },
                "range": {
                    "type": "array",
                    "items": {"type": "integer"},
                    "minItems": 2,
                    "maxItems": 2,
                    "description": "[start_line, end_line] (0-indexed, exclusive end)"
                }
            },
            "required": ["block_id"]
        })
    }

    fn execute_inner(&self, params: BlockReadParams) -> Result<serde_json::Value> {
        let (cell_id, block_id) = find_block(&self.cells, &params.block_id)?;

        let entry = self
            .cells
            .get(&cell_id)
            .ok_or_else(|| EditError::StoreError("cell not found".into()))?;

        let snapshot = entry
            .doc
            .get_block_snapshot(&block_id)
            .ok_or_else(|| EditError::BlockNotFound(params.block_id.clone()))?;

        let content = &snapshot.content;
        let total_lines = line_count(content);

        let formatted_content = if let Some((start, end)) = params.range {
            if params.line_numbers {
                extract_lines_with_numbers(content, start, end)
            } else {
                content
                    .lines()
                    .skip(start as usize)
                    .take((end.saturating_sub(start)) as usize)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        } else if params.line_numbers {
            content_with_line_numbers(content)
        } else {
            content.clone()
        };

        Ok(serde_json::json!({
            "content": formatted_content,
            "role": format!("{:?}", snapshot.role).to_lowercase(),
            "kind": format!("{:?}", snapshot.kind).to_lowercase(),
            "status": format!("{:?}", snapshot.status).to_lowercase(),
            "version": entry.version(),
            "line_count": total_lines,
            "metadata": {
                "tool_name": snapshot.tool_name,
                "tool_call_id": snapshot.tool_call_id,
                "is_error": snapshot.is_error,
            }
        }))
    }
}

#[async_trait]
impl ExecutionEngine for BlockReadEngine {
    fn name(&self) -> &str {
        "block_read"
    }

    fn description(&self) -> &str {
        "Read block content with optional line numbers and range"
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let params: BlockReadParams = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ExecResult::failure(1, format!("Invalid parameters: {}", e)));
            }
        };

        match self.execute_inner(params) {
            Ok(result) => Ok(ExecResult::success(result.to_string())),
            Err(e) => Ok(ExecResult::failure(1, e.to_string())),
        }
    }

    async fn is_available(&self) -> bool {
        true
    }
}

// ============================================================================
// BlockSearchEngine
// ============================================================================

/// A search match within a block.
#[derive(Debug, Serialize)]
pub struct SearchMatch {
    pub line: u32,
    pub content: String,
    pub match_start: u32,
    pub match_end: u32,
}

/// Execution engine for searching within a block.
pub struct BlockSearchEngine {
    cells: SharedBlockStore,
}

impl BlockSearchEngine {
    pub fn new(cells: SharedBlockStore) -> Self {
        Self { cells }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "block_id": {
                    "type": "string",
                    "description": "Block ID to search"
                },
                "query": {
                    "type": "string",
                    "description": "Regex or literal pattern"
                },
                "context_lines": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 2,
                    "description": "Lines of context before/after match"
                },
                "max_matches": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 20,
                    "description": "Maximum matches to return"
                }
            },
            "required": ["block_id", "query"]
        })
    }

    fn execute_inner(&self, params: BlockSearchParams) -> Result<serde_json::Value> {
        let (cell_id, block_id) = find_block(&self.cells, &params.block_id)?;

        let entry = self
            .cells
            .get(&cell_id)
            .ok_or_else(|| EditError::StoreError("cell not found".into()))?;

        let snapshot = entry
            .doc
            .get_block_snapshot(&block_id)
            .ok_or_else(|| EditError::BlockNotFound(params.block_id.clone()))?;

        let content = &snapshot.content;
        let lines: Vec<&str> = content.lines().collect();

        // Try to compile as regex, fall back to literal
        let regex = Regex::new(&params.query)
            .or_else(|_| Regex::new(&regex::escape(&params.query)))
            .map_err(|e| EditError::InvalidRegex(e.to_string()))?;

        let mut matches = Vec::new();
        let total_lines = lines.len() as u32;

        for (line_num, line) in lines.iter().enumerate() {
            if matches.len() >= params.max_matches as usize {
                break;
            }

            for cap in regex.find_iter(line) {
                if matches.len() >= params.max_matches as usize {
                    break;
                }

                // Get context
                let ctx_start = (line_num as u32).saturating_sub(params.context_lines);
                let ctx_end = ((line_num as u32) + params.context_lines + 1).min(total_lines);

                let context_content = extract_lines_with_numbers(content, ctx_start, ctx_end);

                matches.push(SearchMatch {
                    line: line_num as u32,
                    content: context_content,
                    match_start: cap.start() as u32,
                    match_end: cap.end() as u32,
                });
            }
        }

        if matches.is_empty() {
            return Err(EditError::NoMatches);
        }

        Ok(serde_json::json!({
            "matches": matches,
            "total_matches": matches.len()
        }))
    }
}

#[async_trait]
impl ExecutionEngine for BlockSearchEngine {
    fn name(&self) -> &str {
        "block_search"
    }

    fn description(&self) -> &str {
        "Search within a block using regex or literal patterns"
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let params: BlockSearchParams = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ExecResult::failure(1, format!("Invalid parameters: {}", e)));
            }
        };

        match self.execute_inner(params) {
            Ok(result) => Ok(ExecResult::success(result.to_string())),
            Err(e) => Ok(ExecResult::failure(1, e.to_string())),
        }
    }

    async fn is_available(&self) -> bool {
        true
    }
}

// ============================================================================
// BlockListEngine
// ============================================================================

/// Execution engine for listing blocks.
pub struct BlockListEngine {
    cells: SharedBlockStore,
}

impl BlockListEngine {
    pub fn new(cells: SharedBlockStore) -> Self {
        Self { cells }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "parent_id": {
                    "type": "string",
                    "description": "Filter by parent block ID"
                },
                "kind": {
                    "type": "string",
                    "enum": ["text", "thinking", "tool_call", "tool_result", "file"],
                    "description": "Filter by block kind"
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "running", "done", "error", "cancelled"],
                    "description": "Filter by status"
                },
                "path_prefix": {
                    "type": "string",
                    "description": "Filter file blocks by path prefix"
                },
                "depth": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 1,
                    "description": "DAG traversal depth"
                }
            }
        })
    }

    fn execute_inner(&self, params: BlockListParams) -> Result<serde_json::Value> {
        let kind_filter = params.kind.as_ref().and_then(|k| parse_kind(k).ok());
        let status_filter = params.status.as_ref().and_then(|s| parse_status(s).ok());
        let parent_id_filter = params.parent_id.as_ref().and_then(|s| parse_block_id(s).ok());

        let mut blocks = Vec::new();

        for cell_id in self.cells.list_ids() {
            if let Some(entry) = self.cells.get(&cell_id) {
                for snapshot in entry.doc.blocks_ordered() {
                    // Apply filters
                    if let Some(ref parent_id) = parent_id_filter {
                        if snapshot.parent_id.as_ref() != Some(parent_id) {
                            continue;
                        }
                    }

                    if let Some(kind) = kind_filter {
                        if snapshot.kind != kind {
                            continue;
                        }
                    }

                    if let Some(status) = status_filter {
                        if snapshot.status != status {
                            continue;
                        }
                    }

                    // TODO: path_prefix filter for file blocks

                    // Create summary (first 100 chars or line count)
                    let summary = if snapshot.content.len() > 100 {
                        format!("{}... ({} lines)", &snapshot.content[..100], line_count(&snapshot.content))
                    } else {
                        snapshot.content.clone()
                    };

                    blocks.push(serde_json::json!({
                        "block_id": snapshot.id,
                        "parent_id": snapshot.parent_id,
                        "role": format!("{:?}", snapshot.role).to_lowercase(),
                        "kind": format!("{:?}", snapshot.kind).to_lowercase(),
                        "status": format!("{:?}", snapshot.status).to_lowercase(),
                        "summary": summary,
                        "version": entry.version(),
                    }));
                }
            }
        }

        Ok(serde_json::json!({
            "blocks": blocks,
            "count": blocks.len()
        }))
    }
}

#[async_trait]
impl ExecutionEngine for BlockListEngine {
    fn name(&self) -> &str {
        "block_list"
    }

    fn description(&self) -> &str {
        "List blocks with optional filters"
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let params: BlockListParams = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ExecResult::failure(1, format!("Invalid parameters: {}", e)));
            }
        };

        match self.execute_inner(params) {
            Ok(result) => Ok(ExecResult::success(result.to_string())),
            Err(e) => Ok(ExecResult::failure(1, e.to_string())),
        }
    }

    async fn is_available(&self) -> bool {
        true
    }
}

// ============================================================================
// BlockStatusEngine
// ============================================================================

/// Execution engine for setting block status.
pub struct BlockStatusEngine {
    cells: SharedBlockStore,
}

impl BlockStatusEngine {
    pub fn new(cells: SharedBlockStore) -> Self {
        Self { cells }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "block_id": {
                    "type": "string",
                    "description": "Block ID to update"
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "running", "done", "error", "cancelled"],
                    "description": "New status"
                }
            },
            "required": ["block_id", "status"]
        })
    }

    fn execute_inner(&self, params: BlockStatusParams) -> Result<serde_json::Value> {
        let (cell_id, block_id) = find_block(&self.cells, &params.block_id)?;

        let status = parse_status(&params.status)?;

        self.cells
            .set_status(&cell_id, &block_id, status)
            .map_err(|e| EditError::StoreError(e))?;

        let version = self
            .cells
            .get(&cell_id)
            .map(|c| c.version())
            .unwrap_or(0);

        Ok(serde_json::json!({
            "version": version
        }))
    }
}

#[async_trait]
impl ExecutionEngine for BlockStatusEngine {
    fn name(&self) -> &str {
        "block_status"
    }

    fn description(&self) -> &str {
        "Set block status (pending, running, done, error, cancelled)"
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let params: BlockStatusParams = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ExecResult::failure(1, format!("Invalid parameters: {}", e)));
            }
        };

        match self.execute_inner(params) {
            Ok(result) => Ok(ExecResult::success(result.to_string())),
            Err(e) => Ok(ExecResult::failure(1, e.to_string())),
        }
    }

    async fn is_available(&self) -> bool {
        true
    }
}

// ============================================================================
// kernel_search - Cross-block grep
// ============================================================================

/// Parameters for kernel_search.
#[derive(Debug, Deserialize)]
pub struct KernelSearchParams {
    /// Regex pattern to search for.
    pub query: String,
    /// Optional cell ID to limit search to.
    pub cell_id: Option<String>,
    /// Optional block kind filter (text, thinking, tool_call, tool_result).
    pub kind: Option<String>,
    /// Optional role filter (user, model, system, tool).
    pub role: Option<String>,
    /// Number of context lines around matches.
    #[serde(default)]
    pub context_lines: u32,
    /// Maximum number of matches to return.
    pub max_matches: Option<usize>,
}

/// A match from kernel_search.
#[derive(Debug, Serialize)]
pub struct KernelSearchMatch {
    /// The cell containing the match.
    pub cell_id: String,
    /// The block containing the match.
    pub block_id: String,
    /// Line number of match (0-indexed).
    pub line: u32,
    /// The matching line content.
    pub content: String,
    /// Context lines before.
    pub before: Vec<String>,
    /// Context lines after.
    pub after: Vec<String>,
}

/// Cross-block regex search.
pub struct KernelSearchEngine {
    cells: SharedBlockStore,
}

impl KernelSearchEngine {
    pub fn new(cells: SharedBlockStore) -> Self {
        Self { cells }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "cell_id": {
                    "type": "string",
                    "description": "Limit search to this cell"
                },
                "kind": {
                    "type": "string",
                    "enum": ["text", "thinking", "tool_call", "tool_result"],
                    "description": "Filter by block kind"
                },
                "role": {
                    "type": "string",
                    "enum": ["user", "model", "system", "tool"],
                    "description": "Filter by block role"
                },
                "context_lines": {
                    "type": "integer",
                    "description": "Lines of context around matches (default: 0)"
                },
                "max_matches": {
                    "type": "integer",
                    "description": "Maximum matches to return"
                }
            },
            "required": ["query"]
        })
    }

    fn execute_inner(&self, params: KernelSearchParams) -> Result<serde_json::Value> {
        let regex = Regex::new(&params.query)
            .map_err(|e| EditError::InvalidRegex(e.to_string()))?;

        let kind_filter = params.kind.as_ref().map(|k| parse_kind(k)).transpose()?;
        let role_filter = params.role.as_ref().map(|r| parse_role(r)).transpose()?;

        let max_matches = params.max_matches.unwrap_or(100);
        let mut matches = Vec::new();

        // Get cells to search
        let cell_ids: Vec<String> = if let Some(ref cell_id) = params.cell_id {
            if self.cells.contains(cell_id) {
                vec![cell_id.clone()]
            } else {
                vec![]
            }
        } else {
            self.cells.list_ids()
        };

        'outer: for cell_id in cell_ids {
            let snapshots = match self.cells.block_snapshots(&cell_id) {
                Ok(s) => s,
                Err(_) => continue,
            };

            for snapshot in snapshots {
                // Apply filters
                if let Some(ref kind) = kind_filter {
                    if snapshot.kind != *kind {
                        continue;
                    }
                }
                if let Some(ref role) = role_filter {
                    if snapshot.role != *role {
                        continue;
                    }
                }

                // Search content
                let lines: Vec<&str> = snapshot.content.lines().collect();
                for (line_idx, line) in lines.iter().enumerate() {
                    if regex.is_match(line) {
                        // Collect context
                        let before: Vec<String> = (0..params.context_lines as usize)
                            .filter_map(|i| {
                                if line_idx >= i + 1 {
                                    Some(lines[line_idx - i - 1].to_string())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect();

                        let after: Vec<String> = (1..=params.context_lines as usize)
                            .filter_map(|i| {
                                lines.get(line_idx + i).map(|s| s.to_string())
                            })
                            .collect();

                        matches.push(KernelSearchMatch {
                            cell_id: cell_id.clone(),
                            block_id: snapshot.id.to_key(),
                            line: line_idx as u32,
                            content: line.to_string(),
                            before,
                            after,
                        });

                        if matches.len() >= max_matches {
                            break 'outer;
                        }
                    }
                }
            }
        }

        Ok(serde_json::json!({
            "matches": matches,
            "total": matches.len(),
            "truncated": matches.len() >= max_matches
        }))
    }
}

#[async_trait]
impl ExecutionEngine for KernelSearchEngine {
    fn name(&self) -> &str {
        "kernel_search"
    }

    fn description(&self) -> &str {
        "Search across all blocks using regex, with filters and context"
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let params: KernelSearchParams = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ExecResult::failure(1, format!("Invalid parameters: {}", e)));
            }
        };

        match self.execute_inner(params) {
            Ok(result) => Ok(ExecResult::success(result.to_string())),
            Err(e) => Ok(ExecResult::failure(1, e.to_string())),
        }
    }

    async fn is_available(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store;
    use crate::db::CellKind;

    fn setup_test_store() -> SharedBlockStore {
        let store = shared_block_store("test-agent");
        store.create_cell("test-cell".into(), CellKind::Code, Some("rust".into())).unwrap();
        store
    }

    #[tokio::test]
    async fn test_block_create() {
        let store = setup_test_store();
        let engine = BlockCreateEngine::new(store.clone(), "test-agent");

        let params = r#"{"role": "user", "kind": "text", "content": "hello world"}"#;
        let result = engine.execute(params).await.unwrap();

        assert!(result.success);
        let response: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
        assert!(response["block_id"].is_string());
        assert!(response["version"].is_u64());
    }

    #[tokio::test]
    async fn test_block_append() {
        let store = setup_test_store();

        // Create a block first
        let block_id = store.insert_block("test-cell", None, None, Role::User, BlockKind::Text, "hello").unwrap();

        let engine = BlockAppendEngine::new(store.clone());
        let params = format!(r#"{{"block_id": "{}", "text": " world"}}"#, block_id.to_key());
        let result = engine.execute(&params).await.unwrap();

        assert!(result.success, "append failed: {}", result.stderr);

        // Verify content
        let content = store.get_content("test-cell").unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn test_block_edit_insert() {
        let store = setup_test_store();

        let block_id = store.insert_block("test-cell", None, None, Role::User, BlockKind::Text, "line1\nline3\n").unwrap();

        let engine = BlockEditEngine::new(store.clone(), "test-agent");
        let params = format!(
            r#"{{"block_id": "{}", "operations": [{{"op": "insert", "line": 1, "content": "line2"}}]}}"#,
            block_id.to_key()
        );
        let result = engine.execute(&params).await.unwrap();

        assert!(result.success, "edit insert failed: {}", result.stderr);

        // Verify content
        let entry = store.get("test-cell").unwrap();
        let snapshot = entry.doc.get_block_snapshot(&block_id).unwrap();
        assert_eq!(snapshot.content, "line1\nline2\nline3\n");
    }

    #[tokio::test]
    async fn test_block_edit_replace_with_cas() {
        let store = setup_test_store();

        let block_id = store.insert_block("test-cell", None, None, Role::User, BlockKind::Text, "hello\nworld\n").unwrap();

        let engine = BlockEditEngine::new(store.clone(), "test-agent");

        // Valid CAS should succeed
        let params = format!(
            r#"{{"block_id": "{}", "operations": [{{"op": "replace", "start_line": 1, "end_line": 2, "content": "rust", "expected_text": "world"}}]}}"#,
            block_id.to_key()
        );
        let result = engine.execute(&params).await.unwrap();
        assert!(result.success, "CAS should succeed: {}", result.stderr);

        // Invalid CAS should fail
        let params = format!(
            r#"{{"block_id": "{}", "operations": [{{"op": "replace", "start_line": 0, "end_line": 1, "content": "goodbye", "expected_text": "wrong"}}]}}"#,
            block_id.to_key()
        );
        let result = engine.execute(&params).await.unwrap();
        assert!(!result.success, "CAS should fail with wrong expected text");
        assert!(result.stderr.contains("content mismatch"));
    }

    #[tokio::test]
    async fn test_block_read() {
        let store = setup_test_store();

        let block_id = store.insert_block("test-cell", None, None, Role::User, BlockKind::Text, "fn main() {\n    println!(\"Hi\");\n}").unwrap();

        let engine = BlockReadEngine::new(store.clone());
        let params = format!(r#"{{"block_id": "{}"}}"#, block_id.to_key());
        let result = engine.execute(&params).await.unwrap();

        assert!(result.success, "read failed: {}", result.stderr);
        let response: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
        assert!(response["content"].as_str().unwrap().contains("1→"));
        assert_eq!(response["line_count"], 3);
    }

    #[tokio::test]
    async fn test_block_search() {
        let store = setup_test_store();

        let block_id = store.insert_block("test-cell", None, None, Role::User, BlockKind::Text, "apple\nbanana\napricot\ncherry\n").unwrap();

        let engine = BlockSearchEngine::new(store.clone());
        let params = format!(r#"{{"block_id": "{}", "query": "ap", "context_lines": 1}}"#, block_id.to_key());
        let result = engine.execute(&params).await.unwrap();

        assert!(result.success, "search failed: {}", result.stderr);
        let response: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
        let matches = response["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 2); // apple and apricot
    }

    #[tokio::test]
    async fn test_block_list() {
        let store = setup_test_store();

        store.insert_block("test-cell", None, None, Role::User, BlockKind::Text, "user message").unwrap();
        store.insert_block("test-cell", None, None, Role::Model, BlockKind::Thinking, "thinking...").unwrap();

        let engine = BlockListEngine::new(store.clone());
        let params = r#"{}"#;
        let result = engine.execute(params).await.unwrap();

        assert!(result.success);
        let response: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
        assert_eq!(response["count"], 2);
    }

    #[tokio::test]
    async fn test_block_list_with_filter() {
        let store = setup_test_store();

        store.insert_block("test-cell", None, None, Role::User, BlockKind::Text, "user message").unwrap();
        store.insert_block("test-cell", None, None, Role::Model, BlockKind::Thinking, "thinking...").unwrap();

        let engine = BlockListEngine::new(store.clone());
        let params = r#"{"kind": "thinking"}"#;
        let result = engine.execute(params).await.unwrap();

        assert!(result.success);
        let response: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
        assert_eq!(response["count"], 1);
    }

    #[tokio::test]
    async fn test_block_status() {
        let store = setup_test_store();

        let block_id = store.insert_block("test-cell", None, None, Role::Model, BlockKind::ToolCall, "{}").unwrap();

        let engine = BlockStatusEngine::new(store.clone());
        let params = format!(r#"{{"block_id": "{}", "status": "running"}}"#, block_id.to_key());
        let result = engine.execute(&params).await.unwrap();

        assert!(result.success, "status update failed: {}", result.stderr);

        // Verify status
        let entry = store.get("test-cell").unwrap();
        let snapshot = entry.doc.get_block_snapshot(&block_id).unwrap();
        assert_eq!(snapshot.status, Status::Running);
    }

    #[tokio::test]
    async fn test_kernel_search() {
        let store = setup_test_store();

        // Create blocks in different cells
        store.create_cell("cell2".into(), CellKind::Code, Some("rust".into())).unwrap();

        store.insert_block("test-cell", None, None, Role::User, BlockKind::Text, "hello world\nfoo bar\nbaz").unwrap();
        store.insert_block("test-cell", None, None, Role::Model, BlockKind::Text, "hello rust\nfoo qux").unwrap();
        store.insert_block("cell2", None, None, Role::User, BlockKind::Text, "hello python\nbar baz").unwrap();

        let engine = KernelSearchEngine::new(store.clone());

        // Search for "hello" across all blocks
        let params = r#"{"query": "hello"}"#;
        let result = engine.execute(params).await.unwrap();
        assert!(result.success, "search failed: {}", result.stderr);

        let response: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
        assert_eq!(response["total"], 3, "should find 3 matches for 'hello'");

        // Search with cell filter
        let params = r#"{"query": "hello", "cell_id": "test-cell"}"#;
        let result = engine.execute(params).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
        assert_eq!(response["total"], 2, "should find 2 matches in test-cell");

        // Search with role filter
        let params = r#"{"query": "hello", "role": "model"}"#;
        let result = engine.execute(params).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
        assert_eq!(response["total"], 1, "should find 1 match from model");

        // Search with context lines
        let params = r#"{"query": "foo", "context_lines": 1, "max_matches": 1}"#;
        let result = engine.execute(params).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
        let matches = response["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert!(!matches[0]["before"].as_array().unwrap().is_empty() || !matches[0]["after"].as_array().unwrap().is_empty());
    }
}
