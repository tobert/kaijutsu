//! End-to-end integration tests for kaish→engine dispatch paths.
//!
//! # Tiers
//!
//! - **Tier 0:** EngineArgs unit tests — JSON→argv reconstruction in isolation
//! - **Tier 1:** Drift e2e through EmbeddedKaish — smoke tests exercising full dispatch:
//!   `kaish.execute("drift_ls")` → parser → execute_command (not a builtin) →
//!   backend fallback → KaijutsuBackend.call_tool("drift_ls", ...) →
//!   DriftLsEngine.execute(json) → result
//! - **Tier 2:** Drift e2e through EmbeddedKaish — lifecycle with CRDT block verification
//! - **Tier 3:** Git direct-engine via EngineArgs — kaish's git builtin shadows the
//!   backend path, so GitEngine is unreachable through EmbeddedKaish

use std::sync::Arc;

use kaijutsu_kernel::block_store::SharedBlockStore;
use kaijutsu_kernel::db::DocumentKind;
use kaijutsu_kernel::drift::{DriftLsEngine, DriftPushEngine, DriftFlushEngine};
use kaijutsu_kernel::git_engine::GitEngine;
use kaijutsu_kernel::tools::{EngineArgs, ExecutionEngine, ToolInfo};
use kaijutsu_kernel::{shared_block_store, Kernel, LocalBackend};
use kaijutsu_server::EmbeddedKaish;

// ============================================================================
// Shared test setup
// ============================================================================

/// Create an EmbeddedKaish with split drift engines for true e2e testing.
///
/// Returns an EmbeddedKaish that exercises the full dispatch chain:
/// kaish parser → execute_command → backend fallback →
/// KaijutsuBackend.call_tool() → individual drift engines.
async fn setup_drift_e2e() -> (EmbeddedKaish, Arc<Kernel>, SharedBlockStore) {
    let kernel = Arc::new(Kernel::new("e2e-drift").await);
    let documents = shared_block_store("e2e-drift");

    documents
        .create_document("doc-default".to_string(), DocumentKind::Conversation, None)
        .unwrap();

    // Register individual drift engines
    kernel.register_tool_with_engine(
        ToolInfo::new("drift_ls", "List drift contexts", "drift"),
        Arc::new(DriftLsEngine::new(&kernel, "default")),
    ).await;
    kernel.register_tool_with_engine(
        ToolInfo::new("drift_push", "Stage drift content", "drift"),
        Arc::new(DriftPushEngine::new(&kernel, documents.clone(), "default")),
    ).await;
    kernel.register_tool_with_engine(
        ToolInfo::new("drift_flush", "Flush staged drifts", "drift"),
        Arc::new(DriftFlushEngine::new(&kernel, documents.clone(), "default")),
    ).await;

    // Register "default" context in drift router
    {
        let mut router = kernel.drift().write().await;
        router.register("default", "doc-default", None);
    }

    let kaish = EmbeddedKaish::new("e2e-drift", documents.clone(), kernel.clone(), None)
        .expect("EmbeddedKaish::new failed");

    (kaish, kernel, documents)
}

/// Create a kernel with git engine and a real git repo in a tempdir.
async fn setup_git(
    repo_dir: &std::path::Path,
) -> (Arc<GitEngine>, Arc<Kernel>, SharedBlockStore) {
    let kernel = Arc::new(Kernel::new("e2e-git-test").await);
    let documents = shared_block_store("e2e-git-test");

    documents
        .create_document("doc-default".to_string(), DocumentKind::Conversation, None)
        .unwrap();

    // Mount the tempdir at /mnt/repo in the VFS
    kernel
        .vfs()
        .mount("/mnt/repo", LocalBackend::new(repo_dir))
        .await;

    let engine = Arc::new(GitEngine::new(&kernel, documents.clone(), "default"));

    kernel
        .register_tool_with_engine(
            ToolInfo::new("git", "Context-aware git", "vcs"),
            engine.clone(),
        )
        .await;

    // Register context with pwd pointing at the VFS mount
    {
        let mut router = kernel.drift().write().await;
        router.register("default", "doc-default", None);
        router
            .set_pwd("default", Some("/mnt/repo".to_string()))
            .unwrap();
    }

    (engine, kernel, documents)
}

