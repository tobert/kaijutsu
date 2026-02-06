//! Context-aware git ExecutionEngine.
//!
//! Wraps [`crate::git_ops::GitRepo`] as a tool engine, with path resolution
//! through the kernel's VFS and optional LLM-generated commit messages via
//! `--summarize`.
//!
//! All git2 operations run in `tokio::task::spawn_blocking` to avoid starving
//! the async runtime — libgit2 does synchronous file I/O and locking.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use kaijutsu_crdt::{BlockId, BlockSnapshot, DriftKind};

use crate::block_store::SharedBlockStore;
use crate::drift::{build_commit_prompt, COMMIT_SYSTEM_PROMPT};
use crate::git_ops::GitRepo;
use crate::tools::{EngineArgs, ExecResult, ExecutionEngine};
use crate::vfs::VfsOps;

/// Git execution engine — context-aware git with optional LLM commit summaries.
///
/// Resolves repository paths through the kernel's VFS mount table and records
/// commits as `DriftKind::Commit` blocks for conversation provenance.
///
/// **Context binding limitation:** Currently instantiated once per kernel with a
/// fixed `context_name` (typically `"default"`). The pwd lookup and conversation
/// history for `--summarize` are bound to this context. Multi-context scenarios
/// require either per-context engine instances or passing the active context
/// through `ExecutionEngine::execute()`. See also `DriftEngine`.
pub struct GitEngine {
    /// Weak reference to the kernel (avoids reference cycle).
    kernel: std::sync::Weak<crate::kernel::Kernel>,
    /// Shared BlockStore (all contexts' documents).
    documents: SharedBlockStore,
    /// Which context this engine operates as.
    context_name: String,
}

impl GitEngine {
    /// Create a new git engine.
    ///
    /// Takes an `Arc<Kernel>` but stores `Weak` to avoid Kernel→ToolRegistry→GitEngine→Kernel cycle.
    pub fn new(
        kernel: &Arc<crate::kernel::Kernel>,
        documents: SharedBlockStore,
        context_name: impl Into<String>,
    ) -> Self {
        Self {
            kernel: Arc::downgrade(kernel),
            documents,
            context_name: context_name.into(),
        }
    }

    fn kernel(&self) -> Result<Arc<crate::kernel::Kernel>, String> {
        self.kernel
            .upgrade()
            .ok_or_else(|| "kernel has been dropped".to_string())
    }

    // ========================================================================
    // Path resolution
    // ========================================================================

    /// Resolve a repository path from explicit `-C` flag, context pwd, or error.
    async fn resolve_repo_path(&self, explicit_c: Option<&str>) -> Result<PathBuf, String> {
        let kernel = self.kernel()?;

        // Priority 1: explicit -C <vfs_path>
        // Priority 2: context pwd from drift router
        let vfs_path = if let Some(c) = explicit_c {
            c.to_string()
        } else {
            let router = kernel.drift().read().await;
            let short_id = router
                .short_id_for_context(&self.context_name)
                .ok_or_else(|| {
                    format!(
                        "context '{}' not registered in drift router",
                        self.context_name
                    )
                })?;
            router
                .get(short_id)
                .and_then(|h| h.pwd.clone())
                .ok_or_else(|| {
                    "No repository path. Use -C <path> or set context pwd.".to_string()
                })?
        };

        // Resolve VFS path → real filesystem path
        let real = kernel
            .vfs()
            .real_path(std::path::Path::new(&vfs_path))
            .await
            .map_err(|e| format!("VFS error resolving '{}': {}", vfs_path, e))?
            .ok_or_else(|| {
                format!(
                    "'{}' is a memory-backed mount with no filesystem path",
                    vfs_path
                )
            })?;

        if !real.exists() {
            return Err(format!("path does not exist: {}", real.display()));
        }

        Ok(real)
    }

    // ========================================================================
    // Command dispatch
    // ========================================================================

