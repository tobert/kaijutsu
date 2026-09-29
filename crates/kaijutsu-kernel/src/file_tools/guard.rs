//! Workspace permission guard for file tool engines.
//!
//! Checks whether a file path is allowed by the caller's workspace binding.
//! Unbound contexts (no workspace) are unrestricted — kernel perimeter defaults apply.

use parking_lot::Mutex;
use std::sync::Arc;

use crate::kernel_db::{KernelDb, KernelDbError};
use crate::execution::{ExecContext, ExecResult};

/// Shared workspace permission checker for file tool engines.
#[derive(Clone)]
pub struct WorkspaceGuard {
    db: Arc<Mutex<KernelDb>>,
}

impl WorkspaceGuard {
    pub fn new(db: Arc<Mutex<KernelDb>>) -> Self {
        Self { db }
    }

    /// A database fault refuses the operation: without the workspace the
    /// guard cannot tell a read-only path from a writable one.
    fn fault(e: KernelDbError) -> ExecResult {
        tracing::warn!("workspace check failed: {e}");
        ExecResult::failure(1, format!("workspace check failed: {e}"))
    }

    /// Check if a read operation is allowed on this path for the caller's context.
    /// Returns Ok(()) if allowed, or an ExecResult::failure if denied.
    pub fn check_read(&self, ctx: &ExecContext, path: &str) -> Result<(), ExecResult> {
        let db = self.db.lock();
        match db.check_workspace_path(ctx.context_id, path) {
            Ok(None) => Ok(()),    // unbound context — no restriction
            Ok(Some(_)) => Ok(()), // in scope (ro or rw both allow reads)
            Err(KernelDbError::Validation(msg)) => {
                Err(ExecResult::failure(1, format!("workspace: {msg}")))
            }
            Err(KernelDbError::NotFound(_)) => Ok(()), // context not in DB (e.g. file-derived ID)
            Err(e) => Err(Self::fault(e)),
        }
    }

    /// Check if a write operation is allowed on this path for the caller's context.
    /// Returns Ok(()) if allowed, or an ExecResult::failure if denied.
    pub fn check_write(&self, ctx: &ExecContext, path: &str) -> Result<(), ExecResult> {
        let db = self.db.lock();
        match db.check_workspace_path(ctx.context_id, path) {
            Ok(None) => Ok(()),        // unbound context — no restriction
            Ok(Some(false)) => Ok(()), // in scope, read-write
            Ok(Some(true)) => Err(ExecResult::failure(
                1,
                format!("workspace: path '{}' is read-only", path,),
            )),
            Err(KernelDbError::Validation(msg)) => {
                Err(ExecResult::failure(1, format!("workspace: {msg}")))
            }
            Err(KernelDbError::NotFound(_)) => Ok(()),
            Err(e) => Err(Self::fault(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{ContextId, KernelId, PrincipalId, SessionId};

    /// A database fault refuses the file operation instead of waving it
    /// through: the guard cannot tell a read-only path from a writable one
    /// when it cannot read the workspace. Falsified by failing open.
    #[test]
    fn a_database_fault_refuses_reads_and_writes() {
        let db = KernelDb::temporary().unwrap();
        db.conn_for_ledger().execute_batch("ALTER TABLE contexts RENAME TO contexts_gone").unwrap();
        let guard = WorkspaceGuard::new(Arc::new(Mutex::new(db)));
        let ctx = ExecContext::new(PrincipalId::new(), ContextId::new(), "/", SessionId::new(), KernelId::new());

        let write = guard.check_write(&ctx, "/home/user/.bashrc").expect_err("a fault must refuse the write");
        assert!(write.stderr.contains("workspace check failed"), "{}", write.stderr);
        let read = guard.check_read(&ctx, "/home/user/.bashrc").expect_err("a fault must refuse the read");
        assert!(read.stderr.contains("workspace check failed"), "{}", read.stderr);
    }
}
