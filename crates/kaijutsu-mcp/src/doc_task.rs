//! Sole-writer command channel for the MCP `SyncedDocument`.
//!
//! Before this module existed, `RemoteState.synced` had THREE writers: the
//! background event listener (applied `ServerEvent`s, ran resyncs),
//! `HookListener` (authored blocks directly via `synced.lock()` +
//! `doc_mut().insert_*`, then pushed), and `execute_and_poll_shell`'s stall
//! fallback (called `resync_synced` directly). All three took the same
//! `parking_lot::Mutex`, so no individual mutation ever corrupted the doc —
//! but the *sequences* raced. This module made one task — [`run_doc_task`] —
//! the TRUE sole writer, with every mutation arriving as a [`DocCommand`] on
//! one mpsc channel.
//!
//! **Two of those four races are now gone by construction rather than by
//! guard.** Since the hook path authors over RPC (`authorBlock` /
//! `completeBlock`) instead of writing here and pushing, this document has no
//! local writer at all: the only mutations left are *server-sourced* —
//! applying a `ServerEvent`, or applying a fetched snapshot. That deleted the
//! lost-update window (nothing local exists to lose across an
//! `apply_sync_state` swap) and the re-send bug (nothing is pushed, so no
//! frontier needs tracking), along with the pre-fetch flush and its
//! abort-on-failure guard. What remains is the resync coalescing, which is
//! about not fetching four times, not about correctness of local data.
//!
//! The mirror is now a pure read replica. Reads go straight through the
//! shared mutex (`RemoteState.synced`); mutation stays on the channel so the
//! apply→bump ordering has exactly one owner. See
//! `docs/crdt-position-2026-08.md` (slice 3).
//!
//! [`DocTaskHandle`] is the producer-side API. [`spawn_event_bridge`] adapts
//! the actor's broadcast event/status streams into the same channel, so the
//! task loop only ever has one thing to select on: `rx.recv()`.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use kaijutsu_client::{ActorHandle, ConnectionStatus, DocSyncBackend, ServerEvent, SyncEffect, SyncedDocument};
use kaijutsu_crdt::ContextId;

/// Channel capacity for the doc task's command mpsc. Generous — a burst of
/// hook events, hydrated resync triggers (Lagged event + Lagged status +
/// stall fallback can all fire close together), and queued author requests
/// should never need to block a producer under normal operation.
const DOC_TASK_CHANNEL_CAPACITY: usize = 256;

// ============================================================================
// Command / data types
// ============================================================================

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
    /// The resync's server fetch RPC (or the apply of its result) failed.
    Fetch(String),
}

impl std::fmt::Display for DocTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shutdown => write!(f, "doc task is not running"),
            Self::NoDocument => write!(f, "no synced document"),
            Self::Fetch(e) => write!(f, "resync failed: {e}"),
        }
    }
}

impl std::error::Error for DocTaskError {}

