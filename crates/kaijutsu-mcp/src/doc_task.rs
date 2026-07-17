//! Sole-writer command channel for the MCP `SyncedDocument`.
//!
//! Before this module existed, `RemoteState.synced` had THREE writers: the
//! background event listener (applied `ServerEvent`s, ran resyncs),
//! `HookListener` (authored blocks directly via `synced.lock()` +
//! `doc_mut().insert_*`, then pushed), and `execute_and_poll_shell`'s stall
//! fallback (called `resync_synced` directly). All three took the same
//! `parking_lot::Mutex`, so no individual mutation ever corrupted the doc —
//! but the *sequences* raced: a block authored while a resync's fetch RPC was
//! in flight could be wiped by `apply_sync_state` replacing the doc wholesale
//! (the lost-update window), two concurrent resyncs could apply stale-last,
//! and `flush_local_ops` based its push on the *inbound* sync frontier (which
//! local authoring never advances), so every push re-sent every local op ever
//! made. See docs/issues.md, "kaijutsu-mcp — June 2026 SyncedDocument
//! migration review" (HIGH).
//!
//! This module makes one task — [`run_doc_task`] — the TRUE sole writer.
//! Every mutation arrives as a [`DocCommand`] on one mpsc channel: applying a
//! server event, authoring locally-produced blocks, or running a resync.
//! Reads still go straight through the shared mutex (`RemoteState.synced`) —
//! only *mutation* moved to the channel.
//!
//! [`DocTaskHandle`] is the producer-side API. [`spawn_event_bridge`] adapts
//! the actor's broadcast event/status streams into the same channel, so the
//! task loop only ever has one thing to select on: `rx.recv()`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use kaijutsu_client::{ActorHandle, ConnectionStatus, DocSyncBackend, ServerEvent, SyncEffect, SyncedDocument};
use kaijutsu_crdt::{BlockId, BlockKind, ContentType, ContextId, Frontier, Role, Status, ToolKind};

/// Channel capacity for the doc task's command mpsc. Generous — a burst of
/// hook events, hydrated resync triggers (Lagged event + Lagged status +
/// stall fallback can all fire close together), and queued author requests
/// should never need to block a producer under normal operation.
const DOC_TASK_CHANNEL_CAPACITY: usize = 256;

// ============================================================================
// Command / data types
// ============================================================================

/// Data description of a single locally-authored block insertion.
///
/// Replaces what `HookListener::insert_text_block` / `insert_tool_blocks`
/// used to do *imperatively* against `SyncedDocument` directly. The producer
/// (HookListener) now just describes what it wants; the doc task is the one
/// that actually touches the document.
#[derive(Debug, Clone)]
pub enum AuthoredBlock {
    /// A plain text block (session/system/user/model narration).
    Text { role: Role, content: String },
    /// A tool call + its result, inserted as a linked pair and immediately
    /// completed (Done/Error) — mirrors the old `insert_tool_blocks`'s
    /// two-insert + `set_status` sequence in one atomic-under-the-lock unit.
    ToolCallResult {
        tool_name: String,
        tool_input: serde_json::Value,
        result_content: String,
        is_error: bool,
        tool_kind: Option<ToolKind>,
    },
}

/// Why a [`DocCommand::Resync`] was requested — carried for logging /
/// coalescing visibility only, not branched on inside the resync itself
/// (every reason runs the identical flush→fetch→apply routine).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResyncReason {
    /// A `ServerEvent::SyncReset` was applied and reported `NeedsResync`.
    NeedsResync,
    /// The block-events broadcast subscription lagged, dropping `n` events.
    EventsLagged(u64),
    /// The connection-status broadcast subscription lagged, dropping `n`
    /// transitions — treated the same as a possibly-missed reconnect.
    StatusLagged(u64),
    /// A `ConnectionStatus::Connected` transition — reconnect recovery.
    Reconnected,
    /// `execute_and_poll_shell`'s stall fallback: no `change` watch progress
    /// for the current backoff window while a command is pending.
    StallFallback,
}

/// Failure modes the doc task reports back through a command's oneshot ack.
#[derive(Debug, Clone)]
pub enum DocTaskError {
    /// The task's mpsc sender or the ack oneshot was dropped — the task
    /// isn't running (already torn down, or never spawned).
    Shutdown,
    /// No `SyncedDocument` present. Shouldn't happen once `register_session`
    /// has completed (the task is only spawned after the doc is seeded);
    /// defensive.
    NoDocument,
    /// A block insert into the CRDT store failed.
    Insert(String),
    /// The resync's server fetch RPC (or the apply of its result) failed.
    Fetch(String),
    /// A resync's PRE-FETCH flush (pushing unpushed local ops before
    /// pulling the server's snapshot) failed, so the resync was aborted
    /// before fetching or applying anything — see `do_coalesced_resync`.
    Flush(String),
}

