//! `kj` command dispatcher — unified command interface for kernel operations.
//!
//! Three modalities, one implementation:
//! - kaish builtin (`kj context list --tree`)
//! - MCP tool (`context_shell("kj context list --tree")`)
//! - Future: standalone CLI binary
//!
//! All commands go through `KjDispatcher`, which holds Arc refs to shared
//! kernel state and is constructed once per server.

pub mod context;
pub mod drift;
pub mod fork;
pub mod format;
pub mod parse;
pub mod preset;
pub mod prompt;
pub mod refs;
pub mod stage;
pub mod workspace;

use std::sync::Arc;

use kaijutsu_types::{ContentType, ContextId, KernelId, PrincipalId, SessionId};

use crate::block_store::SharedBlockStore;
use crate::drift::{DISTILLATION_SYSTEM_PROMPT, SharedDriftRouter, build_distillation_prompt};
use crate::kernel::Kernel;
use crate::kernel_db::KernelDb;
use crate::mcp_pool::McpServerPool;

// ============================================================================
// KjCaller — per-invocation identity
// ============================================================================

/// Per-invocation caller identity.
///
/// Constructed from ExecContext/ToolContext at call time — NOT stored on KjDispatcher.
/// The `.` context reference resolves to `context_id`.
#[derive(Debug, Clone)]
pub struct KjCaller {
    pub principal_id: PrincipalId,
    pub context_id: Option<ContextId>,
    pub session_id: SessionId,
    /// True when the caller has verified a latch nonce (destructive op confirmed).
    pub confirmed: bool,
}

// ============================================================================
// KjResult — command output
// ============================================================================

/// Result from any kj subcommand.
#[derive(Debug, Clone)]
pub enum KjResult {
    /// Success — exit 0, stdout content.
    Ok {
        message: String,
        content_type: ContentType,
        /// When true, the output is for humans only — excluded from LLM context.
        ephemeral: bool,
    },
    /// Error — exit 1, stderr content.
    Err(String),
    /// Context switch — carries the resolved ContextId for the caller to act on.
    /// The dispatcher resolves the target; the caller (KjBuiltin) updates SharedContextId.
    Switch(ContextId, String),
    /// Destructive op needs confirmation. KjBuiltin converts to ExecResult code 2
    /// via kaish's latch/nonce system.
    Latch {
        /// Nonce scope: the kj subcommand path (e.g., "kj context archive").
        command: String,
        /// Nonce scope: the target label/identifier.
        target: String,
        /// Human-readable summary of what will be affected.
        message: String,
    },
}

impl KjResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, KjResult::Ok { .. } | KjResult::Switch(_, _))
    }

    pub fn is_latch(&self) -> bool {
        matches!(self, KjResult::Latch { .. })
    }

    pub fn message(&self) -> &str {
        match self {
            KjResult::Ok { message, .. }
            | KjResult::Err(message)
            | KjResult::Switch(_, message) => message,
            KjResult::Latch { message, .. } => message,
        }
    }

    /// Convenience: create a plain text Ok result.
    pub fn ok(msg: impl Into<String>) -> Self {
        KjResult::Ok {
            message: msg.into(),
            content_type: ContentType::Plain,
            ephemeral: false,
        }
    }

    /// Convenience: create an Ok result with a content type hint.
    pub fn ok_typed(msg: impl Into<String>, ct: ContentType) -> Self {
        KjResult::Ok {
            message: msg.into(),
            content_type: ct,
            ephemeral: false,
        }
    }

    /// Convenience: create an ephemeral Ok result (excluded from LLM hydration).
    pub fn ok_ephemeral(msg: impl Into<String>, ct: ContentType) -> Self {
        KjResult::Ok {
            message: msg.into(),
            content_type: ct,
            ephemeral: true,
        }
    }
}

// ============================================================================
// KjDispatcher — core dispatcher
// ============================================================================

