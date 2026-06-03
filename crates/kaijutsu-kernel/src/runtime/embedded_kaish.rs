//! Embedded kaish executor using MountBackend + VFS adapters.
//!
//! Instead of spawning kaish as a subprocess, this module embeds the kaish
//! interpreter directly, routing I/O through the kaijutsu kernel's MountTable
//! for real filesystem access and VFS adapters for CRDT blocks.
//!
//! # Architecture
//!
//! ```text
//! kaijutsu-server
//!     │
//!     └── EmbeddedKaish
//!             │
//!             ├── kaish::Kernel (in-process)
//!             │       │
//!             │       ├── /v/docs → KaijutsuFilesystem (CRDT blocks)
//!             │       ├── /v/jobs, /v/blobs → kaish builtins
//!             │       └── everything else → MountBackend
//!             │               │
//!             │               ├── File ops → MountTable → LocalBackend
//!             │               └── Tool calls → KaijutsuBackend
//!             │
//!             └── Shared state with kaijutsu kernel
//! ```
//!
//! This enables kaish scripts to access both CRDT blocks and real files,
//! with tool dispatch routed through the kernel's tool registry.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::{
    ExecuteOptions, Kernel as KaishKernel, KernelBackend, KernelConfig as KaishConfig,
};

use crate::Kernel as KaijutsuKernel;
use crate::block_store::SharedBlockStore;
use crate::kernel_db::KernelDb;
use kaijutsu_types::{ContextId, PrincipalId, SessionId};

use super::docs_filesystem::KaijutsuFilesystem;
use super::input_filesystem::InputFilesystem;
use super::kaish_backend::KaijutsuBackend;
use super::mount_backend::MountBackend;
use super::context_engine::{SessionContextExt, SessionContextMap};

/// Embedded kaish executor backed by CRDT blocks.
///
/// Embeds the kaish interpreter directly and routes all I/O through
/// `KaijutsuBackend`.
pub struct EmbeddedKaish {
    /// The embedded kaish kernel.
    kernel: KaishKernel,
    /// Kernel name/id.
    name: String,
    /// Global session map for context tracking.
    session_contexts: SessionContextMap,
    session_id: SessionId,
    /// Snapshot of the kaijutsu kernel's `TimeoutPolicy` at construction.
    /// Read by `apply_context_config` for the init-script bound; read by
    /// the wrapper accessor `timeouts()` for callers that build their own
    /// `ExecuteOptions` (e.g. `KjDispatcher::run_kai_script`).
    timeouts: kaijutsu_types::TimeoutPolicy,
}

impl EmbeddedKaish {
    /// Create a new embedded kaish executor with default identity.
    ///
    /// Uses `PrincipalId::system()` and a fresh `ContextId`. For real connections,
    /// prefer `with_identity` which accepts the actual connection identity.
    pub fn new(
        name: &str,
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        project_root: Option<PathBuf>,
    ) -> Result<Self> {
        Self::with_identity(
            name,
            blocks,
            kernel,
            project_root,
            PrincipalId::system(),
            ContextId::new(),
            SessionId::new(),
            crate::runtime::context_engine::session_context_map(),
            |_, _, _| {},
        )
    }