    fn execute_inner(
        &self,
        args: Vec<String>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<ExecResult, String>> + Send + '_>> {
        Box::pin(async move {
            if args.is_empty() {
                return self.show_help();
            }

            // Extract -C flag if present
            let (explicit_c, sub_args) = extract_c_flag(&args);
            let subcommand = sub_args.first().map(|s| s.as_str()).unwrap_or("help");

            match subcommand {
                "status" | "st" => self.cmd_status(explicit_c.as_deref()).await,
                "diff" => self.cmd_diff(explicit_c.as_deref(), &sub_args[1..]).await,
                "add" => self.cmd_add(explicit_c.as_deref(), &sub_args[1..]).await,
                "commit" if sub_args.iter().any(|a| a == "--summarize" || a == "-s") => {
                    self.cmd_commit_summarize(explicit_c.as_deref(), &sub_args[1..])
                        .await
                }
                "commit" => self.cmd_commit(explicit_c.as_deref(), &sub_args[1..]).await,
                "log" => self.cmd_log(explicit_c.as_deref(), &sub_args[1..]).await,
                "branch" => self.cmd_branch(explicit_c.as_deref()).await,
                "help" | "-h" | "--help" => self.show_help(),
                other => Err(format!(
                    "git: unknown subcommand '{}'. Use 'git help'.",
                    other
                )),
            }
        })
    }

    // ========================================================================
    // Subcommands
    // ========================================================================

    async fn cmd_status(&self, explicit_c: Option<&str>) -> Result<ExecResult, String> {
        let path = self.resolve_repo_path(explicit_c).await?;

        tokio::task::spawn_blocking(move || {
            let repo = GitRepo::open(&path).map_err(|e| format!("git open: {}", e))?;
            let branch = repo
                .current_branch()
                .map_err(|e| format!("git branch: {}", e))?;
            let statuses = repo.status().map_err(|e| format!("git status: {}", e))?;

            let mut output = String::new();
            if let Some(branch) = &branch {
                output.push_str(&format!("On branch {}\n", branch));
            }

            if statuses.is_empty() {
                output.push_str("nothing to commit, working tree clean\n");
            } else {
                for file in &statuses {
                    output.push_str(&format!("{} {}\n", file.status_chars(), file.path));
                }
            }

            Ok(ExecResult::success(output))
        })
        .await
        .map_err(|e| format!("spawn_blocking: {}", e))?
    }

    async fn cmd_diff(
        &self,
        explicit_c: Option<&str>,
        args: &[String],
    ) -> Result<ExecResult, String> {
        let path = self.resolve_repo_path(explicit_c).await?;
        let cached = args.iter().any(|a| a == "--cached" || a == "--staged");

        tokio::task::spawn_blocking(move || {
            let repo = GitRepo::open(&path).map_err(|e| format!("git open: {}", e))?;
            let diff = if cached {
                repo.diff_cached()
            } else {
                repo.diff()
            }
            .map_err(|e| format!("git diff: {}", e))?;

            if diff.is_empty() {
                Ok(ExecResult::success(""))
            } else {
                Ok(ExecResult::success(diff))
            }
        })
        .await
        .map_err(|e| format!("spawn_blocking: {}", e))?
    }

    async fn cmd_add(
        &self,
        explicit_c: Option<&str>,
        args: &[String],
    ) -> Result<ExecResult, String> {
        if args.is_empty() {
            return Err("Usage: git add <pathspec>...".to_string());
        }

        let path = self.resolve_repo_path(explicit_c).await?;
        let specs: Vec<String> = args.to_vec();

        tokio::task::spawn_blocking(move || {
            let repo = GitRepo::open(&path).map_err(|e| format!("git open: {}", e))?;
            let spec_refs: Vec<&str> = specs.iter().map(|s| s.as_str()).collect();
            repo.add(&spec_refs)
                .map_err(|e| format!("git add: {}", e))?;

            Ok(ExecResult::success(format!(
                "Added {} path(s) to index\n",
                specs.len()
            )))
        })
        .await
        .map_err(|e| format!("spawn_blocking: {}", e))?
    }

