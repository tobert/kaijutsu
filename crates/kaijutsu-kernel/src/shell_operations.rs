//! Durable shell receipts, result reviews, and context-owned kaish job managers.

use std::collections::HashMap;
use std::sync::Arc;

use kaijutsu_types::{BlockId, ContextId, PrincipalId};
use kaijutsu_types::shell_envelope::{ShellEnvelope, ShellStatus};
use kaish_kernel::scheduler::{JobId, JobManager};
use parking_lot::Mutex;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use uuid::Uuid;

use crate::kernel_db::{KernelDb, KernelDbError, KernelDbResult};
use crate::runtime::command_outcome::CommandOutcome;

type OperationResult<T> = Result<T, String>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellOperationReceipt {
    pub operation_id: String,
    pub context_id: ContextId,
    pub command_block_id: BlockId,
    pub output_block_id: BlockId,
    /// Most recent ask, retained after completion. Earlier asks can resolve
    /// the same operation through their pair or result-review links.
    pub ask_id: Option<String>,
    pub job_id: Option<String>,
}

/// The command pair and durable owner accepted before execution or waiting.
pub(crate) struct ShellOperationStart<'a> {
    /// Reserve a model completion notice with this receipt's admission.
    pub notify: bool,
    pub context: ContextId,
    /// The requester is recorded on the receipt; the performer authors the command.
    pub principal: PrincipalId,
    pub actor: PrincipalId,
    pub source: &'a str,
    pub tool: &'a str,
    pub input: serde_json::Value,
    pub kind: kaijutsu_types::ToolKind,
    pub role: kaijutsu_types::Role,
    pub excluded: bool,
    /// Running after admission; Waiting while an ask still blocks execution.
    pub status: kaijutsu_types::Status,
    pub ask: Option<(&'a str, crate::PairOwner)>,
}

impl ShellOperationStart<'_> {
    /// The caller holds the context's document guard, serializing setup retries.
    pub(crate) fn existing(&self, db: &KernelDb) -> crate::kernel_db::KernelDbResult<Option<ShellOperationReceipt>> {
        let Some((ask, owner)) = self.ask else { return Ok(None); };
        let prior = db.conn_for_ledger().query_row(&format!("{SELECT_STATE} WHERE {ASK_OPERATION}"),
            [ask], decode_state).optional()?;
        let Some(prior) = prior else { return Ok(None); };
        let same_identity: bool = db.conn_for_ledger().query_row(
            "SELECT o.principal_id=?2 AND o.actor_id=?3 AND a.context_id=o.context_id
                AND a.principal_id=o.principal_id AND a.actor_id=o.actor_id
                AND a.command_block_id=o.command_block_id AND a.output_block_id=o.output_block_id
                AND a.pair_owner=?4
             FROM shell_operations o JOIN approvals a ON a.request_id=?5 WHERE o.operation_id=?1",
            rusqlite::params![prior.receipt.operation_id, self.principal.as_bytes(), self.actor.as_bytes(), owner.as_str(), ask],
            |row| row.get(0),
        )?;
        if prior.receipt.context_id != self.context || prior.source != self.source || !same_identity {
            return Err(crate::kernel_db::KernelDbError::Validation("ask already owns a different shell operation".into()));
        }
        Ok(Some(prior.receipt))
    }

    pub(crate) fn record(&self, db: &KernelDb, receipt: &ShellOperationReceipt) -> crate::kernel_db::KernelDbResult<()> {
        let epoch = db.continuation_epoch(self.context)?;
        insert_operation(db, receipt, self.principal, self.actor, self.source, epoch)?;
        if self.notify {
            crate::runtime::completion_notice::reserve(db,
                &crate::runtime::completion_notice::Source::Shell(receipt.operation_id.clone()), None)?;
        }
        if let Some((ask, owner)) = self.ask {
            db.link_ask_blocks(ask, &receipt.command_block_id, &receipt.output_block_id, owner)?;
            if self.status == kaijutsu_types::Status::Waiting { db.release_approval_pair(ask)?; }
        }
        Ok(())
    }
}

fn insert_operation(
    db: &KernelDb, receipt: &ShellOperationReceipt, principal: PrincipalId,
    actor: PrincipalId, source: &str, epoch: Option<i64>,
) -> KernelDbResult<()> {
    db.conn_for_ledger().execute(
        "INSERT INTO shell_operations(operation_id,context_id,principal_id,actor_id,
         command_block_id,output_block_id,source,continuation_epoch,ask_id,created_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        rusqlite::params![receipt.operation_id, receipt.context_id.as_bytes(), principal.as_bytes(), actor.as_bytes(),
            receipt.command_block_id.to_key(), receipt.output_block_id.to_key(), source, epoch,
            receipt.ask_id, kaijutsu_types::now_millis() as i64],
    )?;
    Ok(())
}

