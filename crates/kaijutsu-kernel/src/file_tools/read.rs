//! ReadEngine — read file content with optional line numbers and windowing.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::block_tools::translate::{content_with_line_numbers, extract_lines_with_numbers};
use crate::tools::{ExecResult, ExecutionEngine};

use super::cache::FileDocumentCache;

/// Engine for reading file content through the CRDT cache.
pub struct ReadEngine {
    cache: Arc<FileDocumentCache>,
}

impl ReadEngine {
    pub fn new(cache: Arc<FileDocumentCache>) -> Self {
        Self { cache }
    }
}

#[derive(Deserialize)]
struct ReadParams {
    path: String,
    offset: Option<u32>,
    limit: Option<u32>,
}

#[async_trait]
impl ExecutionEngine for ReadEngine {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read file content with optional line numbers and windowed ranges"
    }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to read (relative to VFS root)"
                },
                "offset": {
                    "type": "integer",
                    "description": "Start line (0-indexed). Omit to read from beginning."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to return. Omit for all lines."
                }
            },
            "required": ["path"]
        }))
    }

    #[tracing::instrument(skip(self, params), name = "engine.read")]
    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let p: ReadParams = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid params: {}", e))),
        };

        match self.cache.read_content(&p.path).await {
            Ok(content) => {
                let output = match (p.offset, p.limit) {
                    (Some(offset), Some(limit)) => {
                        extract_lines_with_numbers(&content, offset, offset + limit)
                    }
                    (Some(offset), None) => {
                        let line_count = content.lines().count() as u32;
                        extract_lines_with_numbers(&content, offset, line_count)
                    }
                    _ => content_with_line_numbers(&content),
                };
                Ok(ExecResult::success(output))
            }
            Err(e) => Ok(ExecResult::failure(1, e)),
        }
    }

    async fn is_available(&self) -> bool {
        true
    }
}
