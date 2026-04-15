//! WriteEngine — create or overwrite file content through CRDT.

use std::sync::Arc;

use serde::Deserialize;

use crate::execution::{ExecContext, ExecResult};

use super::cache::FileDocumentCache;
use super::guard::WorkspaceGuard;

/// Engine for writing/creating files.
pub struct WriteEngine {
    cache: Arc<FileDocumentCache>,
    guard: Option<WorkspaceGuard>,
}

impl WriteEngine {
    pub fn new(cache: Arc<FileDocumentCache>) -> Self {
        Self { cache, guard: None }
    }

    pub fn with_guard(mut self, guard: WorkspaceGuard) -> Self {
        self.guard = Some(guard);
        self
    }

    pub fn description(&self) -> &str {
        "Write or create a file with the given content"
    }

    #[tracing::instrument(skip(self, params, ctx), name = "engine.write")]
    pub async fn execute(
        &self,
        params: &str,
        ctx: &ExecContext,
    ) -> anyhow::Result<ExecResult> {
        let p: WriteParams = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid params: {}", e))),
        };

        if let Some(ref guard) = self.guard
            && let Err(denied) = guard.check_write(ctx, &p.path)
        {
            return Ok(denied);
        }

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
}

#[derive(Deserialize)]
struct WriteParams {
    path: String,
    content: String,
}
