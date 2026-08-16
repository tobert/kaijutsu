//! Server-push event types and Cap'n Proto callback forwarders.
//!
//! Provides [`ServerEvent`] — a typed enum of all events the server pushes
//! to the client — and [`ConnectionStatus`] for connection lifecycle tracking.
//!
//! The callback forwarder structs (`BlockEventsForwarder`, `ResourceEventsForwarder`)
//! implement Cap'n Proto's generated Server traits and bridge incoming RPC callbacks
//! into tokio broadcast channels that [`ActorHandle`](crate::ActorHandle) consumers
//! can subscribe to.

use std::rc::Rc;

use capnp::capability::Promise;
use kaijutsu_types::{ContextId, KernelId};
use kaijutsu_types::{BlockId, BlockSnapshot};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::kaijutsu_capnp::{
    block_events, editor_events, kernel_output, permission_events, resource_events, turn_events,
    vfs_activity_events,
};
use crate::rpc::{
    EditorState, VfsActivityEntry, parse_block_id, parse_block_snapshot, parse_editor_state,
    parse_vfs_activity_entry,
};

// ============================================================================
// Event Types
// ============================================================================

/// Why a push subscription ended. Mirrors capnp `SubscriptionEndReason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriptionEndReason {
    /// The client's bounded queue in the kernel filled — it could not keep up.
    SlowSubscriber,
    /// The server is going away.
    ServerShutdown,
    /// Replaced by a newer subscription from the same client instance.
    Superseded,
    /// The server ended the stream because it could not produce a correct one
    /// — not the client's fault, and recovered the same way: refetch.
    InternalFault,
    /// An enumerant this build does not know. Honest rather than guessed.
    Unknown,
}

/// Events pushed from server to app via broadcast.
///
/// These are the typed, deserialized forms of Cap'n Proto callback invocations.
/// Subscribe via [`ActorHandle::subscribe_events()`](crate::ActorHandle::subscribe_events).
#[derive(Clone, Debug)]
pub enum ServerEvent {
    /// A new block was inserted into a document.
    BlockInserted {
        context_id: ContextId,
        block: Box<BlockSnapshot>,
        ops: Vec<u8>,
    },
    /// A block's execution status changed (Pending → Running → Done/Error).
    BlockStatusChanged {
        context_id: ContextId,
        block_id: BlockId,
        status: kaijutsu_types::Status,
    },
    /// A block's structured output data changed. Output is not DTE-tracked, so
    /// it rides its own event rather than the block text op stream.
    BlockOutputChanged {
        context_id: ContextId,
        block_id: BlockId,
        output: Option<kaijutsu_types::OutputData>,
    },
    /// A block's scalar metadata changed (exit_code, stderr, content_type, …).
    /// Frontier-independent — applied directly to the store, so it survives a
    /// reconnect even while text ops await a full resync.
    BlockMetadataChanged {
        context_id: ContextId,
        block_id: BlockId,
        metadata: kaijutsu_types::BlockMetadata,
    },
    /// A block was deleted from a document.
    BlockDeleted {
        context_id: ContextId,
        block_id: BlockId,
    },
    /// A block's collapsed state changed.
    BlockCollapsedChanged {
        context_id: ContextId,
        block_id: BlockId,
        collapsed: bool,
    },
    /// A block's excluded state changed.
    BlockExcludedChanged {
        context_id: ContextId,
        block_id: BlockId,
        excluded: bool,
    },
    /// A block was moved to a new position in the document.
    BlockMoved {
        context_id: ContextId,
        block_id: BlockId,
        after_id: Option<BlockId>,
    },
    /// The kernel TERMINATED this client's block subscription because the
    /// client could not drain it fast enough (the 2026-08-05 backpressure
    /// rework). The view is incomplete as of `delivered`; the connection drops
    /// immediately after, and the actor reconnects with a full resync.
    ///
    /// This is not a resumable gap — do not try to patch forward. It exists so
    /// a UI can say "you were disconnected for falling behind" instead of
    /// silently showing a stale turn forever.
    SubscriptionTerminated {
        /// Why the subscription ended.
        reason: SubscriptionEndReason,
        /// The kernel topic whose event could not be enqueued.
        topic: String,
        /// How many events this subscription delivered before the cut.
        delivered: u64,
        /// The queue depth that was exceeded.
        capacity: u64,
    },
    /// An MCP resource's content was updated.
    ResourceUpdated { server: String, uri: String },
    /// An MCP server's resource list changed.
    ResourceListChanged { server: String },
    /// Server-side context switch (fork, context switch command).
    ContextSwitched { context_id: ContextId },
    /// An in-app editor session's state changed (keystroke, save, or — later —
    /// a remote CRDT merge). Carries the full renderer-facing snapshot.
    EditorStateChanged { state: EditorState },
    /// An in-app editor session closed (`ZQ`/quit). Renderers drop it.
    EditorClosed { session_id: u64 },
    /// The reconnect FSM re-established a dropped connection — a `Connected` that
    /// follows a drop, not the first connect. The re-subscribed block stream
    /// missed everything the kernel published during the outage, so renderers
    /// should re-sync their active view. Emitted by the actor itself, which owns
    /// the FSM and so is the natural place to know a reconnect happened (rather
    /// than re-deriving it downstream from the `ConnectionStatus` stream).
    ///
    /// This is the *coarse* signal: it marks every cached doc stale so a
    /// non-joined context re-syncs when next viewed. The finer-grained catch-up
    /// for the context the client was actively on rides `subscribe_context`'s
    /// change feed instead (`FeedEvent::Resubscribed` — docs/change-feed.md):
    /// the eager CRDT-oplog resync push this doc comment used to describe
    /// (`ServerEvent::ContextResynced`) was deleted in the 2026-08-15 wire flag
    /// day, superseded by that feed-native path.
    Reconnected,
    /// Render a cue (`kj play`, later the track render seam; docs/pcm.md,
    /// docs/midi.md "Render is a wire cue"). A kernel directive, not a
    /// block-log event — it names no block. A render sink (the Bevy app today)
    /// dispatches on `cue.mime` and schedules at `receipt + cue.lead`; this
    /// crate only decodes the wire shape, it never touches audio hardware.
    RenderCue {
        context_id: ContextId,
        cue: kaijutsu_audio::RenderCue,
    },
    /// A low-rate beat reference for a sink's continuous timebase (the metronome
    /// phasor; docs/midi.md "The relative-lead timebase, analyzed"). A kernel
    /// directive, not a block change — emitted while a track's clock rolls.
    /// `context_id` is the track's score context (the same key `RenderCue` uses).
    /// This crate only decodes the wire shape; the phasor lives at the sink.
    BeatSync {
        context_id: ContextId,
        beat_ref: kaijutsu_audio::BeatRef,
    },
    /// A VFS activity digest tick (Lane K, FSN slice-1, `docs/scenes/vfs.md`).
    /// `entries` are the directories whose activity total has changed since
    /// the server-side cursor's last delivered digest — ABSOLUTE totals, not
    /// deltas, so a missed tick self-heals on the next one. Heat is
    /// decorative (world-rendering signal), never a control input.
    VfsActivity {
        entries: Vec<VfsActivityEntry>,
        global_total: u64,
    },
    /// A turn reached an ending the server's turn driver arrived at on purpose:
    /// the model finished, a player cancelled, or a ceiling was hit.
    /// [`stop_reason`](TurnCompletedStopReason) says which.
    ///
    /// This is the push that replaces inferring completion from block status —
    /// polling can tell you a block stopped changing, but never *why*, and never
    /// the difference between a finished turn and a cancelled one.
    TurnCompleted {
        /// The context whose turn ended.
        context_id: ContextId,
        /// The principal the turn ran as.
        principal_id: kaijutsu_types::PrincipalId,
        /// The model's last text block, or `None` when the turn produced no
        /// text (a tool-only turn, or an interrupt before the first token).
        output_block_id: Option<BlockId>,
        /// Why the turn stopped.
        stop_reason: TurnCompletedStopReason,
        /// Who asked for the turn.
        origin: TurnOrigin,
    },
    /// A turn **broke** — hydration failure, provider or stream error. Carries
    /// the server's description. A *cancelled* turn is not a failure: it arrives
    /// as [`ServerEvent::TurnCompleted`] with a cancelled stop reason.
    TurnFailed {
        /// The context whose turn broke.
        context_id: ContextId,
        /// The principal the turn ran as.
        principal_id: kaijutsu_types::PrincipalId,
        /// Human-readable failure description from the server.
        error: String,
        /// Who asked for the turn.
        origin: TurnOrigin,
    },
}