impl std::fmt::Display for DocTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shutdown => write!(f, "doc task is not running"),
            Self::NoDocument => write!(f, "no synced document"),
            Self::Insert(e) => write!(f, "block insert failed: {e}"),
            Self::Fetch(e) => write!(f, "resync failed: {e}"),
            Self::Flush(e) => write!(f, "resync aborted — pre-fetch flush failed: {e}"),
        }
    }
}

impl std::error::Error for DocTaskError {}

/// A single mutation request to the sole-writer doc task.
pub enum DocCommand {
    /// Apply a server-delivered event (the old background listener's job).
    ApplyEvent(ServerEvent),
    /// Author one or more blocks locally (HookListener's job). Acked once
    /// applied to the document — NOT once pushed to the server; push happens
    /// afterward, best-effort, and its own failure doesn't fail this ack (a
    /// later push/resync will carry the ops).
    AuthorBlocks {
        blocks: Vec<AuthoredBlock>,
        done: oneshot::Sender<Result<(), DocTaskError>>,
    },
    /// Run a resync: flush unpushed local ops, fetch the server's
    /// authoritative snapshot, apply it. `done` is `None` for
    /// fire-and-forget triggers (the event bridge); `Some` when a caller
    /// wants to know it completed (the stall fallback).
    Resync {
        reason: ResyncReason,
        done: Option<oneshot::Sender<Result<(), DocTaskError>>>,
    },
}

// ============================================================================
// Handle (producer-side API)
// ============================================================================

/// Cheap-to-clone handle to a running doc task's command channel.
#[derive(Clone)]
pub struct DocTaskHandle {
    tx: mpsc::Sender<DocCommand>,
}

impl DocTaskHandle {
    /// Author blocks and wait for them to be applied to the document.
    pub async fn author_blocks(&self, blocks: Vec<AuthoredBlock>) -> Result<(), DocTaskError> {
        let (done, ack) = oneshot::channel();
        self.tx
            .send(DocCommand::AuthorBlocks { blocks, done })
            .await
            .map_err(|_| DocTaskError::Shutdown)?;
        ack.await.map_err(|_| DocTaskError::Shutdown)?
    }

    /// Request a resync and wait for it to complete. Multiple concurrent
    /// callers each get their own ack, but only ONE fetch runs — see
    /// [`do_coalesced_resync`].
    pub async fn resync(&self, reason: ResyncReason) -> Result<(), DocTaskError> {
        let (done, ack) = oneshot::channel();
        self.tx
            .send(DocCommand::Resync { reason, done: Some(done) })
            .await
            .map_err(|_| DocTaskError::Shutdown)?;
        ack.await.map_err(|_| DocTaskError::Shutdown)?
    }

    /// Fire-and-forget resync trigger — used by [`spawn_event_bridge`],
    /// which has no caller waiting on an ack.
    async fn resync_fire_and_forget(&self, reason: ResyncReason) {
        let _ = self.tx.send(DocCommand::Resync { reason, done: None }).await;
    }

    /// Fire-and-forget event application — used by [`spawn_event_bridge`].
    async fn apply_event(&self, event: ServerEvent) {
        let _ = self.tx.send(DocCommand::ApplyEvent(event)).await;
    }
}

// ============================================================================
// Task loop
// ============================================================================

/// Spawn the sole-writer doc task. Returns a handle for producers plus the
/// task's own `JoinHandle` for supervision (mirrors the old background
/// listener's supervisor pattern in `lib.rs`).
pub fn spawn_doc_task<B>(
    backend: B,
    context_id: ContextId,
    synced: Arc<parking_lot::Mutex<Option<SyncedDocument>>>,
    change: watch::Sender<u64>,
) -> (DocTaskHandle, JoinHandle<()>)
where
    B: DocSyncBackend + Clone + Send + Sync + 'static,
{
    let (tx, rx) = mpsc::channel(DOC_TASK_CHANNEL_CAPACITY);
    let handle = DocTaskHandle { tx };
    let join = tokio::spawn(run_doc_task(backend, context_id, synced, change, rx));
    (handle, join)
}

