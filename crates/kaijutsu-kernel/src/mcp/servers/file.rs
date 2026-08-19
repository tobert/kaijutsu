//! `FileToolsServer` — virtual MCP server exposing file tools (read, edit,
//! write, glob, grep) through the broker.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::file_tools::{
    FileDocumentCache, WorkspaceGuard, CacheReadError,
    path::{resolve_str, is_rc_path, rc_write_denied, deny_etc_write},
    hashline::line_hash,
    vfs_walker::VfsWalkerAdapter,
};
use crate::vfs::{MountTable, VfsOps};
use crate::execution::{ExecContext, ExecResult};

use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{InstanceId, KernelCallParams, KernelTool, KernelToolResult};
use super::adapter::{from_exec_result, to_exec_context};

// ── Typed Params (schemars-derived) ────────────────────────────────────────

/// Parameters for the `read` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    /// File path. Relative paths resolve against the context cwd.
    pub path: String,
    /// 1-indexed line to start at (matches the line numbers in the output and
    /// in grep results). Omit to start at line 1.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Maximum number of lines to return. Omit to use the default cap (2000);
    /// the output notes when the window is partial.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Parameters for the `edit` tool. Two addressing modes — pass exactly one of
/// `old_string` (string mode) or `anchor` (hashline mode).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditParams {
    /// File path to edit.
    pub path: String,
    /// String mode: exact substring to find and replace (whitespace-exact).
    /// Mutually exclusive with `anchor`.
    #[serde(default)]
    pub old_string: Option<String>,
    /// Replacement text. In hashline mode this is the full new content for the
    /// anchored line(s); an empty string deletes them.
    pub new_string: String,
    /// String mode only: replace every occurrence instead of requiring a unique
    /// match (default: false).
    #[serde(default)]
    pub replace_all: bool,
    /// Hashline mode: address a line or range by the `N:hash` anchors that
    /// `read` prints — `42:a3f1` for one line, `42:a3f1..45:0e9c` for an
    /// inclusive range. The hash is reverified before writing, so a stale edit
    /// fails loud instead of corrupting. Mutually exclusive with `old_string`.
    #[serde(default)]
    pub anchor: Option<String>,
}

/// Parameters for the `write` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteParams {
    /// File path to write.
    pub path: String,
    /// File content.
    pub content: String,
}

/// Parameters for the `glob` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobParams {
    /// Glob pattern (e.g., `**/*.rs`).
    pub pattern: String,
    /// Directory to search (defaults to VFS root).
    #[serde(default)]
    pub path: Option<String>,
}

/// Parameters for the `grep` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepParams {
    /// Regex pattern to search for.
    pub pattern: String,
    /// Optional directory to restrict search.
    #[serde(default)]
    pub path: Option<String>,
    /// Optional glob filter for filenames.
    #[serde(default)]
    pub glob: Option<String>,
    /// Lines of context before/after each match.
    #[serde(default)]
    pub context_lines: u32,
}

// ── Server ─────────────────────────────────────────────────────────────────

pub struct FileToolsServer {
    instance_id: InstanceId,
    cache: Arc<FileDocumentCache>,
    vfs: Arc<MountTable>,
    guard: Option<WorkspaceGuard>,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl FileToolsServer {
    pub const INSTANCE: &'static str = "builtin.file";

    pub fn new(
        cache: Arc<FileDocumentCache>,
        vfs: Arc<MountTable>,
        guard: Option<WorkspaceGuard>,
    ) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE),
            cache,
            vfs,
            guard,
            notif_tx,
        }
    }
}

fn tool_def<P: JsonSchema>(
    instance: &InstanceId,
    name: &str,
    description: &str,
) -> McpResult<KernelTool> {
    let schema = schemars::schema_for!(P);
    Ok(KernelTool {
        instance: instance.clone(),
        name: name.to_string(),
        description: Some(description.to_string()),
        input_schema: serde_json::to_value(schema).map_err(McpError::InvalidParams)?,
    })
}

