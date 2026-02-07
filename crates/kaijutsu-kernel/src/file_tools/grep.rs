//! GrepEngine — regex search across VFS files with CRDT awareness.

use std::sync::Arc;

use async_trait::async_trait;
use kaish_glob::{FileWalker, GlobPath, WalkOptions};
use serde::Deserialize;

use crate::tools::{ExecResult, ExecutionEngine};
use crate::vfs::{MountTable, VfsOps};

use super::cache::FileDocumentCache;
use super::vfs_walker::VfsWalkerAdapter;

/// Engine for searching file content with regex.
pub struct GrepEngine {
    cache: Arc<FileDocumentCache>,
    vfs: Arc<MountTable>,
}

impl GrepEngine {
    pub fn new(cache: Arc<FileDocumentCache>, vfs: Arc<MountTable>) -> Self {
        Self { cache, vfs }
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

#[async_trait]
impl ExecutionEngine for GrepEngine {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file content with regex, CRDT-aware (sees uncommitted edits)"
    }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: '/')"
                },
                "glob": {
                    "type": "string",
                    "description": "File glob filter (e.g., '*.rs', '**/*.py')"
                },
                "context_lines": {
                    "type": "integer",
                    "description": "Number of context lines before/after each match (default: 0)"
                }
            },
            "required": ["pattern"]
        }))
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let p: GrepParams = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid params: {}", e))),
        };

        let re = match regex::Regex::new(&p.pattern) {
            Ok(r) => r,
            Err(_) => {
                // Fall back to literal search
                match regex::Regex::new(&regex::escape(&p.pattern)) {
                    Ok(r) => r,
                    Err(e) => {
                        return Ok(ExecResult::failure(
                            1,
                            format!("Invalid pattern: {}", e),
                        ))
                    }
                }
            }
        };

        let search_root = p.path.as_deref().unwrap_or("/");
        let adapter = VfsWalkerAdapter(&self.vfs);

        let options = WalkOptions {
            respect_gitignore: true,
            ..Default::default()
        };

        // Build walker, optionally filtered by glob
        let mut walker = FileWalker::new(&adapter, search_root).with_options(options);

        if let Some(ref glob_pattern) = p.glob {
            match GlobPath::new(glob_pattern) {
                Ok(g) => walker = walker.with_pattern(g),
                Err(e) => {
                    return Ok(ExecResult::failure(
                        1,
                        format!("Invalid glob: {}", e),
                    ))
                }
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
            // fall back to raw VFS read
            let content = match self.cache.read_content(&path_str).await {
                Ok(c) => c,
                Err(_) => {
                    match self.vfs.read_all(file_path).await {
                        Ok(bytes) => match String::from_utf8(bytes) {
                            Ok(s) => s,
                            Err(_) => continue, // skip binary files
                        },
                        Err(_) => continue,
                    }
                }
            };

            if content.len() > MAX_FILE_SIZE {
                continue;
            }

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
                        for i in start..end {
                            let prefix = if i == line_idx { ">" } else { " " };
                            output.push_str(&format!(
                                "{}{}:{}:{}\n",
                                prefix,
                                path_str,
                                i + 1,
                                lines[i]
                            ));
                        }
                        output.push_str("--\n");
                    } else {
                        output.push_str(&format!(
                            "{}:{}:{}\n",
                            path_str,
                            line_idx + 1,
                            line
                        ));
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

    async fn is_available(&self) -> bool {
        true
    }
}
