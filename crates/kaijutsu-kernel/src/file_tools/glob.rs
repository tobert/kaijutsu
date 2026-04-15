//! GlobEngine — file pattern matching via kaish-glob over VFS.

use std::sync::Arc;

use kaish_glob::{FileWalker, GlobPath, WalkOptions};
use serde::Deserialize;

use crate::execution::{ExecContext, ExecResult};
use crate::vfs::MountTable;

use super::guard::WorkspaceGuard;
use super::vfs_walker::VfsWalkerAdapter;

/// Engine for glob pattern matching over the VFS.
pub struct GlobEngine {
    vfs: Arc<MountTable>,
    guard: Option<WorkspaceGuard>,
}

impl GlobEngine {
    pub fn new(vfs: Arc<MountTable>) -> Self {
        Self { vfs, guard: None }
    }

    pub fn with_guard(mut self, guard: WorkspaceGuard) -> Self {
        self.guard = Some(guard);
        self
    }

    pub fn description(&self) -> &str {
        "Find files matching a glob pattern (supports **, gitignore)"
    }

    #[tracing::instrument(skip(self, params, ctx), name = "engine.glob")]
    pub async fn execute(
        &self,
        params: &str,
        ctx: &ExecContext,
    ) -> anyhow::Result<ExecResult> {
        let p: GlobParams = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid params: {}", e))),
        };

        let glob_path = match GlobPath::new(&p.pattern) {
            Ok(g) => g,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid pattern: {}", e))),
        };

        // Determine the search root: use static_prefix optimization + user path
        let default_root = ctx.cwd.to_string_lossy();
        let base = p.path.as_deref().unwrap_or(&default_root);
        let search_root = match glob_path.static_prefix() {
            Some(prefix) => {
                let mut root = std::path::PathBuf::from(base);
                root.push(prefix);
                root
            }
            None => std::path::PathBuf::from(base),
        };

        if let Some(ref guard) = self.guard
            && let Err(denied) = guard.check_read(ctx, &search_root.to_string_lossy())
        {
            return Ok(denied);
        }

        let adapter = VfsWalkerAdapter(&self.vfs);
        let options = WalkOptions {
            respect_gitignore: true,
            ..Default::default()
        };

        let walker = FileWalker::new(&adapter, &search_root)
            .with_pattern(glob_path)
            .with_options(options);

        match walker.collect().await {
            Ok(paths) => {
                let output: String = paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n");

                if paths.is_empty() {
                    Ok(ExecResult::success("No matches found."))
                } else {
                    Ok(ExecResult::success(format!(
                        "{}\n\n{} matches",
                        output,
                        paths.len()
                    )))
                }
            }
            Err(e) => Ok(ExecResult::failure(1, format!("Glob walk failed: {}", e))),
        }
    }
}

#[derive(Deserialize)]
struct GlobParams {
    pattern: String,
    path: Option<String>,
}
