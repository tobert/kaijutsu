//! Durable settlement for rc script execution.
//!
//! A script result is retained in `rc_run_scripts` before its context block is
//! projected. The small in-memory maps below own only database writes that
//! failed after execution; durable pending projections remain owned by the
//! ledger and are recovered without running source again.

use std::collections::HashMap;
use std::sync::Arc;

use approval_ledger::rc_runs;
use approval_ledger::error::LedgerError;
use approval_ledger::types::{RcOutcome, RcRunRow, RcRunScriptRow};
use kaijutsu_types::{BlockKind, ContentType, ContextId, Role, Status};
use serde::{Deserialize, Serialize};

use crate::block_store::{RecordedBlockInsert, ShellResultFields, SharedBlockStore};
use crate::kernel_db::KernelDb;

/// The exact outcome captured from one attempted rc script.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum ScriptExecution {
    Completed {
        result: kaish_kernel::interpreter::ExecResult,
        cancelled: bool,
    },
    NotRun {
        message: String,
        cancelled: bool,
    },
    Fault {
        message: String,
        cancelled: bool,
    },
    /// A pre-settlement-schema row proves execution finished, but retained no
    /// output. Preserve its recorded exit and timestamp without fabricating an
    /// `ExecResult` or running source again.
    LegacyRecorded {
        message: String,
        exit_code: Option<i64>,
    },
}

impl ScriptExecution {
    pub(crate) fn exit_code(&self) -> Option<i64> {
        match self {
            Self::NotRun { cancelled: true, .. }
            | Self::Fault { cancelled: true, .. } => Some(130),
            Self::Completed { result, .. } => Some(result.original_code.unwrap_or(result.code)),
            Self::LegacyRecorded { exit_code, .. } => *exit_code,
            Self::NotRun { .. } | Self::Fault { .. } => None,
        }
    }

    pub(crate) fn failed(&self) -> bool {
        self.cancelled() || matches!(self, Self::NotRun { .. } | Self::Fault { .. } | Self::LegacyRecorded { .. })
            || self.exit_code().is_some_and(|code| code != 0)
    }

