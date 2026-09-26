//! Interactive prompt admission, durable input and kernel-owned preparation.

use std::sync::Arc;
use kaijutsu_types::{BlockId, BlockKind, ContentType, ContextId, InputEdge, PrincipalId, Role, SessionId, Status};
use crate::{ExecContext, Kernel, KjCaller};
use super::admission::ContextAdmission;
use super::turn_request::{StartupRequest, TurnRequest, queue_startup};

pub enum PromptSource {
    Text { content: String, model: Option<String> },
    Draft { edge: Option<InputEdge> },
}

/// Persist input before preparation. Accepted work owns its turn before the
/// first await and survives caller disconnect; the reply reports startup only.
///
/// Reserves a worker-pool slot before minting the `ContextAdmission`, before
/// the user block is inserted, and before a draft is consumed
/// (`docs/resource-admission.md`, rule 1) — a refused prompt or draft
/// submission leaves no block and no consumed draft.
pub async fn submit(
    kernel: &Arc<Kernel>, context: ContextId, principal: PrincipalId,
    session: SessionId, source: PromptSource,
) -> Result<BlockId, String> {
    let slot = kernel.reserve_runtime_slot()?;
    let (admission, lease, turn_live) = {
        let db = kernel.kernel_db().lock();
        let admission = ContextAdmission::acquire(&db, context)?;
        let turn_live = kernel.turn_in_flight(context);
        (admission, kernel.turns().begin(context), turn_live)
    };
    let documents = kernel.blocks();
    let (after_block_id, model, tool_ctx, submit) = match source {
        PromptSource::Text { content, model } => {
            let tool_ctx = match super::shell_state::context_cwd(kernel, context)? {
                Some(cwd) => ExecContext::new(principal, context, cwd, session, kernel.id()),
                None => ExecContext::new_without_cwd(principal, context, session, kernel.id()),
            };
            if documents.get(context).is_none() {
                return Err(format!("context {context} not found — call join_context first"));
            }
            let block = documents.insert_block_as(context, None, documents.last_block_id(context).as_ref(),
                Role::User, BlockKind::Text, &content, Status::Done, ContentType::Plain, Some(principal))
                .map_err(|error| format!("failed to insert user block: {error}"))?;
            (block, model, Some(tool_ctx), None)
        }
        PromptSource::Draft { edge } => {
            let (block, _) = documents.submit_draft(context, principal, edge)
                .map_err(|error| format!("submit: {error}"))?;
            let info = crate::rc::SubmitInfo {
                input_block: block, edge_block: edge.map(|edge| edge.block),
                edge_shown: edge.and_then(|edge| edge.shown),
                log_tail: documents.log_tail(context, &block), turn_live,
            };
            let caller = KjCaller { principal_id: principal, actor_id: principal,
                reviewer_id: None, context_id: Some(context), session_id: session,
                confirmed: false, rc_depth: 0, privileged: false,
                cancel: lease.interrupt().cancel.child_token() };
            (block, None, None, Some((info, caller)))
        }
    };
    queue_startup(kernel, StartupRequest {
        admission, lease, request: TurnRequest {
            context_id: context, after_block_id, content: String::new(),
            principal_id: principal, model, continuation_epoch: None, score: None,
        },
        origin: crate::flows::TurnOrigin::Interactive, tool_ctx, session, submit,
        joins_live_turn: true,
    }, None, slot)?.await.map_err(|_| "turn preparation stopped before replying".to_string())??;
    Ok(after_block_id)
}

/// Start the turn for a submit a running turn accepted but never delivered.
/// The input and its submit lifecycle are already durable.
pub(crate) fn follow_up(
    kernel: &Arc<Kernel>, context: ContextId, block: BlockId, principal: PrincipalId, session: SessionId,
) -> Result<(), String> {
    let slot = kernel.reserve_runtime_slot()?;
    let (admission, lease) = {
        let db = kernel.kernel_db().lock();
        (ContextAdmission::acquire(&db, context)?, kernel.turns().begin(context))
    };
    // The startup result reports only preparation; the turn reports its own end.
    let _startup = queue_startup(kernel, StartupRequest {
        admission, lease, request: TurnRequest {
            context_id: context, after_block_id: block, content: String::new(),
            principal_id: principal, model: None, continuation_epoch: None, score: None,
        },
        origin: crate::flows::TurnOrigin::Interactive, tool_ctx: None, session,
        submit: None, joins_live_turn: false,
    }, None, slot)?;
    Ok(())
}