/// The task loop itself. Owns: apply → bump `change` → push, for every
/// mutation, uniformly (unlike the old three-writer arrangement, where the
/// stall-fallback resync didn't bump `change`).
async fn run_doc_task<B: DocSyncBackend>(
    backend: B,
    context_id: ContextId,
    synced: Arc<parking_lot::Mutex<Option<SyncedDocument>>>,
    change: watch::Sender<u64>,
    mut rx: mpsc::Receiver<DocCommand>,
) {
    // Bootstrap: everything currently in the doc (seeded by register_session
    // from the initial get_context_sync) came FROM the server, so it's
    // already "pushed" as far as we're concerned.
    let mut pushed_frontier: HashMap<BlockId, Frontier> = {
        let guard = synced.lock();
        guard.as_ref().map(|d| d.doc().frontier()).unwrap_or_default()
    };

    while let Some(cmd) = rx.recv().await {
        match cmd {
            DocCommand::ApplyEvent(event) => {
                let effect = apply_event_sync(&synced, &event);
                bump(&change);
                if matches!(effect, Some(SyncEffect::NeedsResync)) {
                    do_coalesced_resync(
                        &backend,
                        context_id,
                        &synced,
                        &change,
                        &mut rx,
                        &mut pushed_frontier,
                        ResyncReason::NeedsResync,
                        None,
                    )
                    .await;
                }
            }
            DocCommand::AuthorBlocks { blocks, done } => {
                let result = author_blocks_sync(&synced, blocks);
                bump(&change);
                let ok = result.is_ok();
                let _ = done.send(result);
                if ok {
                    // Routine "will retry later" on failure — the block is
                    // already applied and acked; push_new_ops already logs.
                    // (Unlike do_coalesced_resync's flush, there's no doc
                    // swap here that a failed push would need to guard.)
                    let _ = push_new_ops(&backend, &synced, context_id, &mut pushed_frontier).await;
                }
            }
            DocCommand::Resync { reason, done } => {
                do_coalesced_resync(
                    &backend,
                    context_id,
                    &synced,
                    &change,
                    &mut rx,
                    &mut pushed_frontier,
                    reason,
                    done,
                )
                .await;
            }
        }
    }
    tracing::debug!(%context_id, "doc task: command channel closed, exiting");
}

fn bump(change: &watch::Sender<u64>) {
    change.send_modify(|g| *g = g.wrapping_add(1));
}

/// Apply one event to the document under the lock. `None` if there's no
/// document yet (shouldn't happen — defensive).
fn apply_event_sync(
    synced: &Arc<parking_lot::Mutex<Option<SyncedDocument>>>,
    event: &ServerEvent,
) -> Option<SyncEffect> {
    let mut guard = synced.lock();
    guard.as_mut().map(|doc| doc.apply_event(event))
}

/// Apply a batch of [`AuthoredBlock`]s under one lock acquisition. Mirrors
/// exactly what `HookListener::insert_text_block` / `insert_tool_blocks` did
/// imperatively before this module existed.
fn author_blocks_sync(
    synced: &Arc<parking_lot::Mutex<Option<SyncedDocument>>>,
    blocks: Vec<AuthoredBlock>,
) -> Result<(), DocTaskError> {
    let mut guard = synced.lock();
    let Some(doc) = guard.as_mut() else {
        return Err(DocTaskError::NoDocument);
    };
    for block in blocks {
        match block {
            AuthoredBlock::Text { role, content } => {
                doc.doc_mut()
                    .insert_block(
                        None,
                        None,
                        role,
                        BlockKind::Text,
                        content,
                        Status::Done,
                        ContentType::Plain,
                    )
                    .map_err(|e| DocTaskError::Insert(e.to_string()))?;
            }
            AuthoredBlock::ToolCallResult {
                tool_name,
                tool_input,
                result_content,
                is_error,
                tool_kind,
            } => {
                let call_id = doc
                    .doc_mut()
                    .insert_tool_call(None, None, tool_name, tool_input, tool_kind, None)
                    .map_err(|e| DocTaskError::Insert(e.to_string()))?;
                doc.doc_mut()
                    .insert_tool_result_block(&call_id, None, result_content, is_error, None, tool_kind)
                    .map_err(|e| DocTaskError::Insert(e.to_string()))?;
                let final_status = if is_error { Status::Error } else { Status::Done };
                doc.doc_mut()
                    .set_status(&call_id, final_status)
                    .map_err(|e| DocTaskError::Insert(e.to_string()))?;
            }
        }
    }
    Ok(())
}

/// Push whatever's changed since `pushed_frontier` — NOT since the inbound
/// sync frontier (the old `flush_local_ops` bug: local authoring never
/// advances the inbound frontier, so every push re-sent every local op ever
/// made). Advances `pushed_frontier` to the doc's own current frontier on a
/// successful push; leaves it alone on failure (returning `Err`) so the next
/// push naturally retries the same ops (plus anything new) — safe because
/// server-side CRDT merge is idempotent.
///
/// Callers differ on what a failure means: the plain `AuthorBlocks` arm in
/// the main loop treats it as routine "will retry later" (the block is
/// already applied and acked either way — only IGNORES the `Result`, relying
/// on the `warn!` below). `do_coalesced_resync`'s pre-fetch flush is
/// different: proceeding into a doc-replacing `apply_sync_state` while
/// holding ops that failed to push would silently lose them, so THAT caller
/// must abort on `Err` rather than continue.
async fn push_new_ops<B: DocSyncBackend>(
    backend: &B,
    synced: &Arc<parking_lot::Mutex<Option<SyncedDocument>>>,
    context_id: ContextId,
    pushed_frontier: &mut HashMap<BlockId, Frontier>,
) -> Result<(), DocTaskError> {
    let Some((ops, new_frontier)) = ({
        let guard = synced.lock();
        guard.as_ref().map(|doc| {
            let ops = doc.doc().ops_since(pushed_frontier);
            let new_frontier = doc.doc().frontier();
            (ops, new_frontier)
        })
    }) else {
        return Ok(());
    };
    if ops.block_ops.is_empty()
        && ops.new_blocks.is_empty()
        && ops.updated_headers.is_empty()
        && ops.deleted_blocks.is_empty()
    {
        return Ok(());
    }
    let bytes = match kaijutsu_types::codec::encode(&ops) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(%context_id, "doc task: push encode failed: {e}");
            return Err(DocTaskError::Flush(format!("encode failed: {e}")));
        }
    };
    match backend.push_ops(context_id, &bytes).await {
        Ok(_ack_version) => {
            *pushed_frontier = new_frontier;
            Ok(())
        }
        Err(e) => {
            // pushed_frontier stays put, so the NEXT push (triggered by the
            // next authoring or resync) naturally re-includes these ops.
            tracing::warn!(%context_id, "doc task: push failed, will retry: {e}");
            Err(DocTaskError::Flush(e.to_string()))
        }
    }
}

