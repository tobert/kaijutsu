//! WriteEngine — create or overwrite file content through CRDT.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::tools::{ExecResult, ExecutionEngine};

use super::cache::FileDocumentCache;

/// Engine for writing/creating files.
pub struct WriteEngine {
    cache: Arc<FileDocumentCache>,
}

impl WriteEngine {
    pub fn new(cache: Arc<FileDocumentCache>) -> Self {
        Self { cache }
    }
}

#[derive(Deserialize)]
struct WriteParams {
    path: String,
    content: String,
}

#[async_trait]
impl ExecutionEngine for WriteEngine {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write or create a file with the given content"
    }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to write"
                },
                "content": {
                    "type": "string",
                    "description": "Full file content to write"
                }
            },
            "required": ["path", "content"]
        }))
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let p: WriteParams = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid params: {}", e))),
        };

        match self.cache.create_or_replace(&p.path, &p.content).await {
            Ok(_) => {
                self.cache.mark_dirty(&p.path);
                Ok(ExecResult::success(format!(
                    "Wrote {} bytes to {}",
                    p.content.len(),
                    p.path
                )))
            }
            Err(e) => Ok(ExecResult::failure(1, e)),
        }
    }

    async fn is_available(&self) -> bool {
        true
    }
}