    /// Create an embedded kaish executor with explicit identity fields.
    ///
    /// Identity flows through to `ToolContext` for drift/whoami engines.
    /// The `context_id` is tracked via the shared `SessionContextMap`.
    ///
    /// The `configure_tools` callback receives the map and session ID so callers
    /// can register tools (like KjBuiltin) that need context awareness.
    pub fn with_identity(
        name: &str,
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        project_root: Option<PathBuf>,
        principal_id: PrincipalId,
        context_id: ContextId,
        session_id: SessionId,
                session_contexts: SessionContextMap,
        configure_tools: impl FnOnce(SessionContextMap, SessionId, &mut kaish_kernel::ToolRegistry),
    ) -> Result<Self> {
        // Initialize session map entry if missing
        session_contexts.entry(session_id).or_insert(context_id);

        let input_fs = Arc::new(InputFilesystem::new(
            blocks.clone(),
            session_contexts.clone(),
            session_id,
        ));
        // The shared CRDT file cache: same instance the MCP file tools use
        // (installed at server startup), or lazily built from this block store
        // in embedded/test paths. Routing MountBackend through it is the whole
        // point of kaish — shell scripting on the same CRDT substrate.
        let file_cache = kernel.file_cache(&blocks);
        let docs_backend = Arc::new(KaijutsuBackend::new(
            blocks,
            kernel.clone(),
            principal_id,
            session_contexts.clone(),
            session_id,
                    ));
        let mount_table = kernel.vfs().clone();

        let mount_backend: Arc<dyn KernelBackend> = Arc::new(MountBackend::new(
            mount_table,
            docs_backend.clone(),
            file_cache,
        ));

        let docs_fs = Arc::new(KaijutsuFilesystem::new(docs_backend));

        // KaishConfig primarily sets the cwd and kernel name. The VFS mode
        // in the config is secondary to kaijutsu's MountTable — real filesystem
        // access is routed through MountBackend → MountTable → LocalBackend,
        // not through kaish's own VFS modes.
        //
        // `project_root` sets the cwd to a specific project directory (used by
        // MCP sessions that operate on a particular repo). When None, cwd
        // defaults to $HOME via `KaishConfig::named()`. The context's persisted
        // cwd (`context_shell.cwd`) is *not* restored here: it must be validated
        // against the shell's backend (the VFS namespace `cd` uses), which is
        // async, so `restore_cwd_from_db` does it post-construction — see
        // `materialize_context_kaish`.
        let mut config = match project_root {
            Some(root) => KaishConfig::mcp_with_root(root),
            None => KaishConfig::named(name),
        };

        // Apply kernel-wide kaish-script default timeout. Per-call sites
        // (rc lifecycle, hook bodies, init scripts) can override via
        // `ExecuteOptions::with_timeout` for stricter per-context bounds.
        config.request_timeout = Some(kernel.timeouts().kaish_request_timeout);

        let ctx_for_tools = session_contexts.clone();
        let sid_for_tools = session_id;
        let timeouts = kernel.timeouts().clone();
        let kaish_kernel = KaishKernel::with_backend(
            mount_backend,
            config,
            |vfs| {
                vfs.mount_arc("/v/docs", docs_fs);
                vfs.mount_arc("/v/input", input_fs);
            },
            |tools| {
                configure_tools(ctx_for_tools, sid_for_tools, tools);
            },
        )?;

        Ok(Self {
            kernel: kaish_kernel,
            name: name.to_string(),
            session_contexts,
            session_id,
            timeouts,
        })
    }

    /// Execute kaish code with the given options.
    ///
    /// Single canonical entry point: `ExecuteOptions` carries the per-call
    /// vars overlay, timeout, and external cancellation token. With no
    /// options (`ExecuteOptions::default()`), the kaish kernel falls back to
    /// the kernel-wide `request_timeout` set by this factory from
    /// `Kernel::timeouts().kaish_request_timeout`.
    pub async fn execute_with_options(
        &self,
        code: &str,
        opts: ExecuteOptions,
    ) -> Result<ExecResult> {
        self.kernel.execute_with_options(code, opts).await
    }

    /// Get a variable value.
    pub async fn get_var(&self, name: &str) -> Option<kaish_kernel::ast::Value> {
        self.kernel.get_var(name).await
    }

    /// Set a variable value.
    pub async fn set_var(&self, name: &str, value: kaish_kernel::ast::Value) {
        self.kernel.set_var(name, value).await
    }