/// Initialize a git repo with an initial commit.
fn init_test_repo(path: &std::path::Path) {
    let repo = git2::Repository::init(path).expect("git init failed");

    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Test User").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();

    let sig = git2::Signature::now("Test User", "test@example.com").unwrap();
    let tree_id = repo.index().unwrap().write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
        .unwrap();
}

/// Build JSON like kaish's `tool_args_to_json` would for a simple positional command.
///
/// `positional_args` becomes `_positional`, flags become `true`-valued keys.
fn kaish_json(positional: &[&str], flags: &[&str]) -> String {
    let pos: Vec<serde_json::Value> = positional
        .iter()
        .map(|s| serde_json::Value::String(s.to_string()))
        .collect();

    let mut obj = serde_json::Map::new();
    if !pos.is_empty() {
        obj.insert("_positional".to_string(), serde_json::Value::Array(pos));
    }
    for flag in flags {
        obj.insert(flag.to_string(), serde_json::Value::Bool(true));
    }

    serde_json::to_string(&serde_json::Value::Object(obj)).unwrap()
}

// ============================================================================
// Tier 0: EngineArgs unit tests (kaish-style JSON → argv reconstruction)
// ============================================================================

#[test]
fn engine_args_kaish_commit_m_reconstructs_correctly() {
    // kaish splits `git commit -m "add hello"` into:
    //   positional: ["commit", "add hello"], flags: {"m"}
    let json = serde_json::json!({
        "_positional": ["commit", "add hello"],
        "m": true
    });
    let argv = EngineArgs::from_json(&json).to_argv();

    // to_argv() should reconstruct: ["commit", "-m", "add hello"]
    assert_eq!(argv[0], "commit");
    assert!(argv.contains(&"-m".to_string()), "missing -m flag in {:?}", argv);
    assert!(argv.contains(&"add hello".to_string()), "missing message in {:?}", argv);
}

#[test]
fn engine_args_kaish_diff_cached_reconstructs_correctly() {
    // kaish splits `git diff --cached` into:
    //   positional: ["diff"], flags: {"cached"}
    let json = serde_json::json!({
        "_positional": ["diff"],
        "cached": true
    });
    let argv = EngineArgs::from_json(&json).to_argv();
    assert_eq!(argv, vec!["diff", "--cached"]);
}

#[test]
fn engine_args_kaish_log_numeric_flag_reconstructs_correctly() {
    // kaish splits `git log -5` into:
    //   positional: ["log"], flags: {"5"}
    let json = serde_json::json!({
        "_positional": ["log"],
        "5": true
    });
    let argv = EngineArgs::from_json(&json).to_argv();
    assert_eq!(argv, vec!["log", "-5"]);
}

#[test]
fn engine_args_llm_passthrough_unchanged() {
    // LLMs put everything in _positional — no flags/named
    let json = serde_json::json!({"_positional": ["commit", "-m", "hello world"]});
    let argv = EngineArgs::from_json(&json).to_argv();
    assert_eq!(argv, vec!["commit", "-m", "hello world"]);
}

#[test]
fn engine_args_numeric_positional_coerced() {
    // `drift cancel 1` — kaish may send 1 as JSON number
    let json = serde_json::json!({"_positional": ["cancel", 1]});
    let argv = EngineArgs::from_json(&json).to_argv();
    assert_eq!(argv, vec!["cancel", "1"]);
}

// ============================================================================
// Tier 1: Drift e2e through EmbeddedKaish — smoke tests
// ============================================================================

#[tokio::test]
async fn drift_ls_shows_default_context() {
    let (kaish, _kernel, _docs) = setup_drift_e2e().await;
    let result = kaish.execute("drift_ls").await.unwrap();

    assert!(result.ok(), "err: {}", result.err);
    assert!(
        result.out.contains("default"),
        "expected 'default' context, got: {}",
        result.out
    );
    assert!(
        result.out.contains("doc-default"),
        "expected 'doc-default' document, got: {}",
        result.out
    );
    assert!(
        result.out.contains("* "),
        "expected '* ' marker for current context, got: {}",
        result.out
    );
}

