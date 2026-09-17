//! Admission-owned delivery of one model turn's notation to a fixed score tick.

use std::sync::Arc;
use std::time::Duration;

use kaijutsu_hyoushigi::{ContextHash, ContextQuery, Fallback, Recipe, Resolution, ResolveError, Resolver, ResolverCtx, ResolverId, WorkId};
use kaijutsu_types::{BlockId, BlockKind, ContextId, PrincipalId, Role, Status, Tick, TrackId};

use crate::flows::{TurnFlow, TurnOrigin};
use crate::runtime::interrupt::ContextInterruptState;
use crate::runtime::turn_state::TurnLease;
use super::{ABC_MIME, Cell, SharedTimeline, Span, validate_abc};

#[derive(Clone, Debug)]
pub struct ScoreIntent {
    pub track: TrackId,
    pub start: Tick,
    pub fallback: Fallback,
}

struct ModelOutput {
    receiver: parking_lot::Mutex<Option<tokio::sync::oneshot::Receiver<Result<Resolution, ResolveError>>>>,
    documents: crate::block_store::SharedBlockStore,
    context: ContextId,
    seed: BlockId,
    interrupt: Arc<ContextInterruptState>,
}

struct CancelOnDrop(Arc<ContextInterruptState>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) { self.0.hard(); }
}

impl Resolver for ModelOutput {
    fn id(&self) -> ResolverId { ResolverId::new("model_output") }
    fn estimate_cost(&self, _: &serde_json::Value, _: &dyn ResolverCtx) -> Duration { Duration::ZERO }
    fn can_respeculate(&self) -> bool { false }
    fn source_block(&self) -> Option<BlockId> { Some(self.seed.clone()) }

    fn compute_basis(&self, _: &serde_json::Value, ctx: &dyn ResolverCtx) -> ContextHash {
        // This is an in-memory read. Missing/unreadable input is a distinct
        // invalid basis, never an empty seed that could pass validation.
        let seed = match self.documents.get_block_snapshot(self.context, &self.seed) {
            Ok(Some(block)) => serde_json::json!({
                "content": block.content, "excluded": block.excluded,
                "ephemeral": block.ephemeral, "status": block.status,
            }),
            other => serde_json::json!({ "unavailable": format!("{other:?}") }),
        };
        ContextHash::of(&serde_json::to_vec(&serde_json::json!({
            "seed": seed, "score": ctx.content_before(ctx.now()),
        })).expect("score basis contains JSON values"))
    }

    fn resolve(&self, _: &serde_json::Value, _: &dyn ResolverCtx) -> kaijutsu_hyoushigi::ResolveFuture {
        let receiver = self.receiver.lock().take().expect("one admitted model output");
        let interrupt = self.interrupt.clone();
        let cancel = CancelOnDrop(interrupt.clone());
        Box::pin(async move {
            let _cancel = cancel;
            tokio::select! {
                biased;
                _ = interrupt.cancel.cancelled() => Err(ResolveError::Failed("model turn interrupted before score delivery".into())),
                result = receiver => result.map_err(|_| ResolveError::Failed("model turn ended before delivering score output".into()))?,
            }
        })
    }
}

/// Runtime-owned output validation and document feedback. The timeline receives
/// only prepared bytes; discarding those bytes cannot undo accepted log writes.
pub(crate) struct OutputDelivery {
    sender: tokio::sync::oneshot::Sender<Result<Resolution, ResolveError>>,
    documents: crate::block_store::SharedBlockStore,
    context: ContextId,
    performer: PrincipalId,
}

impl OutputDelivery {
    pub(crate) async fn finish(mut self, event: &TurnFlow, interrupt: Arc<ContextInterruptState>) {
        if self.sender.is_closed() { return; }
        let block = match event {
            TurnFlow::Completed { output_block_id: Some(block), reason, origin: TurnOrigin::Autonomous, .. }
                if reason.output_is_complete() => block.clone(),
            TurnFlow::Failed { error, .. } => {
                let _ = self.sender.send(Err(ResolveError::Failed(error.clone())));
                return;
            }
            _ => {
                let _ = self.sender.send(Err(ResolveError::Failed("model turn produced no complete score output".into())));
                return;
            }
        };
        let documents = self.documents;
        let context = self.context;
        let performer = self.performer;
        let preparation = super::resolver::prepare_output(move || {
            let output = documents.get_block_snapshot(context, &block)
                .map_err(|error| ResolveError::Failed(format!("read model output: {error}")))?
                .ok_or_else(|| ResolveError::Failed("model output block is missing".into()))?;
            if output.id.principal_id != performer || performer == PrincipalId::beat()
                || output.ephemeral || output.excluded || output.track.is_some()
                || output.role != Role::Model || output.kind != BlockKind::Text || output.status != Status::Done {
                return Err(ResolveError::Failed("score output is not the admitted performer's complete model text".into()));
            }
            if let Err(error) = validate_abc(output.content.as_bytes()) {
                let payload = kaijutsu_types::ErrorPayload {
                    category: kaijutsu_types::ErrorCategory::Parse,
                    severity: kaijutsu_types::ErrorSeverity::Error, code: None,
                    detail: Some(format!("could not use your phrase on the score: {error}")),
                    span: None, source_kind: None,
                };
                documents.insert_error_block_as(context, &block, &payload, payload.summary_line(), Some(performer))
                    .map_err(|write| ResolveError::Failed(format!("{error}; recording notation rejection failed: {write}")))?;
                documents.set_excluded(context, &block, true)
                    .map_err(|write| ResolveError::Failed(format!("{error}; excluding malformed notation failed: {write}")))?;
                return Err(ResolveError::Failed(error.to_string()));
            }
            Ok(Resolution::new(output.content.into_bytes(), ABC_MIME))
        });
        tokio::select! {
            biased;
            _ = self.sender.closed() => {}
            _ = interrupt.cancel.cancelled() => {
                let _ = self.sender.send(Err(ResolveError::Failed("model turn interrupted during score preparation".into())));
            }
            result = preparation => { let _ = self.sender.send(result); }
        }
    }
}

