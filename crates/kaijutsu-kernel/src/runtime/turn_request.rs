//! Headless turn admission. FlowBus reports accepted work; it never authorizes it.

use std::sync::Arc;
use futures::FutureExt;
use kaijutsu_types::{BlockId, ContextId, PrincipalId, SessionId};
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnAdmission {
    Accepted,
    AlreadyActive,
}

impl Kernel {
    /// Admit headless work before publishing its Requested event. Subscriber
    /// presence never determines execution. The lease covers startup and queuing
    /// as well as inference, so an immediate `kj wait` sees accepted work.
    pub fn request_turn(self: &Arc<Self>, request: TurnRequest) -> Result<TurnAdmission, String> {
        let lease = match request.continuation_epoch {
            Some(_) => match self.turns().begin_if_idle(request.context_id) {
                Some(lease) => lease,
                None => return Ok(TurnAdmission::AlreadyActive),
            },
            None => self.turns().begin(request.context_id),
        };
        let accepted = request.clone();
        let kernel = self.clone();
        let (release, ready) = tokio::sync::oneshot::channel();
        self.spawn_command(move |stop| async move {
            if ready.await.is_err() {
                drop(lease);
                report_failure(&kernel, &accepted, "turn admission ended before publication".into());
                return;
            }
            let result = {
                let startup = std::panic::AssertUnwindSafe(async {
                    let tool_ctx = match context_cwd(&kernel, accepted.context_id) {
                        Some(cwd) => ExecContext::new(accepted.principal_id, accepted.context_id,
                            cwd, SessionId::new(), kernel.id()),
                        None => ExecContext::new_without_cwd(accepted.principal_id, accepted.context_id,
                            SessionId::new(), kernel.id()),
                    };
                    spawn_admitted_turn(&kernel, accepted.context_id, accepted.model.as_deref(),
                        &accepted.after_block_id, tool_ctx, accepted.principal_id,
                        TurnOrigin::Autonomous, accepted.continuation_epoch, Some(lease)).await
                }).catch_unwind();
                tokio::pin!(startup);
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => Ok(Err("kernel runtime shut down before turn startup".into())),
                    result = &mut startup => result,
                }
            };
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => report_failure(&kernel, &accepted, error),
                Err(panic) => {
                    report_failure(&kernel, &accepted, "turn startup panicked".into());
                    std::panic::resume_unwind(panic);
                }
            }
        })?;
        self.turn_flows().publish(TurnFlow::Requested {
            context_id: request.context_id, after_block_id: request.after_block_id,
            content: request.content, principal_id: request.principal_id, model: request.model,
            continuation_epoch: request.continuation_epoch,
        });
        // The worker cannot publish a terminal event ahead of Requested.
        let _ = release.send(());
        Ok(TurnAdmission::Accepted)
    }
}

fn report_failure(kernel: &Kernel, request: &TurnRequest, error: String) {
    let payload = kaijutsu_types::ErrorPayload {
        category: kaijutsu_types::ErrorCategory::Stream,
        severity: kaijutsu_types::ErrorSeverity::Error,
        code: None,
        detail: Some(format!("autonomous turn failed to run for this context: {error}")),
        span: None, source_kind: None,
    };
    if let Err(insert_error) = kernel.blocks().insert_error_block_as(
        request.context_id, &request.after_block_id, &payload, payload.summary_line(),
        Some(request.principal_id),
    ) {
        tracing::error!("failed to record rejected turn: {insert_error}");
    }
    kernel.turn_flows().publish(TurnFlow::Failed {
        context_id: request.context_id, principal_id: request.principal_id,
        error, origin: TurnOrigin::Autonomous,
    });
}
