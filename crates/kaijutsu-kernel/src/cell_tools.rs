//! Cell editing tools for AI agents.
//!
//! These tools implement the ExecutionEngine trait to allow AI models
//! to edit cells via the tool execution RPC.

use crate::crdt::CellStore;
use crate::tools::{ExecResult, ExecutionEngine};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

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
pub struct CellEditEngine {
    cells: Arc<Mutex<CellStore>>,
    agent_name: String,
}

impl CellEditEngine {
    /// Create a new cell edit engine.
    pub fn new(cells: Arc<Mutex<CellStore>>, agent_name: impl Into<String>) -> Self {
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

        let mut cells = self.cells.lock().unwrap();

        // Get the cell
        let doc = match cells.get_mut(&params.cell_id) {
            Some(d) => d,
            None => {
                return Ok(ExecResult::failure(
                    1,
                    format!("Cell not found: {}", params.cell_id),
                ));
            }
        };

        // Apply operations
        for op in params.operations {
            let content = doc.content();

            match op {
                EditOp::Insert { line, content: text } => {
                    let pos = line_to_pos(&content, line);
                    let text_with_newline = if text.ends_with('\n') {
                        text
                    } else {
                        format!("{}\n", text)
                    };
                    doc.insert(&self.agent_name, pos, &text_with_newline);
                }
                EditOp::Delete {
                    start_line,
                    end_line,
                } => {
                    let start = line_to_pos(&content, start_line);
                    let end = line_end_pos(&content, end_line);
                    if start < end {
                        doc.delete(&self.agent_name, start, end);
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
                    doc.replace(&self.agent_name, start, end, &text_with_newline);
                }
            }
        }

        // Return success with new content
        let result = serde_json::json!({
            "cell_id": params.cell_id,
            "content": doc.content(),
            "version": doc.frontier_version()
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
    cells: Arc<Mutex<CellStore>>,
}

impl CellReadEngine {
    pub fn new(cells: Arc<Mutex<CellStore>>) -> Self {
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

        let cells = self.cells.lock().unwrap();

        match cells.get(&params.cell_id) {
            Some(doc) => {
                let result = serde_json::json!({
                    "cell_id": params.cell_id,
                    "content": doc.content(),
                    "version": doc.frontier_version(),
                    "kind": format!("{:?}", doc.kind),
                    "language": doc.language
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
    cells: Arc<Mutex<CellStore>>,
}

impl CellListEngine {
    pub fn new(cells: Arc<Mutex<CellStore>>) -> Self {
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
        let cells = self.cells.lock().unwrap();

        let cell_list: Vec<serde_json::Value> = cells
            .iter()
            .map(|doc| {
                serde_json::json!({
                    "id": doc.id,
                    "kind": format!("{:?}", doc.kind),
                    "language": doc.language,
                    "length": doc.len(),
                    "version": doc.frontier_version()
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
    use crate::db::CellKind;

    #[tokio::test]
    async fn test_cell_edit_insert() {
        let store = Arc::new(Mutex::new(CellStore::new()));
        {
            let mut cells = store.lock().unwrap();
            cells
                .create_cell("test".into(), CellKind::Code, Some("rust".into()))
                .unwrap();
        }

        let engine = CellEditEngine::new(store.clone(), "test-agent");

        let params = r#"{"cell_id": "test", "operations": [{"op": "insert", "line": 0, "content": "hello world"}]}"#;
        let result = engine.execute(params).await.unwrap();

        assert!(result.success);

        let cells = store.lock().unwrap();
        let doc = cells.get("test").unwrap();
        assert_eq!(doc.content(), "hello world\n");
    }

    #[tokio::test]
    async fn test_cell_edit_delete() {
        let store = Arc::new(Mutex::new(CellStore::new()));
        {
            let mut cells = store.lock().unwrap();
            let doc = cells
                .create_cell("test".into(), CellKind::Code, Some("rust".into()))
                .unwrap();
            doc.insert("setup", 0, "line1\nline2\nline3\n");
        }

        let engine = CellEditEngine::new(store.clone(), "test-agent");

        // Delete line 1 (middle line)
        let params = r#"{"cell_id": "test", "operations": [{"op": "delete", "start_line": 1, "end_line": 1}]}"#;
        let result = engine.execute(params).await.unwrap();

        assert!(result.success);

        let cells = store.lock().unwrap();
        let doc = cells.get("test").unwrap();
        assert_eq!(doc.content(), "line1\nline3\n");
    }

    #[tokio::test]
    async fn test_cell_edit_replace() {
        let store = Arc::new(Mutex::new(CellStore::new()));
        {
            let mut cells = store.lock().unwrap();
            let doc = cells
                .create_cell("test".into(), CellKind::Code, Some("rust".into()))
                .unwrap();
            doc.insert("setup", 0, "line1\nline2\nline3\n");
        }

        let engine = CellEditEngine::new(store.clone(), "test-agent");

        // Replace lines 0-1 with new content
        let params = r#"{"cell_id": "test", "operations": [{"op": "replace", "start_line": 0, "end_line": 1, "content": "replaced"}]}"#;
        let result = engine.execute(params).await.unwrap();

        assert!(result.success);

        let cells = store.lock().unwrap();
        let doc = cells.get("test").unwrap();
        assert_eq!(doc.content(), "replaced\nline3\n");
    }

    #[tokio::test]
    async fn test_cell_read() {
        let store = Arc::new(Mutex::new(CellStore::new()));
        {
            let mut cells = store.lock().unwrap();
            let doc = cells
                .create_cell("test".into(), CellKind::Code, Some("rust".into()))
                .unwrap();
            doc.insert("setup", 0, "fn main() {}");
        }

        let engine = CellReadEngine::new(store.clone());

        let params = r#"{"cell_id": "test"}"#;
        let result = engine.execute(params).await.unwrap();

        assert!(result.success);
        assert!(result.stdout.contains("fn main() {}"));
    }

    #[tokio::test]
    async fn test_cell_list() {
        let store = Arc::new(Mutex::new(CellStore::new()));
        {
            let mut cells = store.lock().unwrap();
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