/// Run one resync, coalescing any additional `Resync` commands ALREADY
/// sitting in the channel at the moment we start (a burst of Lagged-event +
/// Lagged-status, or a stall fallback landing right behind a NeedsResync,
/// shouldn't cost two fetches). `AuthorBlocks`/`ApplyEvent` commands found
/// in that same pre-fetch drain are applied immediately to the CURRENT
/// (about-to-be-replaced) doc — harmless *as long as the flush below
/// actually succeeds*, since a successful flush pushes them before we
/// fetch, so the snapshot we pull back already reflects them. If the flush
/// FAILS, the resync aborts entirely (no fetch, no apply, no frontier
/// reset) rather than proceed into a doc swap that would silently lose
/// those drained-but-unpushed ops with no way to retry them — see the
/// flush step below.
///
/// Commands that arrive WHILE the fetch RPC is actually in flight are NOT
/// caught by the drain (the task isn't polling `rx` during the `.await`) —
/// they simply wait in the channel and get processed normally by the next
/// `rx.recv()` after this function returns, against the FRESH post-swap
/// document. This is the fix for the lost-update window: the old
/// direct-call `resync_synced` had no way to make a concurrent
/// `insert_text_block` wait for it.
///
/// Staleness of a *queued* `ApplyEvent` replayed after the swap: verified
/// safe, not just assumed. `SyncReset` (the only event that signals
/// `NeedsResync`) and a broadcast `Lagged` error both preserve the
/// broadcast channel's ordering guarantee — `Lagged` skips forward over
/// dropped entries but never reorders, and `SyncReset` itself flows through
/// the same ordered event stream as everything else. So any `ApplyEvent`
/// the doc task processes after a resync it triggered is causally
/// at-or-after that resync's snapshot, never older. That matters because
/// the header-field setters (`set_status` et al.) stamp a fresh LOCAL tick
/// unconditionally rather than doing LWW against the event's own
/// timestamp — replaying a GENUINELY stale event would silently overwrite
/// newer data with a tick that looks newest. The ordering guarantee is what
/// makes "apply it after" safe here; see `SyncedDocument::apply_sync_state`
/// for the sibling case (`pending_events`) where that guarantee does NOT
/// hold and the buffered events are dropped instead.
#[allow(clippy::too_many_arguments)]
async fn do_coalesced_resync<B: DocSyncBackend>(
    backend: &B,
    context_id: ContextId,
    synced: &Arc<parking_lot::Mutex<Option<SyncedDocument>>>,
    change: &watch::Sender<u64>,
    rx: &mut mpsc::Receiver<DocCommand>,
    pushed_frontier: &mut HashMap<BlockId, Frontier>,
    first_reason: ResyncReason,
    first_done: Option<oneshot::Sender<Result<(), DocTaskError>>>,
) {
    let mut dones = Vec::new();
    if let Some(d) = first_done {
        dones.push(d);
    }
    let mut reasons = vec![first_reason];

    loop {
        match rx.try_recv() {
            Ok(DocCommand::Resync { reason, done }) => {
                reasons.push(reason);
                if let Some(d) = done {
                    dones.push(d);
                }
            }
            Ok(DocCommand::AuthorBlocks { blocks, done }) => {
                let result = author_blocks_sync(synced, blocks);
                bump(change);
                let _ = done.send(result);
            }
            Ok(DocCommand::ApplyEvent(event)) => {
                // Ignore the returned effect — even a NeedsResync signal
                // here is superseded by the resync we're already committed
                // to running.
                apply_event_sync(synced, &event);
                bump(change);
            }
            Err(_) => break, // Empty or Disconnected: nothing more queued.
        }
    }

    tracing::info!(
        %context_id,
        coalesced = reasons.len(),
        ?reasons,
        "doc task: running resync",
    );

    // Flush anything local not yet pushed — the fetch below pulls a
    // snapshot, and apply_sync_state replaces the doc wholesale, so any
    // local op that's neither pushed before this point NOR still queued in
    // the channel (to be replayed after) would be lost. If the flush ITSELF
    // fails, that loss is exactly what proceeding would cause: the drained
    // pre-fetch AuthorBlocks above are sitting in the doc unpushed, and
    // fetching+applying now would wipe them via the swap AND reset
    // `pushed_frontier` to the fresh doc's frontier — erasing any record
    // that a future push needs to retry them. So on flush failure, ABORT
    // here: no fetch, no apply, no frontier touch. The unflushed ops stay
    // exactly where `push_new_ops` left them (frontier untouched), so the
    // NEXT resync's flush picks them up again. Callers recover on their own
    // schedule — the stall fallback re-fires on its next backoff window, a
    // Lagged bridge resync re-triggers on the next lag.
    if let Err(e) = push_new_ops(backend, synced, context_id, pushed_frontier).await {
        tracing::error!(
            %context_id,
            "doc task: {e} — refusing to swap the document while local ops are unflushed",
        );
        for done in dones {
            let _ = done.send(Err(e.clone()));
        }
        return;
    }

    let result = match backend.get_context_sync(context_id).await {
        Ok(state) => {
            let mut guard = synced.lock();
            match guard.as_mut() {
                Some(doc) => match doc.apply_sync_state(&state) {
                    Ok(effect) => {
                        tracing::info!(%context_id, ?effect, "doc task: resync applied");
                        // CRITICAL: the doc instance was just replaced
                        // wholesale. Every frontier tracked against the OLD
                        // instance is meaningless now — but nothing local is
                        // lost: we flushed before fetching, so the fresh
                        // doc already reflects everything we'd pushed.
                        // Reset to the fresh doc's own frontier: "we just
                        // got this from the server, so the server already
                        // has all of it."
                        *pushed_frontier = doc.doc().frontier();
                        Ok(())
                    }
                    Err(e) => Err(DocTaskError::Fetch(e.to_string())),
                },
                None => Err(DocTaskError::NoDocument),
            }
        }
        Err(e) => {
            tracing::warn!(%context_id, "doc task: resync fetch failed: {e}");
            Err(DocTaskError::Fetch(e.to_string()))
        }
    };

    bump(change);

    for done in dones {
        let _ = done.send(result.clone());
    }
}