    /// List all variable names.
    pub async fn list_vars(&self) -> Vec<String> {
        self.kernel
            .list_vars()
            .await
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// Snapshot the shell's exported (env) variables as `(name, value)` string
    /// pairs, coerced with the same `value_to_string` a child process sees.
    /// Used to diff a command's effect on durable `context_env`.
    pub async fn exported_vars(&self) -> Vec<(String, String)> {
        self.kernel
            .exported_vars()
            .await
            .into_iter()
            .map(|(name, value)| {
                (name, kaish_kernel::interpreter::value_to_string(&value))
            })
            .collect()
    }

    /// Get the kernel name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Snapshot of the kaijutsu kernel's `TimeoutPolicy` taken at this
    /// `EmbeddedKaish`'s construction. Callers that build per-call
    /// `ExecuteOptions` (rc lifecycle, hook bodies) read their bound from here.
    pub fn timeouts(&self) -> &kaijutsu_types::TimeoutPolicy {
        &self.timeouts
    }

    /// Update the context ID (e.g., after a context switch).
    ///
    /// Propagates to `KaijutsuBackend` via the shared map.
    pub fn set_context_id(&self, id: ContextId) {
        self.session_contexts.insert(self.session_id, id);
    }

    /// Read the current context ID. Returns None if none active.
    pub fn context_id(&self) -> Option<ContextId> {
        self.session_contexts.current(&self.session_id)
    }

    /// Get current working directory.
    pub async fn cwd(&self) -> std::path::PathBuf {
        self.kernel.cwd().await
    }

    /// Set current working directory.
    pub async fn set_cwd(&self, path: std::path::PathBuf) {
        self.kernel.set_cwd(path).await
    }

    /// Set cwd only if `path` resolves to a directory in the shell's backend
    /// (the VFS namespace `cd` validates against). Returns whether it changed.
    pub async fn try_set_cwd(&self, path: std::path::PathBuf) -> bool {
        self.kernel.try_set_cwd(path).await
    }

    /// Restore this context's persisted cwd (`context_shell.cwd`) into the
    /// shell, validating it against the shell's backend — the same namespace
    /// `cd` resolves against, *not* the host filesystem. Returns:
    ///   - `Ok(None)` — nothing persisted; the shell keeps its default cwd.
    ///   - `Ok(Some(path))` — the persisted cwd was restored.
    ///   - `Err(path)` — a cwd was persisted but no longer resolves to a
    ///     directory; the shell keeps its default. Callers should surface this
    ///     rather than swallow it (it would otherwise be a silent fallback).
    pub async fn restore_cwd_from_db(
        &self,
        kernel_db: &Arc<parking_lot::Mutex<KernelDb>>,
        context_id: ContextId,
    ) -> Result<Option<std::path::PathBuf>, std::path::PathBuf> {
        let persisted = {
            let db = kernel_db.lock();
            db.get_context_shell(context_id)
                .ok()
                .flatten()
                .and_then(|row| row.cwd)
        };
        let Some(cwd) = persisted else {
            return Ok(None);
        };
        let path = std::path::PathBuf::from(cwd);
        if self.try_set_cwd(path.clone()).await {
            Ok(Some(path))
        } else {
            Err(path)
        }
    }

    /// Get the last execution result ($?).
    pub async fn last_result(&self) -> Option<ExecResult> {
        Some(self.kernel.last_result().await)
    }

    /// Cancel all running kaish execution (best-effort).
    ///
    /// Signals the kaish cancellation token, which causes any active
    /// `execute()` or `execute_streaming()` call to abort at its next
    /// yield point. Background jobs within the same session are also
    /// terminated when their containing pipeline is cancelled.
    pub fn cancel(&self) {
        self.kernel.cancel();
    }

    /// Seed the shell with the context's durable env vars (`context_env`).
    ///
    /// The context shell is shared state that evolves over the context's
    /// lifetime; its durable identity is `env + cwd` in the DB. cwd is restored
    /// post-construction by `restore_cwd_from_db`; this applies the env half.
    /// Context-setup *scripting* is RC's job now (the former
    /// `context_shell.init_script` was a leftover and has been folded into the
    /// rc lifecycle), so this no longer runs any script.
    pub async fn apply_context_config(
        &self,
        db: &parking_lot::Mutex<KernelDb>,
        context_id: ContextId,
    ) {
        let env_vars = {
            let db_guard = db.lock();
            db_guard.get_context_env(context_id).unwrap_or_default()
        };

        // Export env vars so they propagate to child processes. Uses the
        // kernel-wide kaish default timeout — exports are tiny, no override.
        for var in &env_vars {
            // Shell-escape value to avoid injection.
            let escaped = var.value.replace('\'', "'\\''");
            if let Err(e) = self
                .execute_with_options(
                    &format!("export {}='{}'", var.key, escaped),
                    ExecuteOptions::default(),
                )
                .await
            {
                tracing::warn!(
                    key = %var.key,
                    error = %e,
                    "failed to apply context env var",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store;

    #[tokio::test]
    async fn test_embedded_kaish_creation() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new("test-agent", None).await);

        let kaish = EmbeddedKaish::new("test-kernel", blocks, kernel, None);
        assert!(kaish.is_ok());

        let kaish = kaish.unwrap();
        assert_eq!(kaish.name(), "test-kernel");
    }

    #[tokio::test]
    async fn test_embedded_kaish_variables() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new("test-vars", None).await);
        let kaish = EmbeddedKaish::new("test-vars", blocks, kernel, None).unwrap();

        // Set and get a variable
        kaish
            .set_var("X", kaish_kernel::ast::Value::String("hello".into()))
            .await;
        let val = kaish.get_var("X").await;
        assert!(val.is_some());

        match val.unwrap() {
            kaish_kernel::ast::Value::String(s) => assert_eq!(s, "hello"),
            _ => panic!("Expected String value"),
        }
    }

    #[tokio::test]
    async fn test_named_config_cwd_is_home() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new("test-cwd-home", None).await);
        let kaish = EmbeddedKaish::new("test-cwd-home", blocks, kernel, None).unwrap();

