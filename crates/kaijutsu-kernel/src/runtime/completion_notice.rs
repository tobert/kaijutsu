//! Durable completion delivery, separate from execution and provider admission.

use std::sync::Arc;
use kaijutsu_types::{BlockId, ContextId, PrincipalId};
use rusqlite::OptionalExtension;
use crate::{Kernel, KernelDb};
use crate::kernel_db::{KernelDbError, KernelDbResult};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Source {
    Approval(String),
    Shell(String),
}

impl Source {
    pub(crate) fn kind(&self) -> &'static str {
        match self { Self::Approval(_) => "approval", Self::Shell(_) => "shell" }
    }
    pub(crate) fn id(&self) -> &str {
        match self { Self::Approval(id) | Self::Shell(id) => id }
    }
}

pub(crate) struct Notice {
    pub context: ContextId,
    pub requester: PrincipalId,
    pub performer: PrincipalId,
    pub epoch: Option<i64>,
    pub message: Option<String>,
    pub block: Option<BlockId>,
    pub suppressed: Option<String>,
    pub resume_allowed: bool,
}

pub(crate) fn summary(db: &KernelDb, source: &Source) -> KernelDbResult<Option<serde_json::Value>> {
    Ok(read(db, source)?.map(|notice| {
        let status = if notice.block.is_some() { "delivered" } else if notice.suppressed.is_some() { "suppressed" }
            else if notice.message.is_some() { "ready" } else { "pending" };
        serde_json::json!({"source": source.kind(), "source_id": source.id(), "status": status,
            "block_id": notice.block.map(|id| id.to_key()), "reason": notice.suppressed,
            "resume_allowed": notice.resume_allowed})
    }))
}

