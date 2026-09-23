//! Turn preparation ownership and headless admission on the kernel worker.
//! FlowBus reports accepted work; it never authorizes it.

use std::sync::Arc;
use futures::FutureExt;
use tracing::Instrument;
use kaijutsu_types::{BlockId, ContextId, PrincipalId, SessionId, TurnId};
use crate::{ExecContext, Kernel};
use crate::flows::{TurnFlow, TurnOrigin};
use super::llm_stream::spawn_admitted_turn;
use super::shell_state::context_cwd;

#[derive(Clone)]
pub struct TurnRequest {
    pub context_id: ContextId,
    pub after_block_id: BlockId,
    pub content: String,
    pub principal_id: PrincipalId,
    pub model: Option<String>,
    pub continuation_epoch: Option<i64>,
    pub score: Option<crate::hyoushigi::model::ScoreIntent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnAdmission {
    Accepted { turn_id: TurnId, work_id: Option<kaijutsu_hyoushigi::WorkId> },
    AlreadyActive,
}

impl Kernel {
    /// Admit headless work before publishing its Requested event. Subscriber
    /// presence never determines execution. The lease covers startup and queuing
    /// as well as inference, so an immediate `kj wait` sees accepted work.
    ///
    /// Reserves its own worker-pool slot first (`docs/resource-admission.md`,
    /// rule 1: before it mints the `ContextAdmission`). A caller that must
    /// write something durable of its own — `kj drive`'s seed block — before
    /// this admission mint reserves earlier and calls
    /// [`Self::request_turn_with_slot`] instead, so the reservation still
    /// precedes that write.
    pub fn request_turn(self: &Arc<Self>, request: TurnRequest) -> Result<TurnAdmission, String> {
        let slot = self.reserve_runtime_slot()?;
        self.request_turn_with_slot(request, slot)
    }

