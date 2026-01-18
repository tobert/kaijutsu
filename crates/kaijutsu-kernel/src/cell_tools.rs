//! Cell editing tools for AI agents.
//!
//! These tools implement the ExecutionEngine trait to allow AI models
//! to edit cells via the tool execution RPC.
//!
//! Uses BlockStore for block-based CRDT storage.

use crate::block_store::SharedBlockStore;
use crate::tools::{ExecResult, ExecutionEngine};
use async_trait::async_trait;
use kaijutsu_crdt::BlockContentSnapshot;
use serde::{Deserialize, Serialize};

/// Edit operation on a cell.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EditOp {
    /// Insert text at a line.
    Insert { line: usize, content: String },
    /// Delete lines from start_line to end_line (inclusive).
    Delete { start_line: usize, end_line: usize },
    /// Replace lines with new content.
    Replace {
        start_line: usize,
        end_line: usize,
        content: String,
    },
}

/// Parameters for cell.edit tool.
#[derive(Debug, Deserialize)]
pub struct CellEditParams {
    pub cell_id: String,
    pub operations: Vec<EditOp>,
}

/// Execution engine for cell editing operations.
///
/// Works with block-based cells by finding/creating a primary text block
/// for line-based editing operations.
pub struct CellEditEngine {
    cells: SharedBlockStore,
    agent_name: String,
}

impl CellEditEngine {
    /// Create a new cell edit engine.
    pub fn new(cells: SharedBlockStore, agent_name: impl Into<String>) -> Self {
        Self {
            cells,
            agent_name: agent_name.into(),
        }
    }

    /// Get the JSON schema for this tool's parameters.
    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cell_id": {
                    "type": "string",
                    "description": "Cell ID to edit"
                },
                "operations": {
                    "type": "array",
                    "description": "List of edit operations to apply",
                    "items": {
                        "oneOf": [
                            {
                                "type": "object",
                                "properties": {
                                    "op": {"const": "insert"},
                                    "line": {"type": "integer", "minimum": 0},
                                    "content": {"type": "string"}
                                },
                                "required": ["op", "line", "content"]
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "op": {"const": "delete"},
                                    "start_line": {"type": "integer", "minimum": 0},
                                    "end_line": {"type": "integer", "minimum": 0}
                                },
                                "required": ["op", "start_line", "end_line"]
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "op": {"const": "replace"},
                                    "start_line": {"type": "integer", "minimum": 0},
                                    "end_line": {"type": "integer", "minimum": 0},
                                    "content": {"type": "string"}
                                },
                                "required": ["op", "start_line", "end_line", "content"]
                            }
                        ]
                    }
                }
            },
            "required": ["cell_id", "operations"]
        })
    }
}

/// Convert a line number to a character position in the content.
fn line_to_pos(content: &str, line: usize) -> usize {
    content
        .lines()
        .take(line)
        .map(|l| l.len() + 1) // +1 for newline
        .sum()
}

/// Convert a line number to the end position (start of next line).
fn line_end_pos(content: &str, line: usize) -> usize {
    let lines: Vec<&str> = content.lines().collect();
    if line >= lines.len() {
        content.len()
    } else {
        line_to_pos(content, line + 1)
    }
}

#[async_trait]
impl ExecutionEngine for CellEditEngine {
    fn name(&self) -> &str {
        "cell.edit"
    }

