//! WriteEngine — create or overwrite file content through CRDT.

use std::sync::Arc;

use serde::Deserialize;

use crate::execution::{ExecContext, ExecResult};

use super::cache::FileDocumentCache;
use super::guard::WorkspaceGuard;
use super::path::{deny_etc_write, is_rc_path, rc_write_denied, resolve_str};

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

        let path = match resolve_str(&ctx.cwd, &p.path) {
            Ok(s) => s,
            Err(e) => return Ok(ExecResult::failure(1, e.to_string())),
        };

        // rc scripts live under /etc/rc and run privileged on every fork;
        // writing one via the file tool needs the rc-write capability (an
        // ergonomic nudge — kj rc and host vim always work). Other /etc
        // paths map to the read-only host root and are denied flat.
        if is_rc_path(&path) {
            if !self
                .guard
                .as_ref()
                .is_some_and(|g| g.context_allows_rc_write(ctx))
            {
                return Ok(rc_write_denied(&path));
            }
        } else if let Some(denied) = deny_etc_write(&path) {
            return Ok(denied);
        }

        if let Some(ref guard) = self.guard
            && let Err(denied) = guard.check_write(ctx, &path)
        {
            return Ok(denied);
        }

        let existed = self.cache.exists(&path).await;

        match self.cache.create_or_replace(&path, &p.content).await {
            Ok(_) => {
                self.cache.mark_dirty(&path);
                // Write-through: external tools read the real filesystem, so
                // persist now rather than leaving the edit stranded in the cache.
                if let Err(e) = self.cache.flush_one(&path).await {
                    return Ok(ExecResult::failure(
                        1,
                        format!("wrote to CRDT but failed to flush {}: {}", path, e),
                    ));
                }
                Ok(ExecResult::success(format!(
                    "{} {} ({} bytes)",
                    if existed { "Updated" } else { "Created" },
                    path,
                    p.content.len(),
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
