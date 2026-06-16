//! GrepEngine — regex search across VFS files with CRDT awareness.

use std::sync::Arc;

use kaish_glob::{FileWalker, GlobPath, WalkOptions};
use serde::Deserialize;

use crate::execution::{ExecContext, ExecResult};
use crate::vfs::{MountTable, VfsOps};

use super::cache::{CacheReadError, FileDocumentCache};
use super::guard::WorkspaceGuard;
use super::path::resolve_str;
use super::vfs_walker::VfsWalkerAdapter;

/// Engine for searching file content with regex.
pub struct GrepEngine {
    cache: Arc<FileDocumentCache>,
    vfs: Arc<MountTable>,
    guard: Option<WorkspaceGuard>,
}

impl GrepEngine {
    pub fn new(cache: Arc<FileDocumentCache>, vfs: Arc<MountTable>) -> Self {
        Self {
            cache,
            vfs,
            guard: None,
        }
    }

    pub fn with_guard(mut self, guard: WorkspaceGuard) -> Self {
        self.guard = Some(guard);
        self
    }
}

#[derive(Deserialize)]
struct GrepParams {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    #[serde(default)]
    context_lines: u32,
}

/// Maximum number of matches to return.
const MAX_MATCHES: usize = 200;
/// Maximum file size to search (skip very large files).
const MAX_FILE_SIZE: usize = 1_000_000;

impl GrepEngine {
    pub fn description(&self) -> &str {
        "Search file content with regex, CRDT-aware (sees uncommitted edits)"
    }

    #[tracing::instrument(skip(self, params, ctx), name = "engine.grep")]
    pub async fn execute(
        &self,
        params: &str,
        ctx: &ExecContext,
    ) -> anyhow::Result<ExecResult> {
        let p: GrepParams = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid params: {}", e))),
        };

        // No silent fallback: an invalid regex is a caller error, not an
        // excuse to quietly search for a different (literal) thing.
        let re = match regex::Regex::new(&p.pattern) {
            Ok(r) => r,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid regex pattern: {}", e))),
        };

        // Resolve the search root against cwd so grep agrees with read/edit.
        let search_root = match &p.path {
            Some(pp) => match resolve_str(&ctx.cwd, pp) {
                Ok(s) => s,
                Err(e) => return Ok(ExecResult::failure(1, e.to_string())),
            },
            None => ctx.cwd.to_string_lossy().into_owned(),
        };

        if let Some(ref guard) = self.guard
            && let Err(denied) = guard.check_read(ctx, &search_root)
        {
            return Ok(denied);
        }

        let adapter = VfsWalkerAdapter(&self.vfs);

        let options = WalkOptions {
            respect_gitignore: true,
            ..Default::default()
        };

        // Build walker, optionally filtered by glob
        let mut walker = FileWalker::new(&adapter, &search_root).with_options(options);

        if let Some(ref glob_pattern) = p.glob {
            match GlobPath::new(glob_pattern) {
                Ok(g) => walker = walker.with_pattern(g),
                Err(e) => return Ok(ExecResult::failure(1, format!("Invalid glob: {}", e))),
            }
        }

        let files = match walker.collect().await {
            Ok(f) => f,
            Err(e) => return Ok(ExecResult::failure(1, format!("Walk failed: {}", e))),
        };

        let mut output = String::new();
        let mut total_matches = 0;

        for file_path in &files {
            if total_matches >= MAX_MATCHES {
                break;
            }

            let path_str = file_path.display().to_string();

            // Try reading from CRDT cache first (sees uncommitted edits),
            // fall back to raw VFS read with pre-flight size check.
            // NotCached = binary or absent → fall through to VFS (correct).
            // Backend = real CRDT error → emit a warning line and skip; a
            // swallowed Backend error would produce silent false negatives,
            // so we surface it in output rather than silently continuing.
            let content = match self.cache.try_read_content(&path_str).await {
                Ok(c) => {
                    if c.len() > MAX_FILE_SIZE {
                        continue;
                    }
                    c
                }
                Err(CacheReadError::Backend(e)) => {
                    // Emit a visible warning in the result so the caller knows
                    // a file was skipped due to a real error, not just because
                    // it was binary. False negatives are worse than noisy output.
                    output.push_str(&format!("# WARNING: skipped {} (CRDT error: {})\n", path_str, e));
                    continue;
                }
                Err(CacheReadError::NotCached) => {
                    // Check size before loading to avoid OOM on huge files
                    if let Ok(attr) = self.vfs.getattr(file_path).await
                        && attr.size as usize > MAX_FILE_SIZE
                    {
                        continue;
                    }
                    match self.vfs.read_all(file_path).await {
                        Ok(bytes) => match String::from_utf8(bytes) {
                            Ok(s) => s,
                            Err(_) => continue, // binary file — skip silently
                        },
                        Err(_) => continue,
                    }
                }
            };

            let lines: Vec<&str> = content.lines().collect();
            let ctx = p.context_lines as usize;

            for (line_idx, line) in lines.iter().enumerate() {
                if total_matches >= MAX_MATCHES {
                    break;
                }

                if re.is_match(line) {
                    total_matches += 1;

                    if ctx > 0 {
                        let start = line_idx.saturating_sub(ctx);
                        let end = (line_idx + ctx + 1).min(lines.len());
                        for (i, line_text) in lines[start..end].iter().enumerate() {
                            let abs_idx = start + i;
                            let prefix = if abs_idx == line_idx { ">" } else { " " };
                            output.push_str(&format!(
                                "{}{}:{}:{}\n",
                                prefix,
                                path_str,
                                abs_idx + 1,
                                line_text
                            ));
                        }
                        output.push_str("--\n");
                    } else {
                        output.push_str(&format!("{}:{}:{}\n", path_str, line_idx + 1, line));
                    }
                }
            }
        }

        if total_matches == 0 {
            Ok(ExecResult::success("No matches found."))
        } else {
            let truncated = if total_matches >= MAX_MATCHES {
                format!(" (truncated at {} matches)", MAX_MATCHES)
            } else {
                String::new()
            };
            Ok(ExecResult::success(format!(
                "{}\n{} match{} in {} file{}{}",
                output.trim_end(),
                total_matches,
                if total_matches == 1 { "" } else { "es" },
                files.len(),
                if files.len() == 1 { "" } else { "s" },
                truncated
            )))
        }
    }
}