    fn description(&self) -> &str {
        "Line-based cell editing with insert, delete, and replace operations"
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        // Parse parameters
        let params: CellEditParams = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ExecResult::failure(
                    1,
                    format!("Invalid parameters: {}", e),
                ));
            }
        };

        let mut store = self.cells.write().unwrap();

        // Get the cell
        let cell = match store.get_mut(&params.cell_id) {
            Some(c) => c,
            None => {
                return Ok(ExecResult::failure(
                    1,
                    format!("Cell not found: {}", params.cell_id),
                ));
            }
        };

        // Find the first text block, or create one if empty
        let snapshots = cell.block_snapshots();
        let primary_block_id = snapshots
            .iter()
            .find_map(|(id, snap)| {
                if matches!(snap, BlockContentSnapshot::Text { .. }) {
                    Some(id.clone())
                } else {
                    None
                }
            });

        // If no text block exists, create one
        let block_id = match primary_block_id {
            Some(id) => id,
            None => {
                cell.insert_text_block(None, "")
                    .map_err(|e| anyhow::anyhow!("Failed to create text block: {}", e))?
            }
        };

        // Apply operations
        for op in params.operations {
            let content = cell.content();

            match op {
                EditOp::Insert { line, content: text } => {
                    let pos = line_to_pos(&content, line);
                    let text_with_newline = if text.ends_with('\n') {
                        text
                    } else {
                        format!("{}\n", text)
                    };
                    cell.edit_text(&block_id, pos, &text_with_newline, 0)
                        .map_err(|e| anyhow::anyhow!("Edit failed: {}", e))?;
                }
                EditOp::Delete {
                    start_line,
                    end_line,
                } => {
                    let start = line_to_pos(&content, start_line);
                    let end = line_end_pos(&content, end_line);
                    if start < end {
                        cell.edit_text(&block_id, start, "", end - start)
                            .map_err(|e| anyhow::anyhow!("Edit failed: {}", e))?;
                    }
                }
                EditOp::Replace {
                    start_line,
                    end_line,
                    content: text,
                } => {
                    let start = line_to_pos(&content, start_line);
                    let end = line_end_pos(&content, end_line);
                    let text_with_newline = if text.ends_with('\n') || text.is_empty() {
                        text
                    } else {
                        format!("{}\n", text)
                    };
                    cell.edit_text(&block_id, start, &text_with_newline, end - start)
                        .map_err(|e| anyhow::anyhow!("Edit failed: {}", e))?;
                }
            }
        }

        // Return success with new content
        let result = serde_json::json!({
            "cell_id": params.cell_id,
            "content": cell.content(),
            "version": cell.version()
        });

        Ok(ExecResult::success(result.to_string()))
    }

    async fn is_available(&self) -> bool {
        true
    }
}

/// Parameters for cell.read tool.
#[derive(Debug, Deserialize)]
pub struct CellReadParams {
    pub cell_id: String,
}

/// Execution engine for reading cell content.
pub struct CellReadEngine {
    cells: SharedBlockStore,
}

impl CellReadEngine {
    pub fn new(cells: SharedBlockStore) -> Self {
        Self { cells }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cell_id": {
                    "type": "string",
                    "description": "Cell ID to read"
                }
            },
            "required": ["cell_id"]
        })
    }
}

#[async_trait]
impl ExecutionEngine for CellReadEngine {
    fn name(&self) -> &str {
        "cell.read"
    }

    fn description(&self) -> &str {
        "Read cell content by ID"
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let params: CellReadParams = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ExecResult::failure(
                    1,
                    format!("Invalid parameters: {}", e),
                ));
            }
        };

        let store = self.cells.read().unwrap();

        match store.get(&params.cell_id) {
            Some(cell) => {
                let result = serde_json::json!({
                    "cell_id": params.cell_id,
                    "content": cell.content(),
                    "version": cell.version(),
                    "kind": format!("{:?}", cell.kind),
                    "language": cell.language
                });
                Ok(ExecResult::success(result.to_string()))
            }
            None => Ok(ExecResult::failure(
                1,
                format!("Cell not found: {}", params.cell_id),
            )),
        }
    }

    async fn is_available(&self) -> bool {
        true
    }
}

/// Execution engine for listing cells.
pub struct CellListEngine {
    cells: SharedBlockStore,
}

impl CellListEngine {
    pub fn new(cells: SharedBlockStore) -> Self {
        Self { cells }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }
}

#[async_trait]
impl ExecutionEngine for CellListEngine {
    fn name(&self) -> &str {
        "cell.list"
    }

    fn description(&self) -> &str {
        "List all cells with metadata"
    }