/// A single mutation request to the sole-writer doc task.
pub enum DocCommand {
    /// Apply a server-delivered event (the old background listener's job).
    ApplyEvent(ServerEvent),
    /// Run a resync: fetch the server's authoritative snapshot and apply it.
    /// `done` is `None` for fire-and-forget triggers (the event bridge);
    /// `Some` when a caller wants to know it completed (the stall fallback).
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

/// The task loop itself. Owns apply → bump `change` for every mutation,
/// uniformly (unlike the old three-writer arrangement, where the
/// stall-fallback resync didn't bump `change`).
async fn run_doc_task<B: DocSyncBackend>(
    backend: B,
    context_id: ContextId,
    synced: Arc<parking_lot::Mutex<Option<SyncedDocument>>>,
    change: watch::Sender<u64>,
    mut rx: mpsc::Receiver<DocCommand>,
) {
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
                        ResyncReason::NeedsResync,
                        None,
                    )
                    .await;
                }
            }
            DocCommand::Resync { reason, done } => {
                do_coalesced_resync(&backend, context_id, &synced, &change, &mut rx, reason, done)
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

/// Run one resync, coalescing any additional `Resync` commands ALREADY
/// sitting in the channel at the moment we start (a burst of Lagged-event +
/// Lagged-status, or a stall fallback landing right behind a NeedsResync,
/// shouldn't cost two fetches). `ApplyEvent` commands found in that same
/// pre-fetch drain are applied immediately to the CURRENT
/// (about-to-be-replaced) doc, which is harmless: they came from the server,
/// so the snapshot we are about to fetch already accounts for them.
///
/// There is no pre-fetch flush here anymore, and that is a structural change
/// rather than a simplification. It existed because the drain could apply
/// *locally authored* blocks that the imminent `apply_sync_state` swap would
/// wipe — so the resync had to push them first, and abort if that push
/// failed. Since authoring moved to `authorBlock` RPCs, this document has no
/// local writer: everything in it came from the server, so the swap cannot
/// destroy anything the server does not already have. The lost-update window
/// is gone by construction, not by guard.
///
/// Commands that arrive WHILE the fetch RPC is actually in flight are NOT
/// caught by the drain (the task isn't polling `rx` during the `.await`) —
/// they simply wait in the channel and get processed normally by the next
/// `rx.recv()` after this function returns, against the FRESH post-swap
/// document.
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
async fn do_coalesced_resync<B: DocSyncBackend>(
    backend: &B,
    context_id: ContextId,
    synced: &Arc<parking_lot::Mutex<Option<SyncedDocument>>>,
    change: &watch::Sender<u64>,
    rx: &mut mpsc::Receiver<DocCommand>,
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

    let result = match backend.get_context_sync(context_id).await {
        Ok(state) => {
            let mut guard = synced.lock();
            match guard.as_mut() {
                Some(doc) => match doc.apply_sync_state(&state) {
                    Ok(effect) => {
                        tracing::info!(%context_id, ?effect, "doc task: resync applied");
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
    use kaijutsu_crdt::block_store::BlockStore as CrdtBlockStore;
    use kaijutsu_crdt::{PrincipalId, Role};
    use tokio::sync::Notify;

    fn snapshot_bytes(store: &CrdtBlockStore) -> Vec<u8> {
        kaijutsu_types::codec::encode(&store.snapshot()).expect("serialize snapshot")
    }

    /// A fake [`DocSyncBackend`]: `get_context_sync` reads a snapshot of a
    /// server-side `CrdtBlockStore`. `push_ops` exists only because the trait
    /// demands it — it records that it was called so a test can assert the
    /// doc task never calls it, and panics rather than pretending to work.
    #[derive(Clone)]
    struct FakeBackend {
        ctx: ContextId,
        server_doc: Arc<std::sync::Mutex<CrdtBlockStore>>,
        fetch_gate: Option<Arc<Notify>>,
        fetch_entered: Arc<Notify>,
        fetch_calls: Arc<AtomicUsize>,
        push_calls: Arc<AtomicUsize>,
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
                push_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn fetch_call_count(&self) -> usize {
            self.fetch_calls.load(Ordering::SeqCst)
        }

        fn push_call_count(&self) -> usize {
            self.push_calls.load(Ordering::SeqCst)
        }

        /// Seed a block into the SERVER's store, so the next fetch brings it
        /// back — the only way content legitimately enters the mirror now.
        fn seed_server_block(&self, content: &str) {
            let mut store = self.server_doc.lock().unwrap();
            store
                .insert_block(
                    None,
                    None,
                    Role::User,
                    kaijutsu_crdt::BlockKind::Text,
                    content,
                    kaijutsu_crdt::Status::Done,
                    kaijutsu_crdt::ContentType::Plain,
                )
                .expect("seed server block");
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

        async fn push_ops(&self, _context_id: ContextId, _ops: &[u8]) -> Result<u64, CallError> {
            self.push_calls.fetch_add(1, Ordering::SeqCst);
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

    /// N Resync commands already queued by the time the task starts
    /// processing the first one must coalesce into exactly ONE fetch, with
    /// every caller's ack completed once it's done.
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

    /// **The read-replica invariant** (slice 3 of the CRDT-position
    /// migration). Authoring left this process for `authorBlock` RPCs, so
    /// the mirror has no local writer: everything in it arrives from the
    /// server, and it must never push anything back.
    ///
    /// This is the test that fails if someone reintroduces local authoring
    /// here. That matters more than it looks — the *deletions* this slice
    /// made safe (the pre-fetch flush, the abort-on-flush-failure guard,
    /// `pushed_frontier`) are all justified by "nothing local exists to
    /// lose." A new local write path would silently invalidate that
    /// reasoning and the data it protects, and without this assertion the
    /// suite would stay green while doing it: the block would be applied,
    /// never pushed, and wiped by the next resync's swap.
    ///
    /// It drives the task through the paths that would have pushed under
    /// the old design — a resync, and a server event applied before another
    /// resync — and asserts `push_ops` was never called, while confirming
    /// the mirror still converges on the server's content.
    #[tokio::test]
    async fn the_mirror_is_a_read_replica_and_never_pushes() {
        let ctx = ContextId::new();
        let synced = seeded_synced(ctx);
        let (change_tx, _change_rx) = watch::channel(0u64);
        let backend = FakeBackend::new(ctx);
        backend.seed_server_block("from-the-server");

        let (handle, task) = spawn_doc_task(backend.clone(), ctx, Arc::clone(&synced), change_tx);

        handle
            .resync(ResyncReason::Reconnected)
            .await
            .expect("resync failed");

        assert!(
            doc_contains(&synced, "from-the-server"),
            "the mirror must converge on the server's content"
        );

        // A second round trip, this time with new server-side content, to
        // cover the apply-then-resync path as well as the bare resync.
        backend.seed_server_block("also-from-the-server");
        handle
            .resync(ResyncReason::StallFallback)
            .await
            .expect("second resync failed");

        assert!(
            doc_contains(&synced, "also-from-the-server"),
            "the mirror must keep converging across resyncs"
        );
        assert_eq!(
            backend.push_call_count(),
            0,
            "the mirror must never push — it has no local writer, and the flush this \
             slice deleted was the only thing protecting local ops from the resync swap"
        );

        task.abort();
    }
}