// ============================================================================
// Tier 2: Drift e2e through EmbeddedKaish — lifecycle with CRDT verification
// ============================================================================

#[tokio::test]
async fn drift_push_flush_lifecycle() {
    let (kaish, kernel, documents) = setup_drift_e2e().await;

    // Register a second context as drift target
    let target_short = {
        let mut router = kernel.drift().write().await;
        router.register("target", "doc-target", None)
    };

    documents
        .create_document("doc-target".to_string(), DocumentKind::Conversation, None)
        .unwrap();

    // Stage a drift push via DriftPushEngine through kaish dispatch.
    // drift_push takes JSON params: target_ctx and content.
    let push_result = kaish
        .execute(&format!(
            r#"drift_push "{target_short}" "hello from e2e test""#
        ))
        .await
        .unwrap();
    assert!(push_result.ok(), "push failed: {}", push_result.err);
    assert!(push_result.out.contains("Staged"));

    // Verify queue on router directly (no queue subcommand in split engines)
    {
        let router = kernel.drift().read().await;
        let queue = router.queue();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].content, "hello from e2e test");
    }

    // Flush and verify injection
    let flush_result = kaish.execute("drift_flush").await.unwrap();
    assert!(flush_result.ok(), "flush failed: {}", flush_result.err);
    assert!(flush_result.out.contains("Flushed 1 drifts"));

    // Verify block was injected into target document
    let blocks = documents.block_snapshots("doc-target").unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].kind, kaijutsu_crdt::BlockKind::Drift);
    assert_eq!(blocks[0].content, "hello from e2e test");
}

// ============================================================================
// Tier 3: Git direct-engine via EngineArgs (kaish builtin shadows backend path)
// ============================================================================

#[tokio::test]
async fn git_help_returns_usage() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());

    let (engine, _kernel, _docs) = setup_git(tmp.path()).await;
    let result = engine.execute(&kaish_json(&["help"], &[])).await.unwrap();

    assert!(result.success, "stderr: {}", result.stderr);
    assert!(result.stdout.contains("USAGE:"));
}

#[tokio::test]
async fn git_status_on_clean_repo() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());

    let (engine, _kernel, _docs) = setup_git(tmp.path()).await;
    let result = engine.execute(&kaish_json(&["status"], &[])).await.unwrap();

    assert!(result.success, "stderr: {}", result.stderr);
    assert!(
        result.stdout.contains("nothing to commit"),
        "expected clean status, got: {}",
        result.stdout
    );
}

#[tokio::test]
async fn git_add_commit_log_with_m_flag_reconstruction() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());

    // Create a file to commit
    std::fs::write(tmp.path().join("hello.txt"), "Hello, kaijutsu!\n").unwrap();

    let (engine, _kernel, _docs) = setup_git(tmp.path()).await;

    // git status should show the new file
    let status = engine.execute(&kaish_json(&["status"], &[])).await.unwrap();
    assert!(status.success);
    assert!(status.stdout.contains("hello.txt"));

    // git add hello.txt
    let add = engine.execute(&kaish_json(&["add", "hello.txt"], &[])).await.unwrap();
    assert!(add.success, "add failed: {}", add.stderr);

    // git commit -m "add hello"
    // kaish splits this into: positional: ["commit", "add hello"], flags: {"m"}
    // EngineArgs::to_argv() must reconstruct: ["commit", "-m", "add hello"]
    let commit_json = kaish_json(&["commit", "add hello"], &["m"]);
    let commit = engine.execute(&commit_json).await.unwrap();
    assert!(commit.success, "commit failed: {}", commit.stderr);
    assert!(
        commit.stdout.contains("add hello"),
        "expected commit message, got: {}",
        commit.stdout
    );

    // git log — verify the commit shows up
    let log = engine.execute(&kaish_json(&["log"], &[])).await.unwrap();
    assert!(log.success);
    assert!(
        log.stdout.contains("add hello"),
        "expected 'add hello' in log, got: {}",
        log.stdout
    );
}

