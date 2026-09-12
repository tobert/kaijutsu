//! Durable shell receipts and context-owned kaish job managers.

use std::collections::HashMap;
use std::sync::Arc;

use kaijutsu_types::{BlockId, ContextId, PrincipalId};
use kaijutsu_types::shell_envelope::{ShellEnvelope, ShellStatus};
use kaish_kernel::scheduler::{JobId, JobManager};
use parking_lot::Mutex;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::kernel_db::KernelDb;

type OperationResult<T> = Result<T, String>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellOperationReceipt {
    pub operation_id: String,
    pub context_id: ContextId,
    pub command_block_id: BlockId,
    pub output_block_id: BlockId,
    pub ask_id: Option<String>,
    pub job_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellOperationState {
    pub receipt: ShellOperationReceipt,
    pub source: String,
    pub created_at: i64,
    pub continuation_epoch: Option<i64>,
    pub completed_at: Option<i64>,
    pub envelope: Option<ShellEnvelope>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellOperationSummary {
    pub running_count: u32,
    pub oldest_running_started_at_unix_ms: Option<u64>,
    pub last_finished: Option<ShellOperationFinishedSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOperationFinishedSummary {
    pub status: &'static str,
    pub exit_code: Option<i32>,
    pub finished_at_unix_ms: u64,
}

pub struct ShellOperationRegistry {
    db: Arc<Mutex<KernelDb>>,
    managers: Mutex<HashMap<ContextId, Arc<JobManager>>>,
    jobs: Mutex<HashMap<String, (JobId, Arc<JobManager>)>>,
}

impl ShellOperationRegistry {
    pub fn new(db: Arc<Mutex<KernelDb>>) -> OperationResult<Self> {
        db.lock().conn_for_ledger().execute_batch(
            "CREATE TABLE IF NOT EXISTS shell_operations (
                operation_id TEXT PRIMARY KEY,
                context_id BLOB NOT NULL,
                principal_id BLOB NOT NULL,
                actor_id BLOB NOT NULL,
                command_block_id TEXT NOT NULL,
                output_block_id TEXT NOT NULL UNIQUE,
                source TEXT NOT NULL,
                continuation_epoch INTEGER,
                ask_id TEXT,
                job_id TEXT,
                created_at INTEGER NOT NULL,
                completed_at INTEGER,
                envelope_json TEXT
             );
             CREATE INDEX IF NOT EXISTS shell_operations_context ON shell_operations(context_id);
             CREATE UNIQUE INDEX IF NOT EXISTS shell_operations_ask ON shell_operations(ask_id);"
        ).map_err(|e| e.to_string())?;
        Ok(Self { db, managers: Mutex::new(HashMap::new()), jobs: Mutex::new(HashMap::new()) })
    }

    pub fn context_job_manager(&self, context: ContextId) -> Arc<JobManager> {
        self.managers.lock().entry(context).or_insert_with(|| Arc::new(JobManager::new())).clone()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register(
        &self, context: ContextId, principal: PrincipalId, actor: PrincipalId,
        command: BlockId, output: BlockId, source: &str, epoch: Option<i64>,
    ) -> OperationResult<ShellOperationReceipt> {
        let id = Uuid::now_v7().to_string();
        self.db.lock().conn_for_ledger().execute(
            "INSERT INTO shell_operations(operation_id,context_id,principal_id,actor_id,
             command_block_id,output_block_id,source,continuation_epoch,created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            rusqlite::params![id, context.as_bytes(), principal.as_bytes(), actor.as_bytes(),
                command.to_key(), output.to_key(), source, epoch, kaijutsu_types::now_millis() as i64],
        ).map_err(|e| e.to_string())?;
        Ok(ShellOperationReceipt {
            operation_id: id, context_id: context, command_block_id: command,
            output_block_id: output, ask_id: None, job_id: None,
        })
    }

    pub fn mark_waiting(&self, id: &str, ask: &str) -> OperationResult<()> {
        let changed = self.db.lock().conn_for_ledger().execute(
            "UPDATE shell_operations SET ask_id=?2 WHERE operation_id=?1
             AND completed_at IS NULL AND (ask_id IS NULL OR ask_id=?2)",
            rusqlite::params![id, ask],
        ).map_err(|e| e.to_string())?;
        require_changed(changed, id)
    }

    pub fn attach_job(&self, id: &str, job: JobId, manager: Arc<JobManager>) -> OperationResult<()> {
        let changed = self.db.lock().conn_for_ledger().execute(
            "UPDATE shell_operations SET job_id=?2 WHERE operation_id=?1
             AND completed_at IS NULL AND job_id IS NULL",
            rusqlite::params![id, job.to_string()],
        ).map_err(|e| e.to_string())?;
        require_changed(changed, id)?;
        self.jobs.lock().insert(id.to_owned(), (job, manager));
        Ok(())
    }

    pub fn complete(&self, id: &str, mut envelope: ShellEnvelope) -> OperationResult<bool> {
        if matches!(envelope.status, ShellStatus::Running | ShellStatus::Waiting) {
            return Err("cannot complete a shell operation with a nonterminal receipt".into());
        }
        envelope.operation_id = Some(id.to_owned());
        let json = serde_json::to_string(&envelope).map_err(|e| e.to_string())?;
        let db = self.db.lock();
        let conn = db.conn_for_ledger();
        let changed = conn.execute(
            "UPDATE shell_operations SET completed_at=?2,envelope_json=?3
             WHERE operation_id=?1 AND completed_at IS NULL",
            rusqlite::params![id, kaijutsu_types::now_millis() as i64, json],
        ).map_err(|e| e.to_string())?;
        if changed == 0 {
            let prior: Option<String> = conn.query_row(
                "SELECT envelope_json FROM shell_operations WHERE operation_id=?1",
                [id], |row| row.get(0),
            ).optional().map_err(|e| e.to_string())?;
            if prior.as_deref() != Some(json.as_str()) {
                return Err(format!("shell operation {id} is missing or already has a different result"));
            }
        }
        self.jobs.lock().remove(id);
        Ok(changed == 1)
    }

    pub fn complete_from_ask(&self, ask: &str, envelope: ShellEnvelope) -> OperationResult<()> {
        let id: Option<String> = self.db.lock().conn_for_ledger().query_row(
            "SELECT operation_id FROM shell_operations WHERE ask_id=?1", [ask], |row| row.get(0),
        ).optional().map_err(|e| e.to_string())?;
        match id {
            Some(id) => self.complete(&id, envelope).map(|_| ()),
            None => Err(format!("ask {ask} has no shell operation")),
        }
    }

    pub fn get(&self, id: &str, context: ContextId) -> OperationResult<Option<ShellOperationState>> {
        self.lookup("operation_id", id, context)
    }

    pub fn get_by_output(&self, output: &BlockId, context: ContextId) -> OperationResult<Option<ShellOperationState>> {
        self.lookup("output_block_id", &output.to_key(), context)
    }

    pub fn get_by_ask(&self, ask: &str, context: ContextId) -> OperationResult<Option<ShellOperationState>> {
        self.lookup("ask_id", ask, context)
    }

    fn lookup(&self, column: &str, value: &str, context: ContextId) -> OperationResult<Option<ShellOperationState>> {
        let db = self.db.lock();
        db.conn_for_ledger().query_row(
            &format!("{SELECT_STATE} WHERE {column}=?1 AND context_id=?2"),
            rusqlite::params![value, context.as_bytes()], decode_state,
        ).optional().map_err(|e| e.to_string())
    }

    pub fn list_for_context(&self, context: ContextId) -> OperationResult<Vec<ShellOperationState>> {
        let db = self.db.lock();
        let mut stmt = db.conn_for_ledger().prepare(
            &format!("{SELECT_STATE} WHERE context_id=?1 ORDER BY created_at,operation_id"),
        ).map_err(|e| e.to_string())?;
        stmt.query_map([context.as_bytes()], decode_state).map_err(|e| e.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())
    }

    pub async fn cancel(&self, id: &str, context: ContextId) -> OperationResult<bool> {
        let state = self.get(id, context)?.ok_or_else(|| format!("no shell operation {id} in this context"))?;
        if state.completed_at.is_some() { return Ok(false); }
        let (job, manager) = self.jobs.lock().get(id).cloned()
            .ok_or_else(|| format!("shell operation {id} has no cancellable kaish job; an ask can be denied through kj ledger"))?;
        Ok(manager.mark_killed_and_cancel(job, false).await)
    }

    pub async fn cancel_all_for_context(&self, context: ContextId) -> OperationResult<()> {
        let manager = self.context_job_manager(context);
        for job in manager.list().await {
            if matches!(job.status, kaish_kernel::scheduler::JobStatus::Running | kaish_kernel::scheduler::JobStatus::Stopped) {
                manager.mark_killed_and_cancel(job.id, false).await;
            }
        }
        Ok(())
    }

    pub fn abandon_unfinished(&self) -> OperationResult<usize> {
        let mut envelope = ShellEnvelope::new(ShellStatus::Error);
        envelope.error = Some("kernel restarted before the shell operation finished".into());
        let ids: Vec<String> = {
            let db = self.db.lock();
            let mut stmt = db.conn_for_ledger().prepare(
                "SELECT operation_id FROM shell_operations WHERE completed_at IS NULL",
            ).map_err(|e| e.to_string())?;
            stmt.query_map([], |row| row.get(0)).map_err(|e| e.to_string())?
                .collect::<rusqlite::Result<_>>().map_err(|e| e.to_string())?
        };
        for id in &ids { self.complete(id, envelope.clone())?; }
        Ok(ids.len())
    }

    pub fn summary_by_context(&self) -> OperationResult<HashMap<ContextId, ShellOperationSummary>> {
        let states = {
            let db = self.db.lock();
            let mut stmt = db.conn_for_ledger().prepare(SELECT_STATE).map_err(|e| e.to_string())?;
            stmt.query_map([], decode_state).map_err(|e| e.to_string())?
                .collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())?
        };
        let mut summaries: HashMap<ContextId, ShellOperationSummary> = HashMap::new();
        for state in states {
            let summary = summaries.entry(state.receipt.context_id).or_default();
            if let Some(finished) = state.completed_at {
                if summary.last_finished.as_ref().is_none_or(|last| last.finished_at_unix_ms <= finished as u64) {
                    summary.last_finished = Some(ShellOperationFinishedSummary {
                        status: "exited",
                        exit_code: state.envelope.as_ref().and_then(|e| e.exit_code)
                            .map(|code| code.clamp(i32::MIN as i64, i32::MAX as i64) as i32),
                        finished_at_unix_ms: finished as u64,
                    });
                }
            } else if state.receipt.ask_id.is_none() || state.receipt.job_id.is_some() {
                summary.running_count += 1;
                let started = state.created_at as u64;
                summary.oldest_running_started_at_unix_ms = Some(
                    summary.oldest_running_started_at_unix_ms.map_or(started, |old| old.min(started)),
                );
            }
        }
        Ok(summaries)
    }
}

const SELECT_STATE: &str = "SELECT operation_id,context_id,command_block_id,output_block_id,
    ask_id,job_id,source,created_at,continuation_epoch,completed_at,envelope_json FROM shell_operations";

fn decode_state(row: &rusqlite::Row<'_>) -> rusqlite::Result<ShellOperationState> {
    let invalid = |index, message: &str| rusqlite::Error::FromSqlConversionFailure(
        index, rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, message.to_owned())),
    );
    let context: Vec<u8> = row.get(1)?;
    let command: String = row.get(2)?;
    let output: String = row.get(3)?;
    let envelope: Option<String> = row.get(10)?;
    Ok(ShellOperationState {
        receipt: ShellOperationReceipt {
            operation_id: row.get(0)?,
            context_id: ContextId::try_from_slice(&context).ok_or_else(|| invalid(1, "invalid operation context"))?,
            command_block_id: BlockId::from_key(&command).ok_or_else(|| invalid(2, "invalid operation command block"))?,
            output_block_id: BlockId::from_key(&output).ok_or_else(|| invalid(3, "invalid operation output block"))?,
            ask_id: row.get(4)?, job_id: row.get(5)?,
        },
        source: row.get(6)?, created_at: row.get(7)?, continuation_epoch: row.get(8)?, completed_at: row.get(9)?,
        envelope: envelope.map(|json| serde_json::from_str(&json)
            .map_err(|e| invalid(10, &e.to_string()))).transpose()?,
    })
}