    pub(crate) fn cancelled(&self) -> bool {
        match self {
            Self::Completed { cancelled, .. }
            | Self::NotRun { cancelled, .. }
            | Self::Fault { cancelled, .. } => *cancelled,
            Self::LegacyRecorded { .. } => false,
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ScriptKey {
    run_id: String,
    seq: i64,
}

#[derive(Clone)]
struct PendingResult {
    json: String,
    exit_code: Option<i64>,
    finished_at: i64,
    error: String,
}

#[derive(Clone)]
struct PendingFinish {
    outcome: RcOutcome,
    error: String,
}

/// Kernel-owned retry owner for rc outcomes captured after source execution.
pub(crate) struct RcSettlements {
    db: Arc<parking_lot::Mutex<KernelDb>>,
    blocks: SharedBlockStore,
    pending_results: parking_lot::Mutex<HashMap<ScriptKey, PendingResult>>,
    pending_finishes: parking_lot::Mutex<HashMap<String, PendingFinish>>,
    projection: parking_lot::Mutex<()>,
}

impl RcSettlements {
    pub(crate) fn new(
        db: Arc<parking_lot::Mutex<KernelDb>>,
        blocks: SharedBlockStore,
    ) -> Self {
        Self {
            db,
            blocks,
            pending_results: parking_lot::Mutex::new(HashMap::new()),
            pending_finishes: parking_lot::Mutex::new(HashMap::new()),
            projection: parking_lot::Mutex::new(()),
        }
    }

    /// Retain the captured result, then publish its context-log projection.
    /// Either step may be retried; script source is never part of retry.
    pub(crate) fn retain_and_project(
        &self,
        run_id: &str,
        seq: i64,
        execution: ScriptExecution,
    ) -> Result<(), String> {
        let key = ScriptKey { run_id: run_id.to_owned(), seq };
        let json = serde_json::to_string(&execution).map_err(|e| e.to_string())?;
        let exit_code = execution.exit_code();
        let mut pending = self.pending_results.lock();
        if pending.get(&key).is_some_and(|prior| prior.json != json) {
            return Err(format!(
                "rc run {} script {} already has a different live result owner",
                key.run_id, key.seq,
            ));
        }
        let finished_at = pending.get(&key).map(|prior| prior.finished_at).unwrap_or_else(now_millis);
        if let Err(error) = self.retain_result(&key, &json, exit_code, finished_at) {
            let retain = matches!(&error, LedgerError::Db(_)) || pending.contains_key(&key);
            let message = error.to_string();
            if retain {
                pending.insert(key, PendingResult {
                    json, exit_code, finished_at, error: message.clone(),
                });
            }
            return Err(message);
        }
        pending.remove(&key);
        drop(pending);
        self.project(&key)
    }

    /// Record the intended terminal run outcome. An unsettled dependency keeps
    /// this live owner until result projection makes finalization possible.
    pub(crate) fn finish_run(&self, run_id: &str, outcome: RcOutcome) -> Result<(), String> {
        let mut pending = self.pending_finishes.lock();
        if pending.get(run_id).is_some_and(|prior| prior.outcome != outcome) {
            return Err(format!("rc run {run_id} already has a different live finish owner"));
        }
        let result = {
            let db = self.db.lock();
            rc_runs::finish_settled_run(db.conn_for_ledger(), run_id, outcome)
        };
        match result {
            Ok(()) => {
                pending.remove(run_id);
                Ok(())
            }
            Err(error) => {
                if matches!(error, LedgerError::RunAlreadyFinished(_)) {
                    match rc_runs::get_run(self.db.lock().conn_for_ledger(), run_id) {
                        Ok(Some(run)) if run.outcome == Some(outcome) => {
                            pending.remove(run_id);
                            return Ok(());
                        }
                        Ok(Some(run)) => return Err(format!(
                            "rc run {run_id} already finished as {:?}, requested {outcome}", run.outcome,
                        )),
                        Ok(None) => return Err(format!("rc run {run_id} disappeared after finish conflict")),
                        Err(read_error) => {
                            let message = read_error.to_string();
                            pending.insert(run_id.to_owned(), PendingFinish { outcome, error: message.clone() });
                            return Err(message);
                        }
                    }
                }
                let retain = matches!(&error, LedgerError::Db(_) | LedgerError::RunNotSettled(_))
                    || pending.contains_key(run_id);
                let message = error.to_string();
                if retain {
                    pending.insert(run_id.to_owned(), PendingFinish { outcome, error: message.clone() });
                }
                Err(message)
            }
        }
    }

    /// Retry every retained result, durable projection, and terminal run write.
    pub(crate) fn retry_pending(&self) -> Result<(), String> {
        let pending: Vec<_> = self.pending_results.lock().iter()
            .map(|(key, value)| (key.clone(), value.clone())).collect();
        let mut failures = Vec::new();
        for (key, value) in pending {
            match self.retain_result(&key, &value.json, value.exit_code, value.finished_at) {
                Ok(()) => {
                    self.pending_results.lock().remove(&key);
                    if let Err(error) = self.project(&key) { failures.push(error); }
                }
                Err(error) => {
                    let error = error.to_string();
                    if let Some(pending) = self.pending_results.lock().get_mut(&key) {
                        pending.error = error.clone();
                    }
                    failures.push(error);
                }
            }
        }

        let projections = {
            let db = self.db.lock();
            rc_runs::list_pending_script_projections(db.conn_for_ledger())
                .map_err(|e| e.to_string())?
        };
        for (run, script) in projections {
            if let Err(error) = self.project_loaded(&run, &script) { failures.push(error); }
        }

        let finishes: Vec<_> = self.pending_finishes.lock().iter()
            .map(|(run, finish)| (run.clone(), finish.clone())).collect();
        for (run_id, finish) in finishes {
            if let Err(error) = self.finish_run(&run_id, finish.outcome) {
                failures.push(error);
            }
        }
        if failures.is_empty() { Ok(()) } else { Err(failures.join("; ")) }
    }

    /// Recover prior-lifetime runs without executing any script source.
    pub(crate) fn recover(&self) -> Result<(), String> {
        let runs = {
            let db = self.db.lock();
            rc_runs::list_unfinished_runs(db.conn_for_ledger()).map_err(|e| e.to_string())?
        };
        for run in runs {
            let scripts = {
                let db = self.db.lock();
                rc_runs::list_run_scripts(db.conn_for_ledger(), &run.run_id)
                    .map_err(|e| e.to_string())?
            };
            let mut interrupted = false;
            for script in &scripts {
                if script.result_json.is_none() {
                    interrupted = true;
                    let execution = if script.finished_at.is_some() {
                        ScriptExecution::LegacyRecorded {
                            message: "legacy rc record says execution finished, but predates retained output; source was not run again".into(),
                            exit_code: script.exit_code,
                        }
                    } else {
                        ScriptExecution::Fault {
                            message: "kernel restarted after rc script execution began; effects may have occurred and output was unavailable; source was not run again".into(),
                            cancelled: false,
                        }
                    };
                    let key = ScriptKey { run_id: run.run_id.clone(), seq: script.seq };
                    let json = serde_json::to_string(&execution).map_err(|e| e.to_string())?;
                    self.retain_result(
                        &key,
                        &json,
                        execution.exit_code(),
                        script.finished_at.unwrap_or_else(now_millis),
                    ).map_err(|e| e.to_string())?;
                    self.project(&key)?;
                } else {
                    serde_json::from_str::<ScriptExecution>(script.result_json.as_deref().unwrap())
                        .map_err(|error| format!(
                            "rc run {} script {} retained invalid result: {error}",
                            run.run_id, script.seq,
                        ))?;
                    if script.projected_at.is_none() {
                        self.project_loaded(&run, script)?;
                    }
                }
            }
            let expected = run.script_count.unwrap_or(-1);
            let short = expected < 0 || scripts.len() as i64 != expected;
            let inferred = match run.intended_outcome {
                Some(outcome) => outcome,
                None if interrupted || short => RcOutcome::Abandoned,
                None => {
                    let failed = scripts.iter().map(|script| {
                        let json = script.result_json.as_deref().ok_or_else(|| {
                            format!("rc run {} script {} lost its retained result", run.run_id, script.seq)
                        })?;
                        serde_json::from_str::<ScriptExecution>(json).map_err(|error| {
                            format!("rc run {} script {} retained invalid result: {error}", run.run_id, script.seq)
                        })
                    }).collect::<Result<Vec<_>, String>>()?.into_iter().any(|execution| execution.failed());
                    if failed { RcOutcome::Failed } else { RcOutcome::Ok }
                }
            };
            self.finish_run(&run.run_id, inferred)?;
        }
        Ok(())
    }

    pub(crate) fn unresolved_failures(&self) -> Vec<String> {
        let mut failures: Vec<String> = self.pending_results.lock().iter()
            .map(|(key, pending)| format!("rc run {} script {} result: {}", key.run_id, key.seq, pending.error))
            .collect();
        failures.extend(self.pending_finishes.lock().iter()
            .map(|(run, pending)| format!("rc run {run} finish: {}", pending.error)));
        failures
    }

    fn retain_result(
        &self,
        key: &ScriptKey,
        json: &str,
        exit_code: Option<i64>,
        finished_at: i64,
    ) -> Result<(), LedgerError> {
        let db = self.db.lock();
        rc_runs::retain_run_script_result(
            db.conn_for_ledger(), &key.run_id, key.seq, json, exit_code, finished_at,
        ).map(|_| ())
    }

    fn project(&self, key: &ScriptKey) -> Result<(), String> {
        let (run, script) = {
            let db = self.db.lock();
            let run = rc_runs::get_run(db.conn_for_ledger(), &key.run_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("rc run {} disappeared", key.run_id))?;
            let script = rc_runs::list_run_scripts(db.conn_for_ledger(), &key.run_id)
                .map_err(|e| e.to_string())?.into_iter()
                .find(|script| script.seq == key.seq)
                .ok_or_else(|| format!("rc run {} script {} disappeared", key.run_id, key.seq))?;
            (run, script)
        };
        self.project_loaded(&run, &script)
    }

    fn project_loaded(&self, run: &RcRunRow, script: &RcRunScriptRow) -> Result<(), String> {
        let _projection = self.projection.lock();
        let (run, script) = {
            let db = self.db.lock();
            let run = rc_runs::get_run(db.conn_for_ledger(), &run.run_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("rc run {} disappeared", run.run_id))?;
            let script = rc_runs::list_run_scripts(db.conn_for_ledger(), &run.run_id)
                .map_err(|e| e.to_string())?.into_iter()
                .find(|candidate| candidate.seq == script.seq)
                .ok_or_else(|| format!("rc run {} script {} disappeared", run.run_id, script.seq))?;
            (run, script)
        };
        if script.projected_at.is_some() { return Ok(()); }
        let json = script.result_json.as_deref()
            .ok_or_else(|| format!("rc run {} script {} has no retained result", run.run_id, script.seq))?;
        let execution: ScriptExecution = serde_json::from_str(json).map_err(|e| {
            format!("rc run {} script {} retained invalid result: {e}", run.run_id, script.seq)
        })?;
        let context = ContextId::try_from_slice(&run.context_id)
            .ok_or_else(|| format!("rc run {} has an invalid context id", run.run_id))?;
        if !self.blocks.contains(context) {
            self.blocks.load_one_from_db(context).map_err(|e| e.to_string())?;
        }
        let principal = self.db.lock().get_context(context).map_err(|e| e.to_string())?
            .ok_or_else(|| format!("rc run {} context {} disappeared", run.run_id, context))?
            .created_by;
        let projection = projection(&script.path, &execution);
        let Some(projection) = projection else {
            let db = self.db.lock();
            return rc_runs::mark_run_script_projected(
                db.conn_for_ledger(), &run.run_id, script.seq, None, now_millis(),
            ).map(|_| ()).map_err(|e| e.to_string());
        };
        let ansi = projection.ansi.as_ref().map(|(spans, original)| (spans.clone(), original.as_slice()));
        let run_id = run.run_id.clone();
        let seq = script.seq;
        self.blocks.append_block_recorded(
            context,
            RecordedBlockInsert {
                role: Role::System,
                kind: projection.kind,
                content: projection.content,
                status: projection.status,
                content_type: projection.content_type,
                author: principal,
                ansi,
                shell: Some(ShellResultFields {
                    stderr: projection.stderr,
                    output: projection.output,
                    content_type: projection.content_type,
                    exit_code: execution.exit_code().map(|code| code.clamp(i32::MIN as i64, i32::MAX as i64) as i32),
                    ephemeral: None,
                }),
            },
            |db, block| {
                rc_runs::mark_run_script_projected(
                    db.conn_for_ledger(), &run_id, seq, Some(&block.to_key()), now_millis(),
                )?;
                Ok(())
            },
        ).map(|_| ()).map_err(|e| e.to_string())
    }
}

struct Projection {
    kind: BlockKind,
    content: String,
    status: Status,
    content_type: ContentType,
    stderr: Option<String>,
    output: Option<kaijutsu_types::OutputData>,
    ansi: Option<(Vec<kaijutsu_types::StyleSpan>, Vec<u8>)>,
}

fn projection(path: &str, execution: &ScriptExecution) -> Option<Projection> {
    let sort_key = crate::rc::parse_rc_path(path).map(|parts| parts.sort_key)
        .unwrap_or_else(|_| "S??".into());
    let exit = execution.exit_code();
    let failed = execution.failed();
    let (stdout, stderr, output) = match execution {
        ScriptExecution::Completed { result, .. } => (
            crate::ansi_ingest::raw_stdout(result).into_owned(),
            result.err.clone(),
            crate::runtime::command_result::block_output_data(result),
        ),
        ScriptExecution::NotRun { message, .. }
        | ScriptExecution::Fault { message, .. }
        | ScriptExecution::LegacyRecorded { message, .. } => (
            Vec::new(), message.clone(), None,
        ),
    };
    if !failed && stdout.is_empty() && stderr.is_empty() && output.is_none() {
        return None;
    }
    let header = if failed {
        match exit {
            Some(code) => format!(
                "rc {sort_key} exit {code}: {path}\nrc_path: {path}\nsort_key: {sort_key}\nexit_code: {code}\n\n"
            ),
            None => format!(
                "rc {sort_key} failed: {path}\nrc_path: {path}\nsort_key: {sort_key}\nexit_code: n/a\n\n"
            ),
        }
    } else {
        format!("rc {sort_key} trace: {path}\nrc_path: {path}\nsort_key: {sort_key}\n\n")
    };
    let stdout_len = stdout.len();
    let mut raw = header.into_bytes();
    if execution.cancelled() {
        raw.extend_from_slice(b"rc lifecycle cancelled during script execution\n");
    }
    if !stdout.is_empty() {
        raw.extend_from_slice(b"--- stdout ---\n");
        raw.extend_from_slice(&stdout);
        raw.push(b'\n');
    }
    if !stderr.is_empty() {
        raw.extend_from_slice(b"--- stderr ---\n");
        raw.extend_from_slice(stderr.as_bytes());
    }
    let transformed = crate::ansi_ingest::project(&raw);
    let (content, ansi) = match transformed {
        Some(projected) => (projected.text, Some((projected.spans, raw))),
        None => match String::from_utf8(raw) {
            Ok(content) => (content, None),
            Err(raw) => {
                let bytes = raw.into_bytes();
                let display = String::from_utf8_lossy(&bytes);
                (format!(
                    "[binary rc output displayed with replacement characters; exact {} bytes retained in the run ledger]\n{display}",
                    stdout_len,
                ), None)
            }
        },
    };
    Some(Projection {
        kind: if failed { BlockKind::Error } else { BlockKind::Trace },
        content,
        status: if failed { Status::Error } else { Status::Done },
        content_type: ContentType::Plain,
        stderr: if stderr.is_empty() { None } else { Some(stderr) },
        output,
        ansi,
    })
}

fn now_millis() -> i64 {
    kaijutsu_types::now_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store_with_db;
    use crate::kj::test_helpers::{register_context, test_dispatcher_persistent};
    use kaijutsu_types::PrincipalId;

    fn begin_one(dispatcher: &crate::KjDispatcher, context: ContextId) -> String {
        let db = dispatcher.kernel_db().lock();
        let conn = db.conn_for_ledger();
        let run = rc_runs::start_run(conn, context.as_bytes(), "default", "create").unwrap();
        rc_runs::set_run_script_count(conn, &run, 1).unwrap();
        assert_eq!(rc_runs::begin_run_script(
            conn, &run, "/config/rc/default/create/S00-test.kai", "echo once", now_millis(),
        ).unwrap(), 0);
        run
    }

    fn completed(text: &str) -> ScriptExecution {
        ScriptExecution::Completed {
            result: kaish_kernel::interpreter::ExecResult::success(text),
            cancelled: false,
        }
    }

    #[test]
    fn serialized_execution_preserves_raw_structure_and_original_exit() {
        let mut result = kaish_kernel::interpreter::ExecResult::success_bytes(vec![0, 0xff, 3]);
        result.code = 3;
        result.original_code = Some(0);
        result.did_spill = true;
        result.data = Some(kaish_kernel::ast::Value::Json(serde_json::json!({"sideband": true})));
        result.set_output(Some(kaijutsu_types::OutputData::nodes(vec![
            kaijutsu_types::OutputNode::new("row"),
        ])));
        result.baggage.insert("trace".into(), "kept".into());
        let execution = ScriptExecution::Completed { result: result.clone(), cancelled: true };
        let restored: ScriptExecution = serde_json::from_str(
            &serde_json::to_string(&execution).unwrap(),
        ).unwrap();
        let ScriptExecution::Completed { result: restored, cancelled } = restored else {
            panic!("completed result changed variant");
        };
        assert!(cancelled);
        assert_eq!(restored, result);
        assert_eq!(execution.exit_code(), Some(0), "spill remap must not invent exit 3");
        assert!(execution.failed(), "cancellation remains distinct from physical exit");
    }

    #[test]
    fn binary_projection_keeps_context_and_counts_stdout_bytes() {
        let mut result = kaish_kernel::interpreter::ExecResult::success_bytes(vec![0xff, 0xfe, 0xfd]);
        result.err = "warning from rc\n".into();
        let projected = projection(
            "/config/rc/default/create/S00-binary.kai",
            &ScriptExecution::Completed { result, cancelled: false },
        ).unwrap();
        assert_eq!(projected.content_type, ContentType::Plain);
        assert!(projected.content.contains("rc S00 trace"));
        assert!(projected.content.contains("exact 3 bytes"));
        assert!(projected.content.contains("warning from rc"));
    }

    #[tokio::test]
    async fn retention_fault_keeps_live_owner_and_shutdown_retries_without_duplicate_projection() {
        let dispatcher = test_dispatcher_persistent().await;
        let context = register_context(&dispatcher, Some("rc-retention"), None, PrincipalId::system());
        dispatcher.block_store().create_document(
            context, kaijutsu_types::DocKind::Conversation, None,
        ).unwrap();
        let run = begin_one(&dispatcher, context);
        dispatcher.kernel_db().lock().conn_for_ledger().execute_batch(
            "CREATE TRIGGER reject_rc_result BEFORE UPDATE OF result_json ON rc_run_scripts
             BEGIN SELECT RAISE(ABORT, 'injected rc result retention fault'); END;",
        ).unwrap();
        let settlements = dispatcher.kernel().rc_settlements();
        assert!(settlements.retain_and_project(&run, 0, completed("captured once")).unwrap_err()
            .contains("injected rc result retention fault"));
        assert!(settlements.finish_run(&run, RcOutcome::Failed).is_err());
        let shutdown = dispatcher.kernel().shutdown_runtime_worker().await.unwrap_err();
        assert!(shutdown.contains("injected rc result retention fault"), "{shutdown}");
        dispatcher.kernel_db().lock().conn_for_ledger()
            .execute_batch("DROP TRIGGER reject_rc_result").unwrap();
        dispatcher.kernel().shutdown_runtime_worker().await.unwrap();
        dispatcher.kernel().shutdown_runtime_worker().await.unwrap();
        let scripts = rc_runs::list_run_scripts(
            dispatcher.kernel_db().lock().conn_for_ledger(), &run,
        ).unwrap();
        assert!(scripts[0].result_json.is_some());
        assert!(scripts[0].projected_at.is_some());
        assert_eq!(dispatcher.block_store().block_snapshots(context).unwrap().iter()
            .filter(|block| block.kind == BlockKind::Trace).count(), 1);
        assert_eq!(rc_runs::get_run(
            dispatcher.kernel_db().lock().conn_for_ledger(), &run,
        ).unwrap().unwrap().outcome, Some(RcOutcome::Failed));
    }

    #[tokio::test]
    async fn projection_marker_and_block_are_atomic_then_restart_projects_without_reexecution() {
        let dispatcher = test_dispatcher_persistent().await;
        let context = register_context(&dispatcher, Some("rc-projection"), None, PrincipalId::system());
        dispatcher.block_store().create_document(
            context, kaijutsu_types::DocKind::Conversation, None,
        ).unwrap();
        let run = begin_one(&dispatcher, context);
        dispatcher.kernel_db().lock().conn_for_ledger().execute_batch(
            "CREATE TRIGGER reject_rc_projection BEFORE UPDATE OF projected_at ON rc_run_scripts
             BEGIN SELECT RAISE(ABORT, 'injected rc projection fault'); END;",
        ).unwrap();
        let settlements = dispatcher.kernel().rc_settlements();
        let mut result = kaish_kernel::interpreter::ExecResult::success("project exactly once");
        result.content_type = Some("application/json".into());
        result.data = Some(kaish_kernel::ast::Value::Json(serde_json::json!({"kept": true})));
        assert!(settlements.retain_and_project(
            &run, 0, ScriptExecution::Completed { result, cancelled: false },
        ).unwrap_err()
            .contains("injected rc projection fault"));
        assert!(settlements.finish_run(&run, RcOutcome::Failed).is_err());
        let script = rc_runs::list_run_scripts(
            dispatcher.kernel_db().lock().conn_for_ledger(), &run,
        ).unwrap().remove(0);
        assert!(script.result_json.is_some());
        assert!(script.projected_at.is_none());
        assert!(script.output_block_id.is_none());

        let db = dispatcher.kernel_db().clone();
        let workspace = db.lock().get_or_create_default_workspace(PrincipalId::system()).unwrap();
        let recovered_blocks = shared_block_store_with_db(db.clone(), workspace, PrincipalId::system());
        recovered_blocks.load_one_from_db(context).unwrap();
        assert!(recovered_blocks.block_snapshots(context).unwrap().is_empty(),
            "failed projection must leave no durable partial block");
        db.lock().conn_for_ledger().execute_batch("DROP TRIGGER reject_rc_projection").unwrap();
        let recovered = RcSettlements::new(db.clone(), recovered_blocks.clone());
        recovered.recover().unwrap();
        recovered.recover().unwrap();
        let blocks = recovered_blocks.block_snapshots(context).unwrap();
        assert_eq!(blocks.iter().filter(|block| block.kind == BlockKind::Trace).count(), 1);
        let trace = blocks.iter().find(|block| block.kind == BlockKind::Trace).unwrap();
        assert!(trace.content.contains("project exactly once"));
        assert_eq!(trace.content_type, ContentType::Plain,
            "the wrapper is plain text even when retained execution names another MIME type");
        assert_eq!(trace.output.as_ref().and_then(|output| output.rich_json.as_ref()),
            Some(&serde_json::json!({"kept": true})));
        let script = rc_runs::list_run_scripts(db.lock().conn_for_ledger(), &run).unwrap().remove(0);
        let execution: ScriptExecution = serde_json::from_str(script.result_json.as_deref().unwrap()).unwrap();
        let ScriptExecution::Completed { result, .. } = execution else { panic!("completed result changed variant") };
        assert_eq!(result.content_type.as_deref(), Some("application/json"));
        let row = rc_runs::get_run(db.lock().conn_for_ledger(), &run).unwrap().unwrap();
        assert_eq!(row.outcome, Some(RcOutcome::Failed));
    }

    #[tokio::test]
    async fn concurrent_projection_retries_share_one_block_without_a_spurious_failure() {
        let dispatcher = test_dispatcher_persistent().await;
        let context = register_context(&dispatcher, Some("rc-concurrent-projection"), None, PrincipalId::system());
        dispatcher.block_store().create_document(
            context, kaijutsu_types::DocKind::Conversation, None,
        ).unwrap();
        let run = begin_one(&dispatcher, context);
        let execution = completed("one retained result");
        let json = serde_json::to_string(&execution).unwrap();
        rc_runs::retain_run_script_result(
            dispatcher.kernel_db().lock().conn_for_ledger(),
            &run,
            0,
            &json,
            execution.exit_code(),
            now_millis(),
        ).unwrap();
        let owner = Arc::new(RcSettlements::new(
            dispatcher.kernel_db().clone(), dispatcher.block_store().clone(),
        ));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let owner = owner.clone();
            let barrier = barrier.clone();
            let run = run.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                owner.project(&ScriptKey { run_id: run, seq: 0 })
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        assert_eq!(dispatcher.block_store().block_snapshots(context).unwrap().iter()
            .filter(|block| block.kind == BlockKind::Trace).count(), 1);
    }

    #[tokio::test]
    async fn recovery_rejects_corrupt_retained_results_even_after_projection() {
        let dispatcher = test_dispatcher_persistent().await;
        let context = register_context(&dispatcher, Some("rc-corrupt-result"), None, PrincipalId::system());
        dispatcher.block_store().create_document(
            context, kaijutsu_types::DocKind::Conversation, None,
        ).unwrap();
        let run = begin_one(&dispatcher, context);
        let execution = completed("original result");
        let json = serde_json::to_string(&execution).unwrap();
        let conn = dispatcher.kernel_db().lock();
        rc_runs::retain_run_script_result(
            conn.conn_for_ledger(), &run, 0, &json, execution.exit_code(), now_millis(),
        ).unwrap();
        rc_runs::mark_run_script_projected(
            conn.conn_for_ledger(), &run, 0, None, now_millis(),
        ).unwrap();
        conn.conn_for_ledger().execute(
            "UPDATE rc_run_scripts SET result_json='not valid json' WHERE run_id=?1 AND seq=0",
            [&run],
        ).unwrap();
        drop(conn);

        let error = dispatcher.kernel().rc_settlements().recover().unwrap_err();
        assert!(error.contains("retained invalid result"), "{error}");
        assert!(rc_runs::get_run(
            dispatcher.kernel_db().lock().conn_for_ledger(), &run,
        ).unwrap().unwrap().outcome.is_none());
    }

    #[tokio::test]
    async fn recovery_preserves_a_legacy_finished_exit_without_inventing_output_or_replaying() {
        let dispatcher = test_dispatcher_persistent().await;
        let context = register_context(&dispatcher, Some("rc-legacy-result"), None, PrincipalId::system());
        dispatcher.block_store().create_document(
            context, kaijutsu_types::DocKind::Conversation, None,
        ).unwrap();
        let (run, finished_at) = {
            let db = dispatcher.kernel_db().lock();
            let conn = db.conn_for_ledger();
            let run = rc_runs::start_run(conn, context.as_bytes(), "default", "create").unwrap();
            rc_runs::set_run_script_count(conn, &run, 1).unwrap();
            let finished_at = 1_234_567;
            let body = "0000000000000000000000000000000000000000000000000000000000000000";
            conn.execute(
                "INSERT INTO script_bodies(sha256,body) VALUES(?1,'legacy source must not run')",
                [body],
            ).unwrap();
            conn.execute(
                "INSERT INTO rc_run_scripts(run_id,seq,path,body_sha256,exit_code,started_at,finished_at)
                 VALUES(?1,0,'/config/rc/default/create/S00-legacy.kai',?2,7,?3,?4)",
                rusqlite::params![run, body, finished_at - 5, finished_at],
            ).unwrap();
            (run, finished_at)
        };
        dispatcher.kernel().rc_settlements().recover().unwrap();
        let script = rc_runs::list_run_scripts(
            dispatcher.kernel_db().lock().conn_for_ledger(), &run,
        ).unwrap().remove(0);
        assert_eq!(script.exit_code, Some(7));
        assert_eq!(script.finished_at, Some(finished_at));
        let execution: ScriptExecution = serde_json::from_str(script.result_json.as_deref().unwrap()).unwrap();
        assert!(matches!(execution, ScriptExecution::LegacyRecorded { exit_code: Some(7), .. }));
        let blocks = dispatcher.block_store().block_snapshots(context).unwrap();
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].content.contains("predates retained output"));
        assert_eq!(rc_runs::get_run(
            dispatcher.kernel_db().lock().conn_for_ledger(), &run,
        ).unwrap().unwrap().outcome, Some(RcOutcome::Abandoned));
    }
}