        let cwd = kaish.cwd().await;
        // KaishConfig::named() sets cwd to home_dir(). We can't control HOME
        // in parallel tests, so just verify it's a real existing directory.
        assert!(
            cwd.is_dir(),
            "cwd should be an existing directory, got {:?}",
            cwd
        );
        assert!(cwd.is_absolute(), "cwd should be absolute, got {:?}", cwd);
    }

    #[tokio::test]
    async fn test_mcp_config_cwd_is_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new("test-cwd-project", None).await);
        let kaish = EmbeddedKaish::new(
            "test-cwd-project",
            blocks,
            kernel,
            Some(tmp.path().to_path_buf()),
        )
        .unwrap();

        let cwd = kaish.cwd().await;
        // Canonicalize both to handle symlinks (e.g., /tmp → /private/tmp on macOS)
        let expected = tmp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| tmp.path().to_path_buf());
        let actual = cwd.canonicalize().unwrap_or(cwd.clone());
        assert_eq!(actual, expected, "cwd should be project root");
    }

    /// Context env vars stored in KernelDb should be available after
    /// apply_context_config is called on a freshly-created EmbeddedKaish.
    #[tokio::test]
    async fn test_context_env_applied_on_creation() {
        use crate::kernel_db::{ContextRow, KernelDb};
        use kaijutsu_types::{ConsentMode, ContextState, now_millis};

        let context_id = ContextId::new();
        let principal = PrincipalId::system();
        let db = KernelDb::in_memory().unwrap();

        let ws_id = db
            .get_or_create_default_workspace(principal)
            .unwrap();

        db.insert_context_with_document(
            &ContextRow {
                context_id,
                                label: Some("test-env".into()),
                provider: None,
                model: None,
                system_prompt: None,
                consent_mode: ConsentMode::default(),
                context_state: ContextState::Live,
                forked_from: None,
                fork_kind: None,
                created_by: principal,
                context_type: "default".to_string(),
                created_at: now_millis() as i64,
                archived_at: None,
                workspace_id: None,
                preset_id: None,
            },
            ws_id,
        )
        .unwrap();

        // Store env vars in DB.
        db.set_context_env(context_id, "KJ_TEST_FOO", "bar_value")
            .unwrap();
        db.set_context_env(context_id, "KJ_TEST_NUM", "42")
            .unwrap();

        let kernel_db = Arc::new(parking_lot::Mutex::new(db));
        let blocks = shared_block_store(principal);
        let kernel = Arc::new(KaijutsuKernel::new("test-env", None).await);

        let sid = SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        let kaish = EmbeddedKaish::with_identity(
            "test-env",
            blocks,
            kernel,
            None,
            principal,
            context_id,
            sid,
            session_contexts,
            |_, _, _| {},
        )
        .unwrap();

        // Apply context config (durable env vars).
        kaish.apply_context_config(&kernel_db, context_id).await;

        // Verify env vars are accessible via kaish execution.
        let result = kaish
            .execute_with_options("echo $KJ_TEST_FOO", ExecuteOptions::default())
            .await
            .unwrap();
        assert_eq!(
            result.text_out().trim(),
            "bar_value",
            "KJ_TEST_FOO should be set from context_env",
        );

        let result = kaish
            .execute_with_options("echo $KJ_TEST_NUM", ExecuteOptions::default())
            .await
            .unwrap();
        assert_eq!(
            result.text_out().trim(),
            "42",
            "KJ_TEST_NUM should be set from context_env",
        );
    }

    /// Regression test: a persisted cwd that is a directory in the shell's
    /// *backend* (a VFS mount) but does NOT exist on the host filesystem must
    /// still restore. The old restore gated on host-FS `PathBuf::is_dir()` and
    /// would silently drop it; `restore_cwd_from_db` validates against the same
    /// backend `cd` resolves against.
    #[tokio::test]
    async fn test_persisted_vfs_cwd_restored_against_backend() {
        use crate::kernel_db::{ContextRow, ContextShellRow, KernelDb};
        use crate::vfs::{MemoryBackend, VfsOps};
        use kaijutsu_types::{ConsentMode, ContextState, now_millis};
        use std::path::Path;

        // A VFS-only path: lives in the MemoryBackend mount below, never on disk.
        let vfs_cwd = "/scratch/work";
        assert!(
            !Path::new(vfs_cwd).is_dir(),
            "precondition: cwd must not exist on the host filesystem"
        );

        let context_id = ContextId::new();
        let principal = PrincipalId::system();
        let db = KernelDb::in_memory().unwrap();
        let ws_id = db.get_or_create_default_workspace(principal).unwrap();
        db.insert_context_with_document(
            &ContextRow {
                context_id,
                label: Some("test-restore-vfs".into()),
                provider: None,
                model: None,
                system_prompt: None,
                consent_mode: ConsentMode::default(),
                context_state: ContextState::Live,
                forked_from: None,
                fork_kind: None,
                created_by: principal,
                context_type: "default".to_string(),
                created_at: now_millis() as i64,
                archived_at: None,
                workspace_id: None,
                preset_id: None,
            },
            ws_id,
        )
        .unwrap();
        db.upsert_context_shell(&ContextShellRow {
            context_id,
            cwd: Some(vfs_cwd.to_string()),
            updated_at: now_millis() as i64,
        })
        .unwrap();

        let kernel_db = Arc::new(parking_lot::Mutex::new(db));
        let blocks = shared_block_store(principal);
        let kernel = Arc::new(KaijutsuKernel::new("test-restore-vfs", None).await);

        // Mount an in-memory FS and create the dir there — pure VFS, no host path.
        kernel.mount("/scratch", MemoryBackend::new()).await;
        kernel
            .vfs()
            .mkdir(Path::new(vfs_cwd), 0o755)
            .await
            .expect("mkdir in VFS mount");

        let sid = SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        let kaish = EmbeddedKaish::with_identity(
            "test-restore-vfs",
            blocks,
            kernel,
            None,
            principal,
            context_id,
            sid,
            session_contexts,
            |_, _, _| {},
        )
        .unwrap();

        let restored = kaish
            .restore_cwd_from_db(&kernel_db, context_id)
            .await
            .expect("VFS cwd should restore via backend, not be rejected by a host-FS check");
        assert_eq!(restored.as_deref(), Some(Path::new(vfs_cwd)));
        assert_eq!(
            kaish.cwd().await,
            std::path::PathBuf::from(vfs_cwd),
            "shell cwd should be the restored VFS path"
        );
    }
}
