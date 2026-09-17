//! Durable cwd and exported environment for contextual command execution.

use std::sync::Arc;
use crate::kernel_db::{ContextShellRow, KernelDb};
use crate::runtime::embedded_kaish::EmbeddedKaish;
use kaijutsu_types::ContextId;

/// The durable surface of a context shell: working directory + exported env.
/// Snapshotted before and after a command so we can persist exactly what the
/// command changed. Local shell variables are not persisted.
pub struct ShellStateSnapshot {
    cwd: std::path::PathBuf,
    env: std::collections::BTreeMap<String, String>,
}

pub async fn snapshot_shell_state(kaish: &EmbeddedKaish) -> ShellStateSnapshot {
    ShellStateSnapshot {
        cwd: kaish.cwd().await,
        env: kaish.exported_vars().await.into_iter().collect(),
    }
}

/// Commit changed cwd and exports together. Unchanged values are left alone,
/// preserving concurrent edits that this command did not touch. Failure rolls
/// back the entire change and must be reported before command completion.
/// Skip write-back when execution switched contexts; those snapshots refer
/// to different contexts and cannot be compared.
pub fn persist_shell_state(
    kernel_db: &Arc<parking_lot::Mutex<KernelDb>>,
    context_id: ContextId,
    before: &ShellStateSnapshot,
    after: &ShellStateSnapshot,
) -> Result<(), String> {
    validate_cwd(Some(&after.cwd))?;
    let db = kernel_db.lock();
    let transaction = db.conn_for_ledger().unchecked_transaction()
        .map_err(|e| format!("begin shell state write: {e}"))?;
    if after.cwd != before.cwd {
        db.upsert_context_shell(&ContextShellRow {
            context_id,
            cwd: Some(after.cwd.to_string_lossy().into_owned()),
            updated_at: kaijutsu_types::now_millis() as i64,
        }).map_err(|e| format!("persist context cwd: {e}"))?;
    }
    for (key, value) in &after.env {
        if before.env.get(key) != Some(value) {
            db.set_context_env(context_id, key, value)
                .map_err(|e| format!("persist context env {key}: {e}"))?;
        }
    }
    for key in before.env.keys() {
        if !after.env.contains_key(key) {
            db.delete_context_env(context_id, key)
                .map_err(|e| format!("delete context env {key}: {e}"))?;
        }
    }
    transaction.commit().map_err(|e| format!("commit shell state: {e}"))
}

/// Read the durable cwd. Only an absent value means unset; read failures and
/// relative paths refuse execution. Backend availability is checked by the shell.
pub fn context_cwd(kernel: &crate::Kernel, context_id: ContextId) -> Result<Option<std::path::PathBuf>, String> {
    read_context_cwd(&kernel.kernel_db().lock(), context_id)
}

pub(crate) fn read_context_cwd(db: &KernelDb, context_id: ContextId) -> Result<Option<std::path::PathBuf>, String> {
    let cwd = db.get_context_shell(context_id)
        .map_err(|error| format!("read context_shell for {context_id}: {error}"))?
        .and_then(|row| row.cwd)
        .map(std::path::PathBuf::from);
    validate_cwd(cwd.as_deref())?;
    Ok(cwd)
}

pub(crate) fn validate_cwd(cwd: Option<&std::path::Path>) -> Result<(), String> {
    if let Some(path) = cwd {
        if !path.is_absolute() {
            return Err(format!("cwd '{}' must be absolute; set a valid cwd before executing", path.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers::{register_context, test_dispatcher};
    use kaijutsu_types::PrincipalId;

    #[tokio::test]
    async fn shell_state_rejects_relative_cwd_before_persisting_exports() {
        let dispatcher = test_dispatcher().await;
        let context = register_context(&dispatcher, Some("relative"), None, PrincipalId::new());
        let before = ShellStateSnapshot { cwd: "/before".into(), env: Default::default() };
        let after = ShellStateSnapshot { cwd: "relative/path".into(),
            env: [("KEEP".into(), "unchanged".into())].into_iter().collect() };
        let error = persist_shell_state(dispatcher.kernel_db(), context, &before, &after)
            .expect_err("relative cwd must not poison the next execution");
        assert!(error.contains("must be absolute"), "{error}");
        let db = dispatcher.kernel_db().lock();
        assert!(db.get_context_shell(context).unwrap().is_none());
        assert!(db.get_context_env(context).unwrap().is_empty());
    }

    #[tokio::test]
    async fn shell_state_write_failure_does_not_commit_partial_changes() {
        let dispatcher = test_dispatcher().await;
        let context = register_context(&dispatcher, Some("state"), None, PrincipalId::new());
        let before = ShellStateSnapshot { cwd: "/before".into(), env: Default::default() };
        let after = ShellStateSnapshot { cwd: "/after".into(),
            env: [("A".into(), "first".into()), ("Z".into(), "fail".into())].into_iter().collect() };
        {
            let db = dispatcher.kernel_db().lock();
            db.upsert_context_shell(&ContextShellRow { context_id: context, cwd: Some("/before".into()), updated_at: 0 }).unwrap();
            db.conn_for_ledger().execute_batch(
                "CREATE TRIGGER fail_last_export BEFORE INSERT ON context_env WHEN NEW.key = 'Z'
                 BEGIN SELECT RAISE(ABORT, 'export write failed'); END;"
            ).unwrap();
        }
        let result = persist_shell_state(dispatcher.kernel_db(), context, &before, &after);
        assert!(result.unwrap_err().contains("export write failed"));
        let db = dispatcher.kernel_db().lock();
        assert_eq!(db.get_context_shell(context).unwrap().unwrap().cwd.as_deref(), Some("/before"));
        assert!(db.get_context_env(context).unwrap().is_empty());
    }
}