#[tokio::test]
async fn git_diff_cached_flag_reconstruction() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());

    // Create and stage a file
    std::fs::write(tmp.path().join("staged.txt"), "staged content\n").unwrap();

    let (engine, _kernel, _docs) = setup_git(tmp.path()).await;

    // git add staged.txt
    engine
        .execute(&kaish_json(&["add", "staged.txt"], &[]))
        .await
        .unwrap();

    // git diff --cached
    // kaish splits into: positional: ["diff"], flags: {"cached"}
    // EngineArgs::to_argv() must reconstruct: ["diff", "--cached"]
    let diff_json = kaish_json(&["diff"], &["cached"]);
    let diff = engine.execute(&diff_json).await.unwrap();
    assert!(diff.success, "diff failed: {}", diff.stderr);
    assert!(
        diff.stdout.contains("staged content"),
        "expected staged content in diff, got: {}",
        diff.stdout
    );
}

#[tokio::test]
async fn git_log_with_numeric_count_flag() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());

    let (engine, _kernel, _docs) = setup_git(tmp.path()).await;

    // git log -5
    // kaish splits into: positional: ["log"], flags: {"5"}
    // EngineArgs::to_argv() must reconstruct: ["log", "-5"]
    let log_json = kaish_json(&["log"], &["5"]);
    let log = engine.execute(&log_json).await.unwrap();
    assert!(log.success, "log failed: {}", log.stderr);
    // Should show the initial commit
    assert!(
        log.stdout.contains("Initial commit"),
        "expected 'Initial commit' in log, got: {}",
        log.stdout
    );
}

#[tokio::test]
async fn git_status_without_pwd_gives_guidance() {
    // Kernel WITHOUT setting pwd
    let kernel = Arc::new(Kernel::new("e2e-no-pwd").await);
    let documents = shared_block_store("e2e-no-pwd");

    documents
        .create_document("doc-default".to_string(), DocumentKind::Conversation, None)
        .unwrap();

    let engine = Arc::new(GitEngine::new(&kernel, documents.clone(), "default"));

    kernel
        .register_tool_with_engine(
            ToolInfo::new("git", "Context-aware git", "vcs"),
            engine.clone(),
        )
        .await;

    // Register context but don't set pwd
    {
        let mut router = kernel.drift().write().await;
        router.register("default", "doc-default", None);
    }

    let result = engine.execute(&kaish_json(&["status"], &[])).await.unwrap();
    assert!(!result.success, "expected failure without pwd");
    assert!(
        result.stderr.contains("No repository path") || result.stderr.contains("set context pwd"),
        "expected guidance in error, got: {}",
        result.stderr
    );
}

#[tokio::test]
async fn git_diff_staged_alias_flag() {
    let tmp = tempfile::tempdir().unwrap();
    init_test_repo(tmp.path());

    std::fs::write(tmp.path().join("alias.txt"), "alias test\n").unwrap();

    let (engine, _kernel, _docs) = setup_git(tmp.path()).await;

    engine
        .execute(&kaish_json(&["add", "alias.txt"], &[]))
        .await
        .unwrap();

    // git diff --staged (alias for --cached)
    // kaish splits into: positional: ["diff"], flags: {"staged"}
    let diff_json = kaish_json(&["diff"], &["staged"]);
    let diff = engine.execute(&diff_json).await.unwrap();
    assert!(diff.success, "diff --staged failed: {}", diff.stderr);
    assert!(
        diff.stdout.contains("alias test"),
        "expected 'alias test' in diff, got: {}",
        diff.stdout
    );
}

// ============================================================================
// Tier 4: Shell command e2e through EmbeddedKaish
// ============================================================================

/// Test filesystem fixture rooted in a tempdir.
///
/// Builds a realistic filesystem layout so `ls`, `cat`, etc. work reliably
/// regardless of host system state:
///
/// ```text
/// {tempdir}/
/// ├── home/kaiju/
/// │   └── src/kaijutsu/
/// │       ├── Cargo.toml
/// │       └── README.md
/// └── tmp/
/// ```
struct TestFs {
    _tmpdir: tempfile::TempDir,
    root: std::path::PathBuf,
    home: std::path::PathBuf,
    project: std::path::PathBuf,
}

