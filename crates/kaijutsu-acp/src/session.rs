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
use kaijutsu_client::rpc::KjCommandInfo;
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
    /// Exact command surface most recently advertised to this ACP session.
    /// Prompt classification never accepts a name outside this snapshot.
    commands: Mutex<Vec<KjCommandInfo>>,
    /// Closing this binding stops its event pump. The durable context remains
    /// in the kernel; ACP `session/delete` archives it separately.
    pump_stop: tokio::sync::watch::Sender<bool>,
}

impl Session {
    pub fn new(
        context_id: ContextId,
        label: String,
        mapper: UpdateMapper,
        commands: Vec<KjCommandInfo>,
    ) -> Self {
        let (pump_stop, _) = tokio::sync::watch::channel(false);
        Self {
            context_id,
            label,
            mapper: Arc::new(Mutex::new(mapper)),
            commands: Mutex::new(commands),
            pump_stop,
        }
    }

    pub fn commands(&self) -> Vec<KjCommandInfo> {
        self.commands.lock().clone()
    }

    /// Replace the advertised snapshot, returning whether it changed.
    pub fn replace_commands(&self, commands: Vec<KjCommandInfo>) -> bool {
        let mut current = self.commands.lock();
        if *current == commands {
            return false;
        }
        *current = commands;
        true
    }

    fn stop_pump(&self) {
        let _ = self.pump_stop.send(true);
    }
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

    /// Remove a live ACP binding and tell its pump to stop. This does not
    /// alter the durable context; callers perform the kernel operation first.
    pub fn unbind(&self, id: &SessionId) -> Option<Arc<Session>> {
        let session = self.sessions.lock().remove(id)?;
        session.stop_pump();
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
    let mut pump_stop = session.pump_stop.subscribe();
    // Deletion can race the small bind→spawn window. A receiver subscribed
    // after `send(true)` considers the current value already seen, so check
    // it explicitly instead of waiting for another change that will not come.
    if *pump_stop.borrow() {
        tracing::info!(session = %session_id, "session already unbound; pump not started");
        return Ok(());
    }

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
        let blocks = doc.blocks();
        if replay_history {
            for block in &blocks {
                for update in mapper.observe(block) {
                    let _ = cx.send_notification(SessionNotification::new(
                        session_id.clone(),
                        update,
                    ));
                }
            }
            // One plan, once, after the transcript — not per Task block
            // touched during replay. `build_plan` is idempotent (diffs
            // against `last_plan`, `None` on first call), so this is exactly
            // the same rebuild-and-emit path the live pump and resync use,
            // just called once at the end instead of per event.
            if let Some(update) = mapper.build_plan(&blocks) {
                let _ = cx.send_notification(SessionNotification::new(session_id.clone(), update));
            }
        } else {
            for block in &blocks {
                mapper.mark_seen(block);
            }
            // Silent baseline — an rc-seeded Task block should not be
            // narrated at a client that just opened `session/new`, same
            // reasoning as `mark_seen` for every other kind.
            mapper.baseline_plan(&blocks);
        }
    }

    // Trailing-edge catch-up: the kernel's FlowBus drops events server-side
    // under load (upstream of SSH — no client `Lagged` ever fires), so a
    // gap in OUR context's stream is invisible to the arms below. After any
    // burst of activity touching this context, one sweep re-observes the
    // rebuilt doc; the mapper's marks make it emit exactly what was missed
    // (usually nothing). Idle sessions never sweep. Third live victim
    // 2026-08-05: final tool-status patches + answer text dropped → toad
    // rendered perpetually-running tool calls over a finished turn.
    let mut sweep = tokio::time::interval(std::time::Duration::from_secs(5));
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut dirty = false;

