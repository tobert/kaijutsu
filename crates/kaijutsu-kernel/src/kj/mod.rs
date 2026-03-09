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
pub mod preset;
pub mod refs;
pub mod workspace;

use std::sync::Arc;

use kaijutsu_types::{ContextId, KernelId, PrincipalId, SessionId};

use crate::block_store::SharedBlockStore;
use crate::drift::SharedDriftRouter;
use crate::kernel_db::KernelDb;

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
    pub context_id: ContextId,
    pub session_id: SessionId,
}

// ============================================================================
// KjResult — command output
// ============================================================================

/// Result from any kj subcommand.
#[derive(Debug, Clone)]
pub enum KjResult {
    /// Success — exit 0, stdout content.
    Ok(String),
    /// Error — exit 1, stderr content.
    Err(String),
    /// Context switch — carries the resolved ContextId for the caller to act on.
    /// The dispatcher resolves the target; the caller (KjBuiltin) updates SharedContextId.
    Switch(ContextId, String),
}

impl KjResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, KjResult::Ok(_) | KjResult::Switch(_, _))
    }

    pub fn message(&self) -> &str {
        match self {
            KjResult::Ok(s) | KjResult::Err(s) | KjResult::Switch(_, s) => s,
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
    kernel_db: Arc<std::sync::Mutex<KernelDb>>,
    kernel_id: KernelId,
}

impl KjDispatcher {
    pub fn new(
        drift: SharedDriftRouter,
        blocks: SharedBlockStore,
        kernel_db: Arc<std::sync::Mutex<KernelDb>>,
        kernel_id: KernelId,
    ) -> Self {
        Self {
            drift,
            blocks,
            kernel_db,
            kernel_id,
        }
    }

    /// Dispatch a parsed argv to the appropriate subcommand.
    ///
    /// Expected argv: `["context", "list", "--tree"]` (no leading "kj").
    pub async fn dispatch(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err(self.help());
        }

        match argv[0].as_str() {
            "context" | "ctx" => self.dispatch_context(&argv[1..], caller).await,
            "fork" => self.dispatch_fork(&argv[1..], caller).await,
            "drift" => self.dispatch_drift(&argv[1..], caller).await,
            "preset" => self.dispatch_preset(&argv[1..], caller),
            "workspace" | "ws" => self.dispatch_workspace(&argv[1..], caller),
            "help" | "--help" | "-h" => KjResult::Ok(self.help()),
            other => KjResult::Err(format!("kj: unknown command '{}'\n\n{}", other, self.help())),
        }
    }

    fn help(&self) -> String {
        "\
kj — kernel command interface

USAGE:
    kj <command> [args...]

COMMANDS:
    context (ctx)   Context management (list, info, switch, create)
    fork            Fork the current context
    drift           Cross-context communication (push, flush, queue, cancel)
    preset          Preset templates (list, show)
    workspace (ws)  Workspace management (list, show)
    help            Show this help"
            .to_string()
    }

    // Accessors for subcommand modules
    pub(crate) fn drift_router(&self) -> &SharedDriftRouter {
        &self.drift
    }

    pub(crate) fn block_store(&self) -> &SharedBlockStore {
        &self.blocks
    }

    pub(crate) fn kernel_db(&self) -> &Arc<std::sync::Mutex<KernelDb>> {
        &self.kernel_db
    }

    pub(crate) fn kernel_id(&self) -> KernelId {
        self.kernel_id
    }
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::*;
    use crate::block_store::shared_block_store;
    use crate::drift::shared_drift_router;
    use crate::kernel_db::KernelDb;

    /// Create a KjDispatcher with in-memory state for testing.
    pub fn test_dispatcher() -> KjDispatcher {
        let drift = shared_drift_router();
        let blocks = shared_block_store(PrincipalId::system());
        let kernel_db = Arc::new(std::sync::Mutex::new(
            KernelDb::in_memory().expect("in-memory KernelDb"),
        ));
        let kernel_id = KernelId::new();
        KjDispatcher::new(drift, blocks, kernel_db, kernel_id)
    }

    /// Create a KjCaller with fresh IDs for testing.
    pub fn test_caller() -> KjCaller {
        KjCaller {
            principal_id: PrincipalId::new(),
            context_id: ContextId::new(),
            session_id: SessionId::new(),
        }
    }

    /// Create a caller with a specific context_id.
    pub fn caller_with_context(context_id: ContextId) -> KjCaller {
        KjCaller {
            principal_id: PrincipalId::new(),
            context_id,
            session_id: SessionId::new(),
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

        // Insert into KernelDb
        {
            let db = dispatcher.kernel_db().lock().unwrap();
            let row = crate::kernel_db::ContextRow {
                context_id: id,
                kernel_id,
                label: label.map(|s| s.to_string()),
                provider: None,
                model: None,
                system_prompt: None,
                tool_filter: None,
                consent_mode: kaijutsu_types::ConsentMode::Collaborative,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: created_by,
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
            drift.register(id, label, forked_from, created_by);
        }

        id
    }
}