// ============================================================================
// Event bridge — adapts ActorHandle's broadcast streams into DocCommands
// ============================================================================

/// Bridge the actor's block-events and connection-status broadcast streams
/// into the doc task's command channel. Replaces the old background
/// listener's inline `select!` loop — same two sources, same handling — the
/// only change is that it now converts into commands instead of touching
/// `SyncedDocument` directly.
pub fn spawn_event_bridge(actor: ActorHandle, doc_task: DocTaskHandle) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut event_rx = actor.subscribe_events();
        let mut status_rx = actor.subscribe_status();
        loop {
            tokio::select! {
                ev = event_rx.recv() => match ev {
                    Ok(event) => doc_task.apply_event(event).await,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("event bridge: missed {n} events, forcing resync");
                        doc_task.resync_fire_and_forget(ResyncReason::EventsLagged(n)).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                st = status_rx.recv() => match st {
                    Ok(ConnectionStatus::Connected { .. }) => {
                        tracing::info!("event bridge: reconnected — resyncing");
                        doc_task.resync_fire_and_forget(ResyncReason::Reconnected).await;
                    }
                    // A lagged status stream may have DROPPED a Connected
                    // transition — we can't tell, so resync to be safe.
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            "event bridge: status stream lagged ({n}) — resyncing in case a \
                             reconnect was missed",
                        );
                        doc_task.resync_fire_and_forget(ResyncReason::StatusLagged(n)).await;
                    }
                    Ok(_) => {}
                    // Symmetric with the event_rx arm above: once a
                    // broadcast sender side is gone, `recv()` resolves
                    // `Closed` IMMEDIATELY on every subsequent poll rather
                    // than pending — leaving this unhandled would leave the
                    // `select!` spinning hot on a permanently-ready arm
                    // instead of shutting the bridge down.
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use kaijutsu_client::CallError;
    use kaijutsu_client::rpc::SyncState;
    use kaijutsu_crdt::block_store::{BlockStore as CrdtBlockStore, SyncPayload};
    use kaijutsu_crdt::PrincipalId;
    use tokio::sync::Notify;

    fn snapshot_bytes(store: &CrdtBlockStore) -> Vec<u8> {
        kaijutsu_types::codec::encode(&store.snapshot()).expect("serialize snapshot")
    }

    /// A fake [`DocSyncBackend`]: `get_context_sync` reads a snapshot of a
    /// server-side `CrdtBlockStore`; `push_ops` merges the pushed payload
    /// INTO that same store (so a resync after a push genuinely reflects
    /// it) and records every call for inspection. `get_context_sync` can
    /// optionally be gated behind a `Notify` — the fetch signals
    /// `fetch_entered` then waits on the gate — to deterministically land a
    /// concurrent command mid-fetch.
    #[derive(Clone)]
    struct FakeBackend {
        ctx: ContextId,
        server_doc: Arc<std::sync::Mutex<CrdtBlockStore>>,
        fetch_gate: Option<Arc<Notify>>,
        fetch_entered: Arc<Notify>,
        fetch_calls: Arc<AtomicUsize>,
        push_payloads: Arc<parking_lot::Mutex<Vec<SyncPayload>>>,
        /// Number of upcoming `push_ops` calls that should fail (return
        /// `Err`) before succeeding again — decremented on each call.
        push_fail_countdown: Arc<AtomicUsize>,
    }

    impl FakeBackend {
        fn new(ctx: ContextId) -> Self {
            Self {
                ctx,
                server_doc: Arc::new(std::sync::Mutex::new(CrdtBlockStore::new(
                    ctx,
                    PrincipalId::new(),
                ))),
                fetch_gate: None,
                fetch_entered: Arc::new(Notify::new()),
                fetch_calls: Arc::new(AtomicUsize::new(0)),
                push_payloads: Arc::new(parking_lot::Mutex::new(Vec::new())),
                push_fail_countdown: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn with_gate(mut self, gate: Arc<Notify>) -> Self {
            self.fetch_gate = Some(gate);
            self
        }

        fn fetch_call_count(&self) -> usize {
            self.fetch_calls.load(Ordering::SeqCst)
        }

        fn push_payloads(&self) -> Vec<SyncPayload> {
            self.push_payloads.lock().clone()
        }

        /// Block until `get_context_sync` has been called at least once and
        /// is (if gated) currently blocked awaiting its gate.
        async fn wait_for_fetch_entered(&self) {
            self.fetch_entered.notified().await;
        }

        /// Make the next `n` `push_ops` calls fail (return `Err`, touching
        /// neither `server_doc` nor `push_payloads` — a failed push must
        /// look exactly like it never happened) before succeeding again.
        fn fail_next_pushes(&self, n: usize) {
            self.push_fail_countdown.store(n, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl DocSyncBackend for FakeBackend {
        async fn get_context_sync(&self, context_id: ContextId) -> Result<SyncState, CallError> {
            assert_eq!(context_id, self.ctx, "fake backend fetch for wrong context");
            self.fetch_calls.fetch_add(1, Ordering::SeqCst);
            self.fetch_entered.notify_one();
            if let Some(gate) = &self.fetch_gate {
                gate.notified().await;
            }
            let ops = {
                let store = self.server_doc.lock().unwrap();
                snapshot_bytes(&store)
            };
            Ok(SyncState { context_id, version: 1, ops })
        }

        async fn push_ops(&self, context_id: ContextId, ops: &[u8]) -> Result<u64, CallError> {
            assert_eq!(context_id, self.ctx, "fake backend push for wrong context");
            let remaining = self.push_fail_countdown.load(Ordering::SeqCst);
            if remaining > 0 {
                self.push_fail_countdown.store(remaining - 1, Ordering::SeqCst);
                return Err(CallError::Rpc("simulated push failure".to_string()));
            }
            let payload: SyncPayload =
                kaijutsu_types::codec::decode(ops).expect("decode pushed SyncPayload");
            {
                let mut store = self.server_doc.lock().unwrap();
                store.merge_ops(payload.clone()).expect("merge pushed ops");
            }
            self.push_payloads.lock().push(payload);
            Ok(1)
        }
    }

    fn seeded_synced(ctx: ContextId) -> Arc<parking_lot::Mutex<Option<SyncedDocument>>> {
        Arc::new(parking_lot::Mutex::new(Some(SyncedDocument::new(
            ctx,
            PrincipalId::new(),
        ))))
    }

    fn doc_contains(
        synced: &Arc<parking_lot::Mutex<Option<SyncedDocument>>>,
        needle: &str,
    ) -> bool {
        let guard = synced.lock();
        guard
            .as_ref()
            .unwrap()
            .blocks()
            .iter()
            .any(|b| b.content == needle)
    }

    /// TDD item (a): a block authored WHILE a resync's fetch RPC is in
    /// flight must survive the doc-replacing `apply_sync_state` AND still
    /// reach the push path. Pins the old direct-call `resync_synced`
    /// architecture's flush→apply lost-update window (docs/issues.md,
    /// "kaijutsu-mcp — June 2026 SyncedDocument migration review", HIGH):
    /// under that architecture, HookListener's `insert_text_block` grabbed
    /// the mutex and mutated synchronously at ANY time, including the
    /// window between `resync_synced`'s flush and its `apply_sync_state` —
    /// a block authored there was silently wiped when `*doc = new_store`
    /// replaced the document, and never got pushed (the flush that would
    /// have carried it already ran). This test builds the new
    /// command-channel machinery specifically so an author can't land in
    /// that window anymore: it either lands before the flush (and gets
    /// pushed, so the fetched snapshot already has it) or it queues behind
    /// the in-flight fetch and gets applied — and pushed — afterward.
    #[tokio::test]
    async fn author_during_resync_survives_and_reaches_push() {
        let ctx = ContextId::new();
        let synced = seeded_synced(ctx);
        let (change_tx, _change_rx) = watch::channel(0u64);

        let gate = Arc::new(Notify::new());
        let backend = FakeBackend::new(ctx).with_gate(Arc::clone(&gate));

        let (tx, rx) = mpsc::channel(DOC_TASK_CHANNEL_CAPACITY);
        let handle = DocTaskHandle { tx };
        let task = tokio::spawn(run_doc_task(
            backend.clone(),
            ctx,
            Arc::clone(&synced),
            change_tx,
            rx,
        ));

        // Kick off a resync without awaiting its ack — we need the doc task
        // stuck inside the (gated) fetch before we author.
        let resync_handle = handle.clone();
        let resync_task =
            tokio::spawn(async move { resync_handle.resync(ResyncReason::NeedsResync).await });

        // Deterministically wait until the fetch is actually in flight
        // (past the pre-fetch drain AND the flush) rather than racing a
        // fixed sleep.
        tokio::time::timeout(Duration::from_secs(5), backend.wait_for_fetch_entered())
            .await
            .expect("fetch never entered");

        // Author WHILE the fetch is stuck on the gate. This can't complete
        // until the resync does (the task is single-threaded and busy
        // awaiting the fetch), so it must be its own task too — awaiting it
        // inline here, before releasing the gate, would deadlock the TEST
        // (not the code under test).
        let author_handle = handle.clone();
        let author_task = tokio::spawn(async move {
            author_handle
                .author_blocks(vec![AuthoredBlock::Text {
                    role: Role::User,
                    content: "hello-mid-fetch".to_string(),
                }])
                .await
        });
        // Give the author's send a moment to actually land in the mpsc
        // buffer before we release the gate — otherwise the release could
        // in principle race ahead of the send (unlikely given the gate's
        // own scheduling latency, but let's not lean on luck).
        tokio::task::yield_now().await;

        // Release the fetch and let the resync (and the queued author,
        // which the task processes right after) settle.
        gate.notify_one();
        tokio::time::timeout(Duration::from_secs(5), resync_task)
            .await
            .expect("resync task timed out")
            .unwrap()
            .expect("resync itself failed");

        let author_result = tokio::time::timeout(Duration::from_secs(5), author_task)
            .await
            .expect("author_blocks timed out — did it deadlock behind the gated fetch?")
            .unwrap();
        assert!(author_result.is_ok(), "author_blocks failed: {author_result:?}");

        assert!(
            doc_contains(&synced, "hello-mid-fetch"),
            "authored block lost across the resync's flush→apply window"
        );
        assert!(
            backend
                .push_payloads()
                .iter()
                .any(|p| p.new_blocks.iter().any(|b| b.content == "hello-mid-fetch")),
            "authored block never reached the push path"
        );

        task.abort();
    }

    /// TDD item (b): pushing must send only ops NEW since the last push —
    /// not everything since the (never-advancing-for-local-edits) inbound
    /// sync frontier. Authors N blocks one at a time; each push must carry
    /// exactly the one new block, never a re-send of earlier ones.
    #[tokio::test]
    async fn push_sends_only_new_ops_each_time() {
        let ctx = ContextId::new();
        let synced = seeded_synced(ctx);
        let (change_tx, _change_rx) = watch::channel(0u64);
        let backend = FakeBackend::new(ctx);

        let (handle, task) = spawn_doc_task(backend.clone(), ctx, Arc::clone(&synced), change_tx);

        for i in 0..3 {
            handle
                .author_blocks(vec![AuthoredBlock::Text {
                    role: Role::User,
                    content: format!("msg-{i}"),
                }])
                .await
                .unwrap();
        }

        let pushes = backend.push_payloads();
        assert_eq!(pushes.len(), 3, "expected one push per authored block");
        for (i, payload) in pushes.iter().enumerate() {
            assert_eq!(
                payload.new_blocks.len(),
                1,
                "push {i} carried {} new blocks, expected exactly 1 (no re-send): {:?}",
                payload.new_blocks.len(),
                payload.new_blocks.iter().map(|b| &b.content).collect::<Vec<_>>(),
            );
            assert_eq!(payload.new_blocks[0].content, format!("msg-{i}"));
        }

        task.abort();
    }

    /// TDD item (c): N Resync commands already queued by the time the task
    /// starts processing the first one must coalesce into exactly ONE
    /// fetch, with every caller's ack completed once it's done.
    #[tokio::test]
    async fn queued_resyncs_coalesce_into_one_fetch() {
        let ctx = ContextId::new();
        let synced = seeded_synced(ctx);
        let (change_tx, _change_rx) = watch::channel(0u64);
        let backend = FakeBackend::new(ctx);

        // Build the channel and queue 4 resync requests BEFORE the loop
        // task exists to consume any of them — guarantees they're all
        // sitting in the buffer by the time the very first `rx.recv()`
        // resolves, rather than racing a concurrently-running loop.
        let (tx, rx) = mpsc::channel(DOC_TASK_CHANNEL_CAPACITY);
        let handle = DocTaskHandle { tx };

        let reasons = [
            ResyncReason::NeedsResync,
            ResyncReason::EventsLagged(1),
            ResyncReason::StatusLagged(2),
            ResyncReason::Reconnected,
        ];
        let mut acks = Vec::new();
        for reason in reasons {
            let h = handle.clone();
            acks.push(tokio::spawn(async move { h.resync(reason).await }));
        }
        // Let all 4 sends land in the mpsc buffer before the loop starts.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        let task = tokio::spawn(run_doc_task(
            backend.clone(),
            ctx,
            Arc::clone(&synced),
            change_tx,
            rx,
        ));

        for ack in acks {
            tokio::time::timeout(Duration::from_secs(5), ack)
                .await
                .expect("resync ack timed out")
                .unwrap()
                .expect("resync failed");
        }

        assert_eq!(
            backend.fetch_call_count(),
            1,
            "4 queued resyncs must coalesce into exactly one fetch"
        );

        task.abort();
    }

    /// Reviewer-flagged durability bug (second-voice review before merge):
    /// if `do_coalesced_resync`'s PRE-FETCH `push_new_ops` fails, proceeding
    /// into `get_context_sync` + `apply_sync_state` anyway would wipe the
    /// drained local `AuthorBlocks` via the doc-replacing swap AND reset
    /// `pushed_frontier` to the fresh doc's frontier — erasing any record
    /// that a future push needs to retry them. The fix: abort the resync
    /// entirely on flush failure (no fetch, no apply, no frontier touch).
    ///
    /// This test sends a `Resync` THEN an `AuthorBlocks` command
    /// sequentially into the channel (not as racing concurrent tasks —
    /// their RELATIVE order matters here) before the loop exists to
    /// consume either, so the resync's pre-fetch drain — not the plain
    /// `AuthorBlocks` arm, which has its own harmless "warn and retry
    /// later" push — is what applies the block and owns the one flush
    /// that's configured to fail.
    #[tokio::test]
    async fn resync_aborts_when_the_pre_fetch_flush_fails() {
        let ctx = ContextId::new();
        let synced = seeded_synced(ctx);
        let (change_tx, _change_rx) = watch::channel(0u64);
        let backend = FakeBackend::new(ctx);

        let (tx, rx) = mpsc::channel(DOC_TASK_CHANNEL_CAPACITY);

        let (resync_done, resync_ack) = oneshot::channel();
        tx.send(DocCommand::Resync {
            reason: ResyncReason::NeedsResync,
            done: Some(resync_done),
        })
        .await
        .unwrap();

        let (author_done, author_ack) = oneshot::channel();
        tx.send(DocCommand::AuthorBlocks {
            blocks: vec![AuthoredBlock::Text {
                role: Role::User,
                content: "unpushed".to_string(),
            }],
            done: author_done,
        })
        .await
        .unwrap();

        // The ONE push this resync's flush will attempt must fail.
        backend.fail_next_pushes(1);

        let task = tokio::spawn(run_doc_task(
            backend.clone(),
            ctx,
            Arc::clone(&synced),
            change_tx,
            rx,
        ));

        let author_result = tokio::time::timeout(Duration::from_secs(5), author_ack)
            .await
            .expect("author ack timed out")
            .expect("author oneshot dropped");
        assert!(
            author_result.is_ok(),
            "author, applied during the resync's pre-fetch drain, must still succeed: \
             {author_result:?}"
        );

        let resync_result = tokio::time::timeout(Duration::from_secs(5), resync_ack)
            .await
            .expect("resync ack timed out")
            .expect("resync oneshot dropped");
        assert!(
            matches!(resync_result, Err(DocTaskError::Flush(_))),
            "resync must abort with a Flush error when its pre-fetch push fails, got: \
             {resync_result:?}"
        );

        assert_eq!(
            backend.fetch_call_count(),
            0,
            "an aborted resync must never fetch — proceeding into apply_sync_state while \
             ops are unflushed is exactly the bug being guarded against"
        );
        assert!(
            doc_contains(&synced, "unpushed"),
            "the authored block must survive an aborted resync untouched"
        );

        // A subsequent resync — push now succeeding — must round-trip the
        // block safely: it reaches the server AND survives the fetch+apply.
        let handle = DocTaskHandle { tx };
        tokio::time::timeout(
            Duration::from_secs(5),
            handle.resync(ResyncReason::NeedsResync),
        )
        .await
        .expect("second resync timed out")
        .expect("second resync (push now succeeding) must succeed");

        assert_eq!(
            backend.fetch_call_count(),
            1,
            "the successful resync must fetch exactly once"
        );
        assert!(
            doc_contains(&synced, "unpushed"),
            "the block must still be present after a successful round-trip"
        );
        assert!(
            backend
                .push_payloads()
                .iter()
                .any(|p| p.new_blocks.iter().any(|b| b.content == "unpushed")),
            "the block must have reached the server on the successful push"
        );

        task.abort();
    }
}
