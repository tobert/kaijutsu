//! GlobEngine — file pattern matching via kaish-glob over VFS.

use std::sync::Arc;

use async_trait::async_trait;
use kaish_glob::{FileWalker, GlobPath, WalkOptions};
use serde::Deserialize;

use crate::tools::{ExecResult, ExecutionEngine};
use crate::vfs::MountTable;

use super::vfs_walker::VfsWalkerAdapter;

/// Engine for glob pattern matching over the VFS.
pub struct GlobEngine {
    vfs: Arc<MountTable>,
}

impl GlobEngine {
    pub fn new(vfs: Arc<MountTable>) -> Self {
        Self { vfs }
    }
}

#[derive(Deserialize)]
struct GlobParams {
    pattern: String,
    path: Option<String>,
}

#[async_trait]
impl ExecutionEngine for GlobEngine {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern (supports **, gitignore)"
    }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern (e.g., '**/*.rs', 'src/**/*.ts'). Supports *, ?, **, {a,b}, [abc]."
                },
                "path": {
                    "type": "string",
                    "description": "Root directory to search from (default: '/')"
                }
            },
            "required": ["pattern"]
        }))
    }

    #[tracing::instrument(skip(self, params), name = "engine.glob")]
    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let p: GlobParams = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid params: {}", e))),
        };

        let glob_path = match GlobPath::new(&p.pattern) {
            Ok(g) => g,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid pattern: {}", e))),
        };

        // Determine the search root: use static_prefix optimization + user path
        let base = p.path.as_deref().unwrap_or("/");
        let search_root = match glob_path.static_prefix() {
            Some(prefix) => {
                let mut root = std::path::PathBuf::from(base);
                root.push(prefix);
                root
            }
            None => std::path::PathBuf::from(base),
        };

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

    async fn is_available(&self) -> bool {
        true
    }
}