    /// [`Self::request_turn`] with a reservation the caller already holds —
    /// taken before a durable write of the caller's own that must also
    /// precede this call's `ContextAdmission` mint. See `docs/resource-admission.md`.
    pub(crate) fn request_turn_with_slot(
        self: &Arc<Self>, request: TurnRequest, slot: crate::runtime::RuntimeSlot,
    ) -> Result<TurnAdmission, String> {
        let (admission, mut lease) = {
            let db = self.kernel_db().lock();
            let admission = super::admission::ContextAdmission::acquire(&db, request.context_id)?;
            let lease = match request.continuation_epoch {
                Some(_) => match self.turns().begin_if_idle(request.context_id) {
                    Some(lease) => lease,
                    None => return Ok(TurnAdmission::AlreadyActive),
                },
                None => self.turns().begin(request.context_id),
            };
            (admission, lease)
        };
        let turn_id = lease.id();
        let score = request.score.as_ref().map(|intent|
            crate::hyoushigi::model::admit(self, &mut lease, request.after_block_id.clone(), intent)
        ).transpose()?;
        let work_id = score.as_ref().map(|(id, _)| *id);
        let accepted = request.clone();
        let (release, ready) = tokio::sync::oneshot::channel();
        let admitted = queue_startup(self, StartupRequest {
            admission, lease, request: accepted, origin: TurnOrigin::Autonomous,
            tool_ctx: None, session: SessionId::new(), submit: None,
        }, Some(ready), slot);
        if let Err(error) = admitted {
            if let Some((id, timeline)) = score { timeline.lock().cancel(id); }
            return Err(error);
        }
        self.turn_flows().publish(TurnFlow::Requested {
            turn_id,
            context_id: request.context_id, after_block_id: request.after_block_id,
            content: request.content, principal_id: request.principal_id, model: request.model,
            continuation_epoch: request.continuation_epoch,
        });
        // The worker cannot publish a terminal event ahead of Requested.
        let _ = release.send(());
        Ok(TurnAdmission::Accepted { turn_id, work_id })
    }
}

/// Preparation and inference share one lease, even while a caller waits for
/// the startup result. Dropping that wait does not cancel accepted work.
pub(crate) struct StartupRequest {
    pub admission: super::admission::ContextAdmission,
    pub lease: super::turn_state::TurnLease,
    pub request: TurnRequest,
    pub origin: TurnOrigin,
    pub tool_ctx: Option<ExecContext>,
    pub session: SessionId,
    pub submit: Option<(crate::rc::SubmitInfo, crate::KjCaller)>,
}

pub(crate) fn queue_startup(
    kernel: &Arc<Kernel>, accepted: StartupRequest,
    ready: Option<tokio::sync::oneshot::Receiver<()>>,
    slot: crate::runtime::RuntimeSlot,
) -> Result<tokio::sync::oneshot::Receiver<Result<(), String>>, String> {
    let (reply, result) = tokio::sync::oneshot::channel();
    let host = kernel.clone();
    let span = tracing::Span::current();
    slot.spawn(move |stop| async move {
        let StartupRequest { admission, lease, request, origin, tool_ctx, session, submit } = accepted;
        let turn_id = lease.id();
        let interrupt = lease.interrupt();
        let cancel = interrupt.cancel.clone();
        if let Some(ready) = ready {
            if ready.await.is_err() {
                drop(lease);
                let error = "turn admission ended before publication".to_string();
                report_failure(&host, turn_id, &request, origin, error.clone());
                let _ = reply.send(Err(error));
                return;
            }
        }
        let outcome = {
            let startup = std::panic::AssertUnwindSafe(async {
                if let Some((info, caller)) = submit {
                    let dispatcher = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err("turn cancelled before submit lifecycle".to_string()),
                        dispatcher = host.broker().kj_dispatcher() => dispatcher
                            .ok_or("submit lifecycle requires a registered kj dispatcher")?,
                    };
                    crate::rc::run(&dispatcher, crate::rc::RcInvocation {
                        vars: info.vars(),
                        ..crate::rc::RcInvocation::new(crate::rc::VERB_SUBMIT, &admission, &cancel)
                    }, &caller).await.map_err(|error| format!("rc submit lifecycle: {error}"))?;
                }
                let tool_ctx = match tool_ctx {
                    Some(context) => context,
                    None => match context_cwd(&host, request.context_id)? {
                        Some(cwd) => ExecContext::new(request.principal_id, request.context_id,
                            cwd, session, host.id()),
                        None => ExecContext::new_without_cwd(request.principal_id, request.context_id,
                            session, host.id()),
                    },
                };
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err("turn cancelled before model startup".into()),
                    result = spawn_admitted_turn(&host, request.context_id, request.model.as_deref(),
                        &request.after_block_id, tool_ctx, request.principal_id, origin,
                        request.continuation_epoch, lease, admission) => result,
                }
            }).catch_unwind();
            tokio::pin!(startup);
            tokio::select! {
                biased;
                _ = stop.cancelled() => { interrupt.hard(); startup.await },
                outcome = &mut startup => outcome,
            }
        };
        match outcome {
            Ok(result) => {
                if let Err(error) = &result {
                    if cancel.is_cancelled() {
                        host.turn_flows().publish(TurnFlow::Completed {
                            turn_id, context_id: request.context_id, principal_id: request.principal_id,
                            output_block_id: None, origin,
                            reason: crate::flows::TurnStopReason::Cancelled { immediate: true },
                        });
                    } else {
                        report_failure(&host, turn_id, &request, origin, error.clone());
                    }
                }
                let result = if result.is_err() && stop.is_cancelled() {
                    Err("kernel runtime shut down before turn startup".to_string())
                } else { result };
                let _ = reply.send(result);
            }
            Err(panic) => {
                let error = "turn startup panicked".to_string();
                report_failure(&host, turn_id, &request, origin, error.clone());
                let _ = reply.send(Err(error));
                std::panic::resume_unwind(panic);
            }
        }
    }.instrument(span))?;
    Ok(result)
}