    loop {
        tokio::select! {
            biased;
            stopped = pump_stop.changed() => {
                if stopped.is_err() || *pump_stop.borrow() {
                    tracing::info!(session = %session_id, "session unbound; pump exiting");
                    return Ok(());
                }
            }
            _ = sweep.tick() => {
                if dirty {
                    dirty = false;
                    resync(&bridge, &session, &session_id, &cx, &mut doc, "post-burst sweep").await;
                }
            }
            incoming = events.recv() => match incoming {
                Ok(event) => {
                    if event_context(&event) != Some(context_id) {
                        continue;
                    }
                    dirty = true;
                    if let ServerEvent::BlockDeleted { block_id, .. } = &event {
                        let deleted = *block_id;
                        // Apply to the mirror BEFORE returning. This arm used
                        // to `continue` straight past `doc.apply_event`, so a
                        // block deleted mid-session lived on in the live
                        // `SyncedDocument` (and so in `doc.blocks()`, and so in
                        // any rebuilt Task plan) until a resync threw the
                        // mirror away — `apply_event_inner` has had a real
                        // `BlockDeleted` handler the whole time, it was just
                        // never reached. Affects every block kind's live
                        // rendering, not only Task.
                        let effect = doc.apply_event(&event);
                        if matches!(effect, kaijutsu_client::SyncEffect::NeedsResync) {
                            resync(&bridge, &session, &session_id, &cx, &mut doc, "sync reset").await;
                            continue;
                        }
                        // Deleting a Task changes the plan. `build_plan`
                        // returns `None` when the rebuilt entries match what
                        // was last emitted, so calling it after any deletion
                        // is self-deduplicating — a non-Task delete sends
                        // nothing. One lock scope, deliberately: two chained
                        // `.lock()` calls in a single `if` condition share a
                        // temporary scope and self-deadlock (parking_lot is
                        // not reentrant) — the bug fixed in 183eeaff.
                        let plan = {
                            let mut mapper = session.mapper.lock();
                            mapper.forget(deleted);
                            mapper.build_plan(&doc.blocks())
                        };
                        if let Some(update) = plan {
                            let _ = cx.send_notification(SessionNotification::new(
                                session_id.clone(),
                                update,
                            ));
                        }
                        continue;
                    }
                    let effect = doc.apply_event(&event);
                    if matches!(effect, kaijutsu_client::SyncEffect::NeedsResync) {
                        resync(&bridge, &session, &session_id, &cx, &mut doc, "sync reset").await;
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
                    // `note_task` is a no-op (returns false) for anything
                    // that isn't a changed Task block, so this is safe to
                    // call unconditionally rather than gating on `block.kind`
                    // here too. ONE lock scope for note_task + build_plan:
                    // two `.lock()` calls chained in a single `if` condition
                    // share one temporary scope, so the first guard is still
                    // alive when the second acquires — a self-deadlock on the
                    // first live task event (parking_lot is not reentrant).
                    let plan = {
                        let mut mapper = session.mapper.lock();
                        if mapper.note_task(&block) {
                            mapper.build_plan(&doc.blocks())
                        } else {
                            None
                        }
                    };
                    if let Some(update) = plan {
                        let _ = cx.send_notification(SessionNotification::new(
                            session_id.clone(),
                            update,
                        ));
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(session = %session_id, dropped = n, "event stream lagged");
                    resync(&bridge, &session, &session_id, &cx, &mut doc, "events lagged").await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!(session = %session_id, "event stream closed; pump exiting");
                    return Ok(());
                }
            },
            change = status.recv() => match change {
                Ok(ConnectionStatus::Connected { .. }) => {
                    resync(&bridge, &session, &session_id, &cx, &mut doc, "reconnected").await;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    resync(&bridge, &session, &session_id, &cx, &mut doc, "status lagged").await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!(session = %session_id, "status stream closed; pump exiting");
                    return Ok(());
                }
            },
        }
    }
}

/// Rebuild the CRDT mirror and **catch the client up on the gap**.
///
/// The mapper's high-water marks are kept (pruned only for blocks that no
/// longer exist), and every block in the rebuilt doc is re-observed —
/// `observe()` emits exactly each block's unseen tail plus any tool-call
/// create/patch not yet announced, so the client receives what the gap
/// dropped and nothing twice. First live victim of the old
/// swallow-the-gap behavior (2026-08-05): a FlowBus lag mid-turn ate the
/// final report text — toad rendered the tool call, then silence, over a
/// finished answer.
async fn resync(
    bridge: &KernelBridge,
    session: &Arc<Session>,
    session_id: &SessionId,
    cx: &ConnectionTo<Client>,
    doc: &mut SyncedDocument,
    reason: &str,
) {
    match bridge.synced(session.context_id).await {
        Ok(fresh) => {
            *doc = fresh;
            let updates: Vec<_> = {
                let mut mapper = session.mapper.lock();
                let blocks = doc.blocks();
                let live: std::collections::HashSet<BlockId> =
                    blocks.iter().map(|b| b.id).collect();
                mapper.retain_marks(|id| live.contains(id));
                let mut updates: Vec<_> = blocks.iter().flat_map(|b| mapper.observe(b)).collect();
                // Same idempotence contract as `observe()`: rebuilding the
                // plan from unchanged task state is a no-op, so a resync over
                // a gap with no task activity stays silent, matching
                // `resync_sweep_emits_exactly_the_gap`'s "second sweep must
                // be silent" contract for every other block kind.
                if let Some(plan) = mapper.build_plan(&blocks) {
                    updates.push(plan);
                }
                updates
            };
            let emitted = updates.len();
            for update in updates {
                let _ = cx.send_notification(SessionNotification::new(
                    session_id.clone(),
                    update,
                ));
            }
            tracing::warn!(
                context = %session.context_id.short(),
                reason,
                emitted,
                "resynced; emitted catch-up updates covering the gap"
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

/// How long the event stream must stay quiet before the turn wait polls
/// ground truth. Long enough that active streaming never trips it (deltas
/// arrive every few ms when a turn is live, resetting the window); short
/// enough that a dropped completion resolves in seconds, not a stuck
/// spinner. Polling is unconditional-on-quiet, NOT gated on a client-side
/// `Lagged`: the 2026-08-05 drops happened in the KERNEL's FlowBus
/// (before SSH), so the bridge's own stream looked clean-with-a-hole and a
/// lag-gated recovery never armed — second stuck toad same day.
const TURN_WAIT_QUIET: std::time::Duration = std::time::Duration::from_secs(3);

/// Whether a context's block state says no turn is executing: nothing
/// `Running` and nothing `Pending`. Half of the quiet-poll fallback — the
/// live signal is [`ServerEvent::TurnCompleted`]; this is the durable
/// check when that signal was dropped.
fn turn_is_idle(blocks: &[kaijutsu_types::BlockSnapshot]) -> bool {
    use kaijutsu_types::Status;
    !blocks
        .iter()
        .any(|b| matches!(b.status, Status::Running | Status::Pending))
}

/// The full quiet-poll verdict: the turn both RAN (a model-authored block
/// exists at-or-after our prompt block) and SETTLED (nothing running or
/// pending). The ran-guard is what makes unconditional quiet-polling safe:
/// between `submit_input` and the model's first block the context is idle
/// but the turn hasn't happened yet — resolving there would answer the
/// prompt with `end_turn` before the model ever spoke.
fn turn_ran_and_settled(blocks: &[kaijutsu_types::BlockSnapshot], prompt: &BlockId) -> bool {
    use kaijutsu_types::Role;
    if !turn_is_idle(blocks) {
        return false;
    }
    let Some(prompt_at) = blocks.iter().find(|b| b.id == *prompt).map(|b| b.created_at) else {
        // Our own prompt block isn't visible yet — nothing has settled.
        return false;
    };
    blocks
        .iter()
        .any(|b| b.role == Role::Model && b.id != *prompt && b.created_at >= prompt_at)
}

/// Submit a prompt and wait for the turn to finish.
///
/// The subscription is taken **before** the write: the turn can complete
/// before `submit_input` returns for a fast refusal, and a subscriber that
/// arrived late would wait forever (the bus is lossy and un-journaled — there
/// is no catch-up).
///
/// Dropped-completion recovery: the completion event can be lost anywhere
/// on the lossy path (the kernel FlowBus overflowed live on 2026-08-05 —
/// upstream of SSH, invisible to this side's `Lagged`), so waiting on the
/// stream alone is not an option. Every quiet window triggers a poll of
/// the context's block state; a turn that both ran and settled
/// ([`turn_ran_and_settled`]) resolves as `EndTurn`. That stop reason is
/// honest-best-effort, not exact — the true reason has no block-log shadow
/// (`flows.rs`, "A subscriber that missed the push…"), which is the same
/// gap the tracked turn-id/catch-up work will close properly.
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
        let incoming = match tokio::time::timeout(TURN_WAIT_QUIET, events.recv()).await {
            Ok(r) => r,
            Err(_quiet) => {
                // Stream is quiet — poll ground truth.
                match bridge.synced(context_id).await {
                    Ok(doc) if turn_ran_and_settled(&doc.blocks(), &block_id) => {
                        tracing::warn!(
                            context = %context_id.short(),
                            "turn wait resolved by quiet-poll: turn ran and settled \
                             but no completion event arrived (dropped on the lossy \
                             bus); reporting end_turn (exact stop reason \
                             unrecoverable until the kernel catch-up story)"
                        );
                        settle_delivery(bridge, session, context_id).await;
                        return Ok(TurnOutcome::Stopped(StopReason::EndTurn));
                    }
                    Ok(_) => continue, // not started or still executing; keep waiting
                    Err(e) => {
                        tracing::warn!(
                            context = %context_id.short(),
                            error = %e,
                            "quiet-poll failed; continuing to wait"
                        );
                        continue;
                    }
                }
            }
        };
        match incoming {
            Ok(ServerEvent::TurnCompleted {
                context_id: ctx,
                stop_reason,
                origin,
                ..
            }) if ctx == context_id && origin == TurnOrigin::Interactive => {
                settle_delivery(bridge, session, context_id).await;
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
                // Informational only — quiet-polling runs regardless, and a
                // kernel-side drop never surfaces here anyway.
                tracing::warn!(
                    context = %context_id.short(),
                    dropped = n,
                    "turn wait lagged locally; quiet-poll will recover if the \
                     completion was in the window"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                anyhow::bail!("kernel event stream closed while waiting for the turn");
            }
        }
    }
}

/// Let the pump's delivery catch up with the kernel before the prompt
/// responds.
///
/// Turn events and block events ride separate ordered lanes (FlowBus
/// rework), so `TurnCompleted` can beat the last text chunks to the bridge.
/// Responding at that instant makes the client finalize its message widget
/// and drop the tail — a truncated final answer (first live hit 2026-08-05,
/// toad). Poll the mapper's high-water marks against the kernel's snapshot;
/// bounded, because the pump's 5s trailing-edge sweep repairs anything a
/// grace window this size somehow misses, after the response.
pub(crate) async fn settle_delivery(
    bridge: &KernelBridge,
    session: &Arc<Session>,
    context_id: ContextId,
) {
    const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(100);
    const SETTLE_MAX_POLLS: u32 = 20; // 2s ceiling
    for _ in 0..SETTLE_MAX_POLLS {
        match bridge.synced(context_id).await {
            Ok(doc) => {
                if session.mapper.lock().caught_up_with(&doc.blocks()) {
                    return;
                }
            }
            Err(e) => {
                tracing::warn!(
                    context = %context_id.short(),
                    error = %e,
                    "settle-delivery poll failed; responding without settling"
                );
                return;
            }
        }
        tokio::time::sleep(SETTLE_POLL).await;
    }
    tracing::warn!(
        context = %context_id.short(),
        "prompt response sent before delivery fully settled (2s ceiling); \
         the pump's trailing-edge sweep will deliver the remainder"
    );
}

/// Let the event pump publish a kj command's ToolResult before the ACP prompt
/// response closes the client's turn widget.
pub(crate) async fn settle_command_delivery(
    bridge: &KernelBridge,
    session: &Arc<Session>,
    command_block_id: BlockId,
) {
    const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(100);
    const SETTLE_MAX_POLLS: u32 = 20;
    for _ in 0..SETTLE_MAX_POLLS {
        match bridge.synced(session.context_id).await {
            Ok(doc) => {
                if session
                    .mapper
                    .lock()
                    .delivered_tool_result(&doc.blocks(), command_block_id)
                {
                    return;
                }
            }
            Err(e) => {
                tracing::warn!(
                    context = %session.context_id.short(),
                    error = %e,
                    "command settle-delivery poll failed"
                );
                return;
            }
        }
        tokio::time::sleep(SETTLE_POLL).await;
    }
    tracing::warn!(
        context = %session.context_id.short(),
        command = %command_block_id,
        "command response sent before its tool result was delivered (2s ceiling)"
    );
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
        Session::new(
            context_id,
            "test".into(),
            UpdateMapper::new(SessionId::new("s")),
            Vec::new(),
        )
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
    fn unbinding_removes_the_session_and_stops_its_pump() {
        let reg = SessionRegistry::default();
        let id = SessionId::new("s");
        let bound = reg.bind(id.clone(), session(ctx())).unwrap();
        let stop = bound.pump_stop.subscribe();

        assert!(reg.unbind(&id).is_some());
        assert!(reg.get(&id).is_none());
        assert!(*stop.borrow());
        assert!(reg.unbind(&id).is_none(), "retrying an unbind is harmless");
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

    /// Lag recovery's ground-truth check: a context with any Running or
    /// Pending block is still executing; only a fully-settled block set
    /// (done/error) counts as idle. First live hit 2026-08-05 — FlowBus
    /// overflow dropped the TurnCompleted and toad spun over a finished
    /// answer.
    #[test]
    fn lag_recovery_idle_means_no_running_or_pending_blocks() {
        use kaijutsu_types::Status;
        let c = ctx();
        let p = PrincipalId::new();
        let block = |seq: u64, status: Status| {
            BlockSnapshotBuilder::new(BlockId::new(c, p, seq), BlockKind::Text)
                .role(Role::Model)
                .status(status)
                .build()
        };

        assert!(turn_is_idle(&[]), "an empty context is idle");
        assert!(turn_is_idle(&[block(0, Status::Done), block(1, Status::Error)]));
        assert!(
            !turn_is_idle(&[block(0, Status::Done), block(1, Status::Running)]),
            "a streaming block means the turn is live — never resolve early"
        );
        assert!(
            !turn_is_idle(&[block(0, Status::Pending)]),
            "a queued tool call means the turn is live — never resolve early"
        );
    }

    /// The quiet-poll's full verdict. The ran-guard is what makes polling
    /// on EVERY quiet window safe (the drop can happen kernel-side, where
    /// no client `Lagged` ever fires — second stuck toad, 2026-08-05):
    /// idle-before-the-model-speaks must NOT resolve, and the prompt block
    /// alone is not evidence the turn ran.
    #[test]
    fn quiet_poll_requires_a_model_response_and_a_settled_context() {
        use kaijutsu_types::Status;
        let c = ctx();
        let bridge_p = PrincipalId::new();
        let model_p = PrincipalId::new();
        let mk = |p: PrincipalId, seq: u64, role: Role, status: Status, at: u64| {
            let mut b = BlockSnapshotBuilder::new(BlockId::new(c, p, seq), BlockKind::Text)
                .role(role)
                .status(status)
                .build();
            b.created_at = at;
            b
        };
        let prompt = mk(bridge_p, 1, Role::User, Status::Done, 100);
        let prompt_id = prompt.id;

        // Submitted but the model hasn't spoken: idle, yet NOT settled.
        assert!(!turn_ran_and_settled(std::slice::from_ref(&prompt), &prompt_id));
        // Prompt not even visible yet: not settled.
        assert!(!turn_ran_and_settled(&[], &prompt_id));
        // Model replied and everything is done: settled.
        let reply = mk(model_p, 1, Role::Model, Status::Done, 200);
        assert!(turn_ran_and_settled(&[prompt.clone(), reply.clone()], &prompt_id));
        // Model replied but a tool call is still running: not settled.
        let tool = mk(model_p, 2, Role::Model, Status::Running, 300);
        assert!(!turn_ran_and_settled(&[prompt.clone(), reply.clone(), tool], &prompt_id));
        // A model block from BEFORE our prompt (prior turn) is not evidence.
        let stale = mk(model_p, 0, Role::Model, Status::Done, 50);
        assert!(!turn_ran_and_settled(&[prompt, stale], &prompt_id));
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