    async fn cmd_commit(
        &self,
        explicit_c: Option<&str>,
        args: &[String],
    ) -> Result<ExecResult, String> {
        let message = extract_m_flag(args).ok_or_else(|| {
            "Usage: git commit -m \"message\" (or use --summarize for LLM-generated message)"
                .to_string()
        })?;

        let path = self.resolve_repo_path(explicit_c).await?;

        tokio::task::spawn_blocking(move || {
            let repo = GitRepo::open(&path).map_err(|e| format!("git open: {}", e))?;
            let branch = repo
                .current_branch()
                .ok()
                .flatten()
                .unwrap_or_else(|| "HEAD".to_string());

            let oid = repo
                .commit(&message, None)
                .map_err(|e| format!("git commit: {}", e))?;

            let first_line = message.lines().next().unwrap_or(&message);
            Ok(ExecResult::success(format!(
                "[{} {}] {}\n",
                branch,
                &oid.to_string()[..7],
                first_line,
            )))
        })
        .await
        .map_err(|e| format!("spawn_blocking: {}", e))?
    }

    async fn cmd_commit_summarize(
        &self,
        explicit_c: Option<&str>,
        _args: &[String],
    ) -> Result<ExecResult, String> {
        let kernel = self.kernel()?;
        let path = self.resolve_repo_path(explicit_c).await?;

        // Phase 1 (blocking): get staged diff
        let diff = {
            let p = path.clone();
            tokio::task::spawn_blocking(move || {
                let repo = GitRepo::open(&p).map_err(|e| format!("git open: {}", e))?;
                repo.diff_cached()
                    .map_err(|e| format!("git diff: {}", e))
            })
            .await
            .map_err(|e| format!("spawn_blocking: {}", e))??
        };

        if diff.is_empty() {
            return Err("Nothing staged. Use `git add` first.".to_string());
        }

        // Phase 2 (async): get conversation context + LLM call
        let blocks = {
            let router = kernel.drift().read().await;
            let short_id = router
                .short_id_for_context(&self.context_name)
                .ok_or_else(|| {
                    format!("context '{}' not registered", self.context_name)
                })?
                .to_string();
            let doc_id = router
                .get(&short_id)
                .map(|h| h.document_id.clone())
                .ok_or_else(|| format!("context {} not found", short_id))?;
            drop(router);

            self.documents
                .block_snapshots(&doc_id)
                .unwrap_or_default()
        };

        let user_prompt = build_commit_prompt(&diff, &blocks);

        let registry = kernel.llm().read().await;
        let provider = registry.default_provider().ok_or_else(|| {
            "No LLM configured. Use -m <message> instead of --summarize.".to_string()
        })?;
        let model_name = registry
            .default_model()
            .unwrap_or("claude-sonnet-4-5-20250929")
            .to_string();
        drop(registry);

        let message = provider
            .prompt_with_system(&model_name, Some(COMMIT_SYSTEM_PROMPT), &user_prompt)
            .await
            .map_err(|e| format!("LLM error: {}", e))?;

        // Phase 3 (blocking): commit with LLM-generated message
        let (branch, oid_str) = {
            let msg = message.clone();
            tokio::task::spawn_blocking(move || {
                let repo =
                    GitRepo::open(&path).map_err(|e| format!("git open: {}", e))?;
                let branch = repo
                    .current_branch()
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "HEAD".to_string());
                let oid = repo
                    .commit(&msg, None)
                    .map_err(|e| format!("git commit: {}", e))?;
                Ok::<_, String>((branch, oid.to_string()))
            })
            .await
            .map_err(|e| format!("spawn_blocking: {}", e))??
        };