pub(crate) fn admit(
    kernel: &crate::Kernel, lease: &mut TurnLease, seed: BlockId, intent: &ScoreIntent,
) -> Result<(WorkId, SharedTimeline), String> {
    let context = lease.context();
    let performer = kernel.kernel_db().lock().get_context(context)
        .map_err(|error| format!("read score performer: {error}"))?
        .and_then(|row| row.played_by).ok_or_else(|| "score turn needs an assigned performer".to_string())?;
    let seed_block = kernel.blocks().get_block_snapshot(context, &seed)
        .map_err(|error| format!("read score seed: {error}"))?
        .ok_or_else(|| "score seed is missing".to_string())?;
    if seed_block.excluded || seed_block.ephemeral {
        return Err("score seed must be a durable, included block".into());
    }
    let timeline = kernel.track_timeline(&intent.track)
        .ok_or_else(|| format!("score track '{}' is not armed", intent.track.as_str()))?;
    let (delivery, receiver) = tokio::sync::oneshot::channel();
    let work = timeline.lock().schedule_preparing(Cell::deferred_on(
        Span::instant(intent.start), Recipe {
            resolver: ResolverId::new("model_output"),
            params: serde_json::json!({ "turn_id": lease.id() }),
            query: ContextQuery::default(), fallback: intent.fallback.clone(),
        }, intent.track.clone(), performer,
    ), Box::new(ModelOutput {
        receiver: parking_lot::Mutex::new(Some(receiver)), documents: kernel.blocks().clone(),
        context, seed, interrupt: lease.interrupt(),
    })).map_err(|error| format!("score admission refused: {error}"))?;
    lease.deliver_to(OutputDelivery { sender: delivery, documents: kernel.blocks().clone(), context, performer });
    Ok((work, timeline))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flows::TurnStopReason;
    use kaijutsu_types::{BlockSnapshotBuilder, TurnId};

    #[tokio::test]
    async fn hard_interrupt_wins_over_unobserved_prepared_output() {
        use futures::FutureExt;
        struct Context;
        impl ResolverCtx for Context {
            fn now(&self) -> Tick { Tick::ZERO }
            fn ambient(&self, _: &str) -> Option<Vec<u8>> { None }
            fn content_before(&self, _: Tick) -> Option<kaijutsu_hyoushigi::ContentRef> { None }
        }
        let kernel = crate::Kernel::new_ephemeral("score-interrupt").await;
        let context = ContextId::new();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let interrupt = ContextInterruptState::new();
        let resolver = ModelOutput {
            receiver: parking_lot::Mutex::new(Some(receiver)), documents: kernel.blocks().clone(),
            context, seed: BlockId::new(context, PrincipalId::new(), 0), interrupt: interrupt.clone(),
        };
        let future = resolver.resolve(&serde_json::Value::Null, &Context);
        sender.send(Ok(Resolution::new(b"prepared", ABC_MIME))).unwrap();
        interrupt.hard();
        assert!(future.now_or_never().unwrap().is_err(), "hard cancellation must not deliver unobserved score bytes");
    }

    #[tokio::test]
    async fn score_delivery_requires_complete_autonomous_player_text() {
        let kernel = crate::Kernel::new_ephemeral("score-delivery").await;
        let context = ContextId::new();
        let performer = PrincipalId::new();
        kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        for case in ["normal", "soft", "hard", "interactive", "no-output", "ephemeral", "excluded",
            "track", "wrong-author", "beat", "wrong-role", "wrong-kind", "unfinished"] {
            let author = match case {
                "wrong-author" => PrincipalId::new(), "beat" => PrincipalId::beat(), _ => performer,
            };
            let id = kernel.blocks().reserve_block_id(context, author).unwrap();
            let mut block = BlockSnapshotBuilder::new(id.clone(),
                if case == "wrong-kind" { BlockKind::Error } else { BlockKind::Text })
                .role(if case == "wrong-role" { Role::User } else { Role::Model })
                .status(if case == "unfinished" { Status::Running } else { Status::Done })
                .content("X:1\nK:C\nCDEF|\n")
                .ephemeral(case == "ephemeral").excluded(case == "excluded").build();
            if case == "track" { block.track = Some(TrackId::solo()); }
            kernel.blocks().insert_from_snapshot_as(context, block, None, Some(author)).unwrap();
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let delivery = OutputDelivery { sender, documents: kernel.blocks().clone(), context, performer };
            delivery.finish(&TurnFlow::Completed {
                turn_id: TurnId::new(), context_id: context, principal_id: performer,
                output_block_id: if case == "no-output" { None } else { Some(id) },
                reason: match case {
                    "soft" => TurnStopReason::Cancelled { immediate: false },
                    "hard" => TurnStopReason::Cancelled { immediate: true },
                    _ => TurnStopReason::EndTurn,
                },
                origin: if case == "interactive" { TurnOrigin::Interactive } else { TurnOrigin::Autonomous },
            }, ContextInterruptState::new()).await;
            assert_eq!(receiver.await.unwrap().is_ok(), matches!(case, "normal" | "soft"), "{case}");
        }
    }
}