#[async_trait]
impl McpServerLike for FileToolsServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        Ok(vec![
            tool_def::<ReadParams>(&self.instance_id, "read", 
                "Read file content. Each line is shown as `LINE:hash→ content`; the `LINE:hash` prefix is metadata (not file bytes) — pass it to `edit` as an `anchor` to replace that line without retyping it. Supports windowing via offset/limit."
            )?,
            tool_def::<EditParams>(&self.instance_id, "edit",
                "Edit a file. String mode: exact `old_string`→`new_string` substring replacement (whitespace-exact; set replace_all for many). Hashline mode: pass `anchor` (`N:hash` or `N:hash..M:hash`, the anchors `read` prints) to replace a line/range by reference — the hash is reverified before writing, so a stale edit fails loud instead of corrupting. In hashline mode `new_string` is the full new line content (empty deletes)."
            )?,
            tool_def::<WriteParams>(&self.instance_id, "write",
                "Write or create a file with the given content"
            )?,
            tool_def::<GlobParams>(&self.instance_id, "glob",
                "Find files matching a glob pattern (supports **, gitignore)"
            )?,
            tool_def::<GrepParams>(&self.instance_id, "grep",
                "Search file content with regex, document-aware (sees unflushed edits)"
            )?,
        ])
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        let tool_ctx = to_exec_context(ctx);

        // Every arm below resolves a path against `tool_ctx.cwd` — even the
        // ones with an optional `path` param fall back to it. A `None` cwd
        // used to be silently fabricated as `/` by `to_exec_context`
        // (`CallContext::cwd`'s contract says `None` means filesystem tools
        // must reject; nothing enforced it), which broke path resolution
        // for the exact case it was meant to make safe: a relative
        // `src/main.rs` resolved against a fake root instead of failing
        // clean, landing on `/src/main.rs` and a confusing host permission
        // error. Gate here, before any tool-specific logic runs, so every
        // arm — current and future — inherits the refusal without anyone
        // needing to remember to copy it in.
        let Some(cwd) = tool_ctx.cwd.clone() else {
            return Ok(from_exec_result(refuse_missing_cwd(&params.tool)));
        };

        let exec = match params.tool.as_str() {
            "read" => {
                let p: ReadParams =
                    serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                let path = resolve_str(&cwd, &p.path).map_err(|e| McpError::Protocol(e.to_string()))?;
                if let Some(ref guard) = self.guard
                    && let Err(denied) = guard.check_read(&tool_ctx, &path)
                {
                    denied
                } else {
                    match self.cache.try_read_content(&path).await {
                        Ok(content) => ExecResult::success(render_file(&content, &path, p.offset, p.limit)),
                        Err(CacheReadError::NotCached) => ExecResult::failure(
                            1,
                            format!("{}: not found or not a text file", path),
                        ),
                        Err(CacheReadError::Backend(e)) => ExecResult::failure(1, e),
                    }
                }
            }
            "edit" => {
                let p: EditParams =
                    serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                let path = resolve_str(&cwd, &p.path).map_err(|e| McpError::Protocol(e.to_string()))?;
                if is_rc_path(&path) {
                    if !self.guard.as_ref().is_some_and(|g| g.context_allows_rc_write(&tool_ctx)) {
                        rc_write_denied(&path)
                    } else if let Some(ref guard) = self.guard
                        && let Err(denied) = guard.check_write(&tool_ctx, &path)
                    {
                        denied
                    } else {
                        self.apply_edit_plan(p, path, &tool_ctx).await
                    }
                } else if let Some(denied) = deny_etc_write(&path) {
                    denied
                } else if let Some(ref guard) = self.guard
                    && let Err(denied) = guard.check_write(&tool_ctx, &path)
                {
                    denied
                } else {
                    self.apply_edit_plan(p, path, &tool_ctx).await
                }
            }
            "write" => {
                let p: WriteParams =
                    serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                let path = resolve_str(&cwd, &p.path).map_err(|e| McpError::Protocol(e.to_string()))?;
                if is_rc_path(&path) {
                    if !self.guard.as_ref().is_some_and(|g| g.context_allows_rc_write(&tool_ctx)) {
                        rc_write_denied(&path)
                    } else if let Some(ref guard) = self.guard
                        && let Err(denied) = guard.check_write(&tool_ctx, &path)
                    {
                        denied
                    } else {
                        self.write_file(path, p.content).await
                    }
                } else if let Some(denied) = deny_etc_write(&path) {
                    denied
                } else if let Some(ref guard) = self.guard
                    && let Err(denied) = guard.check_write(&tool_ctx, &path)
                {
                    denied
                } else {
                    self.write_file(path, p.content).await
                }
            }
            "glob" => {
                let p: GlobParams =
                    serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                let glob_path = match kaish_glob::GlobPath::new(&p.pattern) {
                    Ok(g) => g,
                    Err(e) => return Ok(from_exec_result(ExecResult::failure(1, format!("Invalid pattern: {}", e)))),
                };
                let base = match &p.path {
                    Some(pp) => match resolve_str(&cwd, pp) {
                        Ok(s) => s,
                        Err(e) => return Ok(from_exec_result(ExecResult::failure(1, e.to_string()))),
                    },
                    None => cwd.to_string_lossy().into_owned(),
                };
                let search_root = match glob_path.static_prefix() {
                    Some(prefix) => {
                        let mut root = std::path::PathBuf::from(&base);
                        root.push(prefix);
                        root
                    }
                    None => std::path::PathBuf::from(&base),
                };
                // Checked on `search_root`, NOT on `base`: `PathBuf::push`
                // REPLACES the whole path when the pushed component is
                // absolute, so a pattern whose static prefix starts with `/`
                // turns a perfectly good base into `/` after this match. A
                // guard on `base` approves that walk. Same semantic point as
                // grep's check below — the value actually handed to the
                // walker.
                if let Some(refusal) =
                    refuse_filesystem_root_walk("glob", &search_root.to_string_lossy())
                {
                    return Ok(from_exec_result(refusal));
                }
                if let Some(ref guard) = self.guard
                    && let Err(denied) = guard.check_read(&tool_ctx, &search_root.to_string_lossy())
                {
                    denied
                } else {
                    let adapter = VfsWalkerAdapter(&self.vfs);
                    let options = kaish_glob::WalkOptions {
                        respect_gitignore: true,
                        ..Default::default()
                    };
                    let walker = kaish_glob::FileWalker::new(&adapter, &search_root)
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
                                ExecResult::success("No matches found.")
                            } else {
                                ExecResult::success(format!(
                                    "{}\n\n{} matches",
                                    output,
                                    paths.len()
                                ))
                            }
                        }
                        Err(e) => ExecResult::failure(1, format!("Glob walk failed: {}", e)),
                    }
                }
            }
            "grep" => {
                let p: GrepParams =
                    serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                let re = match regex::Regex::new(&p.pattern) {
                    Ok(r) => r,
                    Err(e) => return Ok(from_exec_result(ExecResult::failure(1, format!("Invalid regex pattern: {}", e)))),
                };
                let search_root = match &p.path {
                    Some(pp) => match resolve_str(&cwd, pp) {
                        Ok(s) => s,
                        Err(e) => return Ok(from_exec_result(ExecResult::failure(1, e.to_string()))),
                    },
                    None => cwd.to_string_lossy().into_owned(),
                };
                if let Some(refusal) = refuse_filesystem_root_walk("grep", &search_root) {
                    return Ok(from_exec_result(refusal));
                }
                if let Some(ref guard) = self.guard
                    && let Err(denied) = guard.check_read(&tool_ctx, &search_root)
                {
                    denied
                } else {
                    let adapter = VfsWalkerAdapter(&self.vfs);
                    let options = kaish_glob::WalkOptions {
                        respect_gitignore: true,
                        ..Default::default()
                    };
                    let mut walker = kaish_glob::FileWalker::new(&adapter, &search_root).with_options(options);
                    if let Some(ref glob_pattern) = p.glob {
                        match kaish_glob::GlobPath::new(glob_pattern) {
                            Ok(g) => walker = walker.with_pattern(g),
                            Err(e) => return Ok(from_exec_result(ExecResult::failure(1, format!("Invalid glob: {}", e)))),
                        }
                    }
                    let files = match walker.collect().await {
                        Ok(f) => f,
                        Err(e) => return Ok(from_exec_result(ExecResult::failure(1, format!("Walk failed: {}", e)))),
                    };
                    const MAX_MATCHES: usize = 200;
                    const MAX_FILE_SIZE: usize = 1_000_000;
                    let mut output = String::new();
                    let mut total_matches = 0;
                    for file_path in &files {
                        if total_matches >= MAX_MATCHES {
                            break;
                        }
                        let path_str = file_path.display().to_string();
                        let content = match self.cache.try_read_content(&path_str).await {
                            Ok(c) => {
                                if c.len() > MAX_FILE_SIZE {
                                    continue;
                                }
                                c
                            }
                            Err(CacheReadError::Backend(e)) => {
                                output.push_str(&format!("# WARNING: skipped {} (document error: {})\n", path_str, e));
                                continue;
                            }
                            Err(CacheReadError::NotCached) => {
                                if let Ok(attr) = self.vfs.getattr(file_path).await
                                    && attr.size as usize > MAX_FILE_SIZE
                                {
                                    continue;
                                }
                                match self.vfs.read_all(file_path).await {
                                    Ok(bytes) => match String::from_utf8(bytes) {
                                        Ok(s) => s,
                                        Err(_) => continue,
                                    },
                                    Err(_) => continue,
                                }
                            }
                        };
                        let lines: Vec<&str> = content.lines().collect();
                        let ctx_lines = p.context_lines as usize;
                        for (line_idx, line) in lines.iter().enumerate() {
                            if total_matches >= MAX_MATCHES {
                                break;
                            }
                            if re.is_match(line) {
                                total_matches += 1;
                                if ctx_lines > 0 {
                                    let start = line_idx.saturating_sub(ctx_lines);
                                    let end = (line_idx + ctx_lines + 1).min(lines.len());
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
                        ExecResult::success("No matches found.")
                    } else {
                        let truncated = if total_matches >= MAX_MATCHES {
                            format!(" (truncated at {} matches)", MAX_MATCHES)
                        } else {
                            String::new()
                        };
                        ExecResult::success(format!(
                            "{}\n{} match{} in {} file{}{}",
                            output.trim_end(),
                            total_matches,
                            if total_matches == 1 { "" } else { "es" },
                            files.len(),
                            if files.len() == 1 { "" } else { "s" },
                            truncated
                        ))
                    }
                }
            }
            other => {
                return Err(McpError::ToolNotFound {
                    instance: self.instance_id.clone(),
                    tool: other.to_string(),
                });
            }
        };

        Ok(from_exec_result(exec))
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}

impl FileToolsServer {
    async fn write_file(&self, path: String, content: String) -> ExecResult {
        let vfs_path = std::path::Path::new(&path);

        // Read-only / OS mounts never touch the document cache: let the VFS
        // reject the write cleanly instead of poisoning the cache with an
        // un-flushable edit. Mirrors the gate `MountBackend::raw_write`
        // already applies on the kaish surface (`runtime/mount_backend.rs`).
        // Without it, `create_or_replace` below lands the new content in the
        // kernel block store *before* the flush is even attempted, and a
        // failed flush is never undone: the cache entry's `dirty` flag has
        // no path back to `false` on failure, and staleness detection keys
        // off the VFS file's `generation` — which a failed write never
        // advances — so every later `read` on this path would keep serving
        // the phantom unwritten content forever, not the real (untouched)
        // file. Live-verified before this fix: a write to a read-only mount
        // failed with a clean error, but the very next read returned the
        // tampered content while the on-disk file was still the original.
        if !self.vfs.is_writable(vfs_path).await {
            let existed = self.vfs.exists(vfs_path).await;
            return match self.vfs.write_all(vfs_path, content.as_bytes()).await {
                Ok(()) => ExecResult::success(format!(
                    "{} {} ({} bytes)",
                    if existed { "Updated" } else { "Created" },
                    path,
                    content.len(),
                )),
                Err(e) => ExecResult::failure(1, format!("{}: {}", path, e)),
            };
        }

        let existed = self.cache.exists(&path).await;
        match self.cache.create_or_replace(&path, &content).await {
            Ok(_) => {
                if let Err(e) = self.cache.mark_dirty(&path) {
                    return ExecResult::failure(
                        1,
                        format!("wrote the document but failed to record it dirty: {}", e),
                    );
                }
                if let Err(e) = self.cache.flush_one(&path).await {
                    // Roll all the way back: a plain `invalidate` only drops
                    // the in-memory map entry, but the persisted shadow
                    // document still holds the phantom edit — the next
                    // load's "document already exists" fallback would just
                    // re-adopt that same phantom content instead of
                    // rereading the (unchanged) disk bytes. `invalidate_document`
                    // drops the shadow doc too, so a later read reloads fresh
                    // from the VFS. A rollback failure is logged, never
                    // swallowed — it means the poisoned entry survives.
                    if let Err(inv_err) = self.cache.invalidate_document(&path) {
                        tracing::warn!(
                            "write {}: failed to roll back cache after flush failure: {}",
                            path,
                            inv_err
                        );
                    }
                    return ExecResult::failure(
                        1,
                        format!("wrote the document but failed to flush {}: {}", path, e),
                    );
                }
                ExecResult::success(format!(
                    "{} {} ({} bytes)",
                    if existed { "Updated" } else { "Created" },
                    path,
                    content.len(),
                ))
            }
            Err(e) => ExecResult::failure(1, e),
        }
    }

    async fn apply_edit_plan(&self, p: EditParams, path: String, _tool_ctx: &ExecContext) -> ExecResult {
        match (&p.anchor, &p.old_string) {
            (Some(_), Some(_)) => {
                return ExecResult::failure(
                    1,
                    "provide either `anchor` (hashline) or `old_string` (string mode), not both",
                );
            }
            (None, None) => {
                return ExecResult::failure(
                    1,
                    "edit needs `old_string` (string mode) or `anchor` (hashline mode)",
                );
            }
            (None, Some(old)) if *old == p.new_string => {
                return ExecResult::failure(
                    1,
                    "old_string and new_string are identical",
                );
            }
            _ => {}
        }

        // Same gate as `write_file`: a read-only/OS mount must refuse before
        // anything touches the document cache, not after a flush fails. `edit`
        // has no raw-VFS fallback (it needs the current content to compute a
        // diff against, and a read-only target can never accept the result
        // anyway) — refuse outright.
        if !self.vfs.is_writable(std::path::Path::new(&path)).await {
            return ExecResult::failure(
                1,
                format!("{}: read-only mount (no writes)", path),
            );
        }

        let (ctx_id, block_id) = match self.cache.get_or_load(&path).await {
            Ok(ids) => ids,
            Err(e) => return ExecResult::failure(1, e),
        };

        let content = match self.cache.read_content(&path).await {
            Ok(c) => c,
            Err(e) => return ExecResult::failure(1, e),
        };

        let plan = if let Some(anchor) = &p.anchor {
            plan_anchor_edit(&content, anchor, &p.new_string)
        } else {
            let old = p.old_string.as_deref().unwrap_or_default();
            plan_string_edit(&content, old, &p.new_string, p.replace_all)
        };
        let plan = match plan {
            Ok(plan) => plan,
            Err(msg) => return ExecResult::failure(1, format!("{}: {}", path, msg)),
        };

        let store = self.cache.block_store();
        for op in plan.ops.iter().rev() {
            if let Err(e) =
                store.edit_text(ctx_id, &block_id, op.char_offset, &op.insert, op.char_delete)
            {
                return ExecResult::failure(1, e.to_string());
            }
        }

        if let Err(e) = self.cache.mark_dirty(&path) {
            return ExecResult::failure(
                1,
                format!("edited the document but failed to record it dirty: {}", e),
            );
        }
        if let Err(e) = self.cache.flush_one(&path).await {
            // See `write_file`'s matching rollback: a plain `invalidate`
            // leaves the persisted shadow doc holding the phantom edit,
            // which the next load would just re-adopt. Drop the doc too.
            if let Err(inv_err) = self.cache.invalidate_document(&path) {
                tracing::warn!(
                    "edit {}: failed to roll back cache after flush failure: {}",
                    path,
                    inv_err
                );
            }
            return ExecResult::failure(
                1,
                format!("edited the document but failed to flush {}: {}", path, e),
            );
        }

        let updated = match self.cache.try_read_content(&path).await {
            Ok(c) => c,
            Err(CacheReadError::Backend(e)) => {
                return ExecResult::failure(
                    1,
                    format!("edit applied but post-write read failed for {}: {}", path, e),
                );
            }
            Err(CacheReadError::NotCached) => {
                return ExecResult::failure(
                    1,
                    format!(
                        "edit applied but {} could not be read back to verify it",
                        path
                    ),
                );
            }
        };
        if updated != plan.expected {
            return ExecResult::failure(
                1,
                format!(
                    "edit verification FAILED for {}: the file does not match the \
                     requested change (the edit was misapplied). Re-read the file \
                     before further edits.",
                    path
                ),
            );
        }

        let first_byte = plan
            .ops
            .first()
            .map(|op| {
                updated
                    .char_indices()
                    .nth(op.char_offset)
                    .map(|(b, _)| b)
                    .unwrap_or(updated.len())
            })
            .unwrap_or(0);
        let match_len = plan.ops.first().map(|op| op.insert.len()).unwrap_or(0);
        let context = extract_context(&updated, first_byte, match_len);

        ExecResult::success(format!(
            "Replaced {} occurrence{} in {}\n\n{}",
            plan.replacements,
            if plan.replacements == 1 { "" } else { "s" },
            path,
            context
        ))
    }
}

// ── Private Helpers for Read/Edit ──────────────────────────────────────────

const DEFAULT_LINE_LIMIT: u32 = 2000;
const MAX_LINE_CHARS: usize = 2000;

fn render_file(content: &str, path: &str, offset: Option<u32>, limit: Option<u32>) -> String {
    if content.is_empty() {
        return format!("(empty file: {})", path);
    }

    let total: usize = content.lines().count();
    let start = offset.map(|o| o.saturating_sub(1)).unwrap_or(0) as usize;
    let limit = limit.unwrap_or(DEFAULT_LINE_LIMIT) as usize;
    let end = start.saturating_add(limit).min(total);

    if start >= total {
        return format!(
            "(offset {} is past end of file: {} has {} line{})",
            start + 1,
            path,
            total,
            if total == 1 { "" } else { "s" },
        );
    }

    let width = total.to_string().len().max(4);
    let mut out: Vec<String> = content
        .lines()
        .enumerate()
        .skip(start)
        .take(end - start)
        .map(|(i, line)| {
            format!(
                "{:>width$}:{}→ {}",
                i + 1,
                line_hash(line),
                truncate_line(line),
                width = width
            )
        })
        .collect();

    if start > 0 || end < total {
        out.push(format!(
            "\n({} lines total; showing {}-{})",
            total,
            start + 1,
            end
        ));
    }

    out.join("\n")
}

fn truncate_line(line: &str) -> std::borrow::Cow<'_, str> {
    if line.chars().count() <= MAX_LINE_CHARS {
        return std::borrow::Cow::Borrowed(line);
    }
    let kept: String = line.chars().take(MAX_LINE_CHARS).collect();
    let elided = line.chars().count() - MAX_LINE_CHARS;
    std::borrow::Cow::Owned(format!("{}… (+{} chars truncated)", kept, elided))
}