/// Join the caller's transaction: an executable ask and its pair share a
/// durable execution owner before Waiting is published. Existing links are
/// immutable; repeating the same link does not create another operation.
pub(crate) fn link_approval_pair(
    db: &KernelDb, request: &str, command: &BlockId, output: &BlockId, owner: crate::PairOwner,
) -> KernelDbResult<()> {
    let row = db.get_approval(request)?.ok_or_else(|| KernelDbError::NotFound(request.into()))?;
    let context = command.context_id;
    let actor = row.actor_id.as_deref().and_then(PrincipalId::try_from_slice)
        .ok_or_else(|| KernelDbError::Validation("approval pair has no valid performer".into()))?;
    if row.context_id != context.as_bytes() || output.context_id != context || command.principal_id != actor {
        return Err(KernelDbError::Validation("approval pair does not match its context and performer".into()));
    }
    let command_key = command.to_key();
    let output_key = output.to_key();
    let already_linked = match (row.command_block_id.as_deref(), row.output_block_id.as_deref(), row.pair_owner) {
        (None, None, None) => false,
        (Some(prior_command), Some(prior_output), Some(prior_owner))
            if prior_command == command_key && prior_output == output_key && prior_owner == owner => true,
        _ => return Err(KernelDbError::Validation("approval already belongs to a different block pair or owner".into())),
    };
    if let Some(source) = row.exec_source.as_deref() {
        let principal = PrincipalId::try_from_slice(&row.principal_id)
            .ok_or_else(|| KernelDbError::Validation("approval pair has no valid requester".into()))?;
        let prior = db.conn_for_ledger().query_row(&format!("{SELECT_STATE} WHERE output_block_id=?1"),
            [&output_key], decode_state).optional()?;
        if let Some(prior) = prior {
            let same_identity: bool = db.conn_for_ledger().query_row(
                "SELECT principal_id=?2 AND actor_id=?3 FROM shell_operations WHERE operation_id=?1",
                rusqlite::params![prior.receipt.operation_id, principal.as_bytes(), actor.as_bytes()], |r| r.get(0))?;
            if prior.receipt.context_id != context || prior.receipt.command_block_id != *command || !same_identity
                || (!already_linked && prior.receipt.ask_id.as_deref().is_some_and(|ask| ask != request))
                || (prior.completed_at.is_some() && prior.receipt.ask_id.is_none()) {
                return Err(KernelDbError::Validation("approval pair already belongs to a different execution".into()));
            }
            if prior.receipt.ask_id.is_none() {
                db.conn_for_ledger().execute("UPDATE shell_operations SET ask_id=?2 WHERE operation_id=?1",
                    rusqlite::params![prior.receipt.operation_id, request])?;
            }
        } else {
            let receipt = ShellOperationReceipt {
                operation_id: uuid::Uuid::now_v7().to_string(), context_id: context,
                command_block_id: *command, output_block_id: *output,
                ask_id: Some(request.into()), job_id: None,
            };
            insert_operation(db, &receipt, principal, actor, source, row.continuation_epoch)?;
        }
    }
    approval_ledger::ask::link_ask_blocks(db.conn_for_ledger(), request, &command_key, &output_key, owner)?;
    Ok(())
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

/// Captured execution and its eventual result, shared by every ask in a review.
#[derive(Debug, Clone)]
pub struct ResultReviewState {
    pub review_id: String,
    pub operation_id: Option<String>,
    pub captured: CommandOutcome,
    pub settled: Option<CommandOutcome>,
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RetentionKey { Operation(String), Review(String) }

struct PendingRetention {
    json: String,
    error: String,
    attempted: std::time::Instant,
}

pub struct ShellOperationRegistry {
    db: Arc<Mutex<KernelDb>>,
    managers: Mutex<HashMap<ContextId, Arc<JobManager>>>,
    jobs: Mutex<HashMap<String, (JobId, Arc<JobManager>)>>,
    pending_retention: Mutex<HashMap<RetentionKey, PendingRetention>>,
}

fn initialize_review_store(conn: &rusqlite::Connection) -> OperationResult<()> {
    let columns: Vec<String> = conn.prepare("PRAGMA table_info(shell_result_reviews)").map_err(|e| e.to_string())?
        .query_map([], |row| row.get(1)).map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<_>>().map_err(|e| e.to_string())?;
    let legacy = !columns.is_empty() && !columns.iter().any(|name| name == "review_id");
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    if legacy { tx.execute_batch("ALTER TABLE shell_result_reviews RENAME TO shell_result_reviews_legacy").map_err(|e| e.to_string())?; }
    tx.execute_batch("CREATE TABLE IF NOT EXISTS shell_result_reviews (
        review_id TEXT PRIMARY KEY,
        operation_id TEXT UNIQUE REFERENCES shell_operations(operation_id),
        context_id BLOB NOT NULL,
        principal_id BLOB NOT NULL,
        actor_id BLOB NOT NULL,
        outcome_json TEXT NOT NULL,
        final_json TEXT
    );
    CREATE TABLE IF NOT EXISTS shell_result_review_asks (
        request_id TEXT PRIMARY KEY REFERENCES approvals(request_id),
        review_id TEXT NOT NULL REFERENCES shell_result_reviews(review_id)
    );
    CREATE INDEX IF NOT EXISTS shell_result_review_asks_owner ON shell_result_review_asks(review_id);")
        .map_err(|e| e.to_string())?;
    if legacy {
        let rows: Vec<(String, String)> = tx.prepare("SELECT operation_id,outcome_json FROM shell_result_reviews_legacy")
            .map_err(|e| e.to_string())?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?.collect::<rusqlite::Result<_>>().map_err(|e| e.to_string())?;
        for (id, json) in rows {
            let outcome: CommandOutcome = serde_json::from_str(&json).map_err(|e| e.to_string())?;
            let ask = outcome.envelope().ask_id.ok_or("legacy result review has no ask")?;
            let changed = tx.execute("INSERT INTO shell_result_reviews(review_id,operation_id,context_id,principal_id,actor_id,outcome_json)
                SELECT operation_id,operation_id,context_id,principal_id,actor_id,?2 FROM shell_operations WHERE operation_id=?1",
                rusqlite::params![id, json]).map_err(|e| e.to_string())?;
            require_changed(changed, &id)?;
            tx.execute("INSERT INTO shell_result_review_asks(request_id,review_id) VALUES(?1,?2)",
                rusqlite::params![ask, id]).map_err(|e| e.to_string())?;
        }
        tx.execute_batch("DROP TABLE shell_result_reviews_legacy").map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
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
             CREATE TABLE IF NOT EXISTS shell_operation_outcomes (
                operation_id TEXT PRIMARY KEY REFERENCES shell_operations(operation_id),
                outcome_json TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS shell_operation_projections (
                operation_id TEXT PRIMARY KEY REFERENCES shell_operation_outcomes(operation_id)
             );
             CREATE INDEX IF NOT EXISTS shell_operations_context ON shell_operations(context_id);
             CREATE UNIQUE INDEX IF NOT EXISTS shell_operations_ask ON shell_operations(ask_id);"
        ).map_err(|e| e.to_string())?;
        initialize_review_store(db.lock().conn_for_ledger())?;
        Ok(Self { db, managers: Mutex::new(HashMap::new()), jobs: Mutex::new(HashMap::new()), pending_retention: Mutex::new(HashMap::new()) })
    }

    pub fn context_job_manager(&self, context: ContextId) -> Arc<JobManager> {
        self.managers.lock().entry(context).or_insert_with(|| Arc::new(JobManager::new())).clone()
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register(
        &self, context: ContextId, principal: PrincipalId, actor: PrincipalId,
        command: BlockId, output: BlockId, source: &str, epoch: Option<i64>,
    ) -> OperationResult<ShellOperationReceipt> {
        let receipt = ShellOperationReceipt {
            operation_id: Uuid::now_v7().to_string(), context_id: context, command_block_id: command,
            output_block_id: output, ask_id: None, job_id: None,
        };
        insert_operation(&self.db.lock(), &receipt, principal, actor, source, epoch).map_err(|e| e.to_string())?;
        Ok(receipt)
    }

    #[cfg(test)]
    pub(crate) fn mark_waiting(&self, id: &str, ask: &str) -> OperationResult<()> {
        Self::mark_waiting_in(&self.db.lock(), id, ask)
    }

    pub(crate) fn mark_waiting_in(db: &KernelDb, id: &str, ask: &str) -> OperationResult<()> {
        let changed = db.conn_for_ledger().execute(
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

    #[cfg(test)]
    pub fn complete(&self, id: &str, envelope: ShellEnvelope) -> OperationResult<bool> {
        self.complete_record(id, envelope, None)
    }

    #[cfg(test)]
    pub(crate) fn checkpoint_result_review(
        &self, review_id: &str, operation_id: Option<&str>, call: &crate::mcp::CallContext,
        outcome: &CommandOutcome,
    ) -> OperationResult<()> {
        self.db.lock().in_transaction(|db| Self::checkpoint_result_review_in(
            db.conn_for_ledger(), review_id, operation_id, call, outcome)).map_err(|e| e.to_string())
    }

    /// Link captured execution to its ask inside the ask's transaction. Calls
    /// without a transcript pair retain their invocation identity without a receipt.
    pub(crate) fn checkpoint_result_review_in(
        conn: &rusqlite::Connection, review_id: &str, operation_id: Option<&str>, call: &crate::mcp::CallContext,
        outcome: &CommandOutcome,
    ) -> KernelDbResult<()> {
        if conn.is_autocommit() { return Err(KernelDbError::Validation("result review checkpoint requires the ask transaction".into())); }
        let envelope = outcome.envelope();
        if envelope.status != ShellStatus::Waiting { return Err(KernelDbError::Validation("result review checkpoint is not waiting".into())); }
        let ask = envelope.ask_id.ok_or_else(|| KernelDbError::Validation("result review checkpoint has no ask".into()))?;
        let json = serde_json::to_string(outcome).map_err(|e| KernelDbError::Validation(e.to_string()))?;
        let valid: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM approvals WHERE request_id=?1 AND context_id=?2
             AND principal_id=?3 AND actor_id=?4 AND origin='hook_result' AND exec_source IS NULL)",
            rusqlite::params![ask, call.context_id.as_bytes(), call.principal_id.as_bytes(), call.actor_id.as_bytes()],
            |row| row.get(0),
        )?;
        if !valid { return Err(KernelDbError::Validation("ask is not a result review for this invocation".into())); }
        let prior: Option<String> = conn.query_row("SELECT outcome_json FROM shell_result_reviews WHERE review_id=?1",
            [review_id], |r| r.get(0)).optional()?;
        if let Some(prior) = prior {
            let mut captured: CommandOutcome = serde_json::from_str(&prior).map_err(|e| KernelDbError::Validation(e.to_string()))?;
            let mut incoming = outcome.clone();
            captured.hook = None;
            incoming.hook = None;
            if serde_json::to_string(&captured).map_err(|e| KernelDbError::Validation(e.to_string()))?
                != serde_json::to_string(&incoming).map_err(|e| KernelDbError::Validation(e.to_string()))?
            {
                return Err(KernelDbError::Validation("result review checkpoint cannot rewrite captured execution".into()));
            }
        }
        if let Some(id) = operation_id {
            let changed = conn.execute(
                "UPDATE shell_operations SET ask_id=?2 WHERE operation_id=?1 AND completed_at IS NULL
                 AND context_id=?3 AND principal_id=?4 AND actor_id=?5
                 AND operation_id NOT IN (SELECT operation_id FROM shell_operation_outcomes)",
                rusqlite::params![id, ask, call.context_id.as_bytes(), call.principal_id.as_bytes(), call.actor_id.as_bytes()],
            )?;
            require_changed(changed, id).map_err(KernelDbError::Validation)?;
        }
        let changed = conn.execute(
            "INSERT INTO shell_result_reviews(review_id,operation_id,context_id,principal_id,actor_id,outcome_json)
             VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(review_id) DO UPDATE SET outcome_json=excluded.outcome_json
             WHERE shell_result_reviews.final_json IS NULL AND shell_result_reviews.operation_id IS excluded.operation_id
             AND shell_result_reviews.context_id=excluded.context_id AND shell_result_reviews.principal_id=excluded.principal_id
             AND shell_result_reviews.actor_id=excluded.actor_id",
            rusqlite::params![review_id, operation_id, call.context_id.as_bytes(), call.principal_id.as_bytes(), call.actor_id.as_bytes(), json],
        )?;
        require_changed(changed, review_id).map_err(KernelDbError::Validation)?;
        conn.execute("INSERT OR IGNORE INTO shell_result_review_asks(request_id,review_id) VALUES(?1,?2)",
            rusqlite::params![ask, review_id])?;
        let owner: String = conn.query_row("SELECT review_id FROM shell_result_review_asks WHERE request_id=?1", [&ask], |r| r.get(0))?;
        if owner != review_id { return Err(KernelDbError::Validation("result ask already belongs to another invocation".into())); }
        Ok(())
    }

    /// Finish a receipt-free review if it opened an ask. Calls without an ask
    /// retain no record. A published result is immutable, including on retry.
    pub(crate) fn finish_result_review(&self, review_id: &str, outcome: &CommandOutcome) -> OperationResult<()> {
        if !matches!(outcome.block_status(), kaijutsu_types::Status::Done | kaijutsu_types::Status::Error) {
            return Err("result review completion is not terminal".into());
        }
        self.retain_terminal(RetentionKey::Review(review_id.into()), outcome)
    }

    fn finish_result_review_durable(&self, review_id: &str, json: &str) -> KernelDbResult<()> {
        let db = self.db.lock();
        let tx = db.conn_for_ledger().unchecked_transaction()?;
        let prior: Option<(Option<String>, Option<String>)> = tx.query_row(
            "SELECT operation_id,final_json FROM shell_result_reviews WHERE review_id=?1",
            [review_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        if let Some((operation, prior)) = prior {
            if operation.is_some() { return Err(KernelDbError::Validation("tracked result review requires operation outcome preparation".into())); }
            if let Some(prior) = prior && prior != json { return Err(KernelDbError::Validation("result review already has a different outcome".into())); }
        }
        tx.execute("UPDATE shell_result_reviews SET final_json=?2 WHERE review_id=?1 AND final_json IS NULL",
            rusqlite::params![review_id, json])?;
        tx.commit().map_err(Into::into)
    }

    pub fn result_review_for_ask(&self, ask: &str, context: ContextId) -> OperationResult<Option<ResultReviewState>> {
        let row: Option<(String, Option<String>, String, Option<String>)> = self.db.lock().conn_for_ledger().query_row(
            "SELECT r.review_id,r.operation_id,r.outcome_json,r.final_json FROM shell_result_reviews r
             JOIN shell_result_review_asks a USING(review_id) WHERE a.request_id=?1 AND r.context_id=?2",
            rusqlite::params![ask, context.as_bytes()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).optional().map_err(|e| e.to_string())?;
        row.map(|(review_id, operation_id, captured, settled)| Ok(ResultReviewState {
            review_id, operation_id,
            captured: serde_json::from_str(&captured).map_err(|e| e.to_string())?,
            settled: settled.map(|json| serde_json::from_str(&json)).transpose().map_err(|e| e.to_string())?,
        })).transpose()
    }

    /// A restart cannot resume the in-memory hook snapshot. Retain execution
    /// and report interrupted review instead of executing or accepting it again.
    pub(crate) fn recover_result_reviews(&self) -> OperationResult<()> {
        let reviews: Vec<(String, Option<String>, String)> = {
            let db = self.db.lock();
            let mut stmt = db.conn_for_ledger().prepare("SELECT review_id,operation_id,outcome_json FROM shell_result_reviews WHERE final_json IS NULL")
                .map_err(|e| e.to_string())?;
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).map_err(|e| e.to_string())?
                .collect::<rusqlite::Result<_>>().map_err(|e| e.to_string())?
        };
        for (review_id, operation, json) in reviews {
            let mut outcome: CommandOutcome = serde_json::from_str(&json).map_err(|e| e.to_string())?;
            let reason = "kernel restarted during result review; captured execution was retained and source was not run again";
            let ask = outcome.envelope().ask_id.ok_or("interrupted review lost its ask id")?;
            {
                let db = self.db.lock();
                let row = db.get_approval(&ask).map_err(|e| e.to_string())?.ok_or("interrupted review lost its ask")?;
                if matches!(row.status, approval_ledger::types::ApprovalStatus::Pending | approval_ledger::types::ApprovalStatus::Claimed) {
                    approval_ledger::decide::abandon(db.conn_for_ledger(), &ask, Some(reason)).map_err(|e| e.to_string())?;
                }
            }
            outcome.settlement_error = Some(reason.into());
            match operation {
                Some(id) => self.prepare_settlement(&id, &outcome)?,
                None => self.finish_result_review(&review_id, &outcome)?,
            }
        }
        Ok(())
    }

    /// Retain a terminal outcome before its projections can fail. An existing
    /// record is immutable; a retry must carry exactly the same outcome.
    pub fn prepare_settlement(&self, id: &str, outcome: &CommandOutcome) -> OperationResult<()> {
        if matches!(outcome.envelope().status, ShellStatus::Running | ShellStatus::Waiting) {
            return Err("cannot prepare terminal settlement for a running or waiting command".into());
        }
        self.retain_terminal(RetentionKey::Operation(id.into()), outcome)
    }

    fn prepare_settlement_durable(&self, id: &str, json: &str) -> KernelDbResult<()> {
        let db = self.db.lock();
        let tx = db.conn_for_ledger().unchecked_transaction()?;
        let completed: Option<Option<i64>> = tx.query_row(
            "SELECT completed_at FROM shell_operations WHERE operation_id=?1", [id], |row| row.get(0),
        ).optional()?;
        let completed = completed.ok_or_else(|| KernelDbError::NotFound(format!("shell operation {id} is missing")))?;
        let prior: Option<String> = tx.query_row(
            "SELECT outcome_json FROM shell_operation_outcomes WHERE operation_id=?1", [id], |row| row.get(0),
        ).optional()?;
        match prior {
            Some(prior) if prior != json => return Err(KernelDbError::Validation(format!("shell operation {id} already has a different outcome"))),
            Some(_) => {}
            None if completed.is_some() => return Err(KernelDbError::Validation(format!("shell operation {id} completed without a recoverable outcome"))),
            None => {
                tx.execute("INSERT INTO shell_operation_outcomes(operation_id,outcome_json) VALUES(?1,?2)",
                    rusqlite::params![id, json])?;
            }
        }
        tx.execute("INSERT OR IGNORE INTO shell_operation_projections(operation_id) VALUES(?1)", [id])
            ?;
        let inconsistent: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM shell_result_reviews WHERE operation_id=?1 AND final_json IS NOT NULL AND final_json != ?2)",
            rusqlite::params![id, json], |r| r.get(0))?;
        if inconsistent { return Err(KernelDbError::Validation("result review already has a different outcome".into())); }
        tx.execute("UPDATE shell_result_reviews SET final_json=?2 WHERE operation_id=?1 AND final_json IS NULL",
            rusqlite::params![id, json])?;
        tx.commit().map_err(Into::into)
    }

    /// Keep the first terminal result in memory if its database write fails.
    /// Lock order is retention, then database; callers must not hold the DB guard.
    fn retain_terminal(&self, key: RetentionKey, outcome: &CommandOutcome) -> OperationResult<()> {
        let json = serde_json::to_string(outcome).map_err(|e| e.to_string())?;
        let mut pending = self.pending_retention.lock();
        if pending.get(&key).is_some_and(|prior| prior.json != json) {
            return Err("terminal result already has a different live retention owner".into());
        }
        let result = match &key {
            RetentionKey::Operation(id) => self.prepare_settlement_durable(id, &json),
            RetentionKey::Review(id) => self.finish_result_review_durable(id, &json),
        };
        match result {
            Ok(()) => { pending.remove(&key); Ok(()) }
            Err(error) => {
                // Domain conflicts do not create retry work. A prior live owner
                // remains available for diagnosis if durable state later conflicts.
                if matches!(&error, KernelDbError::Db(_)) || pending.contains_key(&key) {
                    pending.insert(key, PendingRetention { json, error: error.to_string(), attempted: std::time::Instant::now() });
                }
                Err(error.to_string())
            }
        }
    }

    pub(crate) fn retention_error(&self, key: &RetentionKey) -> Option<String> {
        self.pending_retention.lock().get(key).map(|pending| pending.error.clone())
    }

    pub(crate) fn pending_retention(&self, limit: usize) -> OperationResult<Vec<(RetentionKey, CommandOutcome)>> {
        let mut pending = self.pending_retention.lock();
        let mut entries: Vec<_> = pending.iter().collect();
        entries.sort_by_key(|(_, value)| value.attempted);
        let selected = entries.into_iter().take(limit).map(|(key, value)| Ok((key.clone(),
            serde_json::from_str(&value.json).map_err(|e| e.to_string())?))).collect::<OperationResult<Vec<_>>>()?;
        for (key, _) in &selected { pending.get_mut(key).expect("selected retention owner exists").attempted = std::time::Instant::now(); }
        Ok(selected)
    }

    pub(crate) fn retention_failures(&self) -> Vec<String> {
        self.pending_retention.lock().iter().map(|(key, value)| format!("{key:?}: {}", value.error)).collect()
    }

    pub(crate) fn operation_for_retention(&self, id: &str) -> OperationResult<ShellOperationState> {
        self.db.lock().conn_for_ledger().query_row(&format!("{SELECT_STATE} WHERE operation_id=?1"),
            [id], decode_state).map_err(|e| e.to_string())
    }

    pub fn pending_projections(&self) -> OperationResult<Vec<ShellOperationState>> {
        let db = self.db.lock();
        let mut stmt = db.conn_for_ledger().prepare(
            &format!("{SELECT_STATE} WHERE operation_id IN (SELECT operation_id FROM shell_operation_projections) ORDER BY created_at,operation_id"),
        ).map_err(|e| e.to_string())?;
        stmt.query_map([], decode_state).map_err(|e| e.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())
    }

    pub fn finish_projection(&self, id: &str) -> OperationResult<()> {
        let db = self.db.lock();
        let conn = db.conn_for_ledger();
        let completed: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM shell_operations WHERE operation_id=?1 AND completed_at IS NOT NULL)",
            [id], |row| row.get(0),
        ).map_err(|e| e.to_string())?;
        if !completed { return Err(format!("shell operation {id} has not committed its receipt")); }
        conn.execute("DELETE FROM shell_operation_projections WHERE operation_id=?1", [id]).map_err(|e| e.to_string())?;
        drop(db);
        self.jobs.lock().remove(id);
        Ok(())
    }

    /// Commit the public result against its immutable execution record.
    /// A prepared record must match; an unprepared one is inserted atomically.
    pub fn complete_outcome(&self, id: &str, output: &BlockId, outcome: &CommandOutcome) -> OperationResult<bool> {
        let mut envelope = outcome.envelope();
        envelope.block_id = Some(output.to_key());
        self.complete_record(id, envelope, Some(outcome))
    }

    /// Commit in the block journal's transaction without locking the registry
    /// again. The projection owner retires the live job after publication.
    pub(crate) fn complete_outcome_in(db: &KernelDb, id: &str, output: &BlockId, outcome: &CommandOutcome) -> OperationResult<bool> {
        let mut envelope = outcome.envelope();
        envelope.block_id = Some(output.to_key());
        Self::complete_record_in(db, id, envelope, Some(outcome))
    }

    fn complete_record(&self, id: &str, envelope: ShellEnvelope, outcome: Option<&CommandOutcome>) -> OperationResult<bool> {
        let changed = Self::complete_record_in(&self.db.lock(), id, envelope, outcome)?;
        self.jobs.lock().remove(id);
        Ok(changed)
    }

    pub(crate) fn complete_record_in(db: &KernelDb, id: &str, mut envelope: ShellEnvelope, outcome: Option<&CommandOutcome>) -> OperationResult<bool> {
        if matches!(envelope.status, ShellStatus::Running | ShellStatus::Waiting) {
            return Err("cannot complete a shell operation with a nonterminal receipt".into());
        }
        envelope.operation_id = Some(id.to_owned());
        let json = serde_json::to_string(&envelope).map_err(|e| e.to_string())?;
        let outcome_json = outcome.map(serde_json::to_string).transpose().map_err(|e| e.to_string())?;
        let conn = db.conn_for_ledger();
        let tx = if conn.is_autocommit() { Some(conn.unchecked_transaction().map_err(|e| e.to_string())?) } else { None };
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
        if let Some(outcome_json) = outcome_json {
            if changed == 1 {
                conn.execute("INSERT OR IGNORE INTO shell_operation_outcomes(operation_id,outcome_json) VALUES(?1,?2)",
                    rusqlite::params![id, outcome_json]).map_err(|e| e.to_string())?;
            }
            let prior: Option<String> = conn.query_row(
                "SELECT outcome_json FROM shell_operation_outcomes WHERE operation_id=?1",
                [id], |row| row.get(0),
            ).optional().map_err(|e| e.to_string())?;
            if prior.as_deref() != Some(outcome_json.as_str()) {
                return Err(format!("shell operation {id} already has a different outcome"));
            }
        } else if changed == 1 {
            let prepared: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM shell_operation_outcomes WHERE operation_id=?1)", [id], |row| row.get(0),
            ).map_err(|e| e.to_string())?;
            if prepared { return Err(format!("shell operation {id} has a retained outcome; refusing to replace it")); }
        }
        let source = crate::runtime::completion_notice::Source::Shell(id.into());
        if crate::runtime::completion_notice::read(db, &source).map_err(|e| e.to_string())?.is_some() {
            crate::runtime::completion_notice::prepare(db, &source,
                &crate::runtime::completion_notice::shell_message(id, &envelope)).map_err(|e| e.to_string())?;
        }
        if let Some(tx) = tx { tx.commit().map_err(|e| e.to_string())?; }
        Ok(changed == 1)
    }

    pub fn get(&self, id: &str, context: ContextId) -> OperationResult<Option<ShellOperationState>> {
        self.lookup("operation_id", id, context)
    }

    pub fn get_by_output(&self, output: &BlockId, context: ContextId) -> OperationResult<Option<ShellOperationState>> {
        self.lookup("output_block_id", &output.to_key(), context)
    }

    pub fn get_by_ask(&self, ask: &str, context: ContextId) -> OperationResult<Option<ShellOperationState>> {
        self.db.lock().conn_for_ledger().query_row(
            &format!("{SELECT_STATE} WHERE context_id=?2 AND ({ASK_OPERATION})"),
            rusqlite::params![ask, context.as_bytes()], decode_state,
        ).optional().map_err(|e| e.to_string())
    }

    /// Read the execution record without adding raw output to ordinary receipt polls.
    pub fn outcome(&self, id: &str, context: ContextId) -> OperationResult<Option<CommandOutcome>> {
        let json: Option<String> = self.db.lock().conn_for_ledger().query_row(
            "SELECT outcome_json FROM shell_operation_outcomes JOIN shell_operations USING(operation_id)
             WHERE operation_id=?1 AND context_id=?2",
            rusqlite::params![id, context.as_bytes()], |row| row.get(0),
        ).optional().map_err(|e| e.to_string())?;
        json.map(|json| serde_json::from_str(&json).map_err(|e| e.to_string())).transpose()
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

    /// Startup must project these interruptions into their original pairs.
    /// Known outcomes and unresolved result reviews have separate recovery.
    pub(crate) fn unfinished_without_outcome(&self) -> OperationResult<Vec<ShellOperationState>> {
        let db = self.db.lock();
        let mut stmt = db.conn_for_ledger().prepare(&format!(
            "{SELECT_STATE} WHERE completed_at IS NULL
             AND operation_id NOT IN (SELECT operation_id FROM shell_operation_outcomes)
             AND NOT EXISTS (SELECT 1 FROM shell_result_reviews r WHERE r.operation_id=shell_operations.operation_id AND r.final_json IS NULL)",
        )).map_err(|e| e.to_string())?;
        stmt.query_map([], decode_state).map_err(|e| e.to_string())?
            .collect::<rusqlite::Result<_>>().map_err(|e| e.to_string())
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

// The current ask may be a result review. Original execution asks keep their
// pair link; earlier result asks keep their review link to the same operation.
const ASK_OPERATION: &str = "ask_id=?1 OR (command_block_id,output_block_id) IN
    (SELECT command_block_id,output_block_id FROM approvals WHERE request_id=?1)
    OR operation_id IN (SELECT r.operation_id FROM shell_result_reviews r
        JOIN shell_result_review_asks a USING(review_id) WHERE a.request_id=?1)";

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
    #[tokio::test]
    async fn async_completion_notification_is_repeatable_without_duplicate_blocks() {
        use crate::kj::test_helpers::{test_dispatcher_persistent, register_context};
        let dispatcher = test_dispatcher_persistent().await;
        let kernel = dispatcher.kernel();
        let actor = PrincipalId::new();
        let context = register_context(&dispatcher, Some("completion-once"), None, actor);
        kernel.kernel_db().lock().update_context_review(context, Some(actor), Some(PrincipalId::new())).unwrap();
        kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        let call = crate::mcp::CallContext::new(actor, context, kaijutsu_types::SessionId::new(), kernel.id());
        let receipt = crate::runtime::tool_command::create_operation(kernel, &call, "echo captured", None).unwrap();
        let outcome = CommandOutcome::new(crate::runtime::command_outcome::CommandExecution::Completed(
            kaish_kernel::interpreter::ExecResult::success("captured")), 1);
        crate::runtime::command::settle_outcome(kernel, context, &receipt.command_block_id, &receipt.output_block_id, &outcome, None).unwrap();
        for _ in 0..2 {
            crate::runtime::completion_notice::deliver(kernel, &crate::runtime::completion_notice::Source::Shell(receipt.operation_id.clone()),
                &tokio_util::sync::CancellationToken::new()).await.unwrap();
        }
        let notices: Vec<_> = kernel.blocks().block_snapshots(context).unwrap().into_iter()
            .filter(|block| block.content.starts_with("Shell operation ")).collect();
        assert_eq!(notices.len(), 1, "retrying delivery must not append another completion");
    }

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
    fn raw_and_effective_outcomes_commit_together_and_survive_reload() {
        use crate::runtime::command_outcome::{CommandExecution, CommandHookEffect};
        let db = Arc::new(Mutex::new(KernelDb::open(":memory:").unwrap()));
        let registry = ShellOperationRegistry::new(db.clone()).unwrap();
        let context = ContextId::new();
        let receipt = register(&registry, context);
        let mut outcome = CommandOutcome::new(CommandExecution::Completed(
            kaish_kernel::interpreter::ExecResult::failure(7, "raw failure")), 12);
        outcome.hook = Some(CommandHookEffect::Replacement(crate::mcp::KernelToolResult::text("replacement")));
        db.lock().conn_for_ledger().execute_batch(
            "CREATE TRIGGER fail_outcome BEFORE INSERT ON shell_operation_outcomes BEGIN
             SELECT RAISE(ABORT, 'outcome write failed'); END;"
        ).unwrap();
        assert!(registry.complete_outcome(&receipt.operation_id, &receipt.output_block_id, &outcome).is_err());
        let failed = registry.get(&receipt.operation_id, context).unwrap().unwrap();
        assert!(failed.completed_at.is_none());
        assert!(failed.envelope.is_none());
        assert!(registry.outcome(&receipt.operation_id, context).unwrap().is_none());
        db.lock().conn_for_ledger().execute_batch("DROP TRIGGER fail_outcome;").unwrap();
        assert!(registry.complete_outcome(&receipt.operation_id, &receipt.output_block_id, &outcome).unwrap());
        assert!(!registry.complete_outcome(&receipt.operation_id, &receipt.output_block_id, &outcome).unwrap());
        drop(registry);
        let registry = ShellOperationRegistry::new(db).unwrap();
        let saved = registry.get(&receipt.operation_id, context).unwrap().unwrap();
        let envelope = saved.envelope.unwrap();
        assert_eq!(envelope.stdout, "replacement");
        assert_eq!(envelope.exit_code, None);
        assert!(registry.outcome(&receipt.operation_id, ContextId::new()).unwrap().is_none());
        let CommandExecution::Completed(raw) = registry.outcome(&receipt.operation_id, context).unwrap().unwrap().execution else { panic!("lost raw outcome") };
        assert_eq!(raw.code, 7);
        assert_eq!(raw.err, "raw failure\n");
        outcome.execution = CommandExecution::NotRun;
        assert!(registry.complete_outcome(&receipt.operation_id, &receipt.output_block_id, &outcome).is_err(),
            "equal public results must not hide different execution records");
    }

    #[test]
    fn retention_scan_rotates_past_repeated_failures() {
        use crate::runtime::command_outcome::CommandExecution;
        let db = Arc::new(Mutex::new(KernelDb::temporary().unwrap()));
        let registry = ShellOperationRegistry::new(db.clone()).unwrap();
        db.lock().conn_for_ledger().execute_batch(
            "CREATE TRIGGER fail_retention BEFORE INSERT ON shell_operation_outcomes BEGIN SELECT RAISE(ABORT, 'retry fault'); END;"
        ).unwrap();
        let outcome = CommandOutcome::new(CommandExecution::Completed(kaish_kernel::interpreter::ExecResult::success("captured")), 1);
        for _ in 0..5 {
            let receipt = register(&registry, ContextId::new());
            assert!(registry.prepare_settlement(&receipt.operation_id, &outcome).is_err());
        }
        let first = registry.pending_retention(4).unwrap();
        for (key, outcome) in &first {
            let RetentionKey::Operation(id) = key else { unreachable!() };
            assert!(registry.prepare_settlement(id, outcome).is_err());
        }
        let next = registry.pending_retention(1).unwrap();
        assert!(!first.iter().any(|(key, _)| *key == next[0].0), "repeated failures must not starve later captured results");
    }

    #[test]
    fn prepared_settlement_is_atomic_and_cannot_be_abandoned_or_replaced() {
        use crate::runtime::command_outcome::CommandExecution;
        let db = Arc::new(Mutex::new(KernelDb::open(":memory:").unwrap()));
        let registry = ShellOperationRegistry::new(db.clone()).unwrap();
        let context = ContextId::new();
        let receipt = register(&registry, context);
        let outcome = CommandOutcome::new(CommandExecution::Completed(
            kaish_kernel::interpreter::ExecResult::success("already executed")), 1);
        db.lock().conn_for_ledger().execute_batch(
            "CREATE TRIGGER fail_prepare BEFORE INSERT ON shell_operation_projections
             BEGIN SELECT RAISE(ABORT, 'cannot track projection'); END;"
        ).unwrap();
        assert!(registry.prepare_settlement(&receipt.operation_id, &outcome).is_err());
        assert!(registry.outcome(&receipt.operation_id, context).unwrap().is_none());
        assert!(registry.pending_projections().unwrap().is_empty());
        assert_eq!(registry.pending_retention(8).unwrap().len(), 1);
        let mut conflict = outcome.clone();
        conflict.elapsed_ms += 1;
        assert!(registry.prepare_settlement(&receipt.operation_id, &conflict).unwrap_err().contains("different live retention owner"));
        assert_eq!(registry.pending_retention(8).unwrap()[0].1.elapsed_ms, outcome.elapsed_ms);
        db.lock().conn_for_ledger().execute_batch("DROP TRIGGER fail_prepare;").unwrap();
        registry.prepare_settlement(&receipt.operation_id, &outcome).unwrap();
        registry.prepare_settlement(&receipt.operation_id, &outcome).unwrap();
        assert_eq!(registry.pending_projections().unwrap().len(), 1);
        assert!(registry.finish_projection(&receipt.operation_id).is_err());
        assert!(registry.unfinished_without_outcome().unwrap().is_empty(), "restart must retain a known outcome");
        assert!(registry.complete(&receipt.operation_id, ShellEnvelope::new(ShellStatus::Error)).is_err());
        assert!(registry.get(&receipt.operation_id, context).unwrap().unwrap().completed_at.is_none());
        let different = CommandOutcome::new(CommandExecution::Fault("different result".into()), 1);
        assert!(registry.prepare_settlement(&receipt.operation_id, &different).is_err());
        assert!(registry.complete_outcome(&receipt.operation_id, &receipt.output_block_id, &different).is_err());
        assert_eq!(registry.outcome(&receipt.operation_id, context).unwrap().unwrap().envelope().stdout, "already executed");
        registry.complete_outcome(&receipt.operation_id, &receipt.output_block_id, &outcome).unwrap();
        registry.finish_projection(&receipt.operation_id).unwrap();
        assert!(registry.pending_projections().unwrap().is_empty());
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
    fn restart_selects_unfinished_work_without_mutating_receipts() {
        let db = Arc::new(Mutex::new(KernelDb::open(":memory:").unwrap()));
        let registry = ShellOperationRegistry::new(db.clone()).unwrap();
        let context = ContextId::new();
        let receipt = register(&registry, context);
        drop(registry);
        let registry = ShellOperationRegistry::new(db).unwrap();
        let unfinished = registry.unfinished_without_outcome().unwrap();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].receipt.operation_id, receipt.operation_id);
        let state = registry.get(&receipt.operation_id, context).unwrap().unwrap();
        assert!(state.completed_at.is_none());
        assert!(state.envelope.is_none());
    }
}