/// Why a turn stopped — client-owned mirror of the wire `TurnStopReason` enum
/// (kaijutsu-client deliberately doesn't depend on kaijutsu-kernel, the same
/// reason [`crate::rpc::FileType`] is mirrored rather than re-exported).
///
/// Maps 1:1 onto ACP v1's `stopReason`, which is what this event exists to feed:
/// `EndTurn` → `end_turn`, both cancels → `cancelled`, `MaxTokens` →
/// `max_tokens`, `MaxIterations` → `max_turn_requests`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnCompletedStopReason {
    /// The model finished its phrase and asked for nothing more.
    EndTurn,
    /// A player interrupted with `interrupt_context(immediate = false)`. The
    /// in-flight model call was allowed to finish, so the output block is a
    /// complete phrase; only the agentic loop was stopped.
    CancelledSoft,
    /// A player interrupted with `interrupt_context(immediate = true)`. The
    /// stream was aborted mid-token, so the output block is a fragment.
    CancelledImmediate,
    /// The provider hit its output-token ceiling — the response is truncated.
    MaxTokens,
    /// The agentic loop hit its per-turn tool-iteration cap.
    MaxIterations,
}

impl TurnCompletedStopReason {
    /// Whether a player cancelled this turn (either interrupt flavour).
    pub fn is_cancelled(self) -> bool {
        matches!(self, Self::CancelledSoft | Self::CancelledImmediate)
    }
}

/// Who asked for a turn — client-owned mirror of the wire `TurnOrigin` enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOrigin {
    /// A player submitted a prompt (`prompt` / `submit_input`).
    Interactive,
    /// The kernel drove the turn itself (`kj fork --prompt`, drift, cruise).
    Autonomous,
}

/// Connection lifecycle status broadcast by the reconnect FSM.
///
/// Subscribe via [`ActorHandle::subscribe_status()`](crate::ActorHandle::subscribe_status).
///
/// Variants mirror the actor's internal state machine: `Idle` is the initial
/// state, `Connecting` is the handshake in flight, `Connected` means the
/// pipe is live and subscriptions are registered, `Closing` is graceful
/// teardown of a live connection, `Cooldown` is the backoff window between
/// failed attempts, and `Terminal` is a sticky permanent failure.
#[derive(Clone, Debug)]
pub enum ConnectionStatus {
    /// Initial state. No command has triggered the first connect yet.
    Idle,
    /// Handshake in progress.
    Connecting { attempt: u32 },
    /// Connection live; subscriptions registered; liveness ping running.
    Connected {
        kernel_id: KernelId,
        context_id: Option<ContextId>,
        /// Milliseconds since this Connected transition began (best-effort).
        since_ms: u64,
    },
    /// Connection being torn down; reconnect or terminal follows.
    Closing {
        /// Human-readable description of why we're closing.
        cause: String,
    },
    /// Waiting before the next connect attempt.
    Cooldown {
        /// Attempt number that will be used when the timer expires.
        next_attempt: u32,
        /// Unix-epoch milliseconds at which the next attempt fires.
        until_ms: u64,
        /// Human-readable description of the last failure.
        last_error: String,
    },
    /// Permanent failure. Absorbing state — no more attempts.
    Terminal { reason: String },
}

// ============================================================================
// Block Events Forwarder
// ============================================================================

/// Implements the Cap'n Proto `BlockEvents::Server` trait, forwarding each
/// callback into a `broadcast::Sender<ServerEvent>`.
pub(crate) struct BlockEventsForwarder {
    pub event_tx: broadcast::Sender<ServerEvent>,
    /// Last `subSeq` seen on the ordered (content) lane, and on the musical-time
    /// lane. The kernel's bus is lossless per subscriber, so a gap on the
    /// ordered lane can only mean "this subscription was terminated or
    /// restarted" — never silent loss. If we ever see one WITHOUT a
    /// termination, that is a kernel bug and we say so rather than papering
    /// over it with a resync (docs: the 2026-08-05 backpressure rework).
    ///
    /// A gap on the timing lane is legitimate and expected: a missed beat is
    /// missed on purpose (docs/midi.md "The one timebase").
    pub last_ordered_seq: std::sync::atomic::AtomicU64,
    pub last_timing_seq: std::sync::atomic::AtomicU64,
    /// Where an `exchange` call is run (`docs/midi-next.md` "SysEx: the
    /// exchange pattern"). Late-bindable and shared: a MIDI-capable client
    /// installs its hardware worker whenever that worker is ready, with no
    /// re-subscribe. Empty on every client that holds no MIDI hardware, which
    /// is most of them — and an empty slot REFUSES loudly rather than
    /// pretending the device was silent.
    pub midi_exchange: std::sync::Arc<crate::midi_exchange::MidiExchangeSlot>,
}

/// Build a `BlockEvents` callback client plus the receiver its pushes land
/// on, with an explicit MIDI exchange seat.
///
/// The app builds its forwarder inside the actor (which owns the seat and
/// re-seats it across reconnects); this is the same wiring for callers that
/// drive the subscription themselves — the headless e2e tests, and any future
/// non-Bevy sink. Pass [`MidiExchangeSlot::new()`](crate::MidiExchangeSlot) and
/// leave it empty for a client that holds no MIDI hardware: an empty seat
/// refuses an exchange loudly rather than looking like a silent device.
pub fn block_events_channel(
    capacity: usize,
    midi_exchange: std::sync::Arc<crate::midi_exchange::MidiExchangeSlot>,
) -> (
    crate::kaijutsu_capnp::block_events::Client,
    broadcast::Receiver<ServerEvent>,
) {
    let (tx, rx) = broadcast::channel(capacity);
    let client: crate::kaijutsu_capnp::block_events::Client = capnp_rpc::new_client(
        BlockEventsForwarder {
            event_tx: tx,
            midi_exchange,
            last_ordered_seq: std::sync::atomic::AtomicU64::new(0),
            last_timing_seq: std::sync::atomic::AtomicU64::new(0),
        },
    );
    (client, rx)
}

/// Extract a String from a capnp text reader, mapping errors to capnp::Error.
fn read_text(reader: capnp::text::Reader<'_>) -> Result<String, capnp::Error> {
    reader
        .to_str()
        .map(|s| s.to_owned())
        .map_err(|e| capnp::Error::failed(e.to_string()))
}

/// Parse contextId binary data (16-byte UUID) into ContextId at the wire boundary.
fn parse_context_id_data(data: &[u8]) -> Result<ContextId, capnp::Error> {
    ContextId::try_from_slice(data).ok_or_else(|| {
        capnp::Error::failed(format!(
            "invalid context ID: expected 16 bytes, got {}",
            data.len()
        ))
    })
}

/// Convert an RpcError from our parsing helpers into a capnp::Error.
fn rpc_to_capnp(e: crate::rpc::RpcError) -> capnp::Error {
    capnp::Error::failed(e.to_string())
}

/// Parse a Status enum from capnp.
fn parse_status(status: crate::kaijutsu_capnp::Status) -> kaijutsu_types::Status {
    match status {
        crate::kaijutsu_capnp::Status::Pending => kaijutsu_types::Status::Pending,
        crate::kaijutsu_capnp::Status::Running => kaijutsu_types::Status::Running,
        crate::kaijutsu_capnp::Status::Done => kaijutsu_types::Status::Done,
        crate::kaijutsu_capnp::Status::Error => kaijutsu_types::Status::Error,
        crate::kaijutsu_capnp::Status::Draft => kaijutsu_types::Status::Draft,
    }
}

// ============================================================================
// Editor Events Forwarder
// ============================================================================

/// Implements the Cap'n Proto `EditorEvents::Server` trait, forwarding each
/// editor-session push into the shared `broadcast::Sender<ServerEvent>`.
pub(crate) struct EditorEventsForwarder {
    pub event_tx: broadcast::Sender<ServerEvent>,
}

/// Build an `EditorEvents` callback client plus the receiver its pushes land on.
///
/// Pass the returned client to [`KernelHandle::subscribe_editor`](crate::rpc::KernelHandle::subscribe_editor);
/// drain the receiver for [`ServerEvent::EditorStateChanged`] /
/// [`ServerEvent::EditorClosed`]. The app's editor renderer and the headless
/// e2e tests share this one wiring — the single place the push callback is built.
pub fn editor_events_channel(
    capacity: usize,
) -> (
    crate::kaijutsu_capnp::editor_events::Client,
    broadcast::Receiver<ServerEvent>,
) {
    let (tx, rx) = broadcast::channel(capacity);
    let client: crate::kaijutsu_capnp::editor_events::Client =
        capnp_rpc::new_client(EditorEventsForwarder { event_tx: tx });
    (client, rx)
}