impl TestFs {
    fn new() -> Self {
        let tmpdir = tempfile::tempdir().unwrap();
        let root = tmpdir.path().to_path_buf();

        let home = root.join("home/kaiju");
        let project = home.join("src/kaijutsu");
        let tmp = root.join("tmp");

        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&tmp).unwrap();

        // Seed with known files
        std::fs::write(project.join("Cargo.toml"), "[package]\nname = \"kaijutsu\"\n").unwrap();
        std::fs::write(project.join("README.md"), "# Kaijutsu\n").unwrap();

        Self { _tmpdir: tmpdir, root, home, project }
    }
}

/// Create an EmbeddedKaish with a self-contained test filesystem.
///
/// Mounts the test fixture into the kernel's VFS so `ls`, `cat`, etc. resolve
/// against the tempdir, not the host system.
async fn setup_shell_e2e(fs: &TestFs, project_root: Option<std::path::PathBuf>) -> EmbeddedKaish {
    let kernel = Arc::new(Kernel::new("e2e-shell").await);
    let documents = shared_block_store("e2e-shell");

    documents
        .create_document("doc-default".to_string(), DocumentKind::Conversation, None)
        .unwrap();

    // Mount the test filesystem — mirrors real server setup but rooted in tempdir
    kernel.mount("/", LocalBackend::read_only(&fs.root)).await;
    kernel.mount(
        &format!("{}", fs.home.join("src").display()),
        LocalBackend::new(fs.home.join("src")),
    ).await;
    kernel.mount("/tmp", LocalBackend::new(fs.root.join("tmp"))).await;

    EmbeddedKaish::new("e2e-shell", documents, kernel, project_root)
        .expect("EmbeddedKaish::new failed")
}

#[tokio::test]
async fn test_ls_through_embedded_kaish() {
    let fs = TestFs::new();
    let kaish = setup_shell_e2e(&fs, None).await;
    let result = kaish.execute(&format!("ls {}", fs.project.display())).await.unwrap();

    assert_eq!(result.code, 0, "ls failed: {}", result.err);
    assert!(
        result.out.contains("Cargo.toml"),
        "expected Cargo.toml in ls output, got: {}",
        result.out
    );
}

#[tokio::test]
async fn test_ls_tmp() {
    let fs = TestFs::new();
    // Put a file in /tmp so there's something to see
    std::fs::write(fs.root.join("tmp/scratch.txt"), "temp\n").unwrap();

    let kaish = setup_shell_e2e(&fs, None).await;
    let result = kaish.execute("ls /tmp").await.unwrap();

    assert_eq!(result.code, 0, "ls /tmp failed: {}", result.err);
    assert!(
        result.out.contains("scratch.txt"),
        "expected scratch.txt in ls /tmp output, got: {}",
        result.out
    );
}

#[tokio::test]
async fn test_rapid_shell_commands() {
    let fs = TestFs::new();
    let kaish = setup_shell_e2e(&fs, None).await;

    let r1 = kaish.execute("echo a").await.unwrap();
    let r2 = kaish.execute("echo b").await.unwrap();
    let r3 = kaish.execute("echo c").await.unwrap();

    assert_eq!(r1.code, 0, "echo a failed: {}", r1.err);
    assert_eq!(r2.code, 0, "echo b failed: {}", r2.err);
    assert_eq!(r3.code, 0, "echo c failed: {}", r3.err);

    assert_eq!(r1.out.trim(), "a");
    assert_eq!(r2.out.trim(), "b");
    assert_eq!(r3.out.trim(), "c");
}

#[tokio::test]
async fn test_shell_command_with_project_root() {
    let fs = TestFs::new();
    let kaish = setup_shell_e2e(&fs, Some(fs.project.clone())).await;
    let result = kaish.execute("ls").await.unwrap();

    assert_eq!(result.code, 0, "ls failed: {}", result.err);
    assert!(
        result.out.contains("Cargo.toml"),
        "expected Cargo.toml in ls output (cwd=project), got: {}",
        result.out
    );
    assert!(
        result.out.contains("README.md"),
        "expected README.md in ls output (cwd=project), got: {}",
        result.out
    );
}