fn require_changed(changed: usize, id: &str) -> OperationResult<()> {
    if changed == 1 { Ok(()) } else { Err(format!("shell operation {id} is missing or cannot make that transition")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn register(registry: &ShellOperationRegistry, context: ContextId) -> ShellOperationReceipt {
        let principal = PrincipalId::new();
        registry.register(context, principal, principal,
            BlockId::new(context, principal, 1), BlockId::new(context, principal, 2),
            "echo exact", Some(7)).unwrap()
    }

    #[test]
    fn receipts_and_results_survive_registry_recreation_and_are_context_scoped() {
        let db = Arc::new(Mutex::new(KernelDb::open(":memory:").unwrap()));
        let context = ContextId::new();
        let registry = ShellOperationRegistry::new(db.clone()).unwrap();
        let receipt = register(&registry, context);
        registry.mark_waiting(&receipt.operation_id, "ask-1").unwrap();
        assert!(registry.get(&receipt.operation_id, ContextId::new()).unwrap().is_none());
        assert!(registry.list_for_context(ContextId::new()).unwrap().is_empty());
        let mut envelope = ShellEnvelope::new(ShellStatus::Done);
        envelope.stdout = "exact\n".into();
        envelope.exit_code = Some(0);
        assert!(registry.complete(&receipt.operation_id, envelope.clone()).unwrap());
        assert!(!registry.complete(&receipt.operation_id, envelope).unwrap());
        drop(registry);
        let registry = ShellOperationRegistry::new(db).unwrap();
        let state = registry.get_by_ask("ask-1", context).unwrap().unwrap();
        assert_eq!(state.receipt.operation_id, receipt.operation_id);
        assert_eq!(state.continuation_epoch, Some(7));
        assert_eq!(state.envelope.unwrap().stdout, "exact\n");
        assert!(state.completed_at.is_some());
        assert!(registry.get_by_output(&receipt.output_block_id, context).unwrap().is_some());
    }

    #[test]
    fn missing_and_terminal_operations_refuse_new_transitions() {
        let db = Arc::new(Mutex::new(KernelDb::open(":memory:").unwrap()));
        let registry = ShellOperationRegistry::new(db).unwrap();
        assert!(registry.mark_waiting("missing", "ask").is_err());
        assert!(registry.complete("missing", ShellEnvelope::new(ShellStatus::Done)).is_err());
        let receipt = register(&registry, ContextId::new());
        assert!(registry.complete(&receipt.operation_id, ShellEnvelope::new(ShellStatus::Waiting)).is_err());
        registry.complete(&receipt.operation_id, ShellEnvelope::new(ShellStatus::Done)).unwrap();
        assert!(registry.mark_waiting(&receipt.operation_id, "new-ask").is_err());
        assert!(registry.complete(&receipt.operation_id, ShellEnvelope::new(ShellStatus::Error)).is_err());
    }

    #[test]
    fn restart_marks_unfinished_work_without_discarding_receipts() {
        let db = Arc::new(Mutex::new(KernelDb::open(":memory:").unwrap()));
        let registry = ShellOperationRegistry::new(db.clone()).unwrap();
        let context = ContextId::new();
        let receipt = register(&registry, context);
        drop(registry);
        let registry = ShellOperationRegistry::new(db).unwrap();
        assert_eq!(registry.abandon_unfinished().unwrap(), 1);
        assert_eq!(registry.abandon_unfinished().unwrap(), 0);
        let state = registry.get(&receipt.operation_id, context).unwrap().unwrap();
        assert!(state.completed_at.is_some());
        assert!(state.envelope.unwrap().error.unwrap().contains("restarted"));
    }
}

impl crate::Kernel {
    /// Record completion separately from the receipt and resume an eligible model.
    pub async fn complete_async_shell_operation(
        &self, id: &str, context: ContextId, principal: PrincipalId, actor: PrincipalId,
        envelope: ShellEnvelope,
    ) -> OperationResult<()> {
        let state = self.shell_operations().get(id, context)?
            .ok_or_else(|| format!("shell operation {id} is missing"))?;
        if !self.shell_operations().complete(id, envelope.clone())? { return Ok(()); }
        let row = self.kernel_db().lock().get_context(context).map_err(|e| e.to_string())?
            .ok_or_else(|| format!("shell operation context {context} is missing"))?;
        if row.is_archived() || row.played_by != Some(actor) { return Ok(()); }
        let message = format!(
            "Shell operation {id} completed with status {} and exit code {:?}. Output block: {}.\n{}",
            envelope.status.as_str(), envelope.exit_code, state.receipt.output_block_id.to_key(),
            envelope.readable_output(),
        );
        let after = self.blocks().last_block_id(context);
        let notification = self.blocks().insert_block_as(
            context, None, after.as_ref(), kaijutsu_types::Role::User,
            kaijutsu_types::BlockKind::Text, message.clone(), kaijutsu_types::Status::Done,
            kaijutsu_types::ContentType::Plain, Some(PrincipalId::system()),
        ).map_err(|e| e.to_string())?;
        let Some(epoch) = state.continuation_epoch else { return Ok(()); };
        let window = self.gate_resume_window().await?;
        let window_ms = i64::try_from(window.as_millis()).map_err(|e| e.to_string())?;
        if !self.turn_in_flight(context)
            && self.kernel_db().lock().automatic_resume_allowed(
                context, epoch, kaijutsu_types::now_millis() as i64, window_ms,
            ).map_err(|e| e.to_string())?
        {
            self.turn_flows().publish(crate::flows::TurnFlow::Requested {
                context_id: context, after_block_id: notification, content: message,
                principal_id: principal, model: None, continuation_epoch: Some(epoch),
            });
        }
        Ok(())
    }
}
