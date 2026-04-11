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
use kaish_kernel::{Kernel as KaishKernel, KernelBackend, KernelConfig as KaishConfig};

use kaijutsu_kernel::Kernel as KaijutsuKernel;
use kaijutsu_kernel::block_store::SharedBlockStore;
use kaijutsu_kernel::kernel_db::KernelDb;
use kaijutsu_types::{ContextId, KernelId, PrincipalId, SessionId};

use crate::docs_filesystem::KaijutsuFilesystem;
use crate::input_filesystem::InputFilesystem;
use crate::kaish_backend::KaijutsuBackend;
use crate::mount_backend::MountBackend;
use crate::context_engine::{SessionContextExt, SessionContextMap};

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
            KernelId::new(),
            crate::context_engine::session_context_map(),
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
        kernel_id: KernelId,
        session_contexts: SessionContextMap,
        configure_tools: impl FnOnce(SessionContextMap, SessionId, &mut kaish_kernel::ToolRegistry),
    ) -> Result<Self> {
        Self::with_identity_and_db(
            name,
            blocks,
            kernel,
            project_root,
            principal_id,
            context_id,
            session_id,
            kernel_id,
            None,
            session_contexts,
            configure_tools,
        )
    }

    /// Like `with_identity`, but accepts a `KernelDb` to restore persisted
    /// session state (cwd) on creation.
    pub fn with_identity_and_db(
        name: &str,
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        project_root: Option<PathBuf>,
        principal_id: PrincipalId,
        context_id: ContextId,
        session_id: SessionId,
        kernel_id: KernelId,
        kernel_db: Option<&Arc<parking_lot::Mutex<KernelDb>>>,
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
        let docs_backend = Arc::new(KaijutsuBackend::new(
            blocks,
            kernel.clone(),
            principal_id,
            session_contexts.clone(),
            session_id,
            kernel_id,
        ));
        let mount_table = kernel.vfs().clone();

        let mount_backend: Arc<dyn KernelBackend> =
            Arc::new(MountBackend::new(mount_table, docs_backend.clone()));

        let docs_fs = Arc::new(KaijutsuFilesystem::new(docs_backend));

        // KaishConfig primarily sets the cwd and kernel name. The VFS mode
        // in the config is secondary to kaijutsu's MountTable — real filesystem
        // access is routed through MountBackend → MountTable → LocalBackend,
        // not through kaish's own VFS modes.
        //
        // `project_root` sets the cwd to a specific project directory (used by
        // MCP sessions that operate on a particular repo). When None, cwd
        // defaults to $HOME via `KaishConfig::named()`, then we check the DB
        // for a persisted cwd from a previous session.
        let mut config = match project_root {
            Some(root) => KaishConfig::mcp_with_root(root),
            None => KaishConfig::named(name),
        };

        // Restore persisted cwd from context_shell if available.
        if let Some(db) = kernel_db {
            let db_guard = db.lock();
            if let Ok(Some(shell)) = db_guard.get_context_shell(context_id) {
                if let Some(cwd) = shell.cwd {
                    let path = PathBuf::from(&cwd);
                    if path.is_dir() {
                        config.cwd = path;
                    }
                }
            }
        }

        let ctx_for_tools = session_contexts.clone();
        let sid_for_tools = session_id;
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
        })
    }

    /// Execute kaish code and return the result.
    pub async fn execute(&self, code: &str) -> Result<ExecResult> {
        self.kernel.execute(code).await
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

    /// Get the kernel name.
    pub fn name(&self) -> &str {
        &self.name
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

    /// Apply persisted context configuration (env vars + init_script).
    ///
    /// Reads `context_env` and `context_shell.init_script` from the database
    /// and applies them to the kaish kernel. Should be called once after
    /// creation for sessions that have persisted configuration.
    pub async fn apply_context_config(
        &self,
        db: &parking_lot::Mutex<KernelDb>,
        context_id: ContextId,
    ) {
        let (env_vars, init_script) = {
            let db_guard = db.lock();
            let env = db_guard.get_context_env(context_id).unwrap_or_default();
            let script = db_guard
                .get_context_shell(context_id)
                .ok()
                .flatten()
                .and_then(|s| s.init_script);
            (env, script)
        };

        // Export env vars so they propagate to child processes.
        for var in &env_vars {
            // Shell-escape value to avoid injection.
            let escaped = var.value.replace('\'', "'\\''");
            if let Err(e) = self
                .kernel
                .execute(&format!("export {}='{}'", var.key, escaped))
                .await
            {
                tracing::warn!(
                    key = %var.key,
                    error = %e,
                    "failed to apply context env var",
                );
            }
        }

        // Execute init_script if present.
        if let Some(script) = init_script {
            if !script.is_empty() {
                if let Err(e) = self.kernel.execute(&script).await {
                    tracing::warn!(
                        error = %e,
                        "failed to execute context init_script",
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_kernel::block_store::shared_block_store;

    #[tokio::test]
    async fn test_embedded_kaish_creation() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new("test-agent").await);

        let kaish = EmbeddedKaish::new("test-kernel", blocks, kernel, None);
        assert!(kaish.is_ok());

        let kaish = kaish.unwrap();
        assert_eq!(kaish.name(), "test-kernel");
    }

    #[tokio::test]
    async fn test_embedded_kaish_variables() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new("test-vars").await);
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
        let kernel = Arc::new(KaijutsuKernel::new("test-cwd-home").await);
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
        let kernel = Arc::new(KaijutsuKernel::new("test-cwd-project").await);
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
        use kaijutsu_kernel::kernel_db::{ContextRow, KernelDb};
        use kaijutsu_types::{ConsentMode, ContextState, KernelId, now_millis};

        let kernel_id = KernelId::new();
        let context_id = ContextId::new();
        let principal = PrincipalId::system();
        let db = KernelDb::in_memory().unwrap();

        let ws_id = db
            .get_or_create_default_workspace(kernel_id, principal)
            .unwrap();

        db.insert_context_with_document(
            &ContextRow {
                context_id,
                kernel_id,
                label: Some("test-env".into()),
                provider: None,
                model: None,
                system_prompt: None,
                tool_filter: None,
                consent_mode: ConsentMode::default(),
                context_state: ContextState::Live,
                forked_from: None,
                fork_kind: None,
                created_by: principal,
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
        let kernel = Arc::new(KaijutsuKernel::new("test-env").await);

        let sid = SessionId::new();
        let session_contexts = crate::context_engine::session_context_map();
        let kaish = EmbeddedKaish::with_identity_and_db(
            "test-env",
            blocks,
            kernel,
            None,
            principal,
            context_id,
            sid,
            kernel_id,
            Some(&kernel_db),
            session_contexts,
            |_, _, _| {},
        )
        .unwrap();

        // Apply context config (env vars + init_script).
        kaish.apply_context_config(&kernel_db, context_id).await;

        // Verify env vars are accessible via kaish execution.
        let result = kaish.execute("echo $KJ_TEST_FOO").await.unwrap();
        assert_eq!(
            result.text_out().trim(),
            "bar_value",
            "KJ_TEST_FOO should be set from context_env",
        );

        let result = kaish.execute("echo $KJ_TEST_NUM").await.unwrap();
        assert_eq!(
            result.text_out().trim(),
            "42",
            "KJ_TEST_NUM should be set from context_env",
        );
    }

    /// init_script stored in context_shell should be executed after
    /// apply_context_config is called on a freshly-created EmbeddedKaish.
    #[tokio::test]
    async fn test_init_script_applied_on_creation() {
        use kaijutsu_kernel::kernel_db::{ContextRow, ContextShellRow, KernelDb};
        use kaijutsu_types::{ConsentMode, ContextState, KernelId, now_millis};

        let kernel_id = KernelId::new();
        let context_id = ContextId::new();
        let principal = PrincipalId::system();
        let db = KernelDb::in_memory().unwrap();

        let ws_id = db
            .get_or_create_default_workspace(kernel_id, principal)
            .unwrap();

        db.insert_context_with_document(
            &ContextRow {
                context_id,
                kernel_id,
                label: Some("test-init".into()),
                provider: None,
                model: None,
                system_prompt: None,
                tool_filter: None,
                consent_mode: ConsentMode::default(),
                context_state: ContextState::Live,
                forked_from: None,
                fork_kind: None,
                created_by: principal,
                created_at: now_millis() as i64,
                archived_at: None,
                workspace_id: None,
                preset_id: None,
            },
            ws_id,
        )
        .unwrap();

        // Store init_script that sets a variable.
        // Note: kaish requires quoting values like "yes" (ambiguous boolean).
        db.upsert_context_shell(&ContextShellRow {
            context_id,
            cwd: None,
            init_script: Some("export INIT_RAN=\"activated\"".into()),
            updated_at: now_millis() as i64,
        })
        .unwrap();

        let kernel_db = Arc::new(parking_lot::Mutex::new(db));
        let blocks = shared_block_store(principal);
        let kernel = Arc::new(KaijutsuKernel::new("test-init").await);

        let sid = SessionId::new();
        let session_contexts = crate::context_engine::session_context_map();
        let kaish = EmbeddedKaish::with_identity_and_db(
            "test-init",
            blocks,
            kernel,
            None,
            principal,
            context_id,
            sid,
            kernel_id,
            Some(&kernel_db),
            session_contexts,
            |_, _, _| {},
        )
        .unwrap();

        // Apply context config.
        kaish.apply_context_config(&kernel_db, context_id).await;

        // Verify init_script was executed.
        let result = kaish.execute("echo $INIT_RAN").await.unwrap();
        assert_eq!(
            result.text_out().trim(),
            "activated",
            "init_script should have set INIT_RAN=activated",
        );
    }

    /// Regression test: when a context has a persisted cwd in KernelDb,
    /// creating an EmbeddedKaish for that context should restore it.
    ///
    /// Before the fix, cwd defaulted to $HOME regardless of what was
    /// persisted — apply_context_config only ran on `kj context switch`,
    /// not on initial session creation.
    #[tokio::test]
    async fn test_persisted_cwd_restored_on_creation() {
        use kaijutsu_kernel::kernel_db::{ContextRow, ContextShellRow, KernelDb};
        use kaijutsu_types::{ConsentMode, ContextState, KernelId, now_millis};

        let tmp = tempfile::tempdir().unwrap();
        let persisted_cwd = tmp.path().to_path_buf();

        // Set up KernelDb with a context that has a persisted cwd
        let kernel_id = KernelId::new();
        let context_id = ContextId::new();
        let principal = PrincipalId::system();
        let db = KernelDb::in_memory().unwrap();

        let ws_id = db
            .get_or_create_default_workspace(kernel_id, principal)
            .unwrap();

        db.insert_context_with_document(
            &ContextRow {
                context_id,
                kernel_id,
                label: Some("test-restore".into()),
                provider: None,
                model: None,
                system_prompt: None,
                tool_filter: None,
                consent_mode: ConsentMode::default(),
                context_state: ContextState::Live,
                forked_from: None,
                fork_kind: None,
                created_by: principal,
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
            cwd: Some(persisted_cwd.to_string_lossy().into_owned()),
            init_script: None,
            updated_at: now_millis() as i64,
        })
        .unwrap();

        // Verify it's really in the DB
        let shell = db.get_context_shell(context_id).unwrap().unwrap();
        assert_eq!(
            shell.cwd.as_deref(),
            Some(persisted_cwd.to_str().unwrap()),
            "precondition: cwd should be in DB"
        );

        let kernel_db = Arc::new(parking_lot::Mutex::new(db));

        // Create EmbeddedKaish the same way rpc.rs does: no project_root,
        // relying on the persisted context_shell.cwd to be restored.
        let blocks = shared_block_store(principal);
        let kernel = Arc::new(KaijutsuKernel::new("test-restore").await);

        let sid = SessionId::new();
        let session_contexts = crate::context_engine::session_context_map();
        let kaish = EmbeddedKaish::with_identity_and_db(
            "test-restore",
            blocks,
            kernel,
            None, // no project_root — same as SSH connection path
            principal,
            context_id,
            sid,
            kernel_id,
            Some(&kernel_db),
            session_contexts,
            |_, _, _| {},
        )
        .unwrap();

        // BUG: without the fix, this will be $HOME instead of the persisted cwd
        let actual_cwd = kaish.cwd().await;
        let actual = actual_cwd
            .canonicalize()
            .unwrap_or_else(|_| actual_cwd.clone());
        let expected = persisted_cwd
            .canonicalize()
            .unwrap_or_else(|_| persisted_cwd.clone());

        assert_eq!(
            actual, expected,
            "cwd should be restored from context_shell on creation, \
             got {:?} (expected {:?})",
            actual_cwd, persisted_cwd,
        );
    }
}