/// Core dispatcher for kj commands.
///
/// Holds Arc refs to shared kernel state. Constructed once per server,
/// shared across all connections.
pub struct KjDispatcher {
    drift: SharedDriftRouter,
    blocks: SharedBlockStore,
    kernel_db: Arc<parking_lot::Mutex<KernelDb>>,
    kernel_id: KernelId,
    kernel: Arc<Kernel>,
    mcp_pool: Option<Arc<McpServerPool>>,
}

impl KjDispatcher {
    pub fn new(
        drift: SharedDriftRouter,
        blocks: SharedBlockStore,
        kernel_db: Arc<parking_lot::Mutex<KernelDb>>,
        kernel_id: KernelId,
        kernel: Arc<Kernel>,
        mcp_pool: Option<Arc<McpServerPool>>,
    ) -> Self {
        Self {
            drift,
            blocks,
            kernel_db,
            kernel_id,
            kernel,
            mcp_pool,
        }
    }

    /// Dispatch a parsed argv to the appropriate subcommand.
    ///
    /// Expected argv: `["context", "list", "--tree"]` (no leading "kj").
    pub async fn dispatch(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err(self.help());
        }

        let cmd = argv[0].as_str();

        // Commands that don't strictly require an active context
        if cmd == "help" || cmd == "--help" || cmd == "-h" {
            return KjResult::ok_ephemeral(self.help(), ContentType::Markdown);
        }

        // Most context/workspace/preset subcommands work without an active context
        if cmd == "context" || cmd == "ctx" {
            return self.dispatch_context(&argv[1..], caller).await;
        }
        if cmd == "workspace" || cmd == "ws" {
            return self.dispatch_workspace(&argv[1..], caller);
        }
        if cmd == "preset" {
            return self.dispatch_preset(&argv[1..], caller);
        }

        // Everything else requires an active context
        if caller.context_id.is_none() {
            return KjResult::Err("no active context joined. Use 'kj context switch <label>' to join one.".to_string());
        }