    async fn execute(&self, _params: &str) -> anyhow::Result<ExecResult> {
        let store = self.cells.read().unwrap();

        let cell_list: Vec<serde_json::Value> = store
            .iter()
            .map(|cell| {
                serde_json::json!({
                    "id": cell.id,
                    "kind": format!("{:?}", cell.kind),
                    "language": cell.language,
                    "length": cell.content().len(),
                    "version": cell.version()
                })
            })
            .collect();

        let result = serde_json::json!({
            "cells": cell_list,
            "count": cell_list.len()
        });

        Ok(ExecResult::success(result.to_string()))
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

    #[tokio::test]
    async fn test_cell_edit_insert() {
        let store = shared_block_store("test-agent");
        {
            let mut cells = store.write().unwrap();
            cells
                .create_cell("test".into(), CellKind::Code, Some("rust".into()))
                .unwrap();
        }

        let engine = CellEditEngine::new(store.clone(), "test-agent");

        let params = r#"{"cell_id": "test", "operations": [{"op": "insert", "line": 0, "content": "hello world"}]}"#;
        let result = engine.execute(params).await.unwrap();

        assert!(result.success);

        let cells = store.read().unwrap();
        let cell = cells.get("test").unwrap();
        assert_eq!(cell.content(), "hello world\n");
    }

    #[tokio::test]
    async fn test_cell_edit_delete() {
        let store = shared_block_store("test-agent");
        {
            let mut cells = store.write().unwrap();
            let cell = cells
                .create_cell("test".into(), CellKind::Code, Some("rust".into()))
                .unwrap();
            cell.insert_text_block(None, "line1\nline2\nline3\n").unwrap();
        }

        let engine = CellEditEngine::new(store.clone(), "test-agent");

        // Delete line 1 (middle line)
        let params = r#"{"cell_id": "test", "operations": [{"op": "delete", "start_line": 1, "end_line": 1}]}"#;
        let result = engine.execute(params).await.unwrap();

        assert!(result.success);

        let cells = store.read().unwrap();
        let cell = cells.get("test").unwrap();
        assert_eq!(cell.content(), "line1\nline3\n");
    }

    #[tokio::test]
    async fn test_cell_edit_replace() {
        let store = shared_block_store("test-agent");
        {
            let mut cells = store.write().unwrap();
            let cell = cells
                .create_cell("test".into(), CellKind::Code, Some("rust".into()))
                .unwrap();
            cell.insert_text_block(None, "line1\nline2\nline3\n").unwrap();
        }

        let engine = CellEditEngine::new(store.clone(), "test-agent");

        // Replace lines 0-1 with new content
        let params = r#"{"cell_id": "test", "operations": [{"op": "replace", "start_line": 0, "end_line": 1, "content": "replaced"}]}"#;
        let result = engine.execute(params).await.unwrap();

        assert!(result.success);

        let cells = store.read().unwrap();
        let cell = cells.get("test").unwrap();
        assert_eq!(cell.content(), "replaced\nline3\n");
    }

    #[tokio::test]
    async fn test_cell_read() {
        let store = shared_block_store("test-agent");
        {
            let mut cells = store.write().unwrap();
            let cell = cells
                .create_cell("test".into(), CellKind::Code, Some("rust".into()))
                .unwrap();
            cell.insert_text_block(None, "fn main() {}").unwrap();
        }

        let engine = CellReadEngine::new(store.clone());

        let params = r#"{"cell_id": "test"}"#;
        let result = engine.execute(params).await.unwrap();

        assert!(result.success);
        assert!(result.stdout.contains("fn main() {}"));
    }

    #[tokio::test]
    async fn test_cell_list() {
        let store = shared_block_store("test-agent");
        {
            let mut cells = store.write().unwrap();
            cells
                .create_cell("cell1".into(), CellKind::Code, Some("rust".into()))
                .unwrap();
            cells
                .create_cell("cell2".into(), CellKind::Markdown, None)
                .unwrap();
        }

        let engine = CellListEngine::new(store.clone());

        let result = engine.execute("{}").await.unwrap();

        assert!(result.success);
        assert!(result.stdout.contains("cell1"));
        assert!(result.stdout.contains("cell2"));
        assert!(result.stdout.contains(r#""count":2"#));
    }
}