#[allow(refining_impl_trait)]
impl editor_events::Server for EditorEventsForwarder {
    fn on_editor_state(
        self: Rc<Self>,
        params: editor_events::OnEditorStateParams,
        _results: editor_events::OnEditorStateResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        let state: EditorState = match params.get_state() {
            Ok(r) => match parse_editor_state(r) {
                Ok(s) => s,
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            },
            Err(e) => return Promise::err(e),
        };
        if self
            .event_tx
            .send(ServerEvent::EditorStateChanged { state })
            .is_err()
        {
            tracing::warn!("Event channel closed, dropping EditorStateChanged event");
        }
        Promise::ok(())
    }

    fn on_editor_closed(
        self: Rc<Self>,
        params: editor_events::OnEditorClosedParams,
        _results: editor_events::OnEditorClosedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        let session_id = params.get_session_id();
        if self
            .event_tx
            .send(ServerEvent::EditorClosed { session_id })
            .is_err()
        {
            tracing::warn!("Event channel closed, dropping EditorClosed event");
        }
        Promise::ok(())
    }
}

// ============================================================================
// VFS Activity Events Forwarder (Lane K, FSN slice-1, docs/scenes/vfs.md)
// ============================================================================

/// Implements the Cap'n Proto `VfsActivityEvents::Server` trait, forwarding
/// each digest tick into the shared `broadcast::Sender<ServerEvent>`.
pub(crate) struct VfsActivityEventsForwarder {
    pub event_tx: broadcast::Sender<ServerEvent>,
}

/// Build a `VfsActivityEvents` callback client plus the receiver its pushes
/// land on — the dedicated-channel counterpart to [`editor_events_channel`]
/// for callers (headless e2e tests, direct API consumers) that want their
/// own stream rather than riding the actor's shared `event_tx`. Pass the
/// returned client to
/// [`KernelHandle::subscribe_vfs_activity`](crate::rpc::KernelHandle::subscribe_vfs_activity);
/// drain the receiver for [`ServerEvent::VfsActivity`].
pub fn vfs_activity_events_channel(
    capacity: usize,
) -> (
    crate::kaijutsu_capnp::vfs_activity_events::Client,
    broadcast::Receiver<ServerEvent>,
) {
    let (tx, rx) = broadcast::channel(capacity);
    let client: crate::kaijutsu_capnp::vfs_activity_events::Client =
        capnp_rpc::new_client(VfsActivityEventsForwarder { event_tx: tx });
    (client, rx)
}

#[allow(refining_impl_trait)]
impl vfs_activity_events::Server for VfsActivityEventsForwarder {
    fn on_activity_digest(
        self: Rc<Self>,
        params: vfs_activity_events::OnActivityDigestParams,
        _results: vfs_activity_events::OnActivityDigestResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        let mut entries = Vec::new();
        let list = match params.get_entries() {
            Ok(l) => l,
            Err(e) => return Promise::err(e),
        };
        for entry in list.iter() {
            match parse_vfs_activity_entry(entry) {
                Ok(e) => entries.push(e),
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            }
        }
        let global_total = params.get_global_total();
        if self
            .event_tx
            .send(ServerEvent::VfsActivity { entries, global_total })
            .is_err()
        {
            tracing::warn!("Event channel closed, dropping VfsActivity event");
        }
        Promise::ok(())
    }
}

// ============================================================================
// Turn Events Forwarder
// ============================================================================

/// Implements the Cap'n Proto `TurnEvents::Server` trait, forwarding each
/// turn-outcome push into the shared `broadcast::Sender<ServerEvent>`.
pub(crate) struct TurnEventsForwarder {
    pub event_tx: broadcast::Sender<ServerEvent>,
}

/// Build a `TurnEvents` callback client plus the receiver its pushes land on.
///
/// Pass the returned client to [`KernelHandle::subscribe_turn_events`](crate::rpc::KernelHandle::subscribe_turn_events);
/// drain the receiver for [`ServerEvent::TurnCompleted`] /
/// [`ServerEvent::TurnFailed`]. Shaped exactly like [`editor_events_channel`] —
/// one place builds the push callback, and the headless e2e tests, the app, and
/// a future ACP adapter all share that wiring.
///
/// The subscription is kernel-wide, not per-context: each event names its own
/// `context_id`, so a client watching several contexts needs one channel.
pub fn turn_events_channel(
    capacity: usize,
) -> (
    crate::kaijutsu_capnp::turn_events::Client,
    broadcast::Receiver<ServerEvent>,
) {
    let (tx, rx) = broadcast::channel(capacity);
    let client: crate::kaijutsu_capnp::turn_events::Client =
        capnp_rpc::new_client(TurnEventsForwarder { event_tx: tx });
    (client, rx)
}

/// Decode the wire stop reason. A `NotInSchema` (an enumerant a newer server
/// added) is a hard error rather than a silent coercion to `EndTurn`: reporting
/// an unknown ending as a clean end-of-turn is precisely the lie this event
/// exists to prevent.
fn parse_stop_reason(
    wire: crate::kaijutsu_capnp::TurnStopReason,
) -> TurnCompletedStopReason {
    use crate::kaijutsu_capnp::TurnStopReason as W;
    match wire {
        W::EndTurn => TurnCompletedStopReason::EndTurn,
        W::CancelledSoft => TurnCompletedStopReason::CancelledSoft,
        W::CancelledImmediate => TurnCompletedStopReason::CancelledImmediate,
        W::MaxTokens => TurnCompletedStopReason::MaxTokens,
        W::MaxIterations => TurnCompletedStopReason::MaxIterations,
    }
}

/// Decode the wire turn origin.
fn parse_turn_origin(wire: crate::kaijutsu_capnp::TurnOrigin) -> TurnOrigin {
    match wire {
        crate::kaijutsu_capnp::TurnOrigin::Interactive => TurnOrigin::Interactive,
        crate::kaijutsu_capnp::TurnOrigin::Autonomous => TurnOrigin::Autonomous,
    }
}

#[allow(refining_impl_trait)]
impl turn_events::Server for TurnEventsForwarder {
    fn on_turn_completed(
        self: Rc<Self>,
        params: turn_events::OnTurnCompletedParams,
        _results: turn_events::OnTurnCompletedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        let context_id = match params.get_context_id() {
            Ok(bytes) => match ContextId::try_from_slice(bytes) {
                Some(id) => id,
                None => {
                    return Promise::err(capnp::Error::failed(
                        "invalid context_id on TurnCompleted".into(),
                    ));
                }
            },
            Err(e) => return Promise::err(e),
        };
        let principal_id = match params.get_principal_id() {
            Ok(bytes) => match kaijutsu_types::PrincipalId::try_from_slice(bytes) {
                Some(id) => id,
                None => {
                    return Promise::err(capnp::Error::failed(
                        "invalid principal_id on TurnCompleted".into(),
                    ));
                }
            },
            Err(e) => return Promise::err(e),
        };
        // A turn that produced no text sends `hasOutputBlock = false`; the id
        // slot is then meaningless and must NOT be decoded into a plausible
        // -looking block id.
        let output_block_id = if params.get_has_output_block() {
            match params.get_output_block_id() {
                Ok(r) => match parse_block_id(&r) {
                    Ok(id) => Some(id),
                    Err(e) => return Promise::err(rpc_to_capnp(e)),
                },
                Err(e) => return Promise::err(e),
            }
        } else {
            None
        };
        let stop_reason = match params.get_stop_reason() {
            Ok(w) => parse_stop_reason(w),
            Err(e) => {
                return Promise::err(capnp::Error::failed(format!(
                    "unknown TurnStopReason on the wire (newer server?): {e}"
                )));
            }
        };
        let origin = match params.get_origin() {
            Ok(w) => parse_turn_origin(w),
            Err(e) => {
                return Promise::err(capnp::Error::failed(format!(
                    "unknown TurnOrigin on the wire (newer server?): {e}"
                )));
            }
        };
        if self
            .event_tx
            .send(ServerEvent::TurnCompleted {
                context_id,
                principal_id,
                output_block_id,
                stop_reason,
                origin,
            })
            .is_err()
        {
            tracing::warn!("Event channel closed, dropping TurnCompleted event");
        }
        Promise::ok(())
    }