#[derive(Debug, PartialEq, Eq)]
struct ReplaceOp {
    char_offset: usize,
    char_delete: usize,
    insert: String,
}

#[derive(Debug)]
struct EditPlan {
    ops: Vec<ReplaceOp>,
    expected: String,
    replacements: usize,
}

fn byte_to_char(s: &str, byte: usize) -> usize {
    s[..byte].chars().count()
}

fn plan_string_edit(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<EditPlan, String> {
    if old.is_empty() {
        return Err("old_string must not be empty".to_string());
    }

    let byte_offsets: Vec<usize> = content.match_indices(old).map(|(i, _)| i).collect();

    if byte_offsets.is_empty() {
        return Err("old_string not found. Make sure it matches exactly (whitespace \
             included), or use hashline `anchor` addressing instead."
            .to_string());
    }
    if !replace_all && byte_offsets.len() > 1 {
        return Err(format!(
            "old_string found {} times. Pass replace_all: true, add surrounding \
             context to make it unique, or address one line with `anchor`.",
            byte_offsets.len()
        ));
    }

    let char_delete = old.chars().count();
    let take = if replace_all { byte_offsets.len() } else { 1 };
    let ops = byte_offsets
        .iter()
        .take(take)
        .map(|&b| ReplaceOp {
            char_offset: byte_to_char(content, b),
            char_delete,
            insert: new.to_string(),
        })
        .collect::<Vec<_>>();

    let expected = if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    };

    let replacements = ops.len();
    Ok(EditPlan {
        ops,
        expected,
        replacements,
    })
}

