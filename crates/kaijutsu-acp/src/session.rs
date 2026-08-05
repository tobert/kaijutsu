//! Sessions: the ACP↔kj binding, the event pump, and the turn wait.
//!
//! One ACP session binds **one context id** for the prototype. The ACP session
//! id *is* the context id in hex, so it survives a bridge restart, a kernel
//! restart, and a `session/list` round trip with no side table. Whether a
//! session should instead *follow* a context through fork rolls is still open
//! — see docs/acp.md, "Open questions".

use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{SessionId, SessionNotification, StopReason};
use agent_client_protocol::{Client, ConnectionTo};
use kaijutsu_client::{ConnectionStatus, ServerEvent, SyncedDocument, TurnOrigin};
use kaijutsu_crdt::{BlockId, ContextId};
use parking_lot::Mutex;

use crate::bridge::KernelBridge;
use crate::update::{UpdateMapper, acp_stop_reason};

/// A bound ACP session.
pub struct Session {
    pub context_id: ContextId,
    pub label: String,
    /// Shared with the pump task: the pump emits through it, the prompt
    /// handler arms echo suppression on it.
    pub mapper: Arc<Mutex<UpdateMapper>>,
}

/// All bound sessions, keyed by ACP session id.
#[derive(Default)]
pub struct SessionRegistry {
    sessions: Mutex<HashMap<SessionId, Arc<Session>>>,
}

impl SessionRegistry {
    pub fn get(&self, id: &SessionId) -> Option<Arc<Session>> {
        self.sessions.lock().get(id).cloned()
    }

    /// Insert, returning `None` if this session was already bound (in which
    /// case the existing binding — and its pump — is kept).
    pub fn bind(&self, id: SessionId, session: Session) -> Option<Arc<Session>> {
        let mut map = self.sessions.lock();
        if map.contains_key(&id) {
            return None;
        }
        let session = Arc::new(session);
        map.insert(id, Arc::clone(&session));
        Some(session)
    }

