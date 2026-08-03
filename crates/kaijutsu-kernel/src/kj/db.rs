//! `kj db` — kernel database maintenance: hot backup and WAL checkpoint.
//!
//! ```text
//! kj db backup <path>       VACUUM INTO a fresh, compacted backup file
//! kj db checkpoint          PRAGMA wal_checkpoint(TRUNCATE) — flush WAL into kernel.db
//! ```
//!
//! Restore is deliberately **not** a verb here (see `DbCommand::Backup`'s
//! long help and docs/architecture/kernel.md "Backup & restore"): the kernel
//! holds in-memory state a live file swap would desynchronize, so restoring
//! means stop kernel → replace kernel.db (+ drop -wal/-shm) → start kernel.

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "db",
    about = "Kernel database maintenance: hot backup and WAL checkpoint",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct DbArgs {
    #[command(subcommand)]
    command: DbCommand,
}

#[derive(Subcommand, Debug)]
enum DbCommand {
    /// Hot-backup kernel.db to <path> via SQLite's native VACUUM INTO.
    #[command(
        long_about = "Hot-backup kernel.db to <path> via SQLite's native VACUUM INTO — a \
                      single consistent, already-compacted snapshot, safe to take against a \
                      live writer with no prior quiesce step. A bare `cp` of kernel.db is NOT \
                      safe: WAL mode means committed history can live in kernel.db-wal while \
                      the main file lags, so a raw copy can be torn or stale.\n\
                      \n\
                      <path> must be absolute. This verb runs inside the kernel process, whose \
                      working directory is not your shell's — a relative path would resolve \
                      somewhere you can't predict, so it's rejected outright rather than \
                      guessed at.\n\
                      \n\
                      The destination must not already exist — VACUUM INTO refuses to \
                      overwrite, and this verb will not delete an existing file to make room.\n\
                      \n\
                      Restore is deliberately NOT a `kj` verb. The kernel holds in-memory state \
                      (BlockStore documents, registries) that a live file swap would \
                      desynchronize. To restore a backup: stop the kernel, replace kernel.db \
                      (removing any -wal/-shm siblings alongside it), then start the kernel."
    )]
    Backup {
        /// Absolute destination path for the backup file (must not already exist).
        path: String,
    },
    /// Flush the WAL into kernel.db and report whether it completed cleanly.
    #[command(
        long_about = "Run PRAGMA wal_checkpoint(TRUNCATE): flush committed WAL frames into \
                      kernel.db and shrink kernel.db-wal back to zero. This is the quiesce step \
                      for people doing their own btrfs/ZFS snapshot or file copy — after a \
                      clean (non-busy) checkpoint, kernel.db alone reflects all committed \
                      history. Prefer `kj db backup` for a one-shot backup file; use this when \
                      you specifically want to snapshot or copy the raw files yourself."
    )]
    Checkpoint,
}