    fn on_turn_failed(
        self: Rc<Self>,
        params: turn_events::OnTurnFailedParams,
        _results: turn_events::OnTurnFailedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        let context_id = match params.get_context_id() {
            Ok(bytes) => match ContextId::try_from_slice(bytes) {
                Some(id) => id,
                None => {
                    return Promise::err(capnp::Error::failed(
                        "invalid context_id on TurnFailed".into(),
                    ));
                }
            },
            Err(e) => return Promise::err(e),
        };
        let principal_id = match params.get_principal_id() {
            Ok(bytes) => match kaijutsu_types::PrincipalId::try_from_slice(bytes) {
                Some(id) => id,
                None => {
                    return Promise::err(capnp::Error::failed(
                        "invalid principal_id on TurnFailed".into(),
                    ));
                }
            },
            Err(e) => return Promise::err(e),
        };
        let error = match params.get_error() {
            Ok(t) => match t.to_str() {
                Ok(s) => s.to_string(),
                Err(e) => return Promise::err(capnp::Error::failed(e.to_string())),
            },
            Err(e) => return Promise::err(e),
        };
        let origin = match params.get_origin() {
            Ok(w) => parse_turn_origin(w),
            Err(e) => {
                return Promise::err(capnp::Error::failed(format!(
                    "unknown TurnOrigin on the wire (newer server?): {e}"
                )));
            }
        };
        if self
            .event_tx
            .send(ServerEvent::TurnFailed {
                context_id,
                principal_id,
                error,
                origin,
            })
            .is_err()
        {
            tracing::warn!("Event channel closed, dropping TurnFailed event");
        }
        Promise::ok(())
    }
}

// ============================================================================
// Permission asks (D-57, docs/acp.md gap #2)
// ============================================================================

/// One `options` entry offered on a `PermissionAskEnvelope` — decoded, not
/// the raw capnp reader. Free-text `kind` (not a closed enum): ACP's
/// `request_permission` offers option *kinds* like allow-once/allow-always/
/// reject-once/reject-always, but this wire shape isn't boxed into ACP's
/// specific vocabulary.
#[derive(Clone, Debug)]
pub struct PermissionOptionInfo {
    pub id: String,
    pub label: String,
    pub kind: String,
}

/// An incoming permission ask (`HookAction::Ask` fired kernel-side),
/// decoded off the wire and paired with a one-shot channel to send the
/// answer back on. This is what an ACP adapter (or the app) drains after
/// calling [`KernelHandle::subscribe_permission_events`](crate::rpc::KernelHandle::subscribe_permission_events)
/// with the client half of [`permission_events_channel`].
///
/// Dropping `answer` without sending resolves the ask exactly like an
/// explicit decline from the server's perspective (the awaiting capnp
/// promise on `answer_rx.await` errors, which `on_ask` reports as a
/// malformed/no answer) — kaijutsu-kernel's D-57 no-subscriber/timeout
/// policy denies either way, so there is no separate "silently ignore"
/// outcome to model on this side.
#[derive(Debug)]
pub struct PermissionAskEnvelope {
    pub request_id: String,
    pub context_id: ContextId,
    pub description: String,
    pub instance: String,
    pub tool: String,
    pub hook_id: String,
    pub options: Vec<PermissionOptionInfo>,
    pub answer: oneshot::Sender<PermissionAskAnswer>,
}

/// The consumer's verdict on a [`PermissionAskEnvelope`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionAskAnswer {
    pub allow: bool,
    pub selected_option_id: Option<String>,
    pub remember: Option<String>,
}

/// Implements the Cap'n Proto `PermissionEvents::Server` trait, decoding
/// each `onAsk` call into a [`PermissionAskEnvelope`], forwarding it on
/// `tx`, and awaiting the paired oneshot reply before answering the RPC —
/// this is the blocking call/response half `ElicitationEvents::onRequest`
/// only templated (D-57's "genuinely new" gap): the promise this method
/// returns does not resolve until something drains the channel and answers.
struct PermissionAskForwarder {
    tx: mpsc::Sender<PermissionAskEnvelope>,
}

/// Build a `PermissionEvents` callback client plus the receiver its asks
/// land on.
///
/// Pass the returned client to
/// [`KernelHandle::subscribe_permission_events`](crate::rpc::KernelHandle::subscribe_permission_events);
/// drain the receiver, decide allow/deny, and send a [`PermissionAskAnswer`]
/// back through each envelope's `answer` channel. Kernel-wide, following
/// [`turn_events_channel`]: `HookAction::Ask` can fire from any call path,
/// so one subscription serves every context.
pub fn permission_events_channel(
    capacity: usize,
) -> (
    crate::kaijutsu_capnp::permission_events::Client,
    mpsc::Receiver<PermissionAskEnvelope>,
) {
    let (tx, rx) = mpsc::channel(capacity);
    let client: crate::kaijutsu_capnp::permission_events::Client =
        capnp_rpc::new_client(PermissionAskForwarder { tx });
    (client, rx)
}

#[allow(refining_impl_trait)]
impl permission_events::Server for PermissionAskForwarder {
    fn on_ask(
        self: Rc<Self>,
        params: permission_events::OnAskParams,
        mut results: permission_events::OnAskResults,
    ) -> Promise<(), capnp::Error> {
        let tx = self.tx.clone();
        Promise::from_future(async move {
            let request = params.get()?.get_request()?;

            let request_id = request.get_request_id()?.to_str()?.to_string();
            let context_id = parse_context_id_data(request.get_context_id()?)?;
            let description = request.get_description()?.to_str()?.to_string();
            let instance = request.get_instance()?.to_str()?.to_string();
            let tool = request.get_tool()?.to_str()?.to_string();
            let hook_id = request.get_hook_id()?.to_str()?.to_string();
            let options_reader = request.get_options()?;
            let mut options = Vec::with_capacity(options_reader.len() as usize);
            for i in 0..options_reader.len() {
                let o = options_reader.get(i);
                options.push(PermissionOptionInfo {
                    id: o.get_id()?.to_str()?.to_string(),
                    label: o.get_label()?.to_str()?.to_string(),
                    kind: o.get_kind()?.to_str()?.to_string(),
                });
            }

            let (answer_tx, answer_rx) = oneshot::channel();
            let envelope = PermissionAskEnvelope {
                request_id,
                context_id,
                description,
                instance,
                tool,
                hook_id,
                options,
                answer: answer_tx,
            };
            tx.send(envelope).await.map_err(|_| {
                capnp::Error::failed(
                    "permission ask receiver dropped — nobody is draining subscribe_permission_events".into(),
                )
            })?;
            let answer = answer_rx.await.map_err(|_| {
                capnp::Error::failed(
                    "permission ask answer channel dropped without a reply".into(),
                )
            })?;

            let mut response = results.get().init_response();
            response.set_allow(answer.allow);
            if let Some(id) = &answer.selected_option_id {
                response.set_selected_option_id(id.as_str());
                response.set_has_selected_option_id(true);
            }
            if let Some(remember) = &answer.remember {
                response.set_remember(remember.as_str());
                response.set_has_remember(true);
            }
            Ok(())
        })
    }
}

impl BlockEventsForwarder {
    /// Assert continuity on the ordered lane.
    ///
    /// `0` means the peer predates the rework and is not reporting; a restart
    /// back to 1 is a fresh subscription. Anything else that skips is a kernel
    /// bug — the bus terminates a subscriber it cannot serve rather than
    /// dropping an event, so there is no legitimate hole here.
    fn note_ordered_seq(&self, sub_seq: u64) {
        use std::sync::atomic::Ordering;
        if sub_seq == 0 {
            return;
        }
        let prev = self.last_ordered_seq.swap(sub_seq, Ordering::Relaxed);
        if sub_seq == 1 || prev == 0 || sub_seq == prev + 1 {
            return;
        }
        tracing::error!(
            expected = prev + 1,
            got = sub_seq,
            missing = sub_seq.saturating_sub(prev + 1),
            "block-event subscription seq GAP without a termination — the kernel \
             bus promises lossless-or-terminated, so this is a bug, not backpressure"
        );
    }