        let short_oid = &oid_str[..7];
        let first_line = message.lines().next().unwrap_or(&message);

        // Record as drift block for provenance
        let _ = self.record_commit_drift(&message, &model_name).await;

        Ok(ExecResult::success(format!(
            "[{} {}] {}\n",
            branch, short_oid, first_line,
        )))
    }

    async fn cmd_log(
        &self,
        explicit_c: Option<&str>,
        args: &[String],
    ) -> Result<ExecResult, String> {
        let path = self.resolve_repo_path(explicit_c).await?;

        // Parse -<N> count flag
        let count = args
            .iter()
            .find(|a| a.starts_with('-') && a[1..].parse::<usize>().is_ok())
            .and_then(|a| a[1..].parse().ok())
            .unwrap_or(10);

        tokio::task::spawn_blocking(move || {
            let repo = GitRepo::open(&path).map_err(|e| format!("git open: {}", e))?;
            let entries = repo.log(count).map_err(|e| format!("git log: {}", e))?;

            if entries.is_empty() {
                return Ok(ExecResult::success("No commits yet.\n"));
            }

            let mut output = String::new();
            for entry in &entries {
                output.push_str(&format!(
                    "{} {} — {}\n",
                    entry.short_id,
                    entry.message.lines().next().unwrap_or("").trim(),
                    entry.author,
                ));
            }

            Ok(ExecResult::success(output))
        })
        .await
        .map_err(|e| format!("spawn_blocking: {}", e))?
    }

    async fn cmd_branch(&self, explicit_c: Option<&str>) -> Result<ExecResult, String> {
        let path = self.resolve_repo_path(explicit_c).await?;

        tokio::task::spawn_blocking(move || {
            let repo = GitRepo::open(&path).map_err(|e| format!("git open: {}", e))?;
            let current = repo
                .current_branch()
                .map_err(|e| format!("git branch: {}", e))?;

            match current {
                Some(branch) => Ok(ExecResult::success(format!("* {}\n", branch))),
                None => Ok(ExecResult::success("(detached HEAD)\n")),
            }
        })
        .await
        .map_err(|e| format!("spawn_blocking: {}", e))?
    }

    fn show_help(&self) -> Result<ExecResult, String> {
        Ok(ExecResult::success(
            r#"git - Context-aware git with LLM commit summaries

USAGE:
    git [-C <vfs-path>] <command> [args]

COMMANDS:
    status              Show working tree status
    diff [--cached]     Show changes (working tree or staged)
    add <pathspec>...   Stage files
    commit -m "msg"     Commit staged changes
    commit --summarize  Commit with LLM-generated message from diff + context
    log [-N]            Show recent commits (default: 10)
    branch              Show current branch
    help                Show this help

PATH RESOLUTION:
    1. Explicit -C <vfs-path> flag
    2. Context pwd (if set)
    3. Error with guidance

EXAMPLES:
    git -C /mnt/kaijutsu status
    git add src/main.rs
    git commit --summarize
    git log -5
"#,
        ))
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    /// Record a commit as a DriftKind::Commit block in the conversation document.
    async fn record_commit_drift(
        &self,
        message: &str,
        model: &str,
    ) -> Result<(), String> {
        let kernel = self.kernel()?;
        let router = kernel.drift().read().await;
        let short_id = router
            .short_id_for_context(&self.context_name)
            .ok_or_else(|| format!("context '{}' not registered", self.context_name))?
            .to_string();
        let doc_id = router
            .get(&short_id)
            .map(|h| h.document_id.clone())
            .ok_or_else(|| format!("context {} not found", short_id))?;
        drop(router);

        let snapshot = BlockSnapshot::drift(
            BlockId::new("", "", 0),
            None,
            message,
            "git",
            &self.context_name,
            Some(model.to_string()),
            DriftKind::Commit,
        );

        let after = self.documents.last_block_id(&doc_id);
        self.documents
            .insert_from_snapshot(&doc_id, snapshot, after.as_ref())
            .map_err(|e| format!("failed to record commit drift: {}", e))?;

        Ok(())
    }
}