struct Endpoint {
    line: usize,
    hash: String,
}

fn parse_endpoint(s: &str) -> Result<Endpoint, String> {
    let (num, hash) = s.split_once(':').ok_or_else(|| {
        format!("anchor endpoint `{s}` must be `LINE:hash` (e.g. `42:a3f1`)")
    })?;
    let line: usize = num
        .trim()
        .parse()
        .map_err(|_| format!("anchor line `{num}` is not a number"))?;
    if line == 0 {
        return Err("anchor line numbers are 1-indexed (got 0)".to_string());
    }
    if hash.is_empty() {
        return Err(format!("anchor endpoint `{s}` is missing its hash"));
    }
    Ok(Endpoint {
        line,
        hash: hash.trim().to_ascii_lowercase(),
    })
}

fn annotate_region(lines: &[&str], start: usize, end: usize) -> String {
    let lo = start.saturating_sub(1);
    let hi = end.min(lines.len());
    lines[lo..hi]
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{}:{}→ {}", lo + i + 1, line_hash(l), l))
        .collect::<Vec<_>>()
        .join("\n")
}

fn plan_anchor_edit(content: &str, anchor: &str, new: &str) -> Result<EditPlan, String> {
    let (start, end) = match anchor.split_once("..") {
        Some((a, b)) => (parse_endpoint(a)?, parse_endpoint(b)?),
        None => {
            let e = parse_endpoint(anchor)?;
            let line = e.line;
            let hash = e.hash.clone();
            (e, Endpoint { line, hash })
        }
    };
    if start.line > end.line {
        return Err(format!(
            "anchor range start line {} is after end line {}",
            start.line, end.line
        ));
    }

    let lines: Vec<&str> = content.lines().collect();

    if end.line > lines.len() {
        return Err(format!(
            "anchor line {} is past end of file ({} line{})",
            end.line,
            lines.len(),
            if lines.len() == 1 { "" } else { "s" }
        ));
    }

    for ep in [&start, &end] {
        let actual = line_hash(lines[ep.line - 1]);
        if actual != ep.hash {
            return Err(format!(
                "anchor stale: line {} is now `{}`, not `{}`. The file changed \
                 since you read it — re-read and retry. Current lines:\n{}",
                ep.line,
                actual,
                ep.hash,
                annotate_region(&lines, start.line, end.line)
            ));
        }
    }

    let pieces: Vec<&str> = content.split_inclusive('\n').collect();
    debug_assert_eq!(lines.len(), pieces.len());

    let prefix: String = pieces[..start.line - 1].concat();
    let body: String = pieces[start.line - 1..end.line].concat();
    let suffix: String = pieces[end.line..].concat();

    let terminator = if body.ends_with("\r\n") {
        "\r\n"
    } else if body.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    let insert = if new.is_empty() {
        String::new()
    } else {
        format!("{new}{terminator}")
    };

    let char_offset = prefix.chars().count();
    let char_delete = body.chars().count();

    let expected = format!("{prefix}{insert}{suffix}");

    Ok(EditPlan {
        ops: vec![ReplaceOp {
            char_offset,
            char_delete,
            insert,
        }],
        expected,
        replacements: 1,
    })
}