    /// Note a musical-time lane delivery. A gap here is the honest report that
    /// beats were missed, which that lane promises; log it at debug so a sink
    /// can be diagnosed without treating it as an error.
    fn note_timing_seq(&self, sub_seq: u64) {
        use std::sync::atomic::Ordering;
        if sub_seq == 0 {
            return;
        }
        let prev = self.last_timing_seq.swap(sub_seq, Ordering::Relaxed);
        if prev != 0 && sub_seq > prev + 1 {
            tracing::debug!(
                missed = sub_seq - prev - 1,
                "missed beats/cues while behind — by design, never replayed"
            );
        }
    }
}

#[allow(refining_impl_trait)]
impl block_events::Server for BlockEventsForwarder {
    fn on_block_inserted(
        self: Rc<Self>,
        params: block_events::OnBlockInsertedParams,
        _results: block_events::OnBlockInsertedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        self.note_ordered_seq(params.get_sub_seq());

        let context_id = match params.get_context_id() {
            Ok(s) => match parse_context_id_data(s) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let block = match params.get_block() {
            Ok(b) => match parse_block_snapshot(&b) {
                Ok(snap) => Box::new(snap),
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            },
            Err(e) => return Promise::err(e),
        };

        let ops = params.get_ops().map(|d| d.to_vec()).unwrap_or_default();

        let event = ServerEvent::BlockInserted {
            context_id,
            block,
            ops,
        };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("Event channel closed, dropping BlockInserted event");
        }
        Promise::ok(())
    }

    fn on_block_deleted(
        self: Rc<Self>,
        params: block_events::OnBlockDeletedParams,
        _results: block_events::OnBlockDeletedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        self.note_ordered_seq(params.get_sub_seq());

        let context_id = match params.get_context_id() {
            Ok(s) => match parse_context_id_data(s) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            },
            Err(e) => return Promise::err(e),
        };

        let event = ServerEvent::BlockDeleted {
            context_id,
            block_id,
        };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("Event channel closed, dropping BlockDeleted event");
        }
        Promise::ok(())
    }

    fn on_block_collapsed(
        self: Rc<Self>,
        params: block_events::OnBlockCollapsedParams,
        _results: block_events::OnBlockCollapsedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        self.note_ordered_seq(params.get_sub_seq());

        let context_id = match params.get_context_id() {
            Ok(s) => match parse_context_id_data(s) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            },
            Err(e) => return Promise::err(e),
        };

        let event = ServerEvent::BlockCollapsedChanged {
            context_id,
            block_id,
            collapsed: params.get_collapsed(),
        };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("Event channel closed, dropping BlockCollapsedChanged event");
        }
        Promise::ok(())
    }

    fn on_block_excluded_changed(
        self: Rc<Self>,
        params: block_events::OnBlockExcludedChangedParams,
        _results: block_events::OnBlockExcludedChangedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        self.note_ordered_seq(params.get_sub_seq());

        let context_id = match params.get_context_id() {
            Ok(s) => match parse_context_id_data(s) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            },
            Err(e) => return Promise::err(e),
        };

        let event = ServerEvent::BlockExcludedChanged {
            context_id,
            block_id,
            excluded: params.get_excluded(),
        };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("Event channel closed, dropping BlockExcludedChanged event");
        }
        Promise::ok(())
    }

    fn on_block_metadata_changed(
        self: Rc<Self>,
        params: block_events::OnBlockMetadataChangedParams,
        _results: block_events::OnBlockMetadataChangedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        self.note_ordered_seq(params.get_sub_seq());

        let context_id = match params.get_context_id() {
            Ok(s) => match parse_context_id_data(s) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            },
            Err(e) => return Promise::err(e),
        };

        let metadata = match params.get_metadata() {
            Ok(m) => crate::rpc::parse_block_metadata(m),
            Err(e) => return Promise::err(e),
        };

        let event = ServerEvent::BlockMetadataChanged {
            context_id,
            block_id,
            metadata,
        };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("Event channel closed, dropping BlockMetadataChanged event");
        }
        Promise::ok(())
    }

    fn on_block_moved(
        self: Rc<Self>,
        params: block_events::OnBlockMovedParams,
        _results: block_events::OnBlockMovedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        self.note_ordered_seq(params.get_sub_seq());

        let context_id = match params.get_context_id() {
            Ok(s) => match parse_context_id_data(s) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            },
            Err(e) => return Promise::err(e),
        };

        let after_id = if params.get_has_after_id() {
            match params.get_after_id() {
                Ok(b) => match parse_block_id(&b) {
                    Ok(id) => Some(id),
                    Err(e) => return Promise::err(rpc_to_capnp(e)),
                },
                Err(e) => return Promise::err(e),
            }
        } else {
            None
        };

        let event = ServerEvent::BlockMoved {
            context_id,
            block_id,
            after_id,
        };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("Event channel closed, dropping BlockMoved event");
        }
        Promise::ok(())
    }

    fn on_block_status_changed(
        self: Rc<Self>,
        params: block_events::OnBlockStatusChangedParams,
        _results: block_events::OnBlockStatusChangedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        self.note_ordered_seq(params.get_sub_seq());

        let context_id = match params.get_context_id() {
            Ok(s) => match parse_context_id_data(s) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            },
            Err(e) => return Promise::err(e),
        };

        let status = match params.get_status() {
            Ok(s) => parse_status(s),
            Err(e) => return Promise::err(e.into()),
        };

        let event = ServerEvent::BlockStatusChanged {
            context_id,
            block_id,
            status,
        };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("Event channel closed, dropping BlockStatusChanged event");
        }
        Promise::ok(())
    }

    fn on_block_output_changed(
        self: Rc<Self>,
        params: block_events::OnBlockOutputChangedParams,
        _results: block_events::OnBlockOutputChangedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        self.note_ordered_seq(params.get_sub_seq());

        let context_id = match params.get_context_id() {
            Ok(s) => match parse_context_id_data(s) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            },
            Err(e) => return Promise::err(e),
        };

        let output = params
            .get_output()
            .ok()
            .and_then(|r| crate::rpc::parse_output_data(r).ok())
            .filter(|d| !d.root.is_empty() || d.headers.is_some());

        let event = ServerEvent::BlockOutputChanged {
            context_id,
            block_id,
            output,
        };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("Event channel closed, dropping BlockOutputChanged event");
        }
        Promise::ok(())
    }

    fn on_context_switched(
        self: Rc<Self>,
        params: block_events::OnContextSwitchedParams,
        _results: block_events::OnContextSwitchedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        self.note_ordered_seq(params.get_sub_seq());

        let context_id = match params.get_context_id() {
            Ok(s) => match parse_context_id_data(s) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        // Reject nil at the wire boundary — nothing downstream should ever
        // treat ContextId::nil as a real context, and accepting it here would
        // send the app on a useless cache-miss → spawn-actor → fail-join
        // round-trip. Fail loud instead of silently.
        if context_id.is_nil() {
            tracing::error!("server sent ContextSwitched with nil context_id — rejecting");
            return Promise::err(capnp::Error::failed(
                "server sent nil context_id in ContextSwitched".into(),
            ));
        }

        let event = ServerEvent::ContextSwitched { context_id };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("Event channel closed, dropping ContextSwitched event");
        }
        Promise::ok(())
    }

    fn on_render_cue(
        self: Rc<Self>,
        params: block_events::OnRenderCueParams,
        _results: block_events::OnRenderCueResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        self.note_timing_seq(params.get_sub_seq());

        let context_id = match params.get_context_id() {
            Ok(s) => match parse_context_id_data(s) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let cue_reader = match params.get_cue() {
            Ok(r) => r,
            Err(e) => return Promise::err(e),
        };
        let cue = match parse_render_cue(cue_reader) {
            Ok(c) => c,
            Err(e) => return Promise::err(e),
        };

        let event = ServerEvent::RenderCue { context_id, cue };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("Event channel closed, dropping RenderCue event");
        }
        Promise::ok(())
    }

    fn on_beat_sync(
        self: Rc<Self>,
        params: block_events::OnBeatSyncParams,
        _results: block_events::OnBeatSyncResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        self.note_timing_seq(params.get_sub_seq());

        let context_id = match params.get_context_id() {
            Ok(s) => match parse_context_id_data(s) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let beat_reader = match params.get_beat_ref() {
            Ok(r) => r,
            Err(e) => return Promise::err(e),
        };
        let beat_ref = kaijutsu_audio::BeatRef {
            beat: beat_reader.get_beat(),
            tempo_bps: beat_reader.get_tempo_bps(),
            epoch_ns: beat_reader.get_epoch_ns(),
        };

        let event = ServerEvent::BeatSync { context_id, beat_ref };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("Event channel closed, dropping BeatSync event");
        }
        Promise::ok(())
    }

    /// The one CALL on this otherwise push-only interface: run a bounded MIDI
    /// request/reply on this client's hardware and answer with the bytes
    /// (`docs/midi-next.md` "SysEx: the exchange pattern").
    ///
    /// Everything else here forwards into a broadcast and returns
    /// immediately; this one holds the kernel's promise open until the local
    /// sink answers. Failures are `Promise::err` — the kernel turns each into
    /// a named `kj` error — because an empty reply and a hang are the two
    /// things an exchange must never be.
    fn exchange(
        self: Rc<Self>,
        params: block_events::ExchangeParams,
        mut results: block_events::ExchangeResults,
    ) -> Promise<(), capnp::Error> {
        let p = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        let port_or_device = match p.get_port_or_device().and_then(read_text) {
            Ok(v) => v,
            Err(e) => return Promise::err(e),
        };
        let payload = match p.get_payload() {
            Ok(b) => b.to_vec(),
            Err(e) => return Promise::err(e),
        };
        let reply_match = match p.get_reply_match() {
            Ok(b) => b.to_vec(),
            Err(e) => return Promise::err(e),
        };
        // 0 would mean "wait forever" to a worker reading it literally, which
        // is the one thing a bounded call may not do. Refuse at the boundary
        // rather than substituting a default the caller never asked for.
        let timeout_ms = p.get_timeout_ms();
        if timeout_ms == 0 {
            return Promise::err(capnp::Error::failed(
                "exchange: timeoutMs must be > 0 — an exchange is a BOUNDED call".into(),
            ));
        }
        let timeout = std::time::Duration::from_millis(u64::from(timeout_ms));
        let slot = self.midi_exchange.clone();
        Promise::from_future(async move {
            match slot.run(port_or_device, payload, reply_match, timeout).await {
                Ok(reply) => {
                    results.get().set_reply(&reply[..]);
                    Ok(())
                }
                Err(e) => Err(capnp::Error::failed(e)),
            }
        })
    }

    /// THE LAG KICK. The kernel cut this subscription because we could not
    /// drain it — our view of the log is incomplete as of `delivered`.
    ///
    /// There is nothing to patch forward from: the events between are gone by
    /// construction. The server drops the connection immediately after this
    /// call, and the actor's reconnect path does a full resync; all this
    /// handler owes anyone is a loud, accurate account of what happened.
    fn on_subscription_terminated(
        self: Rc<Self>,
        params: block_events::OnSubscriptionTerminatedParams,
        _results: block_events::OnSubscriptionTerminatedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        let reason = match params.get_reason() {
            Ok(crate::kaijutsu_capnp::SubscriptionEndReason::SlowSubscriber) => {
                SubscriptionEndReason::SlowSubscriber
            }
            Ok(crate::kaijutsu_capnp::SubscriptionEndReason::ServerShutdown) => {
                SubscriptionEndReason::ServerShutdown
            }
            Ok(crate::kaijutsu_capnp::SubscriptionEndReason::Superseded) => {
                SubscriptionEndReason::Superseded
            }
            Ok(crate::kaijutsu_capnp::SubscriptionEndReason::InternalFault) => {
                SubscriptionEndReason::InternalFault
            }
            Err(_) => SubscriptionEndReason::Unknown,
        };
        let topic = params
            .get_topic()
            .ok()
            .and_then(|t| t.to_str().ok())
            .unwrap_or("")
            .to_string();
        let delivered = params.get_delivered();
        let capacity = params.get_capacity();

        tracing::error!(
            ?reason,
            topic = %topic,
            delivered,
            capacity,
            "kernel terminated our block subscription — we fell behind. The \
             connection is about to drop; reconnect does a full resync."
        );

        // Reset continuity: the next subscription counts from 1.
        self.last_ordered_seq
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.last_timing_seq
            .store(0, std::sync::atomic::Ordering::Relaxed);

        let event = ServerEvent::SubscriptionTerminated {
            reason,
            topic,
            delivered,
            capacity,
        };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("Event channel closed, dropping SubscriptionTerminated event");
        }
        Promise::ok(())
    }
}

