//! Sessions: the ACP↔kj binding, the event pump, and the turn wait.
//!
//! One ACP session binds **one context id** for the prototype. The ACP session
//! id *is* the context id in hex, so it survives a bridge restart, a kernel
//! restart, and a `session/list` round trip with no side table. Whether a
//! session should instead *follow* a context through fork rolls is still open
//! — see docs/acp.md, "Open questions".

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agent_client_protocol::schema::v1::{SessionId, SessionNotification, SessionUpdate, StopReason};
use agent_client_protocol::{Client, ConnectionTo};
use kaijutsu_client::{
    ContextChange, ContextDelivery, ContextInfo, ContextMirror, FeedEvent, ServerEvent, TurnOrigin,
};
use kaijutsu_client::rpc::KjCommandInfo;
use kaijutsu_types::{BlockId, ContextId};
use parking_lot::Mutex;
use tokio::sync::mpsc;

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

    /// An arbitrary live session's context id, to run an admin `kj` command
    /// in when it doesn't matter which one — e.g. `kj ledger list`, which
    /// reads kernel-wide state and is not scoped to any particular context.
    /// `None` when no session is bound (`permission::poll_ledger` skips its
    /// tick in that case rather than picking nothing to run in).
    pub fn any_context_id(&self) -> Option<ContextId> {
        self.sessions.lock().values().next().map(|s| s.context_id)
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
    mut mirror: ContextMirror,
    mut feed_rx: mpsc::Receiver<FeedEvent>,
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

    // `session/load` wants the conversation replayed as updates so the client
    // can render history. `session/new` does not — a brand-new context has
    // nothing to say, and an rc-seeded one should not narrate its own
    // bootstrap.
    // Fetched before the mapper lock, not inside it — an RPC round trip has
    // no business happening under a `parking_lot::Mutex`. Best-effort: `None`
    // on any failure, and the block below already treats "no usage info"
    // as "nothing to baseline/emit this time" (see `fetch_usage_info`).
    let usage_info = fetch_usage_info(&bridge, &session).await;
    {
        let mut mapper = session.mapper.lock();
        let blocks = mirror.blocks();
        if replay_history {
            for block in blocks {
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
            // the same rebuild-and-emit path the live pump and a rehydrate
            // catch-up use, just called once at the end instead of per event.
            if let Some(update) = mapper.build_plan(blocks) {
                let _ = cx.send_notification(SessionNotification::new(session_id.clone(), update));
            }
            // Same posture for the usage gauge: one emission at the end of
            // replay, not a value per historical turn — `build_usage` stays
            // silent on its own if the window is unknown or unchanged.
            if let Some(info) = &usage_info
                && let Some(update) = mapper.build_usage(info)
            {
                let _ = cx.send_notification(SessionNotification::new(session_id.clone(), update));
            }
        } else {
            for block in blocks {
                mapper.mark_seen(block);
            }
            // Silent baseline — an rc-seeded Task block should not be
            // narrated at a client that just opened `session/new`, same
            // reasoning as `mark_seen` for every other kind.
            mapper.baseline_plan(blocks);
            // Same silent baseline for usage: `session/new` and
            // `session/resume` both land here, and neither should narrate a
            // usage figure the client never asked to see mid-attachment. The
            // first real emission comes at the next turn boundary.
            if let Some(info) = &usage_info {
                mapper.baseline_usage(info);
            }
        }
    }

    // No trailing-edge sweep here on purpose. The old firehose (kernel-wide
    // `ServerEvent` broadcast) could silently drop events under load —
    // upstream of SSH, invisible to this side — so a 5s poll existed purely
    // to paper over that. The change feed cannot do that: a subscriber that
    // falls behind gets `FeedEvent::Terminated` (loud), never a silent gap
    // (docs/change-feed.md, the `mpsc` doc comment on `context_feed.rs`). The
    // `Resubscribed`/`Terminated` arms below are what replace the sweep.
    loop {
        tokio::select! {
            biased;
            stopped = pump_stop.changed() => {
                if stopped.is_err() || *pump_stop.borrow() {
                    tracing::info!(session = %session_id, "session unbound; pump exiting");
                    return Ok(());
                }
            }
            incoming = feed_rx.recv() => match incoming {
                Some(FeedEvent::Changed(delivery)) => {
                    apply_delivery(&session, &session_id, &cx, &mut mirror, delivery);
                }
                Some(FeedEvent::Resubscribed) => {
                    // The actor already re-subscribed on `feed_rx`'s behalf;
                    // nothing published during the outage rides it. Throw the
                    // mirror away and rehydrate on the SAME receiver.
                    tracing::warn!(
                        session = %session_id,
                        "context feed resubscribed after a reconnect; rehydrating"
                    );
                    match bridge.rehydrate_context(context_id).await {
                        Ok(fresh) => {
                            mirror = fresh;
                            catch_up(&bridge, &session, &session_id, &cx, &mirror, "reconnected").await;
                        }
                        Err(e) => tracing::error!(
                            session = %session_id,
                            error = %e,
                            "rehydrate after resubscribe failed; this session's stream is now stale"
                        ),
                    }
                }
                Some(FeedEvent::Terminated { reason, delivered_version }) => {
                    // Rule 28: the subscriber fell behind and the feed will
                    // not resume on this receiver — re-subscribe from
                    // scratch, on a brand-new receiver, and rehydrate.
                    tracing::warn!(
                        session = %session_id,
                        ?reason,
                        delivered_version,
                        "context feed terminated (subscriber fell behind); re-subscribing"
                    );
                    match bridge.hydrate_context(context_id).await {
                        Ok((fresh, new_rx)) => {
                            mirror = fresh;
                            feed_rx = new_rx;
                            catch_up(&bridge, &session, &session_id, &cx, &mirror, "feed terminated").await;
                        }
                        Err(e) => tracing::error!(
                            session = %session_id,
                            error = %e,
                            "re-subscribe after termination failed; this session's stream is now stale"
                        ),
                    }
                }
                None => {
                    tracing::info!(session = %session_id, "context feed closed; pump exiting");
                    return Ok(());
                }
            },
        }
    }
}

/// Apply one delivery to the mirror and send whatever `session/update`s it
/// produces. Thin I/O wrapper over [`deliver_updates`], which does the actual
/// work and is pure — see its doc comment.
fn apply_delivery(
    session: &Arc<Session>,
    session_id: &SessionId,
    cx: &ConnectionTo<Client>,
    mirror: &mut ContextMirror,
    delivery: ContextDelivery,
) {
    for update in deliver_updates(session, session_id, mirror, delivery) {
        let _ = cx.send_notification(SessionNotification::new(session_id.clone(), update));
    }
}

/// Apply one delivery to the mirror, then translate every block it touched
/// into the `session/update`s the client hasn't seen — one `observe()` call
/// per touched block, made AFTER the whole delivery lands rather than one
/// per individual `ContextChange`. That is what gives a tool's final output
/// text and its `Done` status a single combined patch instead of a race: they
/// are two events in one delivery (docs/change-feed.md rule 15), and
/// `observe_tool_result` already folds status+content into one update when it
/// sees both at once.
///
/// No I/O and no `ConnectionTo` — this is the piece worth unit-testing
/// without a kernel, mirroring `UpdateMapper::observe`'s "pure" design in
/// `update.rs`. [`apply_delivery`] is the thin wrapper that sends the result.
fn deliver_updates(
    session: &Arc<Session>,
    session_id: &SessionId,
    mirror: &mut ContextMirror,
    delivery: ContextDelivery,
) -> Vec<SessionUpdate> {
    let mut deleted = Vec::new();
    let mut touched = Vec::new();
    for change in delivery.changes() {
        match change {
            ContextChange::BlockDeleted { block_id } => deleted.push(*block_id),
            ContextChange::BlockInserted { block, .. } => touched.push(block.id),
            ContextChange::BlockMoved { block_id, .. }
            | ContextChange::TextAppended { block_id, .. }
            | ContextChange::TextReplaced { block_id, .. }
            | ContextChange::StatusChanged { block_id, .. }
            | ContextChange::CollapsedChanged { block_id, .. }
            | ContextChange::ExcludedChanged { block_id, .. }
            | ContextChange::MetadataChanged { block_id, .. }
            | ContextChange::OutputChanged { block_id, .. } => touched.push(*block_id),
            // Styling only. ACP sends plain text, so a span reprojection
            // changes nothing this client would emit — the mirror still
            // folds it in below, we just owe the peer no update for it.
            ContextChange::SpansChanged { .. } => {}
        }
    }

    if let Err(e) = mirror.receive(delivery) {
        tracing::error!(
            session = %session_id,
            error = %e,
            "context mirror refused a delivery; this session's stream is now stale"
        );
        return Vec::new();
    }

    let mut seen = HashSet::new();
    touched.retain(|id| seen.insert(*id));
    // A deletion always makes the plan worth rebuilding. `build_plan` is
    // self-deduplicating (see its doc comment), so a non-Task delete costs
    // one no-op diff and sends nothing.
    let mut plan_dirty = !deleted.is_empty();

    let mut mapper = session.mapper.lock();
    for id in &deleted {
        mapper.forget(*id);
    }
    let mut updates = Vec::new();
    for id in &touched {
        let Some(block) = mirror.block(id) else { continue };
        updates.extend(mapper.observe(block));
        if mapper.note_task(block) {
            plan_dirty = true;
        }
    }
    if plan_dirty
        && let Some(plan) = mapper.build_plan(mirror.blocks())
    {
        updates.push(plan);
    }
    updates
}

/// Best-effort fetch of the context's current usage/window info, for the
/// `session/update` usage gauge (`UpdateMapper::build_usage`/
/// `baseline_usage`, docs/acp.md hunk 4).
///
/// `resolve_context_label` — the same narrow, addressed lookup
/// `KernelBridge::open_or_create` already uses to attach by label — is
/// reused here rather than `list_contexts()`: the usage gauge only ever
/// needs ONE context's row, and this session already knows its label. A
/// failure degrades to "no usage update this time", never to failing the
/// pump or the turn — the usage gauge is an ergonomic extra, not load-bearing
/// for the conversation itself.
async fn fetch_usage_info(bridge: &KernelBridge, session: &Session) -> Option<ContextInfo> {
    match bridge.actor().resolve_context_label(&session.label).await {
        Ok(Some(info)) => Some(info),
        Ok(None) => {
            tracing::warn!(
                context = %session.context_id.short(),
                label = %session.label,
                "context vanished while fetching usage info; skipping this usage update"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                context = %session.context_id.short(),
                error = %e,
                "usage info fetch failed; skipping this usage update"
            );
            None
        }
    }
}

/// Send the `session/update` usage gauge once, at a turn boundary —
/// [`run_turn`]'s two `Stopped`-returning arms both call this right after
/// [`settle_delivery`]. Turn boundaries are the natural cadence: unlike text
/// or tool-call deltas, a usage figure only changes when a completed LLM
/// call lands, and emitting on every intermediate event would be a firehose
/// of duplicate values `build_usage`'s dedup would just be swallowing
/// anyway. Best-effort — a fetch failure or an unknown window means no
/// notification, never a fabricated one (see [`fetch_usage_info`] and
/// `UpdateMapper::build_usage`).
async fn emit_usage_update(
    bridge: &KernelBridge,
    session: &Arc<Session>,
    session_id: &SessionId,
    cx: &ConnectionTo<Client>,
) {
    let Some(info) = fetch_usage_info(bridge, session).await else {
        return;
    };
    if let Some(update) = session.mapper.lock().build_usage(&info) {
        let _ = cx.send_notification(SessionNotification::new(session_id.clone(), update));
    }
}

/// Catch the client up after a fresh mirror replaces a stale one
/// (`FeedEvent::Resubscribed` or `Terminated`).
///
/// The mapper's high-water marks are kept (pruned only for blocks no longer
/// live), and every block in the fresh mirror is re-observed — `observe()`
/// emits exactly each block's unseen tail plus any tool-call create/patch not
/// yet announced, so the client receives whatever the gap held and nothing
/// twice. Same treatment for the usage gauge: a resync is exactly the kind
/// of gap that could hide a turn's worth of usage change, so it refreshes
/// unconditionally alongside the plan rebuild — `build_usage`'s own dedup
/// keeps it silent when nothing moved.
async fn catch_up(
    bridge: &KernelBridge,
    session: &Arc<Session>,
    session_id: &SessionId,
    cx: &ConnectionTo<Client>,
    mirror: &ContextMirror,
    reason: &str,
) {
    let blocks = mirror.blocks();
    let usage_info = fetch_usage_info(bridge, session).await;
    let updates: Vec<_> = {
        let mut mapper = session.mapper.lock();
        let live: HashSet<BlockId> = blocks.iter().map(|b| b.id).collect();
        mapper.retain_marks(|id| live.contains(id));
        let mut updates: Vec<_> = blocks.iter().flat_map(|b| mapper.observe(b)).collect();
        // Same idempotence contract as `observe()`: rebuilding the plan from
        // unchanged task state is a no-op, so a rehydrate over a gap with no
        // task activity stays silent.
        if let Some(plan) = mapper.build_plan(blocks) {
            updates.push(plan);
        }
        if let Some(info) = &usage_info
            && let Some(usage) = mapper.build_usage(info)
        {
            updates.push(usage);
        }
        updates
    };
    let emitted = updates.len();
    for update in updates {
        let _ = cx.send_notification(SessionNotification::new(session_id.clone(), update));
    }
    tracing::warn!(
        session = %session_id,
        context = %session.context_id.short(),
        reason,
        emitted,
        "rehydrated the context mirror; emitted catch-up updates covering the gap"
    );
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
    session_id: &SessionId,
    cx: &ConnectionTo<Client>,
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
                match bridge.actor().get_all_blocks(context_id).await {
                    Ok(blocks) if turn_ran_and_settled(&blocks, &block_id) => {
                        tracing::warn!(
                            context = %context_id.short(),
                            "turn wait resolved by quiet-poll: turn ran and settled \
                             but no completion event arrived (dropped on the lossy \
                             bus); reporting end_turn (exact stop reason \
                             unrecoverable until the kernel catch-up story)"
                        );
                        settle_delivery(bridge, session, context_id).await;
                        emit_usage_update(bridge, session, session_id, cx).await;
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
                emit_usage_update(bridge, session, session_id, cx).await;
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
/// Turn events ride `ServerEvent`'s broadcast; block text rides the context
/// change feed — two genuinely separate lanes, so `TurnCompleted` can beat
/// the last text chunks to the bridge. Responding at that instant makes the
/// client finalize its message widget and drop the tail — a truncated final
/// answer (first live hit 2026-08-05, toad). Poll the mapper's high-water
/// marks against the kernel's snapshot; bounded, because the change feed
/// itself never drops a delivery (docs/change-feed.md) — a timeout here just
/// means the pump's own `feed_rx` processing hasn't caught up yet, and it
/// will, on its own schedule, whether or not anything is still watching.
pub(crate) async fn settle_delivery(
    bridge: &KernelBridge,
    session: &Arc<Session>,
    context_id: ContextId,
) {
    const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(100);
    const SETTLE_MAX_POLLS: u32 = 20; // 2s ceiling
    for _ in 0..SETTLE_MAX_POLLS {
        match bridge.actor().get_all_blocks(context_id).await {
            Ok(blocks) => {
                if session.mapper.lock().caught_up_with(&blocks) {
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
         the pump will still deliver the remainder as it drains the feed"
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
        match bridge.actor().get_all_blocks(session.context_id).await {
            Ok(blocks) => {
                if session
                    .mapper
                    .lock()
                    .delivered_tool_result(&blocks, command_block_id)
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
    use kaijutsu_types::PrincipalId;
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

    /// The whole point of the migration (docs/change-feed.md): ACP's
    /// rendering path takes typed `ContextChange`s off the change feed and a
    /// `kaijutsu_client::ContextMirror` — never a `SyncedDocument`, never a
    /// diamond-types op byte. This drives exactly that path — subscribe,
    /// snapshot, deliver — and checks the ACP `SessionUpdate` it produces,
    /// with no text-engine type in scope anywhere in the test.
    #[test]
    fn a_text_change_reaches_acp_rendering_with_no_text_engine_involved() {
        let c = ctx();
        let snap = BlockSnapshotBuilder::new(BlockId::new(c, PrincipalId::new(), 1), BlockKind::Text)
            .role(Role::Model)
            .content("")
            .build();
        let id = snap.id;

        let mut mirror = ContextMirror::new(c);
        mirror.apply_snapshot(vec![snap], 1).unwrap();

        let s = Arc::new(session(c));
        let sid = SessionId::new("s");
        // The text write and the completion are two kernel mutations, so two
        // versions — one delivery, which is the property: a client never sees
        // a finished turn with its text missing.
        let delivery = ContextDelivery {
            context_id: c,
            version: 3,
            events: vec![
                kaijutsu_client::context_feed::VersionedChange {
                    version: 2,
                    change: ContextChange::TextAppended {
                        block_id: id,
                        suffix: "hello from the feed".into(),
                    },
                },
                kaijutsu_client::context_feed::VersionedChange {
                    version: 3,
                    change: ContextChange::StatusChanged {
                        block_id: id,
                        status: kaijutsu_types::Status::Done,
                    },
                },
            ],
        };

        let updates = deliver_updates(&s, &sid, &mut mirror, delivery);
        assert_eq!(updates.len(), 1, "one agent-message chunk: {updates:?}");
        let SessionUpdate::AgentMessageChunk(chunk) = &updates[0] else {
            panic!("expected an agent message chunk, got {:?}", updates[0]);
        };
        let agent_client_protocol::schema::v1::ContentBlock::Text(text) = &chunk.content else {
            panic!("expected text content, got {:?}", chunk.content);
        };
        assert_eq!(text.text, "hello from the feed");

        // The mirror itself now holds the kernel's text — reached purely by
        // applying typed `ContextChange`s, never by decoding a storage-engine op.
        assert_eq!(mirror.block(&id).unwrap().content, "hello from the feed");
        assert_eq!(
            mirror.version(),
            3,
            "the mirror advances to the LAST event's version, not the first"
        );
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

}