impl KjDispatcher {
    pub(crate) fn dispatch_db(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<DbArgs>();
        }
        let parsed = match DbArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj db: {e}"));
            }
        };

        // Both leaves touch the on-disk database directly — operator
        // authority, same rationale as `kj cas put`/`kj doc create`.
        if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::Operator, "db") {
            return denied;
        }

        match parsed.command {
            DbCommand::Backup { path } => self.db_backup(&path),
            DbCommand::Checkpoint => self.db_checkpoint(),
        }
    }

    fn db_backup(&self, path_str: &str) -> KjResult {
        let path = std::path::Path::new(path_str);
        if !path.is_absolute() {
            return KjResult::Err(format!(
                "kj db backup: '{path_str}' is not absolute — the kernel process's working \
                 directory is not your shell's, so a relative path can't be resolved \
                 predictably. Pass an absolute path, e.g. /home/you/backups/kernel-2026-08-03.db"
            ));
        }

        {
            let db = self.kernel_db().lock();
            if let Err(e) = db.vacuum_into(path) {
                return KjResult::Err(format!("kj db backup: {path_str}: {e}"));
            }
        }

        let size = match std::fs::metadata(path) {
            Ok(m) => m.len(),
            // VACUUM INTO reported success; a missing/unreadable file after
            // that would be a filesystem-level surprise worth surfacing
            // rather than silently omitting the size.
            Err(e) => {
                return KjResult::Err(format!(
                    "kj db backup: wrote {path_str} but could not stat it afterward: {e}"
                ));
            }
        };

        let record = serde_json::json!({
            "path": path_str,
            "bytes": size,
        });
        KjResult::ok_with_data(format!("backed up kernel.db to {path_str} ({size} bytes)\n"), record)
    }

    fn db_checkpoint(&self) -> KjResult {
        let (busy, log_frames, checkpointed_frames) = {
            let db = self.kernel_db().lock();
            match db.checkpoint() {
                Ok(v) => v,
                Err(e) => return KjResult::Err(format!("kj db checkpoint: {e}")),
            }
        };
        let is_busy = busy != 0;
        let record = serde_json::json!({
            "busy": is_busy,
            "log_frames": log_frames,
            "checkpointed_frames": checkpointed_frames,
        });
        let msg = if is_busy {
            format!(
                "checkpoint incomplete (busy) — {checkpointed_frames}/{log_frames} WAL frames \
                 checkpointed; a concurrent reader/writer blocked a full TRUNCATE. kernel.db-wal \
                 still holds uncommitted-to-main history; retry once other activity quiets.\n"
            )
        } else {
            format!(
                "checkpoint complete — {checkpointed_frames}/{log_frames} WAL frames flushed, \
                 kernel.db-wal truncated to zero\n"
            )
        };
        KjResult::ok_with_data(msg, record)
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use crate::kj::KjResult;

    fn s(v: &str) -> String {
        v.to_string()
    }

    // ── kj db / kj db backup help ─────────────────────────────────────

    #[tokio::test]
    async fn db_bare_help_renders_without_error() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("db")], &c).await;
        assert!(
            matches!(&result, KjResult::Ok { ephemeral: true, .. }),
            "kj db (no subcommand) should render help, got {result:?}"
        );
    }

    #[tokio::test]
    async fn db_backup_help_documents_the_no_restore_verb_stance() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("db"), s("backup"), s("help")], &c).await;
        assert!(
            matches!(&result, KjResult::Ok { ephemeral: true, .. }),
            "kj db backup help should render, got {result:?}"
        );
        let text = result.message();
        assert!(
            text.contains("VACUUM INTO") && text.contains("Restore is deliberately NOT"),
            "long help must cover the backup mechanism and the no-restore-verb stance: {text}"
        );
    }

    // ── kj db backup ───────────────────────────────────────────────────

    #[tokio::test]
    async fn db_backup_relative_path_errors() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let result = d
            .dispatch(&[s("db"), s("backup"), s("relative/path.db")], &c)
            .await;
        assert!(!result.is_ok(), "relative path must be rejected");
        assert!(
            result.message().contains("not absolute"),
            "got: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn db_backup_absolute_path_succeeds_and_reports_size() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("kernel-backup.db");

        let result = d
            .dispatch(
                &[s("db"), s("backup"), out.to_str().unwrap().to_string()],
                &c,
            )
            .await;
        assert!(result.is_ok(), "backup failed: {}", result.message());
        assert!(out.exists(), "backup file must exist on disk");
        let meta = std::fs::metadata(&out).unwrap();
        assert!(meta.len() > 0, "backup file must be non-empty");

        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["path"], out.to_str().unwrap());
                assert_eq!(v["bytes"], meta.len());
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn db_backup_target_exists_errors_cleanly() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("already-here.db");
        std::fs::write(&out, b"occupying the name").unwrap();

        let result = d
            .dispatch(
                &[s("db"), s("backup"), out.to_str().unwrap().to_string()],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "must not overwrite an existing file");
        // Untouched: proves no pre-delete-then-recreate happened.
        assert_eq!(std::fs::read(&out).unwrap(), b"occupying the name");
    }

    #[tokio::test]
    async fn db_backup_denied_without_operator_capability() {
        let d = test_dispatcher().await;
        // A fresh, never-registered context: `get_context_binding` returns
        // no row for it, so `require_cap` denies by default. (Unlike
        // `broker().clear_binding`, which only drops the broker's in-memory
        // cache — `require_cap` reads the KernelDb row directly, so a
        // *registered* context's DB-persisted grant would still apply.)
        let c = caller_with_context(kaijutsu_types::ContextId::new());
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("denied.db");

        let result = d
            .dispatch(
                &[s("db"), s("backup"), out.to_str().unwrap().to_string()],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "backup without Operator cap must be denied");
        assert!(!out.exists(), "denied backup must not write a file");
    }

    // ── kj db checkpoint ───────────────────────────────────────────────

    #[tokio::test]
    async fn db_checkpoint_reports_non_busy() {
        let d = test_dispatcher().await;
        let c = test_caller();

        // The test dispatcher's KernelDb is in-memory (no WAL file), but
        // `PRAGMA wal_checkpoint` on an in-memory/non-WAL db still succeeds
        // and reports non-busy — proves the dispatch plumbing (lock, call,
        // shape the result) end to end.
        let result = d.dispatch(&[s("db"), s("checkpoint")], &c).await;
        assert!(result.is_ok(), "checkpoint failed: {}", result.message());
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["busy"], false);
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn db_checkpoint_denied_without_operator_capability() {
        let d = test_dispatcher().await;
        // See comment in db_backup_denied_without_operator_capability for
        // why an unregistered context (not a cleared-binding registered one)
        // is the right fixture for this gate.
        let c = caller_with_context(kaijutsu_types::ContextId::new());

        let result = d.dispatch(&[s("db"), s("checkpoint")], &c).await;
        assert!(!result.is_ok(), "checkpoint without Operator cap must be denied");
    }
}
