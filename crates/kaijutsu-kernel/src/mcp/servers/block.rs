//! `BlockToolsServer` — virtual MCP server exposing block and content-creation
//! tools (D-30).

use std::sync::Arc;

use async_trait::async_trait;
use kaijutsu_cas::FileStore;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::block_store::SharedBlockStore;
// The `*_char_*` twins, NOT the byte variants: `apply_op` feeds
// `edit_text_as`, and the CRDT text layer is char-indexed (byte offsets
// corrupt multibyte content — the June file-tools bug class).
use crate::block_tools::translate::{
    content_with_line_numbers, extract_lines_with_numbers, line_count, line_range_to_char_range,
    line_to_char_offset, validate_expected_text,
};
use kaijutsu_crdt::{BlockId, BlockKind, ContentType, Role, Status};
use kaijutsu_types::ContextId;
use kaijutsu_cas::ContentStore;
use crate::execution::{ExecContext, ExecResult};

use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{InstanceId, KernelCallParams, KernelTool, KernelToolResult};
use super::adapter::{from_exec_result, to_exec_context};

// ── Typed Params (schemars-derived) ────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BlockCreateParams {
    /// Parent block ID for DAG relationship (omit for root).
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BlockAppendParams {
    /// Block ID to append to.
    pub block_id: String,
    /// Content to append.
    pub content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BlockEditParams {
    /// Block ID to edit.
    pub block_id: String,
    /// List of edit operations to apply atomically.
    pub operations: Vec<EditOp>,
}

/// Edit operation on a block.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EditOp {
    /// Insert text before a line.
    Insert { line: u32, content: String },
    /// Delete lines from start_line to end_line (exclusive).
    Delete { start_line: u32, end_line: u32 },
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BlockSpliceParams {
    /// Block ID to edit.
    pub block_id: String,
    /// CHARACTER offset (not bytes — the CRDT text layer is char-indexed;
    /// the tool description has always said "character-based" but this
    /// schema doc used to say "byte", steering callers into computing byte
    /// offsets that corrupt multibyte content).
    pub offset: usize,
    /// Number of CHARACTERS to delete.
    pub delete_count: usize,
    /// Text to insert.
    pub insert: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BlockReadParams {
    /// Block ID to read.
    pub block_id: String,
    /// Include line numbers.
    #[serde(default = "default_true")]
    pub line_numbers: bool,
    /// [start_line, end_line] (0-indexed, exclusive end).
    #[serde(default)]
    pub range: Option<(u32, u32)>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BlockSearchParams {
    /// Block ID to search.
    pub block_id: String,
    /// Regex or literal pattern.
    pub query: String,
    /// Lines of context before/after match.
    #[serde(default = "default_context_lines")]
    pub context_lines: u32,
    /// Maximum matches to return.
    #[serde(default = "default_max_matches")]
    pub max_matches: u32,
}

fn default_context_lines() -> u32 {
    2
}

fn default_max_matches() -> u32 {
    20
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BlockListParams {
    /// Filter by parent block ID.
    pub parent_id: Option<String>,
    /// Filter by block kind.
    pub kind: Option<String>,
    /// Filter by status.
    pub status: Option<String>,
    /// Filter file blocks by path prefix.
    pub path_prefix: Option<String>,
    /// DAG traversal depth.
    #[serde(default = "default_depth")]
    pub depth: u32,
}

fn default_depth() -> u32 {
    1
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BlockStatusParams {
    /// Block ID to update.
    pub block_id: String,
    /// New status.
    pub status: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct KernelSearchParams {
    /// Regex pattern to search for.
    pub query: String,
    /// Optional document ID to limit search to.
    pub document_id: Option<String>,
    /// Optional block kind filter (text, thinking, tool_call, tool_result).
    pub kind: Option<String>,
    /// Optional role filter (user, model, system, tool).
    pub role: Option<String>,
    /// Number of context lines around matches.
    #[serde(default)]
    pub context_lines: u32,
    /// Maximum number of matches to return.
    pub max_matches: Option<usize>,
    /// Search all documents instead of just the current context.
    #[serde(default)]
    pub all_documents: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SvgBlockParams {
    /// SVG content (`<svg>...</svg>`).
    pub content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AbcBlockParams {
    /// ABC music notation text.
    pub content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImgBlockParams {
    /// Hex-encoded CAS hash of an image already stored in the CAS.
    pub hash: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImgBlockFromPathParams {
    /// Filesystem path to an image file.
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct SearchMatch {
    pub line: u32,
    pub content: String,
    pub match_start: u32,
    pub match_end: u32,
}

#[derive(Debug, Serialize)]
pub struct KernelSearchMatch {
    pub document_id: String,
    pub block_id: String,
    pub line: u32,
    pub content: String,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

// ── Server ─────────────────────────────────────────────────────────────────

pub struct BlockToolsServer {
    instance_id: InstanceId,
    documents: SharedBlockStore,
    cas: Arc<FileStore>,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl BlockToolsServer {
    pub const INSTANCE: &'static str = "builtin.block";

    pub fn new(documents: SharedBlockStore, cas: Arc<FileStore>) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE),
            documents,
            cas,
            notif_tx,
        }
    }
}

fn tool_def<P: JsonSchema>(
    instance: &InstanceId,
    name: &str,
    description: &str,
) -> McpResult<KernelTool> {
    let schema = schemars::schema_for!(P);
    Ok(KernelTool {
        instance: instance.clone(),
        name: name.to_string(),
        description: Some(description.to_string()),
        input_schema: serde_json::to_value(schema).map_err(McpError::InvalidParams)?,
    })
}

#[async_trait]
impl McpServerLike for BlockToolsServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        Ok(vec![
            tool_def::<BlockCreateParams>(&self.instance_id, "block_create", "Create a new block with role, kind, and optional content")?,
            tool_def::<BlockAppendParams>(&self.instance_id, "block_append", "Append text to a block")?,
            tool_def::<BlockEditParams>(&self.instance_id, "block_edit", "Edit block content atomically with line operations")?,
            tool_def::<BlockSpliceParams>(&self.instance_id, "block_splice", "Character-based editing (for programmatic tools)")?,
            tool_def::<BlockReadParams>(&self.instance_id, "block_read", "Read block content with optional line numbers and range")?,
            tool_def::<BlockSearchParams>(&self.instance_id, "block_search", "Search within a block using regex or literal patterns")?,
            tool_def::<BlockListParams>(&self.instance_id, "block_list", "List blocks with optional filters")?,
            tool_def::<BlockStatusParams>(&self.instance_id, "block_status", "Set block status (pending, running, done, error, cancelled)")?,
            tool_def::<KernelSearchParams>(&self.instance_id, "kernel_search", "Search across all blocks using regex, with filters and context")?,
            tool_def::<SvgBlockParams>(&self.instance_id, "svg_block", "Append an SVG block to the current context. Renders as vector graphics inline.")?,
            tool_def::<AbcBlockParams>(&self.instance_id, "abc_block", "Append an ABC music notation block. Validates parse; renders as sheet music inline.")?,
            tool_def::<ImgBlockParams>(&self.instance_id, "img_block", "Append an image block referencing content already in the CAS by hash.")?,
            tool_def::<ImgBlockFromPathParams>(&self.instance_id, "img_block_from_path", "Read an image file, store it in the CAS, and append an image block.")?,
        ])
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        let tool_ctx = to_exec_context(ctx);

        let exec = match params.tool.as_str() {
            "block_create" => {
                let p: BlockCreateParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let role = self.parse_role(&p.role)?;
                let kind = self.parse_kind(&p.kind)?;
                let content = p.content.unwrap_or_default();
                let parent_id = p.parent_id.as_ref().map(|s| self.parse_block_id(s)).transpose()?;
                let context_id = tool_ctx.context_id;

                if !self.documents.contains(context_id) {
                    return Err(McpError::Protocol(format!("no document for context {}", context_id.short())));
                }

                let block_id = self.documents
                    .insert_block_as(
                        context_id,
                        parent_id.as_ref(),
                        None,
                        role,
                        kind,
                        &content,
                        Status::Done,
                        ContentType::Plain,
                        Some(tool_ctx.principal_id),
                    )
                    .map_err(|e| McpError::Protocol(e.to_string()))?;

                let version = self.documents.get(context_id).map(|c| c.version()).unwrap_or(0);
                let res_json = serde_json::json!({
                    "block_id": block_id.to_key(),
                    "version": version
                });
                ExecResult::success(res_json.to_string())
            }
            "block_append" => {
                let p: BlockAppendParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let block_id = self.parse_block_id(&p.block_id)?;
                let context_id = tool_ctx.context_id;

                if !self.documents.contains(context_id) {
                    return Err(McpError::Protocol(format!("no document for context {}", context_id.short())));
                }

                let char_offset = {
                    let entry = self.documents
                        .get(context_id)
                        .ok_or_else(|| McpError::Protocol(format!("document not found for context {}", context_id.short())))?;

                    let snapshot = entry
                        .doc
                        .get_block_snapshot(&block_id)
                        .ok_or_else(|| McpError::Protocol(format!("block not found: {}", p.block_id)))?;

                    snapshot.content.chars().count()
                };

                self.documents
                    .edit_text_as(context_id, &block_id, char_offset, &p.content, 0, Some(tool_ctx.principal_id))
                    .map_err(|e| McpError::Protocol(e.to_string()))?;

                let version = self.documents.get(context_id).map(|c| c.version()).unwrap_or(0);
                let res_json = serde_json::json!({
                    "block_id": p.block_id,
                    "version": version
                });
                ExecResult::success(res_json.to_string())
            }
            "block_edit" => {
                let p: BlockEditParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let (context_id, block_id) = self.find_block(&p.block_id)?;

                // Pre-validate CAS checks
                {
                    let content = self.documents
                        .get(context_id)
                        .and_then(|entry| {
                            entry
                                .doc
                                .get_block_snapshot(&block_id)
                                .map(|s| s.content.clone())
                        })
                        .unwrap_or_default();

                    for (idx, op) in p.operations.iter().enumerate() {
                        if let EditOp::Replace {
                            start_line,
                            end_line,
                            expected_text: Some(expected),
                            ..
                        } = op
                        {
                            validate_expected_text(&content, *start_line, *end_line, expected)
                                .map_err(|e| McpError::Protocol(format!("CAS error at op {}: {}", idx, e)))?;
                        }
                    }
                }

                for (idx, op) in p.operations.into_iter().enumerate() {
                    self.apply_op(context_id, &block_id, op, &tool_ctx)
                        .map_err(|e| McpError::Protocol(format!("edit error at op {}: {}", idx, e)))?;
                }

                let version = self.documents.get(context_id).map(|c| c.version()).unwrap_or(0);
                let res_json = serde_json::json!({
                    "version": version
                });
                ExecResult::success(res_json.to_string())
            }
            "block_splice" => {
                let p: BlockSpliceParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let (context_id, block_id) = self.find_block(&p.block_id)?;
                let insert = p.insert.unwrap_or_default();

                self.documents
                    .edit_text_as(
                        context_id,
                        &block_id,
                        p.offset,
                        &insert,
                        p.delete_count,
                        Some(tool_ctx.principal_id),
                    )
                    .map_err(|e| McpError::Protocol(e.to_string()))?;

                let version = self.documents.get(context_id).map(|c| c.version()).unwrap_or(0);
                let res_json = serde_json::json!({
                    "version": version
                });
                ExecResult::success(res_json.to_string())
            }
            "block_read" => {
                let p: BlockReadParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let (context_id, block_id) = self.find_block(&p.block_id)?;

                let entry = self.documents
                    .get(context_id)
                    .ok_or_else(|| McpError::Protocol("document not found".into()))?;

                let snapshot = entry
                    .doc
                    .get_block_snapshot(&block_id)
                    .ok_or_else(|| McpError::Protocol(format!("block not found: {}", p.block_id)))?;

                let content = &snapshot.content;
                let total_lines = line_count(content);

                let formatted_content = if let Some((start, end)) = p.range {
                    if p.line_numbers {
                        extract_lines_with_numbers(content, start, end)
                    } else {
                        content
                            .lines()
                            .skip(start as usize)
                            .take((end.saturating_sub(start)) as usize)
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                } else if p.line_numbers {
                    content_with_line_numbers(content)
                } else {
                    content.clone()
                };

                let res_json = serde_json::json!({
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
                });
                ExecResult::success(res_json.to_string())
            }
            "block_search" => {
                let p: BlockSearchParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let (context_id, block_id) = self.find_block(&p.block_id)?;

                let entry = self.documents
                    .get(context_id)
                    .ok_or_else(|| McpError::Protocol("document not found".into()))?;

                let snapshot = entry
                    .doc
                    .get_block_snapshot(&block_id)
                    .ok_or_else(|| McpError::Protocol(format!("block not found: {}", p.block_id)))?;

                let content = &snapshot.content;
                let lines: Vec<&str> = content.lines().collect();

                let regex = regex::Regex::new(&p.query)
                    .or_else(|_| regex::Regex::new(&regex::escape(&p.query)))
                    .map_err(|e| McpError::Protocol(format!("Invalid regex: {}", e)))?;

                let mut search_matches = Vec::new();
                let total_lines = lines.len() as u32;

                for (line_num, line) in lines.iter().enumerate() {
                    if search_matches.len() >= p.max_matches as usize {
                        break;
                    }

                    for cap in regex.find_iter(line) {
                        if search_matches.len() >= p.max_matches as usize {
                            break;
                        }

                        let ctx_start = (line_num as u32).saturating_sub(p.context_lines);
                        let ctx_end = ((line_num as u32) + p.context_lines + 1).min(total_lines);

                        let context_content = extract_lines_with_numbers(content, ctx_start, ctx_end);

                        search_matches.push(SearchMatch {
                            line: line_num as u32,
                            content: context_content,
                            match_start: cap.start() as u32,
                            match_end: cap.end() as u32,
                        });
                    }
                }

                if search_matches.is_empty() {
                    return Err(McpError::Protocol("No matches found".to_string()));
                }

                let res_json = serde_json::json!({
                    "matches": search_matches,
                    "total_matches": search_matches.len()
                });
                ExecResult::success(res_json.to_string())
            }
            "block_list" => {
                let p: BlockListParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let kind_filter = p.kind.as_ref().and_then(|k| self.parse_kind(k).ok());
                let status_filter = p.status.as_ref().and_then(|s| self.parse_status(s).ok());
                let parent_id_filter = p
                    .parent_id
                    .as_ref()
                    .and_then(|s| self.parse_block_id(s).ok());

                let mut blocks = Vec::new();
                let context_ids = self.documents.list_ids();
                for context_id in context_ids {
                    if let Some(entry) = self.documents.get(context_id) {
                        for snapshot in entry.doc.blocks_ordered() {
                            if let Some(ref parent_id) = parent_id_filter
                                && snapshot.parent_id.as_ref() != Some(parent_id)
                            {
                                continue;
                            }
                            if let Some(kind) = kind_filter
                                && snapshot.kind != kind
                            {
                                continue;
                            }
                            if let Some(status) = status_filter
                                && snapshot.status != status
                            {
                                continue;
                            }

                            let summary = if snapshot.content.chars().count() > 100 {
                                let truncated: String = snapshot.content.chars().take(100).collect();
                                format!("{}... ({} lines)", truncated, line_count(&snapshot.content))
                            } else {
                                snapshot.content.clone()
                            };

                            blocks.push(serde_json::json!({
                                "block_id": snapshot.id.to_key(),
                                "parent_id": snapshot.parent_id.as_ref().map(|id| id.to_key()),
                                "role": format!("{:?}", snapshot.role).to_lowercase(),
                                "kind": format!("{:?}", snapshot.kind).to_lowercase(),
                                "status": format!("{:?}", snapshot.status).to_lowercase(),
                                "summary": summary,
                                "version": entry.version(),
                            }));
                        }
                    }
                }

                let res_json = serde_json::json!({
                    "blocks": blocks,
                    "count": blocks.len()
                });
                ExecResult::success(res_json.to_string())
            }
            "block_status" => {
                let p: BlockStatusParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let (context_id, block_id) = self.find_block(&p.block_id)?;
                let status = self.parse_status(&p.status)?;

                self.documents
                    .set_status(context_id, &block_id, status)
                    .map_err(|e| McpError::Protocol(e.to_string()))?;

                let version = self.documents.get(context_id).map(|c| c.version()).unwrap_or(0);
                let res_json = serde_json::json!({
                    "version": version
                });
                ExecResult::success(res_json.to_string())
            }
            "kernel_search" => {
                let p: KernelSearchParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let regex = regex::Regex::new(&p.query)
                    .map_err(|e| McpError::Protocol(format!("Invalid regex: {}", e)))?;

                let kind_filter = p.kind.as_ref().map(|k| self.parse_kind(k)).transpose()?;
                let role_filter = p.role.as_ref().map(|r| self.parse_role(r)).transpose()?;

                let max_matches = p.max_matches.unwrap_or(100);
                let mut search_matches = Vec::new();

                let context_ids: Vec<ContextId> = if let Some(ref doc_id_str) = p.document_id {
                    match ContextId::parse(doc_id_str) {
                        Ok(ctx) if self.documents.contains(ctx) => vec![ctx],
                        _ => vec![],
                    }
                } else if p.all_documents {
                    self.documents.list_ids()
                } else {
                    vec![tool_ctx.context_id]
                };

                'outer: for context_id in context_ids {
                    let snapshots = match self.documents.block_snapshots(context_id) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };

                    for snapshot in snapshots {
                        if let Some(ref kind) = kind_filter
                            && snapshot.kind != *kind
                        {
                            continue;
                        }
                        if let Some(ref role) = role_filter
                            && snapshot.role != *role
                        {
                            continue;
                        }

                        let lines: Vec<&str> = snapshot.content.lines().collect();
                        for (line_idx, line) in lines.iter().enumerate() {
                            if regex.is_match(line) {
                                let before: Vec<String> = (0..p.context_lines as usize)
                                    .filter_map(|i| {
                                        if line_idx > i {
                                            Some(lines[line_idx - i - 1].to_string())
                                        } else {
                                            None
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .into_iter()
                                    .rev()
                                    .collect();

                                let after: Vec<String> = (1..=p.context_lines as usize)
                                    .filter_map(|i| lines.get(line_idx + i).map(|s| s.to_string()))
                                    .collect();

                                search_matches.push(KernelSearchMatch {
                                    document_id: context_id.to_hex(),
                                    block_id: snapshot.id.to_key(),
                                    line: line_idx as u32,
                                    content: line.to_string(),
                                    before,
                                    after,
                                });

                                if search_matches.len() >= max_matches {
                                    break 'outer;
                                }
                            }
                        }
                    }
                }

                let res_json = serde_json::json!({
                    "matches": search_matches,
                    "total": search_matches.len(),
                    "truncated": search_matches.len() >= max_matches
                });
                ExecResult::success(res_json.to_string())
            }
            "svg_block" => {
                let p: SvgBlockParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let key = self.append_block(&tool_ctx, Role::Tool, &p.content, ContentType::Svg)?;
                let res_json = serde_json::json!({ "block_id": key });
                ExecResult::success(res_json.to_string())
            }
            "abc_block" => {
                let p: AbcBlockParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;

                let parse = kaijutsu_abc::parse_with_mode(&p.content, kaijutsu_abc::ParseMode::Strict);
                if parse.has_errors() {
                    let errs: Vec<_> = parse.errors().map(|e| e.message.clone()).collect();
                    return Ok(from_exec_result(ExecResult::failure(
                        1,
                        format!("ABC parse error: {}", errs.join("; ")),
                    )));
                }

                let key = self.append_block(&tool_ctx, Role::Tool, &p.content, ContentType::Abc)?;
                let res_json = serde_json::json!({ "block_id": key });
                ExecResult::success(res_json.to_string())
            }
            "img_block" => {
                let p: ImgBlockParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;

                if p.hash.parse::<kaijutsu_cas::ContentHash>().is_err() {
                    return Ok(from_exec_result(ExecResult::failure(1, format!("invalid hash: {}", p.hash))));
                }

                let key = self.append_block(&tool_ctx, Role::Asset, &p.hash, ContentType::Image)?;
                let res_json = serde_json::json!({ "block_id": key });
                ExecResult::success(res_json.to_string())
            }
            "img_block_from_path" => {
                let p: ImgBlockFromPathParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;

                let data = match std::fs::read(&p.path) {
                    Ok(d) => d,
                    Err(e) => {
                        return Ok(from_exec_result(ExecResult::failure(
                            1,
                            format!("read error {}: {}", p.path, e),
                        )));
                    }
                };

                let mime = crate::kj::cas::mime_from_extension(&p.path);
                let hash = self.cas.store(&data, mime).map_err(|e| McpError::Protocol(format!("CAS error: {e}")))?;
                let hash_str = hash.to_string();

                let key = self.append_block(&tool_ctx, Role::Asset, &hash_str, ContentType::Image)?;
                let res_json = serde_json::json!({ "block_id": key });
                ExecResult::success(res_json.to_string())
            }
            other => {
                return Err(McpError::ToolNotFound {
                    instance: self.instance_id.clone(),
                    tool: other.to_string(),
                });
            }
        };

        Ok(from_exec_result(exec))
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}

impl BlockToolsServer {
    fn parse_block_id(&self, s: &str) -> McpResult<BlockId> {
        BlockId::from_key(s)
            .ok_or_else(|| McpError::Protocol(format!("invalid block_id format: {}", s)))
    }

    fn parse_role(&self, s: &str) -> McpResult<Role> {
        match s.to_lowercase().as_str() {
            "user" | "human" => Ok(Role::User),
            "model" | "assistant" | "agent" => Ok(Role::Model),
            "system" => Ok(Role::System),
            "tool" => Ok(Role::Tool),
            _ => Err(McpError::Protocol(format!("invalid role: {}", s))),
        }
    }

    fn parse_kind(&self, s: &str) -> McpResult<BlockKind> {
        match s.to_lowercase().as_str() {
            "text" => Ok(BlockKind::Text),
            "thinking" => Ok(BlockKind::Thinking),
            "tool_call" | "toolcall" => Ok(BlockKind::ToolCall),
            "tool_result" | "toolresult" => Ok(BlockKind::ToolResult),
            _ => Err(McpError::Protocol(format!("invalid kind: {}", s))),
        }
    }

    fn parse_status(&self, s: &str) -> McpResult<Status> {
        match s.to_lowercase().as_str() {
            "pending" => Ok(Status::Pending),
            "active" | "running" => Ok(Status::Running),
            "done" | "complete" | "completed" => Ok(Status::Done),
            "error" => Ok(Status::Error),
            _ => Err(McpError::Protocol(format!("invalid status: {}", s))),
        }
    }

    fn find_block(&self, block_id_str: &str) -> McpResult<(ContextId, BlockId)> {
        let block_id = self.parse_block_id(block_id_str)?;
        let context_id = block_id.context_id;

        if let Some(entry) = self.documents.get(context_id)
            && entry.doc.get_block_snapshot(&block_id).is_some()
        {
            return Ok((context_id, block_id));
        }
        Err(McpError::Protocol(format!("block not found: {}", block_id_str)))
    }

    fn append_block(
        &self,
        ctx: &ExecContext,
        role: Role,
        content: &str,
        content_type: ContentType,
    ) -> McpResult<String> {
        let context_id = ctx.context_id;
        if !self.documents.contains(context_id) {
            return Err(McpError::Protocol(format!("no document for context {}", context_id.short())));
        }

        let last_block_id = {
            let entry = self.documents.get(context_id);
            entry.and_then(|doc| doc.doc.blocks_ordered().last().map(|b| b.id))
        };

        self.documents
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
            .map_err(|e| McpError::Protocol(e.to_string()))
    }

    fn apply_op(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        op: EditOp,
        ctx: &ExecContext,
    ) -> McpResult<()> {
        let content = {
            let entry = self
                .documents
                .get(context_id)
                .ok_or_else(|| McpError::Protocol("document not found".into()))?;

            entry
                .doc
                .get_block_snapshot(block_id)
                .map(|s| s.content.clone())
                .ok_or_else(|| McpError::Protocol(format!("block not found: {}", block_id)))?
        };

        match op {
            EditOp::Insert {
                line,
                content: text,
            } => {
                let pos = line_to_char_offset(&content, line)
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                let text_with_newline = if text.ends_with('\n') || content.is_empty() {
                    text
                } else {
                    format!("{}\n", text)
                };
                self.documents
                    .edit_text_as(
                        context_id,
                        block_id,
                        pos,
                        &text_with_newline,
                        0,
                        Some(ctx.principal_id),
                    )
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
            }
            EditOp::Delete {
                start_line,
                end_line,
            } => {
                let (start, end) = line_range_to_char_range(&content, start_line, end_line)
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                if start < end {
                    self.documents
                        .edit_text_as(
                            context_id,
                            block_id,
                            start,
                            "",
                            end - start,
                            Some(ctx.principal_id),
                        )
                        .map_err(|e| McpError::Protocol(e.to_string()))?;
                }
            }
            EditOp::Replace {
                start_line,
                end_line,
                content: text,
                expected_text,
            } => {
                if let Some(expected) = expected_text {
                    validate_expected_text(&content, start_line, end_line, &expected).map_err(|e| McpError::Protocol(e.to_string()))?;
                }

                let (start, end) = line_range_to_char_range(&content, start_line, end_line)
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                let text_with_newline = if text.ends_with('\n') || text.is_empty() {
                    text
                } else {
                    format!("{}\n", text)
                };
                self.documents
                    .edit_text_as(
                        context_id,
                        block_id,
                        start,
                        &text_with_newline,
                        end - start,
                        Some(ctx.principal_id),
                    )
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::{shared_block_store_with_db, DocumentKind};
    use crate::kernel_db::{DocumentRow, KernelDb};
    use crate::mcp::{Broker, InstancePolicy, ToolContent};
    use kaijutsu_cas::FileStore;
    use kaijutsu_types::{now_millis, PrincipalId};

    async fn setup() -> (Arc<Broker>, CallContext, Arc<parking_lot::Mutex<KernelDb>>, SharedBlockStore) {
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let creator = PrincipalId::new();
        let ws_id = {
            let g = db.lock();
            g.get_or_create_default_workspace(creator).unwrap()
        };
        let store = shared_block_store_with_db(db.clone(), ws_id, creator);

        let mut ctx = CallContext::test();
        ctx.principal_id = creator;
        {
            let g = db.lock();
            g.insert_document(&DocumentRow {
                document_id: ctx.context_id,
                workspace_id: ws_id,
                doc_kind: DocumentKind::Code,
                language: None,
                path: None,
                created_at: now_millis() as i64,
                created_by: creator,
            })
            .unwrap();
        }
        store
            .create_document(ctx.context_id, DocumentKind::Code, None)
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let cas = Arc::new(FileStore::at_path(tmp.path().join("cas")));
        let server = Arc::new(BlockToolsServer::new(store.clone(), cas));
        let broker = Arc::new(Broker::new());
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();
        std::mem::forget(tmp);
        (broker, ctx, db, store)
    }

    async fn call(broker: &Broker, ctx: &CallContext, tool: &str, args: serde_json::Value) -> KernelToolResult {
        broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(BlockToolsServer::INSTANCE),
                    tool: tool.to_string(),
                    arguments: args,
                },
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap()
    }

    async fn call_res(broker: &Broker, ctx: &CallContext, tool: &str, args: serde_json::Value) -> McpResult<KernelToolResult> {
        broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(BlockToolsServer::INSTANCE),
                    tool: tool.to_string(),
                    arguments: args,
                },
                ctx,
                CancellationToken::new(),
            )
            .await
    }

    fn text_of(r: &KernelToolResult) -> String {
        match r.content.first() {
            Some(ToolContent::Text(s)) => s.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_create_via_broker() {
        let (broker, ctx, _db, _store) = setup().await;
        let result = call(
            &broker,
            &ctx,
            "block_create",
            serde_json::json!({
                "role": "user",
                "kind": "text",
                "content": "hello from mcp broker",
            }),
        )
        .await;

        assert!(!result.is_error, "unexpected error: {:?}", result.content);
        assert!(matches!(result.content.first(), Some(ToolContent::Text(_))));
    }

    #[tokio::test]
    async fn list_tools_exposes_all_thirteen() {
        let (broker, ctx, _db, _store) = setup().await;
        let visible = {
            let mut binding = crate::mcp::ContextToolBinding::new();
            binding.allow(InstanceId::new(BlockToolsServer::INSTANCE));
            broker.set_binding(ctx.context_id, binding).await;
            broker.list_visible_tools(ctx.context_id, &ctx).await.unwrap()
        };
        let names: Vec<_> = visible.iter().map(|(n, _)| n.as_str()).collect();
        for expected in [
            "block_create",
            "block_append",
            "block_edit",
            "block_splice",
            "block_read",
            "block_search",
            "block_list",
            "block_status",
            "kernel_search",
            "svg_block",
            "abc_block",
            "img_block",
            "img_block_from_path",
        ] {
            assert!(names.contains(&expected), "missing {}", expected);
        }
    }

    #[tokio::test]
    async fn test_block_append_persists_to_db() {
        let (broker, ctx, db, store) = setup().await;

        // Create a block (this journals ops via insert_block_as)
        let create_res = call(
            &broker,
            &ctx,
            "block_create",
            serde_json::json!({
                "role": "user",
                "kind": "text",
                "content": "hello",
            }),
        )
        .await;
        assert!(!create_res.is_error);

        // Verify creation was persisted (journal_op writes to oplog)
        {
            let db_guard = db.lock();
            let entries = db_guard.load_oplog_since(ctx.context_id, 0).unwrap();
            assert!(
                !entries.is_empty(),
                "block_create should journal ops to the oplog"
            );
        }

        let response: serde_json::Value = serde_json::from_str(&text_of(&create_res)).unwrap();
        let block_key = response["block_id"].as_str().unwrap();

        let append_res = call(
            &broker,
            &ctx,
            "block_append",
            serde_json::json!({
                "block_id": block_key,
                "content": " world",
            }),
        )
        .await;
        assert!(!append_res.is_error, "append failed: {}", text_of(&append_res));

        // In-memory store has the append
        assert_eq!(store.get_content(ctx.context_id).unwrap(), "hello world");

        // append should journal each op, so the oplog should have more entries.
        let db_guard = db.lock();
        let entries = db_guard.load_oplog_since(ctx.context_id, 0).unwrap();
        assert!(
            entries.len() >= 2,
            "block_append should journal ops to the oplog, got {} entries",
            entries.len(),
        );
    }

    #[tokio::test]
    async fn test_block_create() {
        let (broker, ctx, _db, _store) = setup().await;
        let res = call(
            &broker,
            &ctx,
            "block_create",
            serde_json::json!({
                "role": "user",
                "kind": "text",
                "content": "hello world",
            }),
        )
        .await;
        assert!(!res.is_error);
        let response: serde_json::Value = serde_json::from_str(&text_of(&res)).unwrap();
        assert!(response["block_id"].is_string());
        assert!(response["version"].is_u64());
    }

    #[tokio::test]
    async fn test_block_append() {
        let (broker, ctx, _db, store) = setup().await;

        // Create a block first
        let block_id = store
            .insert_block(
                ctx.context_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "hello",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let res = call(
            &broker,
            &ctx,
            "block_append",
            serde_json::json!({
                "block_id": block_id.to_key(),
                "content": " world",
            }),
        )
        .await;
        assert!(!res.is_error, "append failed: {}", text_of(&res));

        // Verify content
        let content = store.get_content(ctx.context_id).unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn test_block_edit_insert() {
        let (broker, ctx, _db, store) = setup().await;

        let block_id = store
            .insert_block(
                ctx.context_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "line1\nline3\n",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let res = call(
            &broker,
            &ctx,
            "block_edit",
            serde_json::json!({
                "block_id": block_id.to_key(),
                "operations": [{"op": "insert", "line": 1, "content": "line2"}],
            }),
        )
        .await;
        assert!(!res.is_error, "edit insert failed: {}", text_of(&res));

        // Verify content
        let entry = store.get(ctx.context_id).unwrap();
        let snapshot = entry.doc.get_block_snapshot(&block_id).unwrap();
        assert_eq!(snapshot.content, "line1\nline2\nline3\n");
    }

    #[tokio::test]
    async fn test_block_edit_replace_with_cas() {
        let (broker, ctx, _db, store) = setup().await;

        let block_id = store
            .insert_block(
                ctx.context_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "hello\nworld\n",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Valid CAS should succeed
        let res1 = call(
            &broker,
            &ctx,
            "block_edit",
            serde_json::json!({
                "block_id": block_id.to_key(),
                "operations": [{"op": "replace", "start_line": 1, "end_line": 2, "content": "rust", "expected_text": "world"}],
            }),
        )
        .await;
        assert!(!res1.is_error, "CAS should succeed: {}", text_of(&res1));

        // Invalid CAS should fail
        let res2 = call_res(
            &broker,
            &ctx,
            "block_edit",
            serde_json::json!({
                "block_id": block_id.to_key(),
                "operations": [{"op": "replace", "start_line": 0, "end_line": 1, "content": "goodbye", "expected_text": "wrong"}],
            }),
        )
        .await;
        let err = res2.unwrap_err();
        assert!(err.to_string().contains("content mismatch"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_block_read() {
        let (broker, ctx, _db, store) = setup().await;

        let block_id = store
            .insert_block(
                ctx.context_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "fn main() {\n    println!(\"Hi\");\n}",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let res = call(
            &broker,
            &ctx,
            "block_read",
            serde_json::json!({
                "block_id": block_id.to_key(),
            }),
        )
        .await;
        assert!(!res.is_error, "read failed: {}", text_of(&res));
        let response: serde_json::Value = serde_json::from_str(&text_of(&res)).unwrap();
        assert!(response["content"].as_str().unwrap().contains("1→"));
        assert_eq!(response["line_count"], 3);
    }

    #[tokio::test]
    async fn test_block_search() {
        let (broker, ctx, _db, store) = setup().await;

        let block_id = store
            .insert_block(
                ctx.context_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "apple\nbanana\napricot\ncherry\n",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let res = call(
            &broker,
            &ctx,
            "block_search",
            serde_json::json!({
                "block_id": block_id.to_key(),
                "query": "ap",
                "context_lines": 1,
            }),
        )
        .await;
        assert!(!res.is_error, "search failed: {}", text_of(&res));
        let response: serde_json::Value = serde_json::from_str(&text_of(&res)).unwrap();
        let matches = response["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 2); // apple and apricot
    }

    #[tokio::test]
    async fn test_block_list() {
        let (broker, ctx, _db, store) = setup().await;

        store
            .insert_block(
                ctx.context_id,
                None,
                None,
                Role::Model,
                BlockKind::Thinking,
                "thinking...",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let res = call(
            &broker,
            &ctx,
            "block_list",
            serde_json::json!({
                "kind": "thinking",
            }),
        )
        .await;
        assert!(!res.is_error);
        let response: serde_json::Value = serde_json::from_str(&text_of(&res)).unwrap();
        assert_eq!(response["count"], 1);
    }

    #[tokio::test]
    async fn test_block_status() {
        let (broker, ctx, _db, store) = setup().await;

        let block_id = store
            .insert_block(
                ctx.context_id,
                None,
                None,
                Role::Model,
                BlockKind::ToolCall,
                "{}",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let res = call(
            &broker,
            &ctx,
            "block_status",
            serde_json::json!({
                "block_id": block_id.to_key(),
                "status": "running",
            }),
        )
        .await;
        assert!(!res.is_error, "status update failed: {}", text_of(&res));

        // Verify status
        let entry = store.get(ctx.context_id).unwrap();
        let snapshot = entry.doc.get_block_snapshot(&block_id).unwrap();
        assert_eq!(snapshot.status, Status::Running);
    }

    #[tokio::test]
    async fn test_kernel_search() {
        let (broker, ctx, _db, store) = setup().await;

        // Create blocks in different documents
        let ctx2 = ContextId::new();
        store
            .create_document(ctx2, DocumentKind::Code, Some("rust".into()))
            .unwrap();

        store
            .insert_block(
                ctx.context_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "hello world\nfoo bar\nbaz",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        store
            .insert_block(
                ctx.context_id,
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "hello rust\nfoo qux",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        store
            .insert_block(
                ctx2,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "hello python\nbar baz",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Default: search current context only
        let res1 = call(
            &broker,
            &ctx,
            "kernel_search",
            serde_json::json!({
                "query": "hello",
            }),
        )
        .await;
        assert!(!res1.is_error, "search failed: {}", text_of(&res1));
        let response: serde_json::Value = serde_json::from_str(&text_of(&res1)).unwrap();
        assert_eq!(
            response["total"], 2,
            "should find 2 matches in current context"
        );

        // Search across all documents with all_documents flag
        let res2 = call(
            &broker,
            &ctx,
            "kernel_search",
            serde_json::json!({
                "query": "hello",
                "all_documents": true,
            }),
        )
        .await;
        assert!(!res2.is_error);
        let response: serde_json::Value = serde_json::from_str(&text_of(&res2)).unwrap();
        assert_eq!(
            response["total"], 3,
            "should find 3 matches across all docs"
        );

        // Search with document filter (using hex ContextId)
        let res3 = call(
            &broker,
            &ctx,
            "kernel_search",
            serde_json::json!({
                "query": "hello",
                "document_id": ctx.context_id.to_hex(),
            }),
        )
        .await;
        assert!(!res3.is_error);
        let response: serde_json::Value = serde_json::from_str(&text_of(&res3)).unwrap();
        assert_eq!(response["total"], 2, "should find 2 matches in ctx");

        // Search with role filter (current context only)
        let res4 = call(
            &broker,
            &ctx,
            "kernel_search",
            serde_json::json!({
                "query": "hello",
                "role": "model",
            }),
        )
        .await;
        assert!(!res4.is_error);
        let response: serde_json::Value = serde_json::from_str(&text_of(&res4)).unwrap();
        assert_eq!(response["total"], 1, "should find 1 match from model");

        // Search with context lines
        let res5 = call(
            &broker,
            &ctx,
            "kernel_search",
            serde_json::json!({
                "query": "foo",
                "context_lines": 1,
                "max_matches": 1,
            }),
        )
        .await;
        assert!(!res5.is_error);
        let response: serde_json::Value = serde_json::from_str(&text_of(&res5)).unwrap();
        let matches = response["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert!(
            !matches[0]["before"].as_array().unwrap().is_empty()
                || !matches[0]["after"].as_array().unwrap().is_empty()
        );
    }

    #[tokio::test]
    async fn test_batch_edit_cas_pre_validation_rejects_whole_batch() {
        let (broker, ctx, _db, store) = setup().await;
        let block_id = store
            .insert_block(
                ctx.context_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "aaa\nbbb\nccc\n",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Batch: first op is valid replace, second has wrong CAS expected_text.
        // Pre-validation should reject the entire batch — no ops applied.
        let res1 = call_res(
            &broker,
            &ctx,
            "block_edit",
            serde_json::json!({
                "block_id": block_id.to_key(),
                "operations": [
                    {"op": "replace", "start_line": 0, "end_line": 1, "content": "AAA", "expected_text": "aaa"},
                    {"op": "replace", "start_line": 1, "end_line": 2, "content": "BBB", "expected_text": "WRONG"}
                ]
            }),
        )
        .await;
        let err = res1.unwrap_err();
        assert!(
            err.to_string().contains("content mismatch"),
            "error: {}",
            err
        );

        // Verify no operations were applied — content unchanged
        {
            let entry = store.get(ctx.context_id).unwrap();
            let snapshot = entry.doc.get_block_snapshot(&block_id).unwrap();
            assert_eq!(
                snapshot.content, "aaa\nbbb\nccc\n",
                "content must be unchanged after rejected batch"
            );
        }

        // Now submit a fully valid batch — both ops should apply
        let res2 = call(
            &broker,
            &ctx,
            "block_edit",
            serde_json::json!({
                "block_id": block_id.to_key(),
                "operations": [
                    {"op": "replace", "start_line": 0, "end_line": 1, "content": "AAA", "expected_text": "aaa"},
                    {"op": "replace", "start_line": 1, "end_line": 2, "content": "BBB", "expected_text": "bbb"}
                ]
            }),
        )
        .await;
        assert!(
            !res2.is_error,
            "valid batch should succeed: {}",
            text_of(&res2)
        );

        let entry = store.get(ctx.context_id).unwrap();
        let snapshot = entry.doc.get_block_snapshot(&block_id).unwrap();
        assert!(
            snapshot.content.contains("AAA"),
            "first replace should be applied"
        );
        assert!(
            snapshot.content.contains("BBB"),
            "second replace should be applied"
        );
    }

    #[tokio::test]
    async fn test_svg_block_inserts_svg_content_type() {
        let (broker, ctx, _db, store) = setup().await;

        let res = call(
            &broker,
            &ctx,
            "svg_block",
            serde_json::json!({
                "content": "<svg viewBox='0 0 10 10'><circle cx='5' cy='5' r='3'/></svg>"
            }),
        )
        .await;
        assert!(!res.is_error);

        // Verify the block exists with ContentType::Svg
        let doc = store.get(ctx.context_id).unwrap();
        let ordered = doc.doc.blocks_ordered();
        let last = ordered.last().unwrap();
        assert_eq!(last.content_type, ContentType::Svg);
        assert!(last.content.contains("<svg"));
    }

    #[tokio::test]
    async fn test_abc_block_validates_parse() {
        let (broker, ctx, _db, store) = setup().await;

        // Valid ABC
        let res1 = call(
            &broker,
            &ctx,
            "abc_block",
            serde_json::json!({
                "content": "X:1\nT:Test\nK:C\nCDEF GABc"
            }),
        )
        .await;
        assert!(!res1.is_error, "expected success, got: {}", text_of(&res1));

        {
            let doc = store.get(ctx.context_id).unwrap();
            let ordered = doc.doc.blocks_ordered();
            let last = ordered.last().unwrap();
            assert_eq!(last.content_type, ContentType::Abc);
        }

        // Invalid ABC
        let res2 = call(
            &broker,
            &ctx,
            "abc_block",
            serde_json::json!({
                "content": "invalid"
            }),
        )
        .await;
        assert!(res2.is_error, "expected failure for invalid ABC");
        assert!(text_of(&res2).contains("ABC parse error"), "got: {}", text_of(&res2));
    }

    #[tokio::test]
    async fn test_img_block_rejects_invalid_hash() {
        let (broker, ctx, _db, _store) = setup().await;

        let res = call(
            &broker,
            &ctx,
            "img_block",
            serde_json::json!({ "hash": "not-a-hash" }),
        )
        .await;
        assert!(res.is_error);
        assert!(text_of(&res).contains("invalid hash"), "got: {}", text_of(&res));
    }

    // ── block_edit × multibyte content (byte-vs-char offset regression) ──
    //
    // Same disease as the kj `block edit` fix (kj/block.rs): the CRDT text
    // layer is CHAR-indexed (`BlockDocument::edit_text` bounds-checks against
    // `chars().count()` and splices at char positions), so byte offsets from
    // the translate helpers corrupt any block with multibyte UTF-8 before the
    // edit site — silent wrong-splice or spurious PositionOutOfBounds.

    fn insert_multibyte_block(
        store: &SharedBlockStore,
        ctx: &CallContext,
        content: &str,
    ) -> kaijutsu_types::BlockId {
        store
            .insert_block(
                ctx.context_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                content,
                Status::Done,
                ContentType::Plain,
            )
            .unwrap()
    }

    fn content_of(
        store: &SharedBlockStore,
        ctx: &CallContext,
        id: &kaijutsu_types::BlockId,
    ) -> String {
        store
            .get(ctx.context_id)
            .unwrap()
            .doc
            .get_block_snapshot(id)
            .unwrap()
            .content
    }

    #[tokio::test]
    async fn test_block_edit_insert_after_multibyte_line_splices_at_line_start() {
        let (broker, ctx, _db, store) = setup().await;
        // Line 0 is 10 chars / 16 bytes; whole content is 16 chars — the
        // buggy byte offset (16) passes the char bounds check and appends at
        // the END instead of inserting before "second". Silent corruption.
        let block_id = insert_multibyte_block(&store, &ctx, "改善 → done\nsecond");

        let res = call(
            &broker,
            &ctx,
            "block_edit",
            serde_json::json!({
                "block_id": block_id.to_key(),
                "operations": [
                    {"op": "insert", "line": 1, "content": "INSERTED"}
                ]
            }),
        )
        .await;
        assert!(!res.is_error, "insert failed: {}", text_of(&res));
        assert_eq!(
            content_of(&store, &ctx, &block_id),
            "改善 → done\nINSERTED\nsecond",
            "insert must land at the START of line 1"
        );
    }

    #[tokio::test]
    async fn test_block_edit_delete_with_multibyte_before_range() {
        let (broker, ctx, _db, store) = setup().await;
        // Content is 24 chars; the buggy byte range for line 1 is 16..26 —
        // 26 > 24 trips PositionOutOfBounds on a perfectly valid delete.
        let block_id = insert_multibyte_block(&store, &ctx, "改善 → done\nDELETE ME\nkeep");

        let res = call_res(
            &broker,
            &ctx,
            "block_edit",
            serde_json::json!({
                "block_id": block_id.to_key(),
                "operations": [
                    {"op": "delete", "start_line": 1, "end_line": 2}
                ]
            }),
        )
        .await;
        let res = res.expect("valid delete after a multibyte line must not trip bounds");
        assert!(!res.is_error, "delete failed: {}", text_of(&res));
        assert_eq!(content_of(&store, &ctx, &block_id), "改善 → done\nkeep");
    }

    #[tokio::test]
    async fn test_block_edit_replace_with_multibyte_before_range() {
        let (broker, ctx, _db, store) = setup().await;
        // Line 0 is 11 chars / 15 bytes; the buggy byte range 15..19 passes
        // the char bounds check (len 19) but deletes chars 15..19 — "tail",
        // not "old\n". Silent wrong-splice.
        let block_id = insert_multibyte_block(&store, &ctx, "→ arrows ✅\nold\ntail");

        let res = call(
            &broker,
            &ctx,
            "block_edit",
            serde_json::json!({
                "block_id": block_id.to_key(),
                "operations": [
                    {"op": "replace", "start_line": 1, "end_line": 2, "content": "new"}
                ]
            }),
        )
        .await;
        assert!(!res.is_error, "replace failed: {}", text_of(&res));
        assert_eq!(
            content_of(&store, &ctx, &block_id),
            "→ arrows ✅\nnew\ntail",
            "replace must swap line 1, not splice into 'tail'"
        );
    }

    /// CAS (`expected_text`) validates on LINES — unaffected by the offset
    /// units — but the splice after a CAS pass must still land on char
    /// boundaries. Guards the interplay.
    #[tokio::test]
    async fn test_block_edit_replace_cas_with_multibyte_prefix() {
        let (broker, ctx, _db, store) = setup().await;
        let block_id = insert_multibyte_block(&store, &ctx, "改善 ✅\nstale\nkeep");

        let res = call_res(
            &broker,
            &ctx,
            "block_edit",
            serde_json::json!({
                "block_id": block_id.to_key(),
                "operations": [
                    {"op": "replace", "start_line": 1, "end_line": 2,
                     "content": "fresh", "expected_text": "stale"}
                ]
            }),
        )
        .await;
        let res = res.expect("CAS replace after a multibyte line must not trip bounds");
        assert!(!res.is_error, "CAS replace failed: {}", text_of(&res));
        assert_eq!(content_of(&store, &ctx, &block_id), "改善 ✅\nfresh\nkeep");
    }
}