#[async_trait]
impl ExecutionEngine for GitEngine {
    fn name(&self) -> &str {
        "git"
    }

    fn description(&self) -> &str {
        "Context-aware git with LLM commit summaries"
    }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "_positional": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Subcommand and arguments: [-C path] status|diff|add|commit|log|branch|help [args]"
                }
            },
            "required": []
        }))
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let parsed: serde_json::Value = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ExecResult::failure(
                    1,
                    format!("Invalid parameters: {}", e),
                ));
            }
        };

        let args = EngineArgs::from_json(&parsed).to_argv();

        match self.execute_inner(args).await {
            Ok(result) => Ok(result),
            Err(e) => Ok(ExecResult::failure(1, e)),
        }
    }

    async fn is_available(&self) -> bool {
        true
    }
}

// ============================================================================
// Argument parsing helpers
// ============================================================================

/// Extract `-C <path>` from args, returning (path, remaining_args).
fn extract_c_flag(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut path = None;
    let mut remaining = Vec::new();
    let mut skip_next = false;

    for (i, arg) in args.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "-C" {
            if let Some(next) = args.get(i + 1) {
                path = Some(next.clone());
                skip_next = true;
            }
        } else {
            remaining.push(arg.clone());
        }
    }

    (path, remaining)
}

/// Extract `-m <message>` from args.
fn extract_m_flag(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "-m" {
            return iter.next().cloned();
        }
    }
    None
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_c_flag() {
        let args: Vec<String> = vec!["-C", "/mnt/repo", "status"]
            .into_iter()
            .map(String::from)
            .collect();
        let (c, rest) = extract_c_flag(&args);
        assert_eq!(c, Some("/mnt/repo".to_string()));
        assert_eq!(rest, vec!["status"]);
    }

    #[test]
    fn test_extract_c_flag_none() {
        let args: Vec<String> = vec!["status"]
            .into_iter()
            .map(String::from)
            .collect();
        let (c, rest) = extract_c_flag(&args);
        assert_eq!(c, None);
        assert_eq!(rest, vec!["status"]);
    }

    #[test]
    fn test_extract_m_flag() {
        let args: Vec<String> = vec!["-m", "hello world"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(extract_m_flag(&args), Some("hello world".to_string()));
    }

    #[test]
    fn test_extract_m_flag_none() {
        let args: Vec<String> = vec!["--summarize"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(extract_m_flag(&args), None);
    }

    #[tokio::test]
    async fn test_git_engine_help() {
        let kernel = Arc::new(crate::kernel::Kernel::new("test").await);
        let documents = crate::block_store::shared_block_store("test");
        let engine = GitEngine::new(&kernel, documents, "default");

        let result = engine
            .execute(r#"{"_positional": ["help"]}"#)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("git - Context-aware git"));
    }

    #[tokio::test]
    async fn test_git_engine_unknown_subcommand() {
        let kernel = Arc::new(crate::kernel::Kernel::new("test").await);
        let documents = crate::block_store::shared_block_store("test");
        let engine = GitEngine::new(&kernel, documents, "default");

        let result = engine
            .execute(r#"{"_positional": ["frobnicate"]}"#)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.stderr.contains("unknown subcommand"));
    }

    #[tokio::test]
    async fn test_git_engine_no_path_error() {
        let kernel = Arc::new(crate::kernel::Kernel::new("test").await);
        {
            let mut r = kernel.drift().write().await;
            r.register("default", "doc-test", None);
        }
        let documents = crate::block_store::shared_block_store("test");
        let engine = GitEngine::new(&kernel, documents, "default");

        let result = engine
            .execute(r#"{"_positional": ["status"]}"#)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.stderr.contains("No repository path"));
    }
}