    pub fn len(&self) -> usize {
        self.sessions.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The block id an event is about, if any.
fn event_block(event: &ServerEvent) -> Option<BlockId> {
    match event {
        ServerEvent::BlockInserted { block, .. } => Some(block.id),
        ServerEvent::BlockTextOps { block_id, .. }
        | ServerEvent::BlockStatusChanged { block_id, .. }
        | ServerEvent::BlockOutputChanged { block_id, .. }
        | ServerEvent::BlockMetadataChanged { block_id, .. }
        | ServerEvent::BlockCollapsedChanged { block_id, .. }
        | ServerEvent::BlockExcludedChanged { block_id, .. }
        | ServerEvent::BlockMoved { block_id, .. } => Some(*block_id),
        _ => None,
    }
}

/// The context id an event names, if any.
fn event_context(event: &ServerEvent) -> Option<ContextId> {
    match event {
        ServerEvent::BlockInserted { context_id, .. }
        | ServerEvent::BlockTextOps { context_id, .. }
        | ServerEvent::BlockStatusChanged { context_id, .. }
        | ServerEvent::BlockOutputChanged { context_id, .. }
        | ServerEvent::BlockMetadataChanged { context_id, .. }
        | ServerEvent::BlockDeleted { context_id, .. }
        | ServerEvent::BlockCollapsedChanged { context_id, .. }
        | ServerEvent::BlockExcludedChanged { context_id, .. }
        | ServerEvent::BlockMoved { context_id, .. }
        | ServerEvent::SyncReset { context_id, .. } => Some(*context_id),
        _ => None,
    }
}

/// Stream one context's blocks to an ACP client as `session/update`
/// notifications, forever.
///
/// Always returns `Ok(())`: this runs under `ConnectionTo::spawn`, where an
/// `Err` tears the whole ACP connection down. A pump that failed should stop
/// pumping, not hang up on the user.
pub async fn run_pump(
    bridge: KernelBridge,
    session: Arc<Session>,
    session_id: SessionId,
    cx: ConnectionTo<Client>,
    mut doc: SyncedDocument,
    replay_history: bool,
) -> Result<(), agent_client_protocol::Error> {
    let context_id = session.context_id;

    // Subscribe before the first read so nothing that lands during setup is
    // lost between the snapshot and the stream.
    let mut events = bridge.actor().subscribe_events();
    let mut status = bridge.actor().subscribe_status();

    // `session/load` wants the conversation replayed as updates so the client
    // can render history. `session/new` does not — a brand-new context has
    // nothing to say, and an rc-seeded one should not narrate its own
    // bootstrap.
    {
        let mut mapper = session.mapper.lock();
        for block in doc.blocks() {
            if replay_history {
                for update in mapper.observe(&block) {
                    let _ = cx.send_notification(SessionNotification::new(
                        session_id.clone(),
                        update,
                    ));
                }
            } else {
                mapper.mark_seen(&block);
            }
        }
    }

    loop {
        tokio::select! {
            incoming = events.recv() => match incoming {
                Ok(event) => {
                    if event_context(&event) != Some(context_id) {
                        continue;
                    }
                    if let ServerEvent::BlockDeleted { block_id, .. } = &event {
                        session.mapper.lock().forget(*block_id);
                        continue;
                    }
                    let effect = doc.apply_event(&event);
                    if matches!(effect, kaijutsu_client::SyncEffect::NeedsResync) {
                        resync(&bridge, &session, &mut doc, "sync reset").await;
                        continue;
                    }
                    let Some(block_id) = event_block(&event) else { continue };
                    let Some(block) = doc.get_block(&block_id) else { continue };
                    let updates = session.mapper.lock().observe(&block);
                    for update in updates {
                        let _ = cx.send_notification(SessionNotification::new(
                            session_id.clone(),
                            update,
                        ));
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(session = %session_id, dropped = n, "event stream lagged");
                    resync(&bridge, &session, &mut doc, "events lagged").await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!(session = %session_id, "event stream closed; pump exiting");
                    return Ok(());
                }
            },
            change = status.recv() => match change {
                Ok(ConnectionStatus::Connected { .. }) => {
                    resync(&bridge, &session, &mut doc, "reconnected").await;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    resync(&bridge, &session, &mut doc, "status lagged").await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!(session = %session_id, "status stream closed; pump exiting");
                    return Ok(());
                }
            },
        }
    }
}

/// Rebuild the CRDT mirror and re-peg the mapper **without replaying**.
///
/// Anything that changed while we were desynced is lost to this client — a
/// deliberate trade against re-sending the whole transcript. Logged at `warn`
/// so a gap is never silent; a real catch-up story is the same open question
/// the turn-events work deferred (docs/acp.md).
async fn resync(
    bridge: &KernelBridge,
    session: &Arc<Session>,
    doc: &mut SyncedDocument,
    reason: &str,
) {
    match bridge.synced(session.context_id).await {
        Ok(fresh) => {
            *doc = fresh;
            let mut mapper = session.mapper.lock();
            mapper.reset();
            for block in doc.blocks() {
                mapper.mark_seen(&block);
            }
            tracing::warn!(
                context = %session.context_id.short(),
                reason,
                "resynced; updates during the gap were not delivered to this client"
            );
        }
        Err(e) => {
            tracing::error!(
                context = %session.context_id.short(),
                reason,
                error = %e,
                "resync failed; this session's stream is now stale"
            );
        }
    }
}

/// Why a turn wait ended.
#[derive(Debug)]
pub enum TurnOutcome {
    Stopped(StopReason),
    /// The turn itself broke. Distinct from a stop reason on purpose — ACP has
    /// no "failed" stop, so this becomes a JSON-RPC error rather than a
    /// successful turn that quietly produced nothing.
    Failed(String),
}

/// Submit a prompt and wait for the turn to finish.
///
/// The subscription is taken **before** the write: the turn can complete
/// before `submit_input` returns for a fast refusal, and a subscriber that
/// arrived late would wait forever (the bus is lossy and un-journaled — there
/// is no catch-up).
pub async fn run_turn(
    bridge: &KernelBridge,
    session: &Arc<Session>,
    text: &str,
) -> anyhow::Result<TurnOutcome> {
    let context_id = session.context_id;
    let mut events = bridge.actor().subscribe_events();

    session.mapper.lock().arm_echo_suppression();
    let block_id = bridge.send_prompt(context_id, text).await?;
    session.mapper.lock().suppress(block_id);
    tracing::debug!(context = %context_id.short(), block = %block_id, "prompt submitted");

    loop {
        match events.recv().await {
            Ok(ServerEvent::TurnCompleted {
                context_id: ctx,
                stop_reason,
                origin,
                ..
            }) if ctx == context_id && origin == TurnOrigin::Interactive => {
                return Ok(TurnOutcome::Stopped(acp_stop_reason(stop_reason)));
            }
            Ok(ServerEvent::TurnFailed {
                context_id: ctx,
                error,
                origin,
                ..
            }) if ctx == context_id && origin == TurnOrigin::Interactive => {
                return Ok(TurnOutcome::Failed(error));
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                // Our completion may have been in the dropped window. Say so
                // rather than hanging silently forever.
                tracing::warn!(
                    context = %context_id.short(),
                    dropped = n,
                    "turn wait lagged; the completion event may have been dropped"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                anyhow::bail!("kernel event stream closed while waiting for the turn");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_crdt::PrincipalId;
    use kaijutsu_types::{BlockKind, BlockSnapshotBuilder, Role};

    fn ctx() -> ContextId {
        ContextId::new()
    }

    fn session(context_id: ContextId) -> Session {
        Session {
            context_id,
            label: "test".into(),
            mapper: Arc::new(Mutex::new(UpdateMapper::new(SessionId::new("s")))),
        }
    }

    #[test]
    fn a_session_binds_exactly_once() {
        let reg = SessionRegistry::default();
        let id = SessionId::new("s");
        assert!(reg.bind(id.clone(), session(ctx())).is_some());
        assert!(
            reg.bind(id.clone(), session(ctx())).is_none(),
            "re-binding must keep the existing pump, not start a second one"
        );
        assert_eq!(reg.len(), 1);
        assert!(reg.get(&id).is_some());
    }

    #[test]
    fn an_unknown_session_is_not_found() {
        let reg = SessionRegistry::default();
        assert!(reg.get(&SessionId::new("nope")).is_none());
        assert!(reg.is_empty());
    }

    #[test]
    fn events_are_routed_by_the_context_they_name() {
        let mine = ctx();
        let theirs = ctx();
        let block = BlockSnapshotBuilder::new(
            BlockId::new(mine, PrincipalId::new(), 1),
            BlockKind::Text,
        )
        .role(Role::Model)
        .build();

        let ev = ServerEvent::BlockInserted {
            context_id: mine,
            block: Box::new(block.clone()),
            ops: Vec::new(),
        };
        assert_eq!(event_context(&ev), Some(mine));
        assert_ne!(event_context(&ev), Some(theirs));
        assert_eq!(event_block(&ev), Some(block.id));
    }

    #[test]
    fn per_block_events_name_their_block() {
        let c = ctx();
        let b = BlockId::new(c, PrincipalId::new(), 7);
        let ev = ServerEvent::BlockStatusChanged {
            context_id: c,
            block_id: b,
            status: kaijutsu_types::Status::Done,
        };
        assert_eq!(event_context(&ev), Some(c));
        assert_eq!(event_block(&ev), Some(b));
    }

    #[test]
    fn turn_events_carry_no_block_and_are_not_pump_traffic() {
        // The pump ignores them; `run_turn` owns them off its own receiver.
        let c = ctx();
        let ev = ServerEvent::TurnCompleted {
            context_id: c,
            principal_id: kaijutsu_types::PrincipalId::new(),
            output_block_id: None,
            stop_reason: kaijutsu_client::TurnCompletedStopReason::EndTurn,
            origin: TurnOrigin::Interactive,
        };
        assert_eq!(event_block(&ev), None);
        assert_eq!(
            event_context(&ev),
            None,
            "turn events are handled by run_turn, not routed by the pump"
        );
    }
}