fn report_failure(kernel: &Kernel, turn_id: TurnId, request: &TurnRequest, origin: TurnOrigin, error: String) {
    let payload = kaijutsu_types::ErrorPayload {
        category: kaijutsu_types::ErrorCategory::Stream,
        severity: kaijutsu_types::ErrorSeverity::Error,
        code: None,
        detail: Some(format!("turn failed to run for this context: {error}")),
        span: None, source_kind: None,
    };
    if let Err(insert_error) = kernel.blocks().insert_error_block_as(
        request.context_id, &request.after_block_id, &payload, payload.summary_line(),
        Some(request.principal_id),
    ) {
        tracing::error!("failed to record rejected turn: {insert_error}");
    }
    kernel.turn_flows().publish(TurnFlow::Failed {
        turn_id,
        context_id: request.context_id, principal_id: request.principal_id,
        error, origin,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers::{register_context, test_dispatcher};

    #[tokio::test]
    async fn archived_context_refuses_headless_admission_without_publishing_work() {
        let dispatcher = test_dispatcher().await;
        let principal = PrincipalId::new();
        let context = register_context(&dispatcher, Some("archived-turn"), None, principal);
        let blocks = dispatcher.block_store();
        blocks.create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        let anchor = blocks.insert_block_as(context, None, None, kaijutsu_types::Role::User,
            kaijutsu_types::BlockKind::Text, "seed", kaijutsu_types::Status::Done,
            kaijutsu_types::ContentType::Plain, Some(principal)).unwrap();
        dispatcher.kernel_db().lock().archive_context(context).unwrap();
        let mut requested = dispatcher.kernel().turn_flows().subscribe("turn.requested");
        let result = dispatcher.kernel().request_turn(TurnRequest {
            score: None, context_id: context, after_block_id: anchor,
            content: String::new(), principal_id: principal, model: None, continuation_epoch: None,
        });
        dispatcher.kernel().shutdown_runtime_worker().await.unwrap();
        let error = result.expect_err("archived context must not admit a headless turn");
        assert!(error.contains("archived"), "{error}");
        assert!(!dispatcher.kernel().turn_in_flight(context));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(20), requested.recv()).await.is_err());
        assert_eq!(blocks.block_snapshots(context).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unreadable_cwd_fails_before_headless_model_startup() {
        let dispatcher = test_dispatcher().await;
        let principal = PrincipalId::new();
        let context = register_context(&dispatcher, Some("bad-cwd"), None, principal);
        let blocks = dispatcher.block_store();
        blocks.create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        let anchor = blocks.insert_block_as(context, None, None, kaijutsu_types::Role::User,
            kaijutsu_types::BlockKind::Text, "seed", kaijutsu_types::Status::Done,
            kaijutsu_types::ContentType::Plain, Some(principal)).unwrap();
        dispatcher.kernel_db().lock().conn_for_ledger().execute_batch(
            "ALTER TABLE context_shell RENAME TO unavailable_context_shell"
        ).unwrap();
        let mut failures = dispatcher.kernel().turn_flows().subscribe("turn.failed");
        dispatcher.kernel().request_turn(TurnRequest {
            score: None,
            context_id: context, after_block_id: anchor, content: String::new(),
            principal_id: principal, model: None, continuation_epoch: None,
        }).unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), failures.recv())
            .await.unwrap().unwrap();
        match event.payload {
            TurnFlow::Failed { error, .. } => assert!(error.contains("context_shell"), "{error}"),
            other => panic!("expected failed startup, got {other:?}"),
        }
        assert!(!dispatcher.kernel().turn_in_flight(context));
        dispatcher.kernel().shutdown_runtime_worker().await.unwrap();
    }

    #[tokio::test]
    async fn submit_rc_admission_fault_prevents_provider_startup() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        use std::sync::Arc;

        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        dispatcher.kernel().broker().set_kj_dispatcher(&dispatcher).await;
        let kernel = dispatcher.kernel().clone();
        let principal = PrincipalId::new();
        let context = register_context(&dispatcher, Some("submit-rc-fault"), None, principal);
        let blocks = dispatcher.block_store();
        blocks.create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        let input = blocks.insert_block_as(context, None, None, kaijutsu_types::Role::User,
            kaijutsu_types::BlockKind::Text, "submitted", kaijutsu_types::Status::Done,
            kaijutsu_types::ContentType::Plain, Some(principal)).unwrap();
        let admission = {
            let db = kernel.kernel_db().lock();
            crate::runtime::admission::ContextAdmission::acquire(&db, context).unwrap()
        };
        let lease = kernel.turns().begin(context);
        let caller = crate::kj::KjCaller {
            principal_id: principal, actor_id: principal, reviewer_id: None, context_id: Some(context),
            session_id: SessionId::new(), confirmed: false, rc_depth: 0, privileged: false,
            cancel: lease.interrupt().cancel.child_token(),
        };
        kernel.kernel_db().lock().conn_for_ledger().authorizer(Some(|auth: AuthContext<'_>| match auth.action {
            AuthAction::Insert { table_name: "rc_runs" } => Authorization::Deny,
            _ => Authorization::Allow,
        })).unwrap();
        let slot = kernel.reserve_runtime_slot().unwrap();
        let result = queue_startup(&kernel, StartupRequest {
            admission, lease,
            request: TurnRequest { context_id: context, after_block_id: input, content: String::new(),
                principal_id: principal, model: None, continuation_epoch: None, score: None },
            origin: TurnOrigin::Interactive, tool_ctx: None, session: caller.session_id,
            submit: Some((crate::rc::SubmitInfo { input_block: input, edge_block: None,
                edge_shown: None, log_tail: None, turn_live: false }, caller)),
        }, None, slot).unwrap().await.unwrap();
        kernel.kernel_db().lock().conn_for_ledger().authorizer(None::<fn(AuthContext<'_>) -> Authorization>).unwrap();

        let error = result.expect_err("submit rc admission fault must stop startup");
        assert!(error.contains("rc submit lifecycle"), "{error}");
        assert!(!kernel.turn_in_flight(context), "failed submit lifecycle must release its turn lease");
        kernel.shutdown_runtime_worker().await.unwrap();
    }
}