/// Parse a Cap'n Proto `RenderCue` reader into the typed
/// `kaijutsu_audio::RenderCue` — the union arm decides `Inline` vs `Cas`; a
/// malformed CAS hash fails the promise loudly rather than silently dropping
/// the cue (house style: crashing beats data corruption). `leadNanos` is a
/// relative `Duration` the sink re-anchors at `receipt + lead`.
fn parse_render_cue(
    reader: crate::kaijutsu_capnp::render_cue::Reader<'_>,
) -> Result<kaijutsu_audio::RenderCue, capnp::Error> {
    let mime = reader.get_mime()?.to_str()?.to_string();
    let lead = std::time::Duration::from_nanos(reader.get_lead_nanos());
    let epoch_ns = reader.get_epoch_ns();
    let onset_beat = reader.get_has_onset_beat().then(|| reader.get_onset_beat());
    let payload = match reader.which()? {
        crate::kaijutsu_capnp::render_cue::Inline(bytes) => {
            kaijutsu_audio::CuePayload::Inline(bytes?.to_vec())
        }
        crate::kaijutsu_capnp::render_cue::CasHash(text) => {
            let hash_str = text?.to_str()?;
            let hash = kaijutsu_cas::ContentHash::from_str_checked(hash_str)
                .map_err(|e| capnp::Error::failed(format!("invalid CAS hash in RenderCue: {e}")))?;
            kaijutsu_audio::CuePayload::Cas(hash)
        }
    };
    Ok(kaijutsu_audio::RenderCue { mime, payload, lead, epoch_ns, onset_beat })
}

// ============================================================================
// Resource Events Forwarder
// ============================================================================

/// Implements the Cap'n Proto `ResourceEvents::Server` trait, forwarding
/// resource update notifications into a `broadcast::Sender<ServerEvent>`.
pub(crate) struct ResourceEventsForwarder {
    pub event_tx: broadcast::Sender<ServerEvent>,
}

