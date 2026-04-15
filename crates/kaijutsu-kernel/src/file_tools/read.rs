//! ReadEngine — read file content with optional line numbers and windowing.

use std::sync::Arc;

use serde::Deserialize;

use crate::block_tools::translate::{content_with_line_numbers, extract_lines_with_numbers};
use crate::execution::{ExecContext, ExecResult};

use super::cache::FileDocumentCache;
use super::guard::WorkspaceGuard;

/// Engine for reading file content through the CRDT cache.
pub struct ReadEngine {
    cache: Arc<FileDocumentCache>,
    guard: Option<WorkspaceGuard>,
}

impl ReadEngine {
    pub fn new(cache: Arc<FileDocumentCache>) -> Self {
        Self { cache, guard: None }
    }

    pub fn with_guard(mut self, guard: WorkspaceGuard) -> Self {
        self.guard = Some(guard);
        self
    }
}

#[derive(Deserialize)]
struct ReadParams {
    path: String,
    offset: Option<u32>,
    limit: Option<u32>,
}

impl ReadEngine {
    pub fn description(&self) -> &str {
        "Read file content with optional line numbers and windowed ranges"
    }

    #[tracing::instrument(skip(self, params, ctx), name = "engine.read")]
    pub async fn execute(
        &self,
        params: &str,
        ctx: &ExecContext,
    ) -> anyhow::Result<ExecResult> {
        let p: ReadParams = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid params: {}", e))),
        };

        if let Some(ref guard) = self.guard
            && let Err(denied) = guard.check_read(ctx, &p.path)
        {
            return Ok(denied);
        }

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
}