        match cmd {
            "fork" => self.dispatch_fork(&argv[1..], caller).await,
            "stage" => self.dispatch_stage(&argv[1..], caller).await,
            "drift" => self.dispatch_drift(&argv[1..], caller).await,
            "prompt" => self.dispatch_prompt(&argv[1..], caller).await,
            "preset" => self.dispatch_preset(&argv[1..], caller),
            "workspace" | "ws" => self.dispatch_workspace(&argv[1..], caller),
            other => KjResult::Err(format!(
                "kj: unknown command '{}'\n\n{}",
                other,
                self.help()
            )),
        }
    }

    fn help(&self) -> String {
        include_str!("../../docs/help/kj.md").to_string()
    }

    // Accessors for subcommand modules
    pub(crate) fn drift_router(&self) -> &SharedDriftRouter {
        &self.drift
    }

    pub(crate) fn block_store(&self) -> &SharedBlockStore {
        &self.blocks
    }

    pub fn kernel_db(&self) -> &Arc<parking_lot::Mutex<KernelDb>> {
        &self.kernel_db
    }

    pub fn kernel_id(&self) -> KernelId {
        self.kernel_id
    }

    pub(crate) fn kernel(&self) -> &Arc<Kernel> {
        &self.kernel
    }

    pub(crate) fn mcp_pool(&self) -> Option<&Arc<McpServerPool>> {
        self.mcp_pool.as_ref()
    }

    /// Summarize a context's blocks via LLM.
    ///
    /// Used by `fork --compact`, `drift pull`, and `drift merge`.
    /// Resolves the model from the context's DriftRouter entry, falling back
    /// to the registry default.
    pub(crate) async fn summarize(
        &self,
        context_id: ContextId,
        directed_prompt: Option<&str>,
    ) -> Result<String, String> {
        let blocks = self
            .blocks
            .block_snapshots(context_id)
            .map_err(|e| e.to_string())?;
        if blocks.is_empty() {
            return Err("context has no blocks to summarize".into());
        }

        let user_prompt = build_distillation_prompt(&blocks, directed_prompt);

        let model_name = {
            let router = self.drift.read().await;
            router.get(context_id).and_then(|h| h.model.clone())
        };
        let registry = self.kernel.llm().read().await;

        let (provider, model) = match &model_name {
            Some(m) => registry
                .resolve_model(m)
                .ok_or_else(|| format!("model '{}' not found in registry", m))?,
            None => {
                let p = registry.default_provider().ok_or("no LLM configured")?;
                let m = registry
                    .default_model()
                    .ok_or("no default model configured")?
                    .to_string();
                (p, m)
            }
        };

        provider
            .prompt_with_system(&model, Some(DISTILLATION_SYSTEM_PROMPT), &user_prompt)
            .await
            .map_err(|e| format!("LLM summarization failed: {e}"))
    }
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::*;
    use crate::block_store::shared_block_store;
    use crate::drift::shared_drift_router;
    use crate::kernel_db::KernelDb;

    /// Create a KjDispatcher with in-memory state for testing.
    ///
    /// Must be called from an async context (e.g., `#[tokio::test]`).
    pub async fn test_dispatcher() -> KjDispatcher {
        let drift = shared_drift_router();
        let blocks = shared_block_store(PrincipalId::system());
        let kernel_db = Arc::new(parking_lot::Mutex::new(
            KernelDb::in_memory().expect("in-memory KernelDb"),
        ));
        let kernel_id = KernelId::new();
        // Create default workspace for test contexts
        {
            let db = kernel_db.lock();
            db.get_or_create_default_workspace(kernel_id, PrincipalId::system())
                .unwrap();
        }
        let kernel = Arc::new(Kernel::new("test").await);
        KjDispatcher::new(drift, blocks, kernel_db, kernel_id, kernel, None)
    }

    /// Create a KjCaller with fresh IDs for testing.
    pub fn test_caller() -> KjCaller {
        KjCaller {
            principal_id: PrincipalId::new(),
            context_id: Some(ContextId::new()),
            session_id: SessionId::new(),
            confirmed: false,
        }
    }

    /// Create a caller with a specific context_id.
    pub fn caller_with_context(context_id: ContextId) -> KjCaller {
        KjCaller {
            principal_id: PrincipalId::new(),
            context_id: Some(context_id),
            session_id: SessionId::new(),
            confirmed: false,
        }
    }

    /// Create a confirmed caller (for testing destructive ops post-latch).
    pub fn confirmed_caller(context_id: ContextId) -> KjCaller {
        KjCaller {
            principal_id: PrincipalId::new(),
            context_id: Some(context_id),
            session_id: SessionId::new(),
            confirmed: true,
        }
    }

    /// Register a context in both KernelDb and DriftRouter.
    pub async fn register_context(
        dispatcher: &KjDispatcher,
        label: Option<&str>,
        forked_from: Option<ContextId>,
        created_by: PrincipalId,
    ) -> ContextId {
        let id = ContextId::new();
        let kernel_id = dispatcher.kernel_id();

        // Insert document + context into KernelDb
        {
            let db = dispatcher.kernel_db().lock();
            let ws_id = db
                .get_or_create_default_workspace(kernel_id, created_by)
                .unwrap();

            // Document row first (contexts FK to documents)
            db.insert_document(&crate::kernel_db::DocumentRow {
                document_id: id,
                kernel_id,
                workspace_id: ws_id,
                doc_kind: kaijutsu_types::DocKind::Conversation,
                language: None,
                path: None,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by,
            })
            .unwrap();

            let row = crate::kernel_db::ContextRow {
                context_id: id,
                kernel_id,
                label: label.map(|s| s.to_string()),
                provider: None,
                model: None,
                system_prompt: None,
                tool_filter: None,
                consent_mode: kaijutsu_types::ConsentMode::Collaborative,
                context_state: kaijutsu_types::ContextState::Live,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by,
                forked_from,
                fork_kind: forked_from.map(|_| kaijutsu_types::ForkKind::Full),
                archived_at: None,
                workspace_id: None,
                preset_id: None,
            };
            db.insert_context(&row).unwrap();
        }

        // Register in DriftRouter
        {
            let mut drift = dispatcher.drift_router().write().await;
            drift.register(id, label, forked_from, created_by).unwrap();
        }

        id
    }
}