#[allow(refining_impl_trait)]
impl resource_events::Server for ResourceEventsForwarder {
    fn on_resource_updated(
        self: Rc<Self>,
        params: resource_events::OnResourceUpdatedParams,
        _results: resource_events::OnResourceUpdatedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let server = match params.get_server() {
            Ok(s) => match read_text(s) {
                Ok(s) => s,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let uri = match params.get_uri() {
            Ok(s) => match read_text(s) {
                Ok(s) => s,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let event = ServerEvent::ResourceUpdated { server, uri };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("Event channel closed, dropping ResourceUpdated event");
        }
        Promise::ok(())
    }

    fn on_resource_list_changed(
        self: Rc<Self>,
        params: resource_events::OnResourceListChangedParams,
        _results: resource_events::OnResourceListChangedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let server = match params.get_server() {
            Ok(s) => match read_text(s) {
                Ok(s) => s,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let event = ServerEvent::ResourceListChanged { server };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("Event channel closed, dropping ResourceListChanged event");
        }
        Promise::ok(())
    }
}

// ============================================================================
// Kernel Output Events
// ============================================================================

/// Events from an `execute()` RPC: stdout, stderr, and exit code.
#[derive(Clone, Debug)]
pub enum OutputEvent {
    Stdout { exec_id: u64, text: String },
    Stderr { exec_id: u64, text: String },
    ExitCode { exec_id: u64, code: i32 },
}

/// Implements Cap'n Proto `KernelOutput::Server`, forwarding each output event
/// into an mpsc channel for consumption by tests and actors.
pub(crate) struct KernelOutputForwarder {
    pub tx: tokio::sync::mpsc::UnboundedSender<OutputEvent>,
}

#[allow(refining_impl_trait)]
impl kernel_output::Server for KernelOutputForwarder {
    fn on_output(
        self: Rc<Self>,
        params: kernel_output::OnOutputParams,
        _results: kernel_output::OnOutputResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let event_reader = match params.get_event() {
            Ok(e) => e,
            Err(e) => return Promise::err(e),
        };

        let exec_id = event_reader.get_exec_id();

        let output_event = match event_reader.get_event() {
            Ok(inner) => match inner.which() {
                Ok(crate::kaijutsu_capnp::output_event::Stdout(text)) => {
                    match text {
                        Ok(t) => match t.to_str() {
                            Ok(s) => OutputEvent::Stdout { exec_id, text: s.to_owned() },
                            Err(e) => return Promise::err(capnp::Error::failed(e.to_string())),
                        },
                        Err(e) => return Promise::err(e),
                    }
                }
                Ok(crate::kaijutsu_capnp::output_event::Stderr(text)) => {
                    match text {
                        Ok(t) => match t.to_str() {
                            Ok(s) => OutputEvent::Stderr { exec_id, text: s.to_owned() },
                            Err(e) => return Promise::err(capnp::Error::failed(e.to_string())),
                        },
                        Err(e) => return Promise::err(e),
                    }
                }
                Ok(crate::kaijutsu_capnp::output_event::ExitCode(code)) => {
                    OutputEvent::ExitCode { exec_id, code }
                }
                Ok(crate::kaijutsu_capnp::output_event::Structured(_)) => {
                    // Structured output not yet used — ignore silently.
                    return Promise::ok(());
                }
                Err(e) => return Promise::err(e.into()),
            },
            Err(e) => return Promise::err(e),
        };

        if self.tx.send(output_event).is_err() {
            tracing::warn!("Output channel closed, dropping output event");
        }
        Promise::ok(())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod turn_events_tests {
    //! Cap'n Proto round-trip for the turn-outcome payloads.
    //!
    //! These drive the real generated client/server pair built by
    //! [`turn_events_channel`] — set the wire fields, send, and assert on the
    //! [`ServerEvent`] that falls out the other side. The decode is where a
    //! wire change goes quietly wrong (an enumerant mapped to the wrong
    //! variant, an unset optional read as if it were set), so it is worth
    //! exercising through the generated code rather than against a hand-built
    //! reader.

    use super::*;
    use kaijutsu_types::PrincipalId;

    /// Run `f` on a LocalSet — capnp-rpc clients are `Rc`-based and must not
    /// cross threads.
    fn run_local<F: std::future::Future<Output = ()>>(f: F) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, f);
    }

    /// A completed turn carrying an output block: every field survives the
    /// round-trip, and the block id is reassembled whole (context + principal +
    /// seq), not just its seq.
    #[test]
    fn completed_with_output_block_round_trips() {
        run_local(async {
            let (client, mut rx) = turn_events_channel(16);
            let ctx = ContextId::new();
            let principal = PrincipalId::new();
            let output = BlockId::new(ctx, principal, 42);

            let mut req = client.on_turn_completed_request();
            {
                let mut p = req.get();
                p.set_context_id(ctx.as_bytes());
                p.set_principal_id(principal.as_bytes());
                p.set_stop_reason(crate::kaijutsu_capnp::TurnStopReason::MaxTokens);
                p.set_origin(crate::kaijutsu_capnp::TurnOrigin::Interactive);
                p.set_has_output_block(true);
                let mut b = p.init_output_block_id();
                b.set_context_id(output.context_id.as_bytes());
                b.set_principal_id(output.principal_id.as_bytes());
                b.set_seq(output.seq);
            }
            req.send().promise.await.expect("callback delivered");

            match rx.try_recv().expect("a TurnCompleted event arrived") {
                ServerEvent::TurnCompleted {
                    context_id,
                    principal_id,
                    output_block_id,
                    stop_reason,
                    origin,
                } => {
                    assert_eq!(context_id, ctx);
                    assert_eq!(principal_id, principal);
                    assert_eq!(output_block_id, Some(output));
                    assert_eq!(stop_reason, TurnCompletedStopReason::MaxTokens);
                    assert_eq!(origin, TurnOrigin::Interactive);
                }
                other => panic!("expected TurnCompleted, got {other:?}"),
            }
        });
    }

    /// `hasOutputBlock = false` decodes to `None`. The id slot is zeroed on the
    /// wire, and reading it anyway would hand the consumer a nil-context block
    /// id that looks real — the exact silent fallback the flag exists to avoid.
    #[test]
    fn completed_without_output_block_decodes_to_none() {
        run_local(async {
            let (client, mut rx) = turn_events_channel(16);
            let ctx = ContextId::new();
            let principal = PrincipalId::new();

            let mut req = client.on_turn_completed_request();
            {
                let mut p = req.get();
                p.set_context_id(ctx.as_bytes());
                p.set_principal_id(principal.as_bytes());
                p.set_stop_reason(crate::kaijutsu_capnp::TurnStopReason::EndTurn);
                p.set_origin(crate::kaijutsu_capnp::TurnOrigin::Autonomous);
                p.set_has_output_block(false);
            }
            req.send().promise.await.expect("callback delivered");

            match rx.try_recv().expect("a TurnCompleted event arrived") {
                ServerEvent::TurnCompleted {
                    output_block_id,
                    origin,
                    ..
                } => {
                    assert_eq!(output_block_id, None, "no text → None, never a nil id");
                    assert_eq!(origin, TurnOrigin::Autonomous);
                }
                other => panic!("expected TurnCompleted, got {other:?}"),
            }
        });
    }

    /// Every stop reason maps to its own client variant. A collapsed mapping
    /// (two endings decoding to one) is the failure this pins down: it would
    /// make a cancelled turn read as a finished one at the frontend.
    #[test]
    fn every_stop_reason_maps_distinctly() {
        run_local(async {
            use crate::kaijutsu_capnp::TurnStopReason as W;
            let cases = [
                (W::EndTurn, TurnCompletedStopReason::EndTurn),
                (W::CancelledSoft, TurnCompletedStopReason::CancelledSoft),
                (
                    W::CancelledImmediate,
                    TurnCompletedStopReason::CancelledImmediate,
                ),
                (W::MaxTokens, TurnCompletedStopReason::MaxTokens),
                (W::MaxIterations, TurnCompletedStopReason::MaxIterations),
            ];
            let (client, mut rx) = turn_events_channel(16);
            let ctx = ContextId::new();
            let principal = PrincipalId::new();

            for (wire, expected) in cases {
                let mut req = client.on_turn_completed_request();
                {
                    let mut p = req.get();
                    p.set_context_id(ctx.as_bytes());
                    p.set_principal_id(principal.as_bytes());
                    p.set_stop_reason(wire);
                    p.set_origin(crate::kaijutsu_capnp::TurnOrigin::Autonomous);
                    p.set_has_output_block(false);
                }
                req.send().promise.await.expect("callback delivered");

                match rx.try_recv().expect("a TurnCompleted event arrived") {
                    ServerEvent::TurnCompleted { stop_reason, .. } => {
                        assert_eq!(stop_reason, expected, "{wire:?} must decode distinctly");
                    }
                    other => panic!("expected TurnCompleted, got {other:?}"),
                }
            }

            // Both cancels report as cancelled; nothing else does.
            assert!(TurnCompletedStopReason::CancelledSoft.is_cancelled());
            assert!(TurnCompletedStopReason::CancelledImmediate.is_cancelled());
            assert!(!TurnCompletedStopReason::EndTurn.is_cancelled());
            assert!(!TurnCompletedStopReason::MaxTokens.is_cancelled());
            assert!(!TurnCompletedStopReason::MaxIterations.is_cancelled());
        });
    }

    /// A failed turn round-trips its error text and origin.
    #[test]
    fn failed_round_trips() {
        run_local(async {
            let (client, mut rx) = turn_events_channel(16);
            let ctx = ContextId::new();
            let principal = PrincipalId::new();

            let mut req = client.on_turn_failed_request();
            {
                let mut p = req.get();
                p.set_context_id(ctx.as_bytes());
                p.set_principal_id(principal.as_bytes());
                p.set_error("hydration failed: could not read conversation history");
                p.set_origin(crate::kaijutsu_capnp::TurnOrigin::Interactive);
            }
            req.send().promise.await.expect("callback delivered");

            match rx.try_recv().expect("a TurnFailed event arrived") {
                ServerEvent::TurnFailed {
                    context_id,
                    principal_id,
                    error,
                    origin,
                } => {
                    assert_eq!(context_id, ctx);
                    assert_eq!(principal_id, principal);
                    assert_eq!(error, "hydration failed: could not read conversation history");
                    assert_eq!(origin, TurnOrigin::Interactive);
                }
                other => panic!("expected TurnFailed, got {other:?}"),
            }
        });
    }

    /// A malformed context id fails the callback loudly rather than inventing
    /// an id. Crashing the push beats handing a consumer a turn outcome
    /// attributed to a context that does not exist.
    #[test]
    fn malformed_context_id_is_an_error_not_a_guess() {
        run_local(async {
            let (client, mut rx) = turn_events_channel(16);
            let mut req = client.on_turn_completed_request();
            {
                let mut p = req.get();
                p.set_context_id(&[0u8; 3]); // not 16 bytes
                p.set_principal_id(PrincipalId::new().as_bytes());
                p.set_stop_reason(crate::kaijutsu_capnp::TurnStopReason::EndTurn);
                p.set_origin(crate::kaijutsu_capnp::TurnOrigin::Autonomous);
                p.set_has_output_block(false);
            }
            let err = match req.send().promise.await {
                Ok(_) => panic!("a 3-byte context id must not decode"),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("context_id"),
                "the error should name the bad field, got: {err}"
            );
            assert!(
                rx.try_recv().is_err(),
                "nothing may be published from a malformed push"
            );
        });
    }
}

#[cfg(test)]
mod permission_events_tests {
    //! Cap'n Proto round-trip for `HookAction::Ask` (D-57, docs/acp.md gap
    //! #2), driving the real generated client/server pair built by
    //! [`permission_events_channel`].
    //!
    //! Unlike the push-only channels above, `onAsk` is a blocking
    //! call/response: the server's `on_ask_request().send().promise` does
    //! not resolve until something drains the envelope off the receiver and
    //! sends an answer back through it. These tests are the proof that half
    //! of the D-57 wire actually works end to end (decode the request,
    //! round-trip the answer) — the other half, the server-side dispatch
    //! from `HookAction::Ask` through the bridge to this callback, is
    //! covered by `kaijutsu-server`'s `permission_ask_wire` e2e test.

    use super::*;

    fn run_local<F: std::future::Future<Output = ()>>(f: F) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, f);
    }

    /// Send a request, answer "allow" from a spawned task (mimicking a
    /// consumer draining the envelope receiver), and assert the response
    /// that comes back over the wire.
    #[test]
    fn allow_round_trips() {
        run_local(async {
            let (client, mut rx) = permission_events_channel(16);
            let ctx = ContextId::new();

            tokio::task::spawn_local(async move {
                let envelope = rx.recv().await.expect("envelope");
                assert_eq!(envelope.request_id, "req-1");
                assert_eq!(envelope.context_id, ctx);
                assert_eq!(envelope.description, "about to rm -rf a workspace");
                assert_eq!(envelope.instance, "builtin.shell");
                assert_eq!(envelope.tool, "shell.exec");
                assert_eq!(envelope.hook_id, "confirm-shell");
                assert!(envelope.options.is_empty());
                envelope
                    .answer
                    .send(PermissionAskAnswer {
                        allow: true,
                        selected_option_id: None,
                        remember: Some("session".into()),
                    })
                    .expect("answer channel open");
            });

            let mut req = client.on_ask_request();
            {
                let mut r = req.get().init_request();
                r.set_request_id("req-1");
                r.set_context_id(ctx.as_bytes());
                r.set_description("about to rm -rf a workspace");
                r.set_instance("builtin.shell");
                r.set_tool("shell.exec");
                r.set_hook_id("confirm-shell");
                r.init_options(0);
            }
            let reply = req.send().promise.await.expect("onAsk round trip");
            let response = reply.get().unwrap().get_response().unwrap();
            assert!(response.get_allow());
            assert!(!response.get_has_selected_option_id());
            assert!(response.get_has_remember());
            assert_eq!(
                response.get_remember().unwrap().to_str().unwrap(),
                "session"
            );
        });
    }

    /// Same round trip, answering "deny" with a selected option id — the
    /// negative-control complement to `allow_round_trips`, and proof
    /// `selectedOptionId` survives independently of `remember`.
    #[test]
    fn deny_with_selected_option_round_trips() {
        run_local(async {
            let (client, mut rx) = permission_events_channel(16);
            let ctx = ContextId::new();

            tokio::task::spawn_local(async move {
                let envelope = rx.recv().await.expect("envelope");
                envelope
                    .answer
                    .send(PermissionAskAnswer {
                        allow: false,
                        selected_option_id: Some("reject_always".into()),
                        remember: None,
                    })
                    .expect("answer channel open");
            });

            let mut req = client.on_ask_request();
            {
                let mut r = req.get().init_request();
                r.set_request_id("req-2");
                r.set_context_id(ctx.as_bytes());
                r.set_description("about to rm -rf a workspace");
                r.set_instance("builtin.shell");
                r.set_tool("shell.exec");
                r.set_hook_id("confirm-shell");
                r.init_options(0);
            }
            let reply = req.send().promise.await.expect("onAsk round trip");
            let response = reply.get().unwrap().get_response().unwrap();
            assert!(!response.get_allow());
            assert!(response.get_has_selected_option_id());
            assert_eq!(
                response.get_selected_option_id().unwrap().to_str().unwrap(),
                "reject_always"
            );
            assert!(!response.get_has_remember());
        });
    }

    /// The `options` list — present on the wire for a future richer prompt
    /// (ACP-style allow-once/allow-always/...) even though `HookAction::Ask`
    /// doesn't populate it today — decodes intact.
    #[test]
    fn options_list_round_trips() {
        run_local(async {
            let (client, mut rx) = permission_events_channel(16);
            let ctx = ContextId::new();

            tokio::task::spawn_local(async move {
                let envelope = rx.recv().await.expect("envelope");
                assert_eq!(envelope.options.len(), 2);
                assert_eq!(envelope.options[0].id, "allow_once");
                assert_eq!(envelope.options[0].label, "Allow once");
                assert_eq!(envelope.options[0].kind, "allow_once");
                assert_eq!(envelope.options[1].id, "reject_always");
                envelope
                    .answer
                    .send(PermissionAskAnswer {
                        allow: true,
                        selected_option_id: Some("allow_once".into()),
                        remember: None,
                    })
                    .expect("answer channel open");
            });

            let mut req = client.on_ask_request();
            {
                let mut r = req.get().init_request();
                r.set_request_id("req-3");
                r.set_context_id(ctx.as_bytes());
                r.set_description("d");
                r.set_instance("i");
                r.set_tool("t");
                r.set_hook_id("h");
                let mut opts = r.init_options(2);
                {
                    let mut o = opts.reborrow().get(0);
                    o.set_id("allow_once");
                    o.set_label("Allow once");
                    o.set_kind("allow_once");
                }
                {
                    let mut o = opts.reborrow().get(1);
                    o.set_id("reject_always");
                    o.set_label("Reject always");
                    o.set_kind("reject_always");
                }
            }
            req.send().promise.await.expect("onAsk round trip");
        });
    }

    /// A malformed context id fails the promise loudly — same discipline as
    /// `turn_events_tests::malformed_context_id_is_an_error_not_a_guess`,
    /// and here it also proves nothing was forwarded to the envelope
    /// receiver on a decode failure (no half-built ask reaches a consumer).
    #[test]
    fn malformed_context_id_is_an_error_not_a_guess() {
        run_local(async {
            let (client, mut rx) = permission_events_channel(16);
            let mut req = client.on_ask_request();
            {
                let mut r = req.get().init_request();
                r.set_request_id("req-bad");
                r.set_context_id(&[0u8; 3]); // not 16 bytes
                r.set_description("d");
                r.set_instance("i");
                r.set_tool("t");
                r.set_hook_id("h");
                r.init_options(0);
            }
            let err = match req.send().promise.await {
                Ok(_) => panic!("a 3-byte context id must not decode"),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("context"),
                "the error should name the bad field, got: {err}"
            );
            assert!(
                rx.try_recv().is_err(),
                "nothing may be forwarded from a malformed ask"
            );
        });
    }

    /// If nobody is draining the envelope receiver (dropped, or the
    /// subscriber never wired one up), `onAsk` fails loudly rather than
    /// hanging forever — the caller-side timeout in
    /// `Broker::run_permission_ask` is the real backstop, but the capnp
    /// promise itself must still resolve so the bridge task's per-request
    /// slot isn't wedged waiting on a channel with no reader.
    #[test]
    fn dropped_receiver_fails_the_promise_instead_of_hanging() {
        run_local(async {
            let (client, rx) = permission_events_channel(16);
            drop(rx);

            let mut req = client.on_ask_request();
            {
                let mut r = req.get().init_request();
                r.set_request_id("req-orphan");
                r.set_context_id(ContextId::new().as_bytes());
                r.set_description("d");
                r.set_instance("i");
                r.set_tool("t");
                r.set_hook_id("h");
                r.init_options(0);
            }
            let err = match req.send().promise.await {
                Ok(_) => panic!("onAsk must fail, not hang, when the envelope receiver is gone"),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("receiver dropped"),
                "got: {err}"
            );
        });
    }
}