fn extract_context(content: &str, pos: usize, match_len: usize) -> String {
    let pieces: Vec<&str> = content.split_inclusive('\n').collect();
    if pieces.is_empty() {
        return String::new();
    }

    let line_of = |byte: usize| -> usize {
        let mut acc = 0;
        for (i, piece) in pieces.iter().enumerate() {
            acc += piece.len();
            if byte < acc {
                return i;
            }
        }
        pieces.len() - 1
    };

    let first = line_of(pos);
    let last = line_of(pos + match_len);
    let start = first.saturating_sub(2);
    let end = (last + 3).min(pieces.len());
    let width = end.to_string().len().max(4);

    pieces[start..end]
        .iter()
        .enumerate()
        .map(|(i, piece)| {
            let line = piece.strip_suffix('\n').unwrap_or(piece);
            let line = line.strip_suffix('\r').unwrap_or(line);
            format!("{:>width$}→ {}", start + i + 1, line, width = width)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Refuse a filesystem tool call whose context has no working directory.
///
/// `call_tool` checks this before any tool-specific logic, for every arm in
/// the match below — the whole reason `ExecContext::cwd` (and
/// `CallContext::cwd` beneath it) is `Option<PathBuf>` rather than a bare
/// path is to make a missing cwd unrepresentable as `/` and reject it
/// structurally instead of relying on each tool author to remember a guard.
/// Before this, `to_exec_context` silently defaulted `None` to `/`, so a
/// relative `src/main.rs` resolved to `/src/main.rs` and failed as a
/// confusing host permission error rather than a clean refusal. This
/// message is deliberately worded differently from
/// `refuse_filesystem_root_walk` below: that one fires when the model asked
/// to search a tree that resolves to the root — this one fires when the
/// model asked for nothing of the sort and the system had no cwd to give it
/// in the first place.
fn refuse_missing_cwd(tool: &str) -> ExecResult {
    ExecResult::failure(
        1,
        format!(
            "{tool}: this context has no working directory set, so it cannot \
             resolve any path — relative or otherwise. `cd` to a directory \
             (e.g. from the interactive shell) to give this context a cwd, \
             then retry.",
        ),
    )
}

/// Refuse a tree walk rooted at the filesystem root.
///
/// `glob`/`grep` without an explicit `path` walk from the caller's cwd. A
/// context with NO cwd is caught earlier by `refuse_missing_cwd` above — so
/// by the time a search root reaches here, the context has a real cwd and
/// still ended up at `/`, which only happens by climbing out via `..`
/// (`resolve_str` folds `../../..` etc. against a concrete base). Servicing
/// that walk means descending the entire filesystem: above any repository,
/// so `.gitignore` never applies and the walk falls into build-output trees
/// until the 120s tool timeout fires. That cost a wasted agentic iteration
/// every time an autonomous turn tried to orient itself (see
/// `docs/issues.md`, the 2026-08-07 cwd entry).
///
/// A refusal that names the fix is strictly better than a walk that cannot
/// finish: the model can retry with a real `path` on the next tool call
/// instead of burning two minutes discovering the timeout.
fn refuse_filesystem_root_walk(tool: &str, root: &str) -> Option<ExecResult> {
    if !normalizes_to_filesystem_root(root) {
        return None;
    }
    Some(ExecResult::failure(
        1,
        format!(
            "{tool}: refusing to walk the filesystem root ('{root}' resolves to \
             `/`). Either this context has no usable working directory, or the \
             path climbs out of one. Pass an explicit `path` under the tree you \
             mean.",
        ),
    ))
}

/// Does this path denote the filesystem root once `.` and `..` are folded?
///
/// Purely lexical — deliberately NOT `canonicalize()`, which hits the host
/// filesystem and would both lie about the VFS (paths here may be virtual,
/// e.g. `/v/docs`) and fail outright on a root that doesn't exist there.
///
/// `Path` equality alone is not enough. It compares components, so `//`,
/// `/.`, `///` already equal `/` — but `..` is NOT resolved by
/// `Components`, so `/tmp/..` compares unequal to `/` while the walker
/// happily descends from the root. That gap let a path climb out of its
/// workspace and reinstate the whole-filesystem walk this guard exists to
/// stop.
fn normalizes_to_filesystem_root(path: &str) -> bool {
    use std::path::Component;

    let p = std::path::Path::new(path);
    if !p.is_absolute() {
        // Relative paths are resolved against a cwd before reaching here;
        // a bare relative path can't denote the root on its own.
        return false;
    }

    let mut depth = 0usize;
    for component in p.components() {
        match component {
            Component::Normal(_) => depth += 1,
            // `..` above the root stays at the root, as the kernel does.
            Component::ParentDir => depth = depth.saturating_sub(1),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    depth == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::{shared_block_store, shared_block_store_with_db, DocumentKind};
    use crate::file_tools::FileDocumentCache;
    use crate::kernel_db::KernelDb;
    use crate::mcp::{Broker, InstancePolicy, ToolContent};
    use crate::vfs::backends::MemoryBackend;
    use crate::vfs::MountTable;
    use kaijutsu_types::PrincipalId;

    /// A fresh in-memory `KernelDb` for tests that don't otherwise need one —
    /// `FileDocumentCache` requires a handle to back its durable swap-file
    /// marker (docs/file-buffers.md), even when the test never exercises it.
    fn test_kernel_db() -> Arc<parking_lot::Mutex<KernelDb>> {
        Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()))
    }

    async fn broker_with_file(path: &str, content: &str) -> (Arc<Broker>, Arc<FileDocumentCache>) {
        let blocks = shared_block_store(PrincipalId::system());
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/tmp", MemoryBackend::new()).await;
        let cache = Arc::new(FileDocumentCache::new(blocks, vfs.clone(), test_kernel_db()));
        cache.create_or_replace(path, content).await.unwrap();

        let server = Arc::new(FileToolsServer::new(cache.clone(), vfs, None));
        let broker = Arc::new(Broker::new());
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();
        (broker, cache)
    }

    /// The default test context: a concrete cwd under the mounted `/tmp`
    /// backend. Most tests pass absolute paths and don't care what this is,
    /// but it must be `Some` — a `None` cwd is refused up front by every
    /// tool now (`refuse_missing_cwd`), so tests that aren't specifically
    /// exercising that refusal need a real one to get past it.
    fn default_ctx() -> CallContext {
        CallContext::test().with_cwd(std::path::PathBuf::from("/tmp"))
    }

    async fn call(broker: &Broker, tool: &str, args: serde_json::Value) -> KernelToolResult {
        call_with_ctx(broker, tool, args, &default_ctx()).await
    }

    async fn call_with_ctx(
        broker: &Broker,
        tool: &str,
        args: serde_json::Value,
        ctx: &CallContext,
    ) -> KernelToolResult {
        broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(FileToolsServer::INSTANCE),
                    tool: tool.to_string(),
                    arguments: args,
                },
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap()
    }

    fn text_of(r: &KernelToolResult) -> String {
        match r.content.first() {
            Some(ToolContent::Text(s)) => s.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// Every tool in this server must refuse a context with no cwd, up
    /// front — before parsing its own params, let alone resolving a path.
    /// `CallContext::test()` carries no cwd; previously `to_exec_context`
    /// fabricated `/` for it, which both violated `CallContext::cwd`'s own
    /// documented contract ("`None` means filesystem tools must reject")
    /// and, concretely, sent autonomous turns walking the entire filesystem
    /// on a pathless `glob`/`grep` until the 120s tool timeout fired (see
    /// `docs/issues.md`, the 2026-08-07 cwd entry). Empty `{}` args are
    /// enough: the check runs before any arg is even deserialized, so a
    /// tool that would otherwise reject missing-required-field input must
    /// still surface the cwd refusal first.
    #[tokio::test]
    async fn every_tool_rejects_a_context_with_no_cwd() {
        let (broker, _cache) = broker_with_file("/tmp/a.rs", "fn main() {}\n").await;

        for tool in ["read", "edit", "write", "glob", "grep"] {
            let res = call_with_ctx(&broker, tool, serde_json::json!({}), &CallContext::test()).await;
            assert!(res.is_error, "{tool}: expected refusal, got: {}", text_of(&res));
            let msg = text_of(&res);
            assert!(
                msg.contains("no working directory set"),
                "{tool}: refusal must name the actual problem, not the root-walk wording \
                 (a context with no cwd never asked to search anything): {msg}"
            );
        }
    }

    /// The concrete case gemini's review flagged: a relative path on a
    /// cwd-less context must be refused, not silently resolved against a
    /// fabricated `/` (which would turn `src/main.rs` into `/src/main.rs`
    /// and fail as a confusing host permission error instead of a clean
    /// refusal naming the real problem).
    #[tokio::test]
    async fn relative_path_with_no_cwd_is_refused_not_resolved_against_root() {
        let (broker, _cache) = broker_with_file("/tmp/a.rs", "fn main() {}\n").await;

        let res = call_with_ctx(
            &broker,
            "read",
            serde_json::json!({ "path": "src/main.rs" }),
            &CallContext::test(),
        )
        .await;

        assert!(res.is_error, "expected refusal, got: {}", text_of(&res));
        let msg = text_of(&res);
        assert!(
            msg.contains("no working directory set"),
            "got: {msg}"
        );
        assert!(
            !msg.contains("/src/main.rs"),
            "must refuse before resolving against a fabricated root, not after: {msg}"
        );
    }

    /// A pathless `glob` from a context whose cwd genuinely IS the
    /// filesystem root must still refuse, not walk. Unlike the cwd-less
    /// case above (caught earlier by `refuse_missing_cwd`), this context has
    /// a real, concrete cwd — it just happens to be `/` — so it reaches
    /// `refuse_filesystem_root_walk` instead.
    #[tokio::test]
    async fn glob_without_path_refuses_a_filesystem_root_walk() {
        let (broker, _cache) = broker_with_file("/tmp/a.rs", "fn main() {}\n").await;
        let ctx = CallContext::test().with_cwd(std::path::PathBuf::from("/"));

        let res = call_with_ctx(&broker, "glob", serde_json::json!({ "pattern": "**/Cargo.toml" }), &ctx).await;

        assert!(res.is_error, "expected refusal, got: {}", text_of(&res));
        let msg = text_of(&res);
        assert!(
            msg.contains("refusing to walk the filesystem root"),
            "refusal must say what it refused: {msg}"
        );
        assert!(
            msg.contains("path"),
            "refusal must name the fix (pass an explicit path): {msg}"
        );
    }

    /// Same guard on `grep` — it reached the walk by a different line.
    #[tokio::test]
    async fn grep_without_path_refuses_a_filesystem_root_walk() {
        let (broker, _cache) = broker_with_file("/tmp/b.rs", "fn main() {}\n").await;
        let ctx = CallContext::test().with_cwd(std::path::PathBuf::from("/"));

        let res = call_with_ctx(&broker, "grep", serde_json::json!({ "pattern": "fn" }), &ctx).await;

        assert!(res.is_error, "expected refusal, got: {}", text_of(&res));
        assert!(
            text_of(&res).contains("refusing to walk the filesystem root"),
            "grep must refuse a rootless walk too: {}",
            text_of(&res)
        );
    }

    /// A path that CLIMBS OUT of its workspace reaches the root walk the
    /// guard exists to stop, even from a context with a perfectly ordinary
    /// cwd (`/tmp`). `Path` equality misses this: `..` is not folded by
    /// `Components`, so `/tmp/..` compares unequal to `/` while the walker
    /// descends from the root all the same.
    #[tokio::test]
    async fn glob_refuses_a_path_that_climbs_out_to_the_root() {
        let (broker, _cache) = broker_with_file("/tmp/c.rs", "fn main() {}\n").await;

        let res = call(
            &broker,
            "glob",
            serde_json::json!({ "pattern": "**/*.rs", "path": "/tmp/.." }),
        )
        .await;

        assert!(res.is_error, "expected refusal, got: {}", text_of(&res));
        assert!(
            text_of(&res).contains("refusing to walk the filesystem root"),
            "a path resolving to `/` must be refused however it spells it: {}",
            text_of(&res)
        );
    }

    /// An absolute *pattern* must not escape the given `path`.
    ///
    /// `PathBuf::push` replaces the whole path when handed an absolute
    /// component, so if `static_prefix()` ever returned one, the base would
    /// silently become `/` — which is why the guard sits on `search_root`
    /// rather than on `base`. Checked against kaish-glob 0.13: it never
    /// does. `/etc/*.conf` yields `Some("etc")` (leading slash stripped) and
    /// `/**/*.rs`, `/*`, `/**` all yield `None`. This test pins that
    /// assumption, so a future kaish-glob that starts returning absolute
    /// prefixes fails here instead of quietly widening every walk.
    #[tokio::test]
    async fn an_absolute_pattern_does_not_escape_the_given_path() {
        let (broker, _cache) = broker_with_file("/tmp/d.rs", "fn main() {}\n").await;

        let res = call(
            &broker,
            "glob",
            serde_json::json!({ "pattern": "/**/*.rs", "path": "/tmp" }),
        )
        .await;

        assert!(
            !text_of(&res).contains("refusing to walk the filesystem root"),
            "walk stayed under /tmp, so the root guard must not fire: {}",
            text_of(&res)
        );
    }

    #[test]
    fn root_normalization_folds_dots_and_parents() {
        // Component-equality already handles these.
        for s in ["/", "//", "/.", "/./", "///"] {
            assert!(normalizes_to_filesystem_root(s), "{s} should be the root");
        }
        // These are the ones plain `Path` equality misses.
        for s in ["/tmp/..", "/a/b/../..", "/a/../"] {
            assert!(
                normalizes_to_filesystem_root(s),
                "{s} climbs back to the root and must be caught"
            );
        }
        // And these must NOT be refused.
        for s in ["/tmp", "/a/b/..", "/home/atobey/src", "relative/path"] {
            assert!(
                !normalizes_to_filesystem_root(s),
                "{s} is a real directory, not the root"
            );
        }
    }

    /// The guard must not over-refuse: an explicit path is let through.
    /// Without this, "refuse the root" could be satisfied by refusing
    /// everything.
    ///
    /// Asserts only that the *root refusal* doesn't fire — this harness mounts
    /// a `MemoryBackend` the walker doesn't surface files from (the sibling
    /// `glob_via_broker` likewise only asserts it didn't error), so a match
    /// count would be testing the fixture, not the guard.
    #[tokio::test]
    async fn glob_with_an_explicit_path_is_not_refused() {
        let (broker, _cache) = broker_with_file("/tmp/found.rs", "fn main() {}\n").await;

        let res = call(
            &broker,
            "glob",
            serde_json::json!({ "pattern": "*.rs", "path": "/tmp" }),
        )
        .await;

        assert!(
            !text_of(&res).contains("refusing to walk the filesystem root"),
            "an explicit path must not trip the root guard: {}",
            text_of(&res)
        );
    }

    #[tokio::test]
    async fn edit_multibyte_string_mode_round_trips_via_broker() {
        let path = "/tmp/issues.md";
        let content = "# 改善\n\n- α bullet\n- target line →\n";
        let (broker, cache) = broker_with_file(path, content).await;

        let res = call(
            &broker,
            "edit",
            serde_json::json!({
                "path": path,
                "old_string": "target line →",
                "new_string": "REPLACED",
            }),
        )
        .await;
        assert!(!res.is_error, "edit failed: {}", text_of(&res));
        assert_eq!(
            cache.read_content(path).await.unwrap(),
            "# 改善\n\n- α bullet\n- REPLACED\n"
        );
    }

    #[tokio::test]
    async fn edit_anchor_mode_round_trips_via_broker() {
        let path = "/tmp/doc.md";
        let content = "# 改善\n\n- α bullet\n- target line →\n";
        let (broker, cache) = broker_with_file(path, content).await;

        let read = call(&broker, "read", serde_json::json!({ "path": path })).await;
        let rendered = text_of(&read);
        let line = rendered
            .lines()
            .find(|l| l.contains("target"))
            .expect("read output should contain the target line");
        let anchor = line.split_once('→').unwrap().0.trim().to_string();

        let res = call(
            &broker,
            "edit",
            serde_json::json!({
                "path": path,
                "anchor": anchor,
                "new_string": "- done",
            }),
        )
        .await;
        assert!(!res.is_error, "anchor edit failed: {}", text_of(&res));
        assert_eq!(
            cache.read_content(path).await.unwrap(),
            "# 改善\n\n- α bullet\n- done\n"
        );
    }

    #[tokio::test]
    async fn edit_stale_anchor_fails_loud_via_broker() {
        let path = "/tmp/stale.md";
        let (broker, cache) = broker_with_file(path, "one\ntwo\nthree\n").await;

        let res = call(
            &broker,
            "edit",
            serde_json::json!({
                "path": path,
                "anchor": "2:0000",
                "new_string": "X",
            }),
        )
        .await;
        assert!(res.is_error, "stale anchor should fail");
        assert!(text_of(&res).contains("stale"), "got: {}", text_of(&res));
        assert_eq!(cache.read_content(path).await.unwrap(), "one\ntwo\nthree\n");
    }

    #[tokio::test]
    async fn glob_via_broker() {
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let creator = PrincipalId::system();
        let ws_id = db
            .lock()
            .get_or_create_default_workspace(creator)
            .unwrap();
        let store = shared_block_store_with_db(db.clone(), ws_id, creator);
        let _ = (&store, DocumentKind::File);

        let vfs = Arc::new(MountTable::new());
        let cache = Arc::new(FileDocumentCache::new(store, vfs.clone(), db));

        let server = Arc::new(FileToolsServer::new(cache, vfs, None));
        let broker = Arc::new(Broker::new());
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        // `CallContext::test()` carries no cwd, and every tool now refuses
        // that up front (`refuse_missing_cwd`), so this needs a concrete
        // one to reach the broker→tool plumbing this test actually checks.
        // The `path` param stays explicit too — a pathless walk would still
        // hit the root-walk guard, see `glob_without_path_refuses_a_filesystem_root_walk`.
        let ctx = CallContext::test().with_cwd(std::path::PathBuf::from("/tmp"));
        let result = broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(FileToolsServer::INSTANCE),
                    tool: "glob".to_string(),
                    arguments: serde_json::json!({
                        "pattern": "**/*.nonexistent",
                        "path": "/tmp",
                    }),
                },
                &ctx,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(!result.is_error, "glob should not error: {:?}", result.content);
        assert!(matches!(result.content.first(), Some(ToolContent::Text(_))));
    }

    // ── Render file tests ────────────────────────────────────────────────────

    #[test]
    fn empty_file_notes_emptiness() {
        assert!(render_file("", "/x.rs", None, None).contains("empty file"));
    }

    #[test]
    fn offset_is_one_indexed() {
        let content = "a\nb\nc\nd\n";
        // offset=2 should start at line 2 ("b"), not line 3.
        let out = render_file(content, "/x", Some(2), Some(1));
        assert!(out.contains(&format!("2:{}→ b", line_hash("b"))), "got: {out}");
        assert!(!out.contains("→ c"));
    }

    #[test]
    fn lines_carry_a_content_hash() {
        let out = render_file("alpha\n", "/x", None, None);
        // `LINE:hash→ content` — the anchor `edit` addresses lines by.
        assert!(out.contains(&format!("1:{}→ alpha", line_hash("alpha"))), "got: {out}");
    }

    #[test]
    fn default_limit_bounds_output_and_adds_footer() {
        let content: String = (1..=3000).map(|n| format!("line{n}\n")).collect();
        let out = render_file(&content, "/big", None, None);
        let shown = out.lines().filter(|l| l.contains("→ line")).count();
        assert_eq!(shown, DEFAULT_LINE_LIMIT as usize);
        assert!(out.contains("3000 lines total; showing 1-2000"), "got footer: {out}");
    }

    #[test]
    fn full_short_file_has_no_footer() {
        let out = render_file("one\ntwo\n", "/x", None, None);
        assert!(!out.contains("lines total"));
        assert!(out.contains(&format!("1:{}→ one", line_hash("one"))));
        assert!(out.contains(&format!("2:{}→ two", line_hash("two"))));
    }

    #[test]
    fn long_lines_are_truncated() {
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        let content = format!("{long}\nshort\n");
        let out = render_file(&content, "/x", None, None);
        assert!(out.contains("(+50 chars truncated)"), "got: {out}");
        assert!(out.contains(&format!("2:{}→ short", line_hash("short"))));
        // The hash is over the full line, not the truncated display.
        assert!(out.contains(&format!("1:{}", line_hash(&long))), "got: {out}");
    }

    #[test]
    fn offset_past_end_is_explicit() {
        let out = render_file("a\nb\n", "/x", Some(99), None);
        assert!(out.contains("past end of file"), "got: {out}");
    }

    // ── String mode ─────────────────────────────────────────────────────────

    #[test]
    fn string_unique_match_plans_one_op() {
        let plan = plan_string_edit("foo bar baz", "bar", "QUX", false).unwrap();
        assert_eq!(plan.replacements, 1);
        assert_eq!(plan.expected, "foo QUX baz");
        assert_eq!(
            plan.ops,
            vec![ReplaceOp {
                char_offset: 4,
                char_delete: 3,
                insert: "QUX".to_string(),
            }]
        );
    }

    #[test]
    fn string_no_match_is_an_error() {
        let err = plan_string_edit("hello", "xyz", "Z", false).unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn string_ambiguous_match_refused_without_replace_all() {
        let err = plan_string_edit("a a a", "a", "b", false).unwrap_err();
        assert!(err.contains("found 3 times"), "got: {err}");
    }

    #[test]
    fn string_replace_all_replaces_every_occurrence() {
        let plan = plan_string_edit("a a a", "a", "b", true).unwrap();
        assert_eq!(plan.replacements, 3);
        assert_eq!(plan.expected, "b b b");
    }

    #[test]
    fn string_empty_old_is_rejected() {
        let err = plan_string_edit("anything", "", "x", false).unwrap_err();
        assert!(err.contains("must not be empty"), "got: {err}");
    }

    #[test]
    fn string_multibyte_uses_char_offsets_not_byte_offsets() {
        let content = "α=1\n改善\ntarget\n";
        let byte_off = content.find("target").unwrap();
        assert!(byte_off > byte_to_char(content, byte_off), "fixture must be multibyte");

        let plan = plan_string_edit(content, "target", "TASK", false).unwrap();
        let op = &plan.ops[0];
        assert_eq!(op.char_offset, byte_to_char(content, byte_off));
        assert!(op.char_offset < byte_off, "must be the smaller char index");
        assert_eq!(op.char_delete, 6); // "target" is 6 chars
        assert_eq!(plan.expected, "α=1\n改善\nTASK\n");
    }

    #[test]
    fn string_multibyte_within_old_string_counts_chars() {
        let plan = plan_string_edit("x 改善 y", "改善", "kaizen", false).unwrap();
        let op = &plan.ops[0];
        assert_eq!(op.char_offset, 2); // "x " is 2 chars
        assert_eq!(op.char_delete, 2); // 改善 is 2 chars, not 6 bytes
        assert_eq!(plan.expected, "x kaizen y");
    }

    // ── Hashline mode ────────────────────────────────────────────────────────

    fn anchor_for(content: &str, line_1indexed: usize) -> String {
        let l = content.lines().nth(line_1indexed - 1).unwrap();
        format!("{}:{}", line_1indexed, line_hash(l))
    }

    #[test]
    fn anchor_single_line_replace() {
        let content = "one\ntwo\nthree\n";
        let plan = plan_anchor_edit(content, &anchor_for(content, 2), "TWO").unwrap();
        assert_eq!(plan.expected, "one\nTWO\nthree\n");
        assert_eq!(plan.replacements, 1);
    }

    #[test]
    fn anchor_range_replace_collapses_lines() {
        let content = "a\nb\nc\nd\n";
        let anchor = format!("{}..{}", anchor_for(content, 2), {
            let l = content.lines().nth(2).unwrap();
            format!("3:{}", line_hash(l))
        });
        let plan = plan_anchor_edit(content, &anchor, "X\nY").unwrap();
        assert_eq!(plan.expected, "a\nX\nY\nd\n");
    }

    #[test]
    fn anchor_empty_new_deletes_the_line() {
        let content = "keep\ndrop\nkeep2\n";
        let plan = plan_anchor_edit(content, &anchor_for(content, 2), "").unwrap();
        assert_eq!(plan.expected, "keep\nkeep2\n");
    }

    #[test]
    fn anchor_last_line_without_trailing_newline() {
        let content = "a\nb\nc"; // no final newline
        let plan = plan_anchor_edit(content, &anchor_for(content, 3), "C").unwrap();
        assert_eq!(plan.expected, "a\nb\nC");
    }

    #[test]
    fn anchor_multibyte_line_replace() {
        let content = "α\n改善\nz\n";
        let plan = plan_anchor_edit(content, &anchor_for(content, 2), "kaizen").unwrap();
        assert_eq!(plan.expected, "α\nkaizen\nz\n");
    }

    #[test]
    fn anchor_stale_hash_fails_loud() {
        let content = "one\ntwo\nthree\n";
        let stale = "2:dead";
        let err = plan_anchor_edit(content, stale, "X").unwrap_err();
        assert!(err.contains("stale"), "got: {err}");
        assert!(err.contains(&line_hash("two")), "should show current hash: {err}");
    }

    #[test]
    fn anchor_crlf_preserves_line_endings() {
        let content = "a\r\nb\r\nc\r\n";
        let plan = plan_anchor_edit(content, &anchor_for(content, 2), "B").unwrap();
        assert_eq!(plan.expected, "a\r\nB\r\nc\r\n");
    }

    #[test]
    fn anchor_crlf_delete_consumes_full_terminator() {
        let content = "a\r\nb\r\nc\r\n";
        let plan = plan_anchor_edit(content, &anchor_for(content, 2), "").unwrap();
        assert_eq!(plan.expected, "a\r\nc\r\n");
    }

    #[test]
    fn anchor_empty_file_errors_without_panic() {
        let err = plan_anchor_edit("", "1:abcd", "x").unwrap_err();
        assert!(err.contains("past end"), "got: {err}");
    }

    #[test]
    fn anchor_out_of_range_is_an_error() {
        let content = "a\nb\n";
        let err = plan_anchor_edit(content, "9:abcd", "x").unwrap_err();
        assert!(err.contains("past end"), "got: {err}");
    }

    #[test]
    fn anchor_malformed_is_an_error() {
        let content = "a\nb\n";
        assert!(plan_anchor_edit(content, "nope", "x").is_err());
        assert!(plan_anchor_edit(content, "0:abcd", "x").is_err());
        assert!(plan_anchor_edit(content, "2:", "x").is_err());
    }

    #[test]
    fn anchor_reversed_range_is_an_error() {
        let content = "a\nb\nc\n";
        let anchor = format!("{}..{}", anchor_for(content, 3), {
            let l = content.lines().next().unwrap();
            format!("1:{}", line_hash(l))
        });
        let err = plan_anchor_edit(content, &anchor, "x").unwrap_err();
        assert!(err.contains("after end"), "got: {err}");
    }

    // ── read-only mount: clean errors, never corruption ────────────────────

    /// Build a broker over a *real* read-only `LocalBackend` mount (not
    /// `MemoryBackend`) so `write`/`edit` hit the same `VfsError::ReadOnly`
    /// path production's read-only root mount does
    /// (`kernel.mount("/", LocalBackend::read_only("/"))` in
    /// `kaijutsu-server/src/rpc.rs`). Returns the broker plus the real
    /// on-disk file path and the `TempDir` (held so it isn't cleaned up
    /// early).
    async fn broker_with_readonly_file(
        content: &str,
    ) -> (Arc<Broker>, std::path::PathBuf, tempfile::TempDir) {
        use crate::vfs::backends::LocalBackend;
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("ro.txt");
        std::fs::write(&file_path, content).unwrap();

        let blocks = shared_block_store(PrincipalId::system());
        let vfs = Arc::new(MountTable::new());
        vfs.mount(tmp.path().to_str().unwrap(), LocalBackend::read_only(tmp.path()))
            .await;
        let cache = Arc::new(FileDocumentCache::new(blocks, vfs.clone(), test_kernel_db()));
        let server = Arc::new(FileToolsServer::new(cache, vfs, None));
        let broker = Arc::new(Broker::new());
        broker.register(server, InstancePolicy::default()).await.unwrap();

        (broker, file_path, tmp)
    }

    /// Regression: `write` to a read-only mount must fail cleanly AND must
    /// NOT leave the document cache holding the rejected content — a later
    /// `read` on the same path must still see the real (untouched) file, not
    /// a phantom edit that never reached disk.
    ///
    /// Before the `is_writable` gate in `write_file`, `create_or_replace`
    /// landed the new content in the kernel block store unconditionally, and a
    /// failed `flush_one` never rolled it back: `dirty` had no path back to
    /// `false` on failure, and staleness detection is keyed on the VFS
    /// file's `generation`, which a failed write never advances. Live
    /// reproduction before the fix: `write` failed with a clean error, but
    /// the immediately following `read` returned the tampered content while
    /// the on-disk file was still the original bytes.
    ///
    /// The `is_writable` gate and the `invalidate_document` rollback are
    /// deliberately redundant defenses on this path: mutation-verified
    /// (2026-08-10) that disabling the gate alone still passes (the
    /// rollback covers it) and disabling BOTH makes this test fail. Keep
    /// both — the gate refuses before the cache is ever touched; the
    /// rollback covers every *other* way a flush can fail.
    #[tokio::test]
    async fn write_to_readonly_mount_fails_clean_and_does_not_poison_later_reads() {
        let (broker, file_path, _tmp) = broker_with_readonly_file("on-disk-original").await;
        let path = file_path.to_str().unwrap().to_string();
        let ctx = CallContext::test().with_cwd(file_path.parent().unwrap().to_path_buf());

        let w = call_with_ctx(
            &broker,
            "write",
            serde_json::json!({ "path": path, "content": "TAMPERED" }),
            &ctx,
        )
        .await;
        assert!(w.is_error, "write to a read-only mount must fail");
        assert!(
            text_of(&w).to_lowercase().contains("read-only") || text_of(&w).to_lowercase().contains("read only"),
            "refusal must name the read-only mount, got: {}",
            text_of(&w)
        );

        let r = call_with_ctx(&broker, "read", serde_json::json!({ "path": path }), &ctx).await;
        assert!(!r.is_error, "read after a refused write must still succeed: {}", text_of(&r));
        assert!(
            text_of(&r).contains("on-disk-original"),
            "read must return the real file content, not a phantom unflushed edit: {}",
            text_of(&r)
        );
        assert!(
            !text_of(&r).contains("TAMPERED"),
            "read must NOT serve the rejected write's content: {}",
            text_of(&r)
        );

        // The real file on disk was never touched either.
        assert_eq!(
            std::fs::read_to_string(&file_path).unwrap(),
            "on-disk-original",
            "the on-disk file must be byte-identical to before the refused write"
        );
    }

    /// `edit` on a read-only mount must refuse up front, before it ever
    /// touches the document cache — there is no raw-VFS fallback for edit (it
    /// needs the current content to diff against, and the result could never
    /// be written anyway). A subsequent `read` must see the untouched file.
    #[tokio::test]
    async fn edit_to_readonly_mount_fails_clean_and_does_not_poison_later_reads() {
        let (broker, file_path, _tmp) = broker_with_readonly_file("fn main() {}\n").await;
        let path = file_path.to_str().unwrap().to_string();
        let ctx = CallContext::test().with_cwd(file_path.parent().unwrap().to_path_buf());

        let e = call_with_ctx(
            &broker,
            "edit",
            serde_json::json!({
                "path": path,
                "old_string": "fn main()",
                "new_string": "fn tampered()",
            }),
            &ctx,
        )
        .await;
        assert!(e.is_error, "edit on a read-only mount must fail");
        assert!(
            text_of(&e).to_lowercase().contains("read-only") || text_of(&e).to_lowercase().contains("read only"),
            "refusal must name the read-only mount, got: {}",
            text_of(&e)
        );

        let r = call_with_ctx(&broker, "read", serde_json::json!({ "path": path }), &ctx).await;
        assert!(!r.is_error, "read after a refused edit must still succeed: {}", text_of(&r));
        assert!(
            text_of(&r).contains("fn main()"),
            "read must return the real file content, not a phantom edit: {}",
            text_of(&r)
        );
        assert!(
            !text_of(&r).contains("tampered"),
            "read must NOT serve the rejected edit's content: {}",
            text_of(&r)
        );
        assert_eq!(
            std::fs::read_to_string(&file_path).unwrap(),
            "fn main() {}\n",
            "the on-disk file must be byte-identical to before the refused edit"
        );
    }
}
