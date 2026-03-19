//! Workspace permission guard for file tool engines.
//!
//! Checks whether a file path is allowed by the caller's workspace binding.
//! Unbound contexts (no workspace) are unrestricted — kernel perimeter defaults apply.

use std::sync::Arc;
use parking_lot::Mutex;

use crate::kernel_db::{KernelDb, KernelDbError};
use crate::tools::{ExecResult, ToolContext};

/// Shared workspace permission checker for file tool engines.
#[derive(Clone)]
pub struct WorkspaceGuard {
    db: Arc<Mutex<KernelDb>>,
}

impl WorkspaceGuard {
    pub fn new(db: Arc<Mutex<KernelDb>>) -> Self {
        Self { db }
    }

    /// Check if a read operation is allowed on this path for the caller's context.
    /// Returns Ok(()) if allowed, or an ExecResult::failure if denied.
    pub fn check_read(&self, ctx: &ToolContext, path: &str) -> Result<(), ExecResult> {
        let db = self.db.lock();
        match db.check_workspace_path(ctx.context_id, path) {
            Ok(None) => Ok(()),       // unbound context — no restriction
            Ok(Some(_)) => Ok(()),    // in scope (ro or rw both allow reads)
            Err(KernelDbError::Validation(msg)) => {
                Err(ExecResult::failure(1, format!("workspace: {msg}")))
            }
            Err(KernelDbError::NotFound(_)) => Ok(()), // context not in DB (e.g. file-derived ID)
            Err(e) => {
                tracing::warn!("workspace check failed: {e}");
                Ok(()) // fail open on DB errors — don't block on transient issues
            }
        }
    }

    /// Check if a write operation is allowed on this path for the caller's context.
    /// Returns Ok(()) if allowed, or an ExecResult::failure if denied.
    pub fn check_write(&self, ctx: &ToolContext, path: &str) -> Result<(), ExecResult> {
        let db = self.db.lock();
        match db.check_workspace_path(ctx.context_id, path) {
            Ok(None) => Ok(()),        // unbound context — no restriction
            Ok(Some(false)) => Ok(()), // in scope, read-write
            Ok(Some(true)) => {
                Err(ExecResult::failure(1, format!(
                    "workspace: path '{}' is read-only",
                    path,
                )))
            }
            Err(KernelDbError::Validation(msg)) => {
                Err(ExecResult::failure(1, format!("workspace: {msg}")))
            }
            Err(KernelDbError::NotFound(_)) => Ok(()),
            Err(e) => {
                tracing::warn!("workspace check failed: {e}");
                Ok(())
            }
        }
    }
}