pub(crate) fn operation_summaries(db: &KernelDb, id: &str) -> KernelDbResult<Vec<serde_json::Value>> {
    let mut summaries = Vec::new();
    if let Some(notice) = summary(db, &Source::Shell(id.into()))? { summaries.push(notice); }
    let requests = {
        let mut stmt = db.conn_for_ledger().prepare(
            "SELECT a.request_id FROM approvals a JOIN shell_operations o
             ON a.command_block_id=o.command_block_id AND a.output_block_id=o.output_block_id
             JOIN execution_notifications n ON n.kind='approval' AND n.source_id=a.request_id
             WHERE o.operation_id=?1 ORDER BY a.request_id",
        )?;
        stmt.query_map([id], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for request in requests {
        if let Some(notice) = summary(db, &Source::Approval(request))? { summaries.push(notice); }
    }
    Ok(summaries)
}

fn invalid(message: &str) -> KernelDbError { KernelDbError::Validation(message.into()) }

pub(crate) fn recipient_disposition(db: &KernelDb, notice: &Notice) -> KernelDbResult<Option<&'static str>> {
    Ok(match db.get_context(notice.context)? {
        None => Some("completion context no longer exists"),
        Some(row) if row.is_archived() || row.context_state != kaijutsu_types::ContextState::Live => Some("completion context is no longer live"),
        Some(row) if row.played_by != Some(notice.performer) => Some("completion context has a different performer"),
        _ => None,
    })
}

pub(crate) fn suppress(db: &KernelDb, source: &Source, reason: &str) -> KernelDbResult<()> {
    db.conn_for_ledger().execute(
        "UPDATE execution_notifications SET suppressed_reason=?3 WHERE kind=?1 AND source_id=?2 AND block_id IS NULL AND suppressed_reason IS NULL",
        rusqlite::params![source.kind(), source.id(), reason],
    )?;
    Ok(())
}

/// Reserve with the execution claim or receipt admission, never afterward.
pub(crate) fn reserve(db: &KernelDb, source: &Source, suppressed: Option<&str>) -> KernelDbResult<()> {
    if db.conn_for_ledger().is_autocommit() { return Err(invalid("completion reservation requires its owner's transaction")); }
    db.conn_for_ledger().execute(
        "INSERT INTO execution_notifications(kind,source_id,suppressed_reason) VALUES(?1,?2,?3)",
        rusqlite::params![source.kind(), source.id(), suppressed],
    )?;
    read(db, source)?.ok_or_else(|| invalid("completion reservation has no owner"))?;
    Ok(())
}

pub(crate) fn read(db: &KernelDb, source: &Source) -> KernelDbResult<Option<Notice>> {
    let fields = db.conn_for_ledger().query_row(
        "SELECT n.message,n.block_id,n.suppressed_reason,n.resume_allowed,
         COALESCE(a.context_id,o.context_id),COALESCE(a.principal_id,o.principal_id),
         COALESCE(a.actor_id,o.actor_id),COALESCE(a.continuation_epoch,o.continuation_epoch)
         FROM execution_notifications n
         LEFT JOIN approvals a ON n.kind='approval' AND a.request_id=n.source_id
         LEFT JOIN shell_operations o ON n.kind='shell' AND o.operation_id=n.source_id
         WHERE n.kind=?1 AND n.source_id=?2",
        rusqlite::params![source.kind(), source.id()], |row| Ok((
            row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<String>>(2)?, row.get::<_, bool>(3)?,
            row.get::<_, Vec<u8>>(4)?, row.get::<_, Vec<u8>>(5)?, row.get::<_, Vec<u8>>(6)?, row.get::<_, Option<i64>>(7)?,
        )),
    ).optional()?;
    let Some((message, block, suppressed, resume_allowed, context, requester, performer, epoch)) = fields else { return Ok(None); };
    Ok(Some(Notice {
        message, suppressed, resume_allowed, epoch,
        context: ContextId::try_from_slice(&context).ok_or_else(|| invalid("completion owner has an invalid context"))?,
        requester: PrincipalId::try_from_slice(&requester).ok_or_else(|| invalid("completion owner has an invalid requester"))?,
        performer: PrincipalId::try_from_slice(&performer).ok_or_else(|| invalid("completion owner has an invalid performer"))?,
        block: block.map(|id| BlockId::from_key(&id).ok_or_else(|| invalid("completion has an invalid delivery block"))).transpose()?,
    }))
}

/// Retain the first completion message. Delivery retries cannot replace it.
pub(crate) fn prepare(db: &KernelDb, source: &Source, message: &str) -> KernelDbResult<()> {
    let notice = read(db, source)?.ok_or_else(|| invalid("completion has no reserved owner"))?;
    if notice.message.is_some() || notice.suppressed.is_some() { return Ok(()); }
    db.conn_for_ledger().execute(
        "UPDATE execution_notifications SET message=?3 WHERE kind=?1 AND source_id=?2 AND message IS NULL",
        rusqlite::params![source.kind(), source.id(), message],
    )?;
    Ok(())
}

pub(crate) fn sources(db: &KernelDb, ready: bool, limit: usize) -> KernelDbResult<Vec<Source>> {
    let mut stmt = db.conn_for_ledger().prepare(
        "SELECT kind,source_id FROM execution_notifications WHERE block_id IS NULL AND suppressed_reason IS NULL
         AND (?1=0 OR message IS NOT NULL) ORDER BY rowid LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![ready, limit as i64], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(|(kind, id)| match kind.as_str() {
        "approval" => Ok(Source::Approval(id)), "shell" => Ok(Source::Shell(id)),
        _ => Err(invalid("unknown completion source")),
    }).collect()
}

pub(crate) fn shell_message(id: &str, envelope: &kaijutsu_types::shell_envelope::ShellEnvelope) -> String {
    format!("Shell operation {id} completed with status {} and exit code {:?}. Output block: {}.\n{}",
        envelope.status.as_str(), envelope.exit_code, envelope.block_id.as_deref().unwrap_or("unavailable"), envelope.readable_output())
}

/// Resolve lost live owners after result and interruption recovery. Existing
/// ready messages survive, but startup never replays their provider wakes.
pub(crate) fn recover(kernel: &Kernel) -> Result<(), String> {
    let pending = sources(&kernel.kernel_db().lock(), false, i64::MAX as usize).map_err(|e| e.to_string())?;
    for source in pending {
        let notice = read(&kernel.kernel_db().lock(), &source).map_err(|e| e.to_string())?.ok_or("completion disappeared during recovery")?;
        let operation = match &source {
            Source::Shell(id) => kernel.shell_operations().get(id, notice.context)?,
            Source::Approval(id) => kernel.shell_operations().get_by_ask(id, notice.context)?,
        };
        let message = match operation {
            Some(operation) => {
                if operation.completed_at.is_none() { return Err("completion recovery requires a terminal operation".into()); }
                let envelope = operation.envelope.as_ref().ok_or("completed operation has no envelope")?;
                let output = shell_message(&operation.receipt.operation_id, envelope);
                match &source { Source::Approval(id) => format!("Approval {id}: {output}"), Source::Shell(_) => output }
            }
            None => match &source {
                Source::Approval(id) => format!("Approval {id} was claimed, but its command outcome is unavailable after restart. Source was not run again. Inspect the approval before deciding what to do next."),
                Source::Shell(_) => return Err("reserved shell completion lost its receipt".into()),
            },
        };
        let db = kernel.kernel_db().lock();
        db.in_transaction(|db| {
            prepare(db, &source, &message)?;
            db.conn_for_ledger().execute("UPDATE execution_notifications SET resume_allowed=0 WHERE kind=?1 AND source_id=?2",
                rusqlite::params![source.kind(), source.id()])?;
            Ok(())
        }).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Publish once, then consider a live continuation. Failure to admit a turn
/// cannot remove the durable message or authorize a second copy.
pub(crate) async fn deliver(kernel: &Arc<Kernel>, source: &Source, stop: &tokio_util::sync::CancellationToken) -> Result<(), String> {
    let notice = {
        let db = kernel.kernel_db().lock();
        let notice = read(&db, source).map_err(|e| e.to_string())?.ok_or("completion has no reserved owner")?;
        if notice.block.is_some() || notice.suppressed.is_some() { return Ok(()); }
        if let Some(reason) = recipient_disposition(&db, &notice).map_err(|e| e.to_string())? {
            suppress(&db, source, reason).map_err(|e| e.to_string())?;
            return Ok(());
        }
        notice
    };
    if !kernel.blocks().contains(notice.context) { kernel.blocks().load_one_from_db(notice.context).map_err(|e| e.to_string())?; }
    let Some((block, notice, message)) = kernel.blocks().insert_completion_notice(notice.context, source).map_err(|e| e.to_string())? else { return Ok(()); };
    kernel.turns().conversations().evict(notice.context);
    if stop.is_cancelled() || !notice.resume_allowed || kernel.turn_in_flight(notice.context) { return Ok(()); }
    let Some(epoch) = notice.epoch else { return Ok(()); };
    let window = kernel.gate_resume_window().await?;
    let window_ms = i64::try_from(window.as_millis()).map_err(|e| e.to_string())?;
    if stop.is_cancelled() { return Ok(()); }
    let allowed = {
        let db = kernel.kernel_db().lock();
        let row = db.get_context(notice.context).map_err(|e| e.to_string())?.ok_or("completion context disappeared")?;
        row.context_state == kaijutsu_types::ContextState::Live && !row.is_archived() && row.played_by == Some(notice.performer)
            && db.automatic_resume_allowed(notice.context, epoch, kaijutsu_types::now_millis() as i64, window_ms).map_err(|e| e.to_string())?
    };
    if allowed {
        kernel.request_turn(super::turn_request::TurnRequest { score: None, context_id: notice.context,
            after_block_id: block, content: message, principal_id: notice.requester, model: None, continuation_epoch: Some(epoch) })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers::{test_dispatcher_persistent, register_context};

    #[tokio::test]
    async fn ready_backlog_drains_past_one_scan_without_another_event() {
        let dispatcher = test_dispatcher_persistent().await;
        let kernel = dispatcher.kernel();
        let actor = PrincipalId::new();
        let context = register_context(&dispatcher, Some("completion-backlog"), None, actor);
        kernel.kernel_db().lock().update_context_review(context, Some(actor), Some(PrincipalId::new())).unwrap();
        kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        let call = crate::mcp::CallContext::new(actor, context, kaijutsu_types::SessionId::new(), kernel.id());
        let mut keys = Vec::new();
        for _ in 0..6 {
            let receipt = super::super::tool_command::create_operation(kernel, &call, "never run", None).unwrap();
            let source = Source::Shell(receipt.operation_id);
            prepare(&kernel.kernel_db().lock(), &source, "controlled completion").unwrap();
            keys.push(source);
        }
        kernel.start_approval_delivery().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while keys.iter().any(|key| read(&kernel.kernel_db().lock(), key).unwrap().unwrap().block.is_none()) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("a backlog beyond the scan cap must drain without another event");
        kernel.shutdown_runtime_worker().await.unwrap();
        assert_eq!(kernel.blocks().block_snapshots(context).unwrap().iter().filter(|b| b.content == "controlled completion").count(), 6);
    }

    #[tokio::test]
    async fn reassigned_recipient_records_suppression_without_writing_a_notice() {
        let dispatcher = test_dispatcher_persistent().await;
        let kernel = dispatcher.kernel();
        let actor = PrincipalId::new();
        let context = register_context(&dispatcher, Some("changed-notice-recipient"), None, actor);
        kernel.kernel_db().lock().update_context_review(context, Some(actor), Some(PrincipalId::new())).unwrap();
        kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        let call = crate::mcp::CallContext::new(actor, context, kaijutsu_types::SessionId::new(), kernel.id());
        let receipt = super::super::tool_command::create_operation(kernel, &call, "never run", None).unwrap();
        let source = Source::Shell(receipt.operation_id.clone());
        prepare(&kernel.kernel_db().lock(), &source, "completion for the original performer").unwrap();
        kernel.kernel_db().lock().update_context_review(context, Some(PrincipalId::new()), Some(PrincipalId::new())).unwrap();
        deliver(kernel, &source, &tokio_util::sync::CancellationToken::new()).await.unwrap();
        let status = summary(&kernel.kernel_db().lock(), &source).unwrap().unwrap();
        assert_eq!(status["status"], "suppressed");
        assert!(status["reason"].as_str().unwrap().contains("different performer"));
        assert!(status["block_id"].is_null());
        assert_eq!(kernel.blocks().block_snapshots(context).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn notification_marker_and_block_recover_together_without_a_provider_wake() {
        for fault in ["marker", "journal", "compaction"] {
            let dispatcher = test_dispatcher_persistent().await;
            let kernel = dispatcher.kernel();
            let actor = PrincipalId::new();
            let context = register_context(&dispatcher, Some("notice-recovery"), None, actor);
            kernel.kernel_db().lock().update_context_review(context, Some(actor), Some(PrincipalId::new())).unwrap();
            kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
            let now = kaijutsu_types::now_millis() as i64;
            let epoch = kernel.kernel_db().lock().begin_continuation(context, now).unwrap().epoch;
            kernel.kernel_db().lock().record_continuation_request(context, epoch, now).unwrap();
            kernel.kernel_db().lock().record_continuation_yield(context, epoch, now).unwrap();
            let call = crate::mcp::CallContext::new(actor, context, kaijutsu_types::SessionId::new(), kernel.id());
            let receipt = super::super::tool_command::create_operation(kernel, &call, "never execute on recovery", None).unwrap();
            let source = Source::Shell(receipt.operation_id.clone());
            let stdout = if fault == "compaction" { "x".repeat(1_048_576) } else { "captured output".into() };
            let outcome = super::super::command_outcome::CommandOutcome::new(super::super::command_outcome::CommandExecution::Completed(
                kaish_kernel::interpreter::ExecResult::success(&stdout)), 1);
            super::super::command::settle_outcome(kernel, context, &receipt.command_block_id, &receipt.output_block_id, &outcome, None).unwrap();
            let db = kernel.kernel_db().clone();
            db.lock().conn_for_ledger().execute_batch(match fault {
                "marker" => "CREATE TRIGGER reject_notice BEFORE UPDATE OF block_id ON execution_notifications BEGIN SELECT RAISE(ABORT, 'injected marker fault'); END;",
                "journal" => "CREATE TRIGGER reject_notice BEFORE INSERT ON oplog BEGIN SELECT RAISE(ABORT, 'injected journal fault'); END;",
                _ => "CREATE TRIGGER reject_notice BEFORE INSERT ON doc_snapshots BEGIN SELECT RAISE(ABORT, 'injected compaction fault'); END;",
            }).unwrap();
            let error = deliver(kernel, &source, &tokio_util::sync::CancellationToken::new()).await.unwrap_err();
            assert!(error.contains(&format!("injected {fault} fault")), "{error}");
            let committed = fault == "compaction";
            assert_eq!(read(&db.lock(), &source).unwrap().unwrap().block.is_some(), committed);
            db.lock().conn_for_ledger().execute_batch("DROP TRIGGER reject_notice").unwrap();
            let workspace = db.lock().get_or_create_default_workspace(PrincipalId::system()).unwrap();
            let blocks = crate::block_store::shared_block_store_with_db(db.clone(), workspace, PrincipalId::system());
            blocks.load_one_from_db(context).unwrap();
            assert_eq!(blocks.block_snapshots(context).unwrap().iter().filter(|b| b.content.starts_with("Shell operation ")).count(), usize::from(committed));
            let dir = tempfile::tempdir().unwrap();
            let recovered = Arc::new(Kernel::new("notice-recovery", dir.path(), blocks, db.clone()).await);
            recovered.shutdown_runtime_worker().await.unwrap();
            let before = read(&db.lock(), &source).unwrap().unwrap();
            assert!(committed || !before.resume_allowed, "startup cannot replay a provider wake");
            for _ in 0..2 { deliver(&recovered, &source, &tokio_util::sync::CancellationToken::new()).await.unwrap(); }
            let notice = read(&db.lock(), &source).unwrap().unwrap();
            let id = notice.block.unwrap();
            let message = recovered.blocks().get_block_snapshot(context, &id).unwrap().unwrap();
            assert!(message.content.ends_with(&stdout));
            assert_eq!(message.id.principal_id, PrincipalId::system());
            assert_eq!(recovered.blocks().block_snapshots(context).unwrap().iter().filter(|b| b.content.starts_with("Shell operation ")).count(), 1);
        }
    }

    #[tokio::test]
    async fn admission_reserves_delivery_before_execution_and_recovers_an_interruption() {
        let dispatcher = test_dispatcher_persistent().await;
        let kernel = dispatcher.kernel();
        let actor = PrincipalId::new();
        let context = register_context(&dispatcher, Some("interrupted-notice"), None, actor);
        kernel.kernel_db().lock().update_context_review(context, Some(actor), Some(PrincipalId::new())).unwrap();
        kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        let call = crate::mcp::CallContext::new(actor, context, kaijutsu_types::SessionId::new(), kernel.id());
        let receipt = super::super::tool_command::create_operation(kernel, &call, "never execute after restart", None).unwrap();
        let source = Source::Shell(receipt.operation_id.clone());
        assert!(read(&kernel.kernel_db().lock(), &source).unwrap().unwrap().message.is_none());
        let db = kernel.kernel_db().clone();
        let workspace = db.lock().get_or_create_default_workspace(PrincipalId::system()).unwrap();
        let blocks = crate::block_store::shared_block_store_with_db(db.clone(), workspace, PrincipalId::system());
        let dir = tempfile::tempdir().unwrap();
        let recovered = Arc::new(Kernel::new("interrupted-notice", dir.path(), blocks, db.clone()).await);
        deliver(&recovered, &source, &tokio_util::sync::CancellationToken::new()).await.unwrap();
        let notice = read(&db.lock(), &source).unwrap().unwrap();
        assert!(notice.message.as_deref().unwrap().contains("restarted"));
        assert!(notice.block.is_some());
        assert!(!notice.resume_allowed);
        let result = recovered.shell_operations().get(&receipt.operation_id, context).unwrap().unwrap().envelope.unwrap();
        assert_eq!(result.exit_code, None);
    }
}
