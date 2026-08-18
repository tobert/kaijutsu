//! FSM-based RPC actor with explicit state transitions and typed errors.
//!
//! # State machine
//!
//! ```text
//!     ┌──────┐
//!     │ Idle │  (transient bootstrap; dials immediately on start)
//!     └──┬───┘
//!        │ eager
//!        ▼
//! ┌──────────────────────────┐
//! │ Connecting { attempt }   │ ◄──┐
//! │ (handshake task running) │    │ timer expired
//! └──┬───────┬───────┬───────┘    │
//!    │       │       │            │
//!    │ Ok    │ trans │ perm       │
//!    ▼       ▼       ▼            │
//! ┌──────┐ ┌────────┐ ┌──────────┐│
//! │ Conn │ │Cooldown│ │ Terminal ││
//! └──┬───┘ └───┬────┘ └──────────┘│
//!    │ close   └─────────────────┘
//!    ▼
//! ┌──────────┐
//! │ Closing  │  (drop ConnectionState, abort ping task)
//! └──┬───────┘
//!    ▼
//!  Cooldown OR Terminal (depending on `cause`)
//! ```
//!
//! # Invariants
//!
//! 1. `ConnectionState` is owned only by the `Connected` arm of the state.
//!    All state mutations happen inside the actor's `run` loop, never as
//!    side effects of awaits inside helper futures. This means a cancelled
//!    handshake future can't leak a half-built connection into the actor.
//!
//! 2. The connect handshake runs as a `spawn_local` task whose `JoinHandle`
//!    the actor owns. Aborting the handle drops the task frame cleanly —
//!    no resources move into the actor's state mid-handshake.
//!
//! 3. Per-phase deadlines wrap each step (SSH dial, bind_kernel, join_context,
//!    subscribe). The total connect budget acts as a safety net; the per-phase
//!    budget lets the failure message name the slow phase.
//!
//! 4. A liveness ping task spawned during `Connected` detects RPC-layer
//!    wedges that the SSH keepalive can't see (e.g., RPC system aborted
//!    while channels stay open). Pings have their own per-ping deadline.
//!
//! 5. Backoff time is consulted by the loop's match arm on `Cooldown`, not
//!    by an `if` inside a command handler. There is no "skip backoff because
//!    connection is Some" path — the connection only exists in `Connected`.
//!
//! 6. Close signals are coalesced through a `mpsc::channel(1)` with `try_send`.
//!    The first failure wins; a burst of 32 in-flight failures becomes one
//!    close, not 32 log lines.
//!
//! 7. The `instance` UUID is set once at actor construction and reused for
//!    every `join_context` and every `subscribe_*` call. The server uses
//!    `(principal, instance)` to dedupe subscriptions across reconnects.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kaijutsu_types::{ContextId, KernelId};
use kaijutsu_types::{BlockFilter, BlockId, BlockQuery, BlockSnapshot, Status};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::Instrument;

use crate::constants::{
    BACKOFF_BASE, BACKOFF_MAX, CONNECT_TOTAL_BUDGET, PING_INTERVAL, PING_TIMEOUT,
    RPC_BIND_KERNEL_TIMEOUT, RPC_CALL_TIMEOUT, RPC_JOIN_CONTEXT_TIMEOUT, SSH_DIAL_TIMEOUT,
    SUBSCRIBE_TIMEOUT,
};
/// Depth of the actor-internal queue between a context feed's observer and the
/// consumer's receiver.
///
/// Deep enough that an ordinary streaming burst never backs up into the RPC
/// callback, shallow enough that a consumer which has stopped draining applies
/// backpressure the kernel can see and act on — it terminates a subscriber that
/// cannot keep up rather than dropping events behind its back.
const CONTEXT_FEED_QUEUE: usize = 256;

use crate::rpc::{
    Completion, ContextCluster, ContextInfo, EditorState, HistoryEntry, Identity, InputState,
    KernelInfo, LlmConfigInfo, McpResource, McpToolResult, PeerInfo, ShellValue, SimilarContext,
    StagedDriftInfo, SubmitResult, ToolResult, ToolSchema, VersionSnapshot,
};
use crate::subscriptions::{
    BlockEventsForwarder, ConnectionStatus, EditorEventsForwarder, LedgerEventsForwarder,
    ResourceEventsForwarder, ServerEvent, TurnEventsForwarder, VfsActivityEventsForwarder,
};
use crate::{ConnectError, KernelHandle, RpcClient, SshConfig, connect_ssh};

// ────────────────────────────────────────────────────────────────────────────
// Capacities
// ────────────────────────────────────────────────────────────────────────────

/// Channel capacity — when 32 commands are queued, callers block on send.
/// This is the natural backpressure: when the actor is saturated (or rejecting
/// commands during reconnect), senders wait.
const CHANNEL_CAPACITY: usize = 32;

/// Broadcast capacity for server events.
const EVENT_BROADCAST_CAPACITY: usize = 256;

/// Broadcast capacity for connection status events.
const STATUS_BROADCAST_CAPACITY: usize = 16;

/// Broadcast capacity for the approval-ledger change stream. The kernel
/// already coalesces a burst to one notification per
/// `context_feed::FEED_BATCH_WINDOW` (`kaijutsu-server`'s
/// `subscribe_ledger_events`), and only the newest generation ever matters —
/// so this only needs to be big enough that a momentarily-slow subscriber
/// doesn't lag off a fast one; it is not a throughput budget the way
/// `EVENT_BROADCAST_CAPACITY` is.
const LEDGER_BROADCAST_CAPACITY: usize = 16;

// ────────────────────────────────────────────────────────────────────────────
// Errors (public API)
// ────────────────────────────────────────────────────────────────────────────

/// Errors returned by every `ActorHandle` method.
///
/// Variants distinguish *why* a call didn't complete so callers can react
/// appropriately: a poller can quietly skip on `NotReady`, but a user-facing
/// command should surface `PermanentlyFailed` loudly.
#[derive(Debug, Clone, thiserror::Error)]
pub enum CallError {
    /// The actor's FSM is in a state that can't serve this call right now.
    /// Includes the reason so callers can show useful UI ("connecting...",
    /// "next retry in 12s", etc.).
    #[error("not ready: {0}")]
    NotReady(NotReadyReason),

    /// Permanent failure — retries won't help. Auth rejected, host key
    /// mismatch, repeated subscribe wedge with no recovery path. Callers
    /// should surface this to the user.
    #[error("permanently failed: {0}")]
    PermanentlyFailed(String),

    /// RPC was attempted, the pipe was alive, and the kernel returned an
    /// error. Connection is still healthy; retry the call (with different
    /// args, presumably) if the caller wants to.
    #[error("RPC error: {0}")]
    Rpc(String),

    /// Per-call deadline exceeded — `RPC_CALL_TIMEOUT` for most commands
    /// (`dispatch!`), or a per-call override for the few dispatched through
    /// `dispatch_deadline!` instead (today: `ExecuteTool`, `CallMcpTool`
    /// and `ExecuteKj`, at `kaijutsu_types::timeout::gate::CLIENT_CALL`,
    /// because each can reach a gate holding for a human answer). The carried
    /// `Duration` is always the deadline that actually fired, so the
    /// message names the right number either way.
    /// Connection is NOT torn down — the handler hung, not the pipe.
    #[error("call timed out after {0:?}")]
    Timeout(Duration),

    /// The actor task is no longer running. Either an unrecoverable bug or
    /// shutdown in progress. Callers should stop sending commands.
    #[error("actor shut down")]
    Shutdown,
}

/// Why the actor declined to serve a call. Returned inside `CallError::NotReady`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum NotReadyReason {
    /// Actor hasn't left its transient bootstrap yet. With eager connect the
    /// actor dials on start and never rests here, so a caller should not
    /// normally observe this; it remains for completeness of the mapping.
    #[error("idle")]
    Idle,
    /// Handshake in progress.
    #[error("connecting (attempt {attempt})")]
    Connecting { attempt: u32 },
    /// Last attempt failed; waiting before retrying.
    #[error("cooldown until {until_ms} ms (last error: {last_error})")]
    Cooldown {
        /// Unix-epoch milliseconds when the next attempt is allowed.
        until_ms: u64,
        /// Stringified error from the last attempt.
        last_error: String,
    },
    /// Connection is being torn down; reconnect will follow.
    #[error("closing")]
    Closing,
}

// ────────────────────────────────────────────────────────────────────────────
// Internal state
// ────────────────────────────────────────────────────────────────────────────

/// Internal FSM state. Private — observers use `ConnectionStatus` instead.
#[derive(Debug)]
enum ActorState {
    /// Transient bootstrap state. The run loop dials immediately (eager
    /// connect), so the actor never rests here; it also serves as the
    /// placeholder `finish_closing` swaps in before computing the next state.
    Idle,
    Connecting {
        attempt: u32,
        started_at: Instant,
    },
    Connected {
        since: Instant,
    },
    Closing {
        cause: CloseCause,
        /// The reconnect attempt count carried in from the state we left, so
        /// `finish_closing` can compute the next backoff. 0 when we closed
        /// from a healthy `Connected` (the next reconnect is attempt 1).
        attempt: u32,
    },
    Cooldown {
        next_attempt: u32,
        until: Instant,
        last_error: String,
    },
    Terminal {
        reason: String,
    },
}

impl ActorState {
    fn name(&self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Connecting { .. } => "Connecting",
            Self::Connected { .. } => "Connected",
            Self::Closing { .. } => "Closing",
            Self::Cooldown { .. } => "Cooldown",
            Self::Terminal { .. } => "Terminal",
        }
    }
}

/// Why a Closing transition was initiated. Determines whether the next
/// state is Cooldown (retry) or Terminal (give up).
#[derive(Debug, Clone)]
enum CloseCause {
    /// A child task observed `Disconnected` on the RPC pipe.
    RpcError(String),
    /// Liveness ping deadline exceeded or ping returned an error.
    PingFailed(String),
    /// Server's bound kernel ID changed under us (kernel restart).
    KernelIdChanged { expected: KernelId, got: KernelId },
    /// External shutdown signal (mpsc closed).
    Shutdown,
}

impl CloseCause {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Shutdown)
    }

    fn to_error_string(&self) -> String {
        match self {
            Self::RpcError(s) => format!("rpc error: {s}"),
            Self::PingFailed(s) => format!("ping failed: {s}"),
            Self::KernelIdChanged { expected, got } => {
                format!("kernel ID changed: expected {expected}, got {got}")
            }
            Self::Shutdown => "shutdown".into(),
        }
    }
}

/// Outcome of the handshake task spawned during `Connecting`.
enum ConnectOutcome {
    Ok(BuiltConnection),
    Transient(String),
    Permanent(String),
}

/// A fully-built, subscribed-and-ready connection produced by the handshake.
///
/// The handshake task returns this; the actor's run loop moves it into
/// `RpcActor::connection` only on the `Ok` arm.
struct BuiltConnection {
    client: RpcClient,
    kernel: KernelHandle,
    kernel_id: KernelId,
    joined_context: Option<ContextId>,
}

/// Wraps the live connection while the actor is in `Connected`.
///
/// The bound kernel ID lives on `RpcActor::bound_kernel_id` so the ping task
/// can capture it without holding a reference to this struct.
struct ConnectionState {
    client: RpcClient,
    kernel: KernelHandle,
}

/// Internal messages spawned child tasks send back to the actor loop.
///
/// Used so a long-running RPC (e.g., `join_context` against a slow kernel)
/// doesn't block the actor's main loop and the loop can still react to
/// close signals like a ping failure in the meantime.
enum InternalMsg {
    /// A `join_context` call returned successfully — update cached context.
    JoinedContext(ContextId),
}

// ────────────────────────────────────────────────────────────────────────────
// RPC commands (internal mpsc payload)
// ────────────────────────────────────────────────────────────────────────────

/// Internal command sent from `ActorHandle` → `RpcActor` via mpsc.
///
/// Each variant carries its arguments and a oneshot reply channel. World-level
/// and FSM-mutating commands are handled inline in the run loop; kernel-level
/// commands are dispatched concurrently via `spawn_local`.
// One mpsc payload per RPC call shape, so variant sizes vary with each
// call's own arguments — boxing the larger ones trades enum size for a
// heap alloc on every dispatch of this actor's hottest path; not a clear
// win either way, left as a design call.
#[allow(clippy::large_enum_variant)]
enum RpcCommand {
    // ── Drift ────────────────────────────────────────────────────────────
    DriftQueue {
        reply: oneshot::Sender<Result<Vec<StagedDriftInfo>, CallError>>,
    },
    DriftCancel {
        staged_id: u64,
        reply: oneshot::Sender<Result<bool, CallError>>,
    },

    // ── Context ──────────────────────────────────────────────────────────
    GetContextId {
        reply: oneshot::Sender<Result<(ContextId, String), CallError>>,
    },
    ListContexts {
        reply: oneshot::Sender<Result<Vec<ContextInfo>, CallError>>,
    },
    ListTracks {
        reply: oneshot::Sender<Result<Vec<crate::rpc::TrackInfo>, CallError>>,
    },
    // ── VFS (FSN world stage-0/1 plumbing, docs/scenes/vfs.md) ─────────────
    VfsSnapshot {
        path: String,
        depth: u32,
        max_entries: u32,
        reply: oneshot::Sender<Result<crate::rpc::SnapshotResult, CallError>>,
    },
    /// Start (or no-op if already started) the VFS activity digest push
    /// subscription for this connection. Handled entirely inline by
    /// `RpcActor::dispatch` (needs `self.event_tx` to build the forwarder,
    /// same reason `AttachPeer`/`JoinContext`/`ResubscribeBlocks` are
    /// special-cased there) — never routed through `dispatch_kernel_command`.
    SubscribeVfsActivity {
        interval_ms: u32,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    Conclude {
        context_id: ContextId,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    RenameContext {
        context_id: ContextId,
        label: String,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    PromoteContext {
        context_id: ContextId,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    DemoteContext {
        context_id: ContextId,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    SetContextPaused {
        context_id: ContextId,
        paused: bool,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    SetContextOriginHost {
        context_id: ContextId,
        origin_host: String,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    /// Author one block over RPC — `authorBlock @106`, the path that lets a
    /// client write blocks without authoring or decoding storage-engine
    /// operations.
    AuthorBlock {
        req: Box<crate::rpc::AuthorBlock>,
        reply: oneshot::Sender<Result<BlockId, CallError>>,
    },
    /// Move an already-authored block to a terminal state — `completeBlock
    /// @107`, the flow half of reserve-then-flow.
    CompleteBlock {
        context_id: ContextId,
        block_id: BlockId,
        status: Status,
        is_error: bool,
        exit_code: Option<i32>,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    ArchiveContext {
        context_id: ContextId,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    SearchSimilar {
        query: String,
        k: u32,
        reply: oneshot::Sender<Result<Vec<SimilarContext>, CallError>>,
    },
    GetNeighbors {
        context_id: ContextId,
        k: u32,
        reply: oneshot::Sender<Result<Vec<SimilarContext>, CallError>>,
    },
    GetClusters {
        min_cluster_size: u32,
        reply: oneshot::Sender<Result<Vec<ContextCluster>, CallError>>,
    },
    CreateContext {
        label: String,
        context_type: String,
        reply: oneshot::Sender<Result<ContextId, CallError>>,
    },
    /// DB-driven label lookup — bypasses the DriftRouter `ListContexts`
    /// reads. `None` reply payload means no context currently holds the
    /// label. See `KernelHandle::resolve_context_label`.
    ResolveContextLabel {
        label: String,
        reply: oneshot::Sender<Result<Option<crate::rpc::ContextInfo>, CallError>>,
    },

    // ── Blocks / Change Feed ─────────────────────────────────────────────
    GetBlocks {
        context_id: ContextId,
        query: BlockQuery,
        reply: oneshot::Sender<Result<Vec<BlockSnapshot>, CallError>>,
    },
    /// Follow one context's change feed. The actor keeps the sender and
    /// re-subscribes on every reconnect, so the consumer's receiver outlives
    /// the connection (docs/change-feed.md).
    SubscribeContext {
        context_id: ContextId,
        sender: mpsc::Sender<crate::context_feed::FeedEvent>,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    /// Same query, but keeping the context version the blocks were read at —
    /// the snapshot half of the change feed's recovery protocol
    /// (docs/change-feed.md rules 21-26).
    GetBlocksVersioned {
        context_id: ContextId,
        query: BlockQuery,
        reply: oneshot::Sender<Result<(Vec<BlockSnapshot>, u64), CallError>>,
    },
    GetContextVersion {
        context_id: ContextId,
        reply: oneshot::Sender<Result<u64, CallError>>,
    },
    CompactContext {
        context_id: ContextId,
        reply: oneshot::Sender<Result<(u64, u64), CallError>>,
    },

    // ── Shell / Execution ────────────────────────────────────────────────
    Execute {
        code: String,
        reply: oneshot::Sender<Result<u64, CallError>>,
    },
    ShellExecute {
        code: String,
        context_id: ContextId,
        user_initiated: bool,
        reply: oneshot::Sender<Result<BlockId, CallError>>,
    },
    SetBlockExcluded {
        context_id: ContextId,
        block_id: BlockId,
        excluded: bool,
        reply: oneshot::Sender<Result<u64, CallError>>,
    },
    Interrupt {
        exec_id: u64,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    Complete {
        partial: String,
        cursor: u32,
        reply: oneshot::Sender<Result<Vec<Completion>, CallError>>,
    },
    GetCommandHistory {
        limit: u32,
        reply: oneshot::Sender<Result<Vec<HistoryEntry>, CallError>>,
    },

    // ── Addressed Shell State ───────────────────────────────────────────
    GetContextCwd {
        context_id: ContextId,
        reply: oneshot::Sender<Result<Option<String>, CallError>>,
    },
    SetContextCwd {
        context_id: ContextId,
        path: String,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    ExecuteKj {
        context_id: ContextId,
        argv: Vec<String>,
        reply: oneshot::Sender<Result<crate::rpc::KjExecutionResult, CallError>>,
    },
    GetKjCommandCatalog {
        context_id: ContextId,
        reply: oneshot::Sender<Result<Vec<crate::rpc::KjCommandInfo>, CallError>>,
    },

    // ── Shell Variables ──────────────────────────────────────────────────
    GetShellVar {
        name: String,
        reply: oneshot::Sender<Result<(Option<ShellValue>, bool), CallError>>,
    },
    SetShellVar {
        name: String,
        value: ShellValue,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    ListShellVars {
        reply: oneshot::Sender<Result<Vec<(String, ShellValue)>, CallError>>,
    },

    // ── Per-client durable view state (docs/shared-state.md "Retiring KV") ──
    SetLastContext {
        client_id: String,
        context_id: ContextId,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    GetClientView {
        client_id: String,
        reply: oneshot::Sender<Result<Option<ContextId>, CallError>>,
    },

    // ── Input Document ──────────────────────────────────────────────────
    EditInput {
        context_id: ContextId,
        pos: u64,
        insert: String,
        delete: u64,
        reply: oneshot::Sender<Result<u64, CallError>>,
    },
    GetInputState {
        context_id: ContextId,
        reply: oneshot::Sender<Result<InputState, CallError>>,
    },
    SubmitInput {
        context_id: ContextId,
        is_shell: bool,
        reply: oneshot::Sender<Result<SubmitResult, CallError>>,
    },
    ClearInput {
        context_id: ContextId,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    CommitCapture {
        context_id: ContextId,
        mime: String,
        payload: Vec<u8>,
        reply: oneshot::Sender<Result<BlockId, CallError>>,
    },
    ReportClockEstimate {
        context_id: ContextId,
        beat: f64,
        tempo_bps: f64,
        epoch_ns: u64,
        source: String,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    ReportMidiPresence {
        device: String,
        present: bool,
        backend: String,
        ports: Vec<(String, String)>,
        epoch_ns: u64,
        /// This sink's name for the machine it runs on — display/provenance
        /// only (`kj midi list` answering *where*). The kernel reaps presence
        /// by the connection, never by this.
        sink_host: String,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    VfsReadAll {
        path: String,
        reply: oneshot::Sender<Result<Vec<u8>, CallError>>,
    },

    // ── Editor (vi) ──────────────────────────────────────────────────────
    EditorKeys {
        session_id: u64,
        keys: String,
        reply: oneshot::Sender<Result<EditorState, CallError>>,
    },

    // ── Tool Execution ───────────────────────────────────────────────────
    ExecuteTool {
        tool: String,
        params: String,
        reply: oneshot::Sender<Result<ToolResult, CallError>>,
    },
    GetToolSchemas {
        reply: oneshot::Sender<Result<Vec<ToolSchema>, CallError>>,
    },
    CallMcpTool {
        tool: String,
        arguments: serde_json::Value,
        reply: oneshot::Sender<Result<McpToolResult, CallError>>,
    },

    // ── MCP Resources ────────────────────────────────────────────────────
    ListMcpResources {
        server: String,
        reply: oneshot::Sender<Result<Vec<McpResource>, CallError>>,
    },

    // ── LLM ──────────────────────────────────────────────────────────────
    Prompt {
        content: String,
        model: Option<String>,
        context_id: ContextId,
        reply: oneshot::Sender<Result<String, CallError>>,
    },
    ConfigureLlm {
        context_id: ContextId,
        provider: String,
        model: String,
        reply: oneshot::Sender<Result<bool, CallError>>,
    },
    GetLlmConfig {
        reply: oneshot::Sender<Result<LlmConfigInfo, CallError>>,
    },
    GetConfig {
        path: String,
        reply: oneshot::Sender<Result<String, CallError>>,
    },
    SetDefaultProvider {
        provider: String,
        reply: oneshot::Sender<Result<bool, CallError>>,
    },
    SetDefaultModel {
        provider: String,
        model: String,
        reply: oneshot::Sender<Result<bool, CallError>>,
    },

    // ── Timeline ─────────────────────────────────────────────────────────
    CherryPickBlock {
        block_id: BlockId,
        target_context: ContextId,
        reply: oneshot::Sender<Result<BlockId, CallError>>,
    },
    GetContextHistory {
        context_id: ContextId,
        limit: u32,
        reply: oneshot::Sender<Result<Vec<VersionSnapshot>, CallError>>,
    },

    // ── Kernel Info ──────────────────────────────────────────────────────
    GetInfo {
        reply: oneshot::Sender<Result<KernelInfo, CallError>>,
    },

    // ── Interrupt ─────────────────────────────────────────────────────────
    InterruptContext {
        context_id: ContextId,
        immediate: bool,
        reply: oneshot::Sender<Result<bool, CallError>>,
    },
    ListPresets {
        reply: oneshot::Sender<Result<Vec<crate::PresetInfo>, CallError>>,
    },

    // ── World-level (handled inline) ─────────────────────────────────────
    Whoami {
        reply: oneshot::Sender<Result<Identity, CallError>>,
    },
    ListKernels {
        reply: oneshot::Sender<Result<Vec<KernelInfo>, CallError>>,
    },

    // ── Join Context (inline — updates actor state) ─────────────────────
    JoinContext {
        context_id: ContextId,
        reply: oneshot::Sender<Result<ContextId, CallError>>,
    },

    // ── Re-subscribe block events (inline — uses live connection) ───────
    /// Re-issue the block-events subscription scoped to the actor's current
    /// context. Recovers a subscription the server may have reaped after a
    /// sustained callback stall, without a full reconnect.
    ResubscribeBlocks {
        reply: oneshot::Sender<Result<(), CallError>>,
    },

    // ── Peers ────────────────────────────────────────────────────────────
    AttachPeer {
        config: PeerConfig,
        invocation_tx: std::sync::mpsc::Sender<PeerInvocation>,
        reply: oneshot::Sender<Result<PeerAttachResult, CallError>>,
    },
    InvokePeer {
        nick: String,
        action: String,
        params: Vec<u8>,
        reply: oneshot::Sender<Result<Vec<u8>, CallError>>,
    },
    ListPeers {
        reply: oneshot::Sender<Result<Vec<PeerInfo>, CallError>>,
    },
}

// ── Client-side peer types ──────────────────────────────────────────────────

/// Configuration for attaching as a peer to the kernel.
#[derive(Debug, Clone, Default)]
pub struct PeerConfig {
    pub nick: String,
    /// Unique-per-process token (a UUID minted once at startup) so two windows
    /// of the same `nick` coexist in the registry. Empty → keyed by nick.
    pub instance: String,
}

/// Result from a successful peer attachment.
#[derive(Debug, Clone)]
pub struct PeerAttachResult {
    pub nick: String,
}

/// An invocation received from the kernel via the PeerCommands callback.
pub struct PeerInvocation {
    pub action: String,
    pub params: Vec<u8>,
    pub reply: oneshot::Sender<Result<Vec<u8>, String>>,
}

impl RpcCommand {
    /// Send `Err(err)` on the command's reply channel without matching all fields.
    fn reply_err(self, err: CallError) {
        match self {
            Self::DriftQueue { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::DriftCancel { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetContextId { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ListContexts { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ListTracks { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::VfsSnapshot { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SubscribeVfsActivity { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::Conclude { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::RenameContext { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::PromoteContext { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::DemoteContext { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SetContextPaused { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SetContextOriginHost { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::AuthorBlock { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::CompleteBlock { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ArchiveContext { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SearchSimilar { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetNeighbors { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetClusters { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::CreateContext { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ResolveContextLabel { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetBlocks { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetBlocksVersioned { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SubscribeContext { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetContextVersion { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::CompactContext { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::Execute { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ShellExecute { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SetBlockExcluded { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::Interrupt { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::Complete { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetCommandHistory { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetContextCwd { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SetContextCwd { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ExecuteKj { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetKjCommandCatalog { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetShellVar { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SetShellVar { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ListShellVars { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SetLastContext { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetClientView { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::EditInput { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetInputState { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SubmitInput { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ClearInput { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::CommitCapture { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ReportClockEstimate { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ReportMidiPresence { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::VfsReadAll { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::EditorKeys { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ExecuteTool { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetToolSchemas { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::CallMcpTool { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ListMcpResources { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::Prompt { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ConfigureLlm { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetLlmConfig { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetConfig { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SetDefaultProvider { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SetDefaultModel { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::CherryPickBlock { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetContextHistory { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetInfo { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::InterruptContext { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ListPresets { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::Whoami { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ListKernels { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::JoinContext { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ResubscribeBlocks { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::AttachPeer { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::InvokePeer { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ListPeers { reply } => { let _ = reply.send(Err(err)); }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Channel envelope (carries caller span)
// ────────────────────────────────────────────────────────────────────────────

/// Wraps an `RpcCommand` with the caller's tracing span so that actor-side
/// dispatch inherits the correct parent context.
struct ChannelCmd {
    command: RpcCommand,
    span: tracing::Span,
}

// ────────────────────────────────────────────────────────────────────────────
// ActorHandle (Send + Sync public API)
// ────────────────────────────────────────────────────────────────────────────

/// Send+Sync handle to an RPC actor running in a LocalSet.
///
/// Each method sends a command via a bounded mpsc channel and awaits the
/// oneshot reply. Backpressure: when 32 commands are queued, callers block
/// on `.send().await` until slots free up.
#[derive(Clone)]
pub struct ActorHandle {
    tx: mpsc::Sender<ChannelCmd>,
    event_tx: broadcast::Sender<ServerEvent>,
    status_tx: broadcast::Sender<ConnectionStatus>,
    /// Level-readable mirror of `status_tx`. The broadcast carries the
    /// transition *stream* (every Idle→Connecting→Connected edge), but a
    /// late subscriber misses edges that already fired and a healthy
    /// Connected actor is silent. This watch always holds the latest value,
    /// so a caller can read "are we connected?" without racing the one-shot
    /// broadcast. See [`Self::current_status`] / [`Self::watch_status`].
    status_watch_rx: watch::Receiver<ConnectionStatus>,
    /// The seat a MIDI-capable client installs its hardware worker into, so
    /// the kernel's `exchange` calls have somewhere to land
    /// (`docs/midi-next.md` "SysEx: the exchange pattern"). Shared with every
    /// block-events forwarder this actor builds — including the ones a
    /// reconnect rebuilds — so installing once survives reconnects.
    midi_exchange: Arc<crate::midi_exchange::MidiExchangeSlot>,
    /// The kernel-wide approval-ledger change stream — a `broadcast` fan-out
    /// (see [`Self::subscribe_ledger_events`] for why).
    ledger_tx: broadcast::Sender<i64>,
}

impl ActorHandle {
    /// Generic send helper — creates a oneshot, sends the command, awaits reply.
    async fn send<T: Send + 'static>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, CallError>>) -> RpcCommand,
    ) -> Result<T, CallError> {
        let (reply, rx) = oneshot::channel();
        let cmd = ChannelCmd {
            command: build(reply),
            span: tracing::Span::current(),
        };
        self.tx.send(cmd).await.map_err(|_| CallError::Shutdown)?;
        rx.await.map_err(|_| CallError::Shutdown)?
    }

    // ── Subscriptions ────────────────────────────────────────────────────

    /// The MIDI exchange seat (`docs/midi-next.md` "SysEx: the exchange
    /// pattern"). A client that owns MIDI hardware installs its worker's
    /// channel here; every other client leaves it empty and the kernel is
    /// told so, loudly, rather than waiting on a sink that was never there.
    pub fn midi_exchange(&self) -> Arc<crate::midi_exchange::MidiExchangeSlot> {
        self.midi_exchange.clone()
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<ServerEvent> {
        self.event_tx.subscribe()
    }

    /// Subscribe to the kernel-wide approval-ledger change stream — each
    /// item is the ledger's generation after a change (`LedgerFlow::Changed`
    /// bridged over `LedgerEvents::onChanged`, coalesced server-side).
    ///
    /// A broadcast: `onChanged` expects no answer, so any number of callers
    /// can subscribe and each receives every notification independently.
    /// Concretely, this is what lets the Bevy app show a ledger-dirty
    /// indicator while an ACP adapter, in the same process or a different
    /// one, separately decides whether to re-render a pending prompt —
    /// neither has to coordinate with or steal from the other.
    ///
    /// Stays valid across reconnects: `connect_handshake` best-effort
    /// re-subscribes on every successful (re)connect and forwards straight
    /// into this same persistent sender, so a kernel restart doesn't
    /// require a caller to notice and re-subscribe.
    pub fn subscribe_ledger_events(&self) -> broadcast::Receiver<i64> {
        self.ledger_tx.subscribe()
    }

    pub fn subscribe_status(&self) -> broadcast::Receiver<ConnectionStatus> {
        self.status_tx.subscribe()
    }

    /// The current connection status as a *level* (the latest value), readable
    /// at any time without racing the one-shot transition broadcast. A caller
    /// that comes up after the actor already reached `Connected` still reads
    /// `Connected` here — unlike [`Self::subscribe_status`], which only
    /// delivers transitions that happen after the subscription.
    ///
    /// Because this mirrors only the *latest* value, rapid back-to-back
    /// transitions can coalesce (e.g. the transient `Closing` before `Cooldown`
    /// may never be observed here). For the full transition stream — e.g. to
    /// drive UI through every state — use [`Self::subscribe_status`].
    pub fn current_status(&self) -> ConnectionStatus {
        self.status_watch_rx.borrow().clone()
    }

    /// A `watch` receiver for connection status. `watch_status().wait_for(..)`
    /// checks the current value *before* awaiting a change, so a level
    /// condition like "reached Connected" cannot be missed by a late
    /// subscriber. Use this (not `subscribe_status`) to gate on a state.
    ///
    /// Latest-value semantics apply (see [`Self::current_status`]): intermediate
    /// transitions can coalesce, so don't use this to count or observe every
    /// edge — use [`Self::subscribe_status`] for the full stream.
    pub fn watch_status(&self) -> watch::Receiver<ConnectionStatus> {
        self.status_watch_rx.clone()
    }

    // ── Drift ────────────────────────────────────────────────────────────

    #[tracing::instrument(skip(self))]
    pub async fn drift_queue(&self) -> Result<Vec<StagedDriftInfo>, CallError> {
        self.send(|reply| RpcCommand::DriftQueue { reply }).await
    }

    #[tracing::instrument(skip(self))]
    pub async fn drift_cancel(&self, staged_id: u64) -> Result<bool, CallError> {
        self.send(|reply| RpcCommand::DriftCancel { staged_id, reply })
            .await
    }

    // ── Context ──────────────────────────────────────────────────────────

    #[tracing::instrument(skip(self))]
    pub async fn get_context_id(&self) -> Result<(ContextId, String), CallError> {
        self.send(|reply| RpcCommand::GetContextId { reply }).await
    }

    #[tracing::instrument(skip(self))]
    pub async fn list_contexts(&self) -> Result<Vec<ContextInfo>, CallError> {
        self.send(|reply| RpcCommand::ListContexts { reply }).await
    }

    /// List every track's live state (docs/tracks.md). Empty when no tracks
    /// exist or the kernel runs without a beat scheduler.
    #[tracing::instrument(skip(self))]
    pub async fn list_tracks(&self) -> Result<Vec<crate::rpc::TrackInfo>, CallError> {
        self.send(|reply| RpcCommand::ListTracks { reply }).await
    }

    /// Recursive VFS snapshot listing (`docs/scenes/vfs.md`'s FSN-world
    /// plumbing) — thin passthrough to [`crate::rpc::RpcClient::vfs_snapshot`],
    /// depth/cap clamped kernel-side regardless of what's asked.
    #[tracing::instrument(skip(self))]
    pub async fn vfs_snapshot(
        &self,
        path: &str,
        depth: u32,
        max_entries: u32,
    ) -> Result<crate::rpc::SnapshotResult, CallError> {
        let path = path.to_string();
        self.send(|reply| RpcCommand::VfsSnapshot { path, depth, max_entries, reply }).await
    }

    /// Start the VFS activity digest push subscription (Lane K, FSN slice-1,
    /// `docs/scenes/vfs.md`). Events surface on [`Self::subscribe_events`] as
    /// [`ServerEvent::VfsActivity`] — same shared stream as blocks/editor,
    /// not a separate channel. Idempotent: a second call while already
    /// subscribed on this connection is a no-op (the actor guards against
    /// duplicate subscribes; see `RpcActor::dispatch`). The subscription is
    /// remembered and best-effort re-issued on every reconnect (heat is
    /// decorative — a failed re-subscribe logs and never forces another
    /// reconnect attempt).
    #[tracing::instrument(skip(self))]
    pub async fn subscribe_vfs_activity(&self, interval_ms: u32) -> Result<(), CallError> {
        self.send(|reply| RpcCommand::SubscribeVfsActivity { interval_ms, reply })
            .await
    }

    /// Conclude a context — the explicit "done" act (sets `concluded`/stamps
    /// `concludedAt` server-side). Idempotent.
    #[tracing::instrument(skip(self))]
    pub async fn conclude(&self, context_id: ContextId) -> Result<(), CallError> {
        self.send(|reply| RpcCommand::Conclude { context_id, reply }).await
    }

    /// Rename a context's human-friendly label.
    #[tracing::instrument(skip(self))]
    pub async fn rename_context(&self, context_id: ContextId, label: &str) -> Result<(), CallError> {
        let label = label.to_string();
        self.send(|reply| RpcCommand::RenameContext { context_id, label, reply }).await
    }

    /// Promote a context into the time-well's ring 0 ("active"). First-write-
    /// wins server-side — re-promoting an already-promoted context is a no-op
    /// success. Fails loud when the active ring is full (10 seats,
    /// `ACTIVE_RING_CAPACITY` kernel-side) — the caller surfaces that error.
    #[tracing::instrument(skip(self))]
    pub async fn promote_context(&self, context_id: ContextId) -> Result<(), CallError> {
        self.send(|reply| RpcCommand::PromoteContext { context_id, reply }).await
    }

    /// Push a context outward one step on the kernel-owned demote ladder:
    /// promoted → automatic placement; automatic → demoted; already demoted →
    /// archived (single context, no subtree, no latch).
    #[tracing::instrument(skip(self))]
    pub async fn demote_context(&self, context_id: ContextId) -> Result<(), CallError> {
        self.send(|reply| RpcCommand::DemoteContext { context_id, reply }).await
    }

    /// Set or clear a context's "suspend activity" flag (`pausedAt`).
    /// Design-only for now — persisted and on the wire, no behavioral gating.
    #[tracing::instrument(skip(self))]
    pub async fn set_context_paused(
        &self,
        context_id: ContextId,
        paused: bool,
    ) -> Result<(), CallError> {
        self.send(|reply| RpcCommand::SetContextPaused { context_id, paused, reply }).await
    }

    /// Set (or clear, on `""`) a context's advisory `origin_host` — the
    /// registering client's own hostname, recorded once at creation. See
    /// `RpcClient::set_context_origin_host`'s doc comment.
    #[tracing::instrument(skip(self))]
    pub async fn set_context_origin_host(
        &self,
        context_id: ContextId,
        origin_host: &str,
    ) -> Result<(), CallError> {
        self.send(|reply| RpcCommand::SetContextOriginHost {
            context_id,
            origin_host: origin_host.to_string(),
            reply,
        })
        .await
    }

    /// Author one block over RPC. See `crate::rpc::AuthorBlock` for the
    /// field meanings and `docs/crdt-position-2026-08.md` for why this
    /// exists.
    pub async fn author_block(
        &self,
        req: crate::rpc::AuthorBlock,
    ) -> Result<BlockId, CallError> {
        self.send(|reply| RpcCommand::AuthorBlock {
            req: Box::new(req),
            reply,
        })
        .await
    }

    /// Complete an already-authored block.
    pub async fn complete_block(
        &self,
        context_id: ContextId,
        block_id: BlockId,
        status: Status,
        is_error: bool,
        exit_code: Option<i32>,
    ) -> Result<(), CallError> {
        self.send(|reply| RpcCommand::CompleteBlock {
            context_id,
            block_id,
            status,
            is_error,
            exit_code,
            reply,
        })
        .await
    }

    /// Archive a single context — the well's single-keystroke archive act.
    /// Unlike the `kj context archive` builtin (latched, recurses into
    /// structural children), this is single-context, unlatched, idempotent.
    #[tracing::instrument(skip(self))]
    pub async fn archive_context(&self, context_id: ContextId) -> Result<(), CallError> {
        self.send(|reply| RpcCommand::ArchiveContext { context_id, reply }).await
    }

    /// Semantic search: contexts similar to a free-text query (top `k`).
    #[tracing::instrument(skip(self, query))]
    pub async fn search_similar(
        &self,
        query: &str,
        k: u32,
    ) -> Result<Vec<SimilarContext>, CallError> {
        let query = query.to_string();
        self.send(|reply| RpcCommand::SearchSimilar { query, k, reply }).await
    }

    /// Contexts semantically similar to a given context (top `k` neighbors).
    #[tracing::instrument(skip(self))]
    pub async fn get_neighbors(
        &self,
        context_id: ContextId,
        k: u32,
    ) -> Result<Vec<SimilarContext>, CallError> {
        self.send(|reply| RpcCommand::GetNeighbors { context_id, k, reply }).await
    }

    /// Semantic clusters of contexts (only clusters with ≥ `min_cluster_size`
    /// members; each kernel-labeled).
    #[tracing::instrument(skip(self))]
    pub async fn get_clusters(
        &self,
        min_cluster_size: u32,
    ) -> Result<Vec<ContextCluster>, CallError> {
        self.send(|reply| RpcCommand::GetClusters { min_cluster_size, reply }).await
    }

    #[tracing::instrument(skip(self))]
    pub async fn create_context(&self, label: &str) -> Result<ContextId, CallError> {
        self.create_context_typed(label, "").await
    }

    /// Create a context with an explicit `context_type` (mode bundle).
    ///
    /// The type selects which `/etc/rc/<context_type>/create/*` scripts run
    /// server-side. Empty `context_type` is treated as `"default"`.
    #[tracing::instrument(skip(self))]
    pub async fn create_context_typed(
        &self,
        label: &str,
        context_type: &str,
    ) -> Result<ContextId, CallError> {
        self.send(|reply| RpcCommand::CreateContext {
            label: label.into(),
            context_type: context_type.into(),
            reply,
        })
        .await
    }

    /// DB-driven label lookup — see `KernelHandle::resolve_context_label`.
    /// `Ok(None)` means no context currently holds this label.
    #[tracing::instrument(skip(self))]
    pub async fn resolve_context_label(
        &self,
        label: &str,
    ) -> Result<Option<crate::rpc::ContextInfo>, CallError> {
        self.send(|reply| RpcCommand::ResolveContextLabel {
            label: label.into(),
            reply,
        })
        .await
    }

    // ── Blocks / Change Feed ─────────────────────────────────────────────

    #[tracing::instrument(skip(self, query))]
    pub async fn get_blocks_query(
        &self,
        context_id: ContextId,
        query: BlockQuery,
    ) -> Result<Vec<BlockSnapshot>, CallError> {
        self.send(|reply| RpcCommand::GetBlocks {
            context_id,
            query,
            reply,
        })
        .await
    }

    /// Follow one context's change feed (docs/change-feed.md).
    ///
    /// The returned receiver outlives reconnects: the actor re-subscribes on
    /// every new connection and sends [`FeedEvent::Resubscribed`] first, which
    /// is the consumer's cue to refetch a snapshot with
    /// [`Self::get_blocks_versioned`] and hydrate a fresh
    /// [`ContextMirror`](crate::ContextMirror). Subscribing while disconnected
    /// is fine — the feed starts at the next connection.
    ///
    /// Subscribing twice to one context replaces the first receiver.
    #[tracing::instrument(skip(self))]
    pub async fn subscribe_context(
        &self,
        context_id: ContextId,
    ) -> Result<mpsc::Receiver<crate::context_feed::FeedEvent>, CallError> {
        let (sender, receiver) = mpsc::channel(CONTEXT_FEED_QUEUE);
        self.send(|reply| RpcCommand::SubscribeContext {
            context_id,
            sender,
            reply,
        })
        .await?;
        Ok(receiver)
    }

    /// Query blocks and keep the version they were read at, atomically.
    ///
    /// A caller joining a snapshot to a live subscription needs this one:
    /// blocks and version come from a single kernel guard, so no mutation can
    /// slip between them (docs/change-feed.md rules 21-26).
    #[tracing::instrument(skip(self, query))]
    pub async fn get_blocks_versioned(
        &self,
        context_id: ContextId,
        query: BlockQuery,
    ) -> Result<(Vec<BlockSnapshot>, u64), CallError> {
        self.send(|reply| RpcCommand::GetBlocksVersioned {
            context_id,
            query,
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_block(
        &self,
        context_id: ContextId,
        block_id: BlockId,
    ) -> Result<Option<BlockSnapshot>, CallError> {
        let mut blocks = self
            .get_blocks_query(context_id, BlockQuery::ByIds(vec![block_id]))
            .await?;
        Ok(blocks.pop())
    }

    #[tracing::instrument(skip(self, block_ids))]
    pub async fn get_blocks(
        &self,
        context_id: ContextId,
        block_ids: Vec<BlockId>,
    ) -> Result<Vec<BlockSnapshot>, CallError> {
        self.get_blocks_query(context_id, BlockQuery::ByIds(block_ids))
            .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_all_blocks(
        &self,
        context_id: ContextId,
    ) -> Result<Vec<BlockSnapshot>, CallError> {
        self.get_blocks_query(context_id, BlockQuery::All).await
    }

    #[tracing::instrument(skip(self, filter))]
    pub async fn query_blocks(
        &self,
        context_id: ContextId,
        filter: BlockFilter,
    ) -> Result<Vec<BlockSnapshot>, CallError> {
        self.get_blocks_query(context_id, BlockQuery::ByFilter(filter))
            .await
    }

    /// Projected revision of a context's block document, no oplog bytes.
    #[tracing::instrument(skip(self))]
    pub async fn get_context_version(&self, context_id: ContextId) -> Result<u64, CallError> {
        self.send(|reply| RpcCommand::GetContextVersion { context_id, reply })
            .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn compact_context(&self, context_id: ContextId) -> Result<(u64, u64), CallError> {
        self.send(|reply| RpcCommand::CompactContext { context_id, reply })
            .await
    }

    // ── Shell / Execution ────────────────────────────────────────────────

    #[tracing::instrument(skip(self, code))]
    pub async fn execute(&self, code: &str) -> Result<u64, CallError> {
        self.send(|reply| RpcCommand::Execute {
            code: code.into(),
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self, code))]
    pub async fn shell_execute(
        &self,
        code: &str,
        context_id: ContextId,
        user_initiated: bool,
    ) -> Result<BlockId, CallError> {
        self.send(|reply| RpcCommand::ShellExecute {
            code: code.into(),
            context_id,
            user_initiated,
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn set_block_excluded(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        excluded: bool,
    ) -> Result<u64, CallError> {
        let bid = *block_id;
        self.send(|reply| RpcCommand::SetBlockExcluded {
            context_id,
            block_id: bid,
            excluded,
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn interrupt(&self, exec_id: u64) -> Result<(), CallError> {
        self.send(|reply| RpcCommand::Interrupt { exec_id, reply })
            .await
    }

    #[tracing::instrument(skip(self, partial))]
    pub async fn complete(
        &self,
        partial: &str,
        cursor: u32,
    ) -> Result<Vec<Completion>, CallError> {
        self.send(|reply| RpcCommand::Complete {
            partial: partial.into(),
            cursor,
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_command_history(&self, limit: u32) -> Result<Vec<HistoryEntry>, CallError> {
        self.send(|reply| RpcCommand::GetCommandHistory { limit, reply })
            .await
    }

    // ── Shell Variables ─────────────────────────────────────────────────

    #[tracing::instrument(skip(self))]
    pub async fn get_context_cwd(
        &self,
        context_id: ContextId,
    ) -> Result<Option<String>, CallError> {
        self.send(|reply| RpcCommand::GetContextCwd { context_id, reply }).await
    }

    #[tracing::instrument(skip(self, path))]
    pub async fn set_context_cwd(
        &self,
        context_id: ContextId,
        path: &str,
    ) -> Result<(), CallError> {
        self.send(|reply| RpcCommand::SetContextCwd {
            context_id,
            path: path.into(),
            reply,
        }).await
    }

    #[tracing::instrument(skip(self, argv))]
    pub async fn execute_kj(
        &self,
        context_id: ContextId,
        argv: Vec<String>,
    ) -> Result<crate::rpc::KjExecutionResult, CallError> {
        self.send(|reply| RpcCommand::ExecuteKj { context_id, argv, reply }).await
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_kj_command_catalog(
        &self,
        context_id: ContextId,
    ) -> Result<Vec<crate::rpc::KjCommandInfo>, CallError> {
        self.send(|reply| RpcCommand::GetKjCommandCatalog { context_id, reply }).await
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_shell_var(
        &self,
        name: &str,
    ) -> Result<(Option<ShellValue>, bool), CallError> {
        self.send(|reply| RpcCommand::GetShellVar {
            name: name.into(),
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self, value))]
    pub async fn set_shell_var(&self, name: &str, value: ShellValue) -> Result<(), CallError> {
        self.send(|reply| RpcCommand::SetShellVar {
            name: name.into(),
            value,
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn list_shell_vars(&self) -> Result<Vec<(String, ShellValue)>, CallError> {
        self.send(|reply| RpcCommand::ListShellVars { reply }).await
    }

    // ── Per-client durable view state (docs/shared-state.md "Retiring KV") ──

    #[tracing::instrument(skip(self))]
    pub async fn set_last_context(
        &self,
        client_id: &str,
        context_id: ContextId,
    ) -> Result<(), CallError> {
        self.send(|reply| RpcCommand::SetLastContext {
            client_id: client_id.into(),
            context_id,
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_client_view(&self, client_id: &str) -> Result<Option<ContextId>, CallError> {
        self.send(|reply| RpcCommand::GetClientView {
            client_id: client_id.into(),
            reply,
        })
        .await
    }

    // ── Input Document ──────────────────────────────────────────────────

    #[tracing::instrument(skip(self, insert))]
    pub async fn edit_input(
        &self,
        context_id: ContextId,
        pos: u64,
        insert: &str,
        delete: u64,
    ) -> Result<u64, CallError> {
        self.send(|reply| RpcCommand::EditInput {
            context_id,
            pos,
            insert: insert.into(),
            delete,
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_input_state(&self, context_id: ContextId) -> Result<InputState, CallError> {
        self.send(|reply| RpcCommand::GetInputState { context_id, reply })
            .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn submit_input(
        &self,
        context_id: ContextId,
        is_shell: bool,
    ) -> Result<SubmitResult, CallError> {
        self.send(|reply| RpcCommand::SubmitInput {
            context_id,
            is_shell,
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn clear_input(&self, context_id: ContextId) -> Result<(), CallError> {
        self.send(|reply| RpcCommand::ClearInput { context_id, reply })
            .await
    }

    /// Commit a captured-MIDI batch onto the capture context's track
    /// (`docs/midi.md` M2). Returns the score-context block id the kernel
    /// landed.
    #[tracing::instrument(skip(self, payload))]
    pub async fn commit_capture(
        &self,
        context_id: ContextId,
        mime: impl Into<String> + std::fmt::Debug,
        payload: Vec<u8>,
    ) -> Result<BlockId, CallError> {
        let mime = mime.into();
        self.send(|reply| RpcCommand::CommitCapture {
            context_id,
            mime,
            payload,
            reply,
        })
        .await
    }

    /// Report one profile-matched MIDI device's presence (`docs/midi-next.md`
    /// "Presence is sink-fed"). Kernel-global, not context-scoped: presence is
    /// a fact about the rig. `present = false` (unplug) is a report we owe the
    /// kernel, not a silence — stale presence that lies is worse than none.
    #[tracing::instrument(skip(self, ports))]
    pub async fn report_midi_presence(
        &self,
        device: impl Into<String> + std::fmt::Debug,
        present: bool,
        backend: impl Into<String> + std::fmt::Debug,
        ports: Vec<(String, String)>,
        epoch_ns: u64,
        sink_host: impl Into<String> + std::fmt::Debug,
    ) -> Result<(), CallError> {
        let device = device.into();
        let backend = backend.into();
        let sink_host = sink_host.into();
        self.send(|reply| RpcCommand::ReportMidiPresence {
            device,
            present,
            backend,
            ports,
            epoch_ns,
            sink_host,
            reply,
        })
        .await
    }

    /// Read a whole VFS file through the kernel's existing `Vfs` capability.
    /// The sink's device-profile fetch rides this (`/etc/midi/devices/<name>`).
    #[tracing::instrument(skip(self))]
    pub async fn vfs_read_all(&self, path: impl Into<String> + std::fmt::Debug) -> Result<Vec<u8>, CallError> {
        let path = path.into();
        self.send(|reply| RpcCommand::VfsReadAll { path, reply }).await
    }

    /// Ship one observer clock reference (`docs/midi.md` M3, ~2 Hz stream).
    #[tracing::instrument(skip(self))]
    pub async fn report_clock_estimate(
        &self,
        context_id: ContextId,
        beat: f64,
        tempo_bps: f64,
        epoch_ns: u64,
        source: impl Into<String> + std::fmt::Debug,
    ) -> Result<(), CallError> {
        let source = source.into();
        self.send(|reply| RpcCommand::ReportClockEstimate {
            context_id,
            beat,
            tempo_bps,
            epoch_ns,
            source,
            reply,
        })
        .await
    }

    // ── Editor (vi) ──────────────────────────────────────────────────────

    /// Feed a vi key sequence (kernel notation: `"i"`, `"<Esc>"`, `"dw"`) to an
    /// open editor session and return the resulting [`EditorState`]. The push
    /// subscription also echoes this state, so the renderer normally updates
    /// from there; callers fire-and-forget.
    #[tracing::instrument(skip(self, keys))]
    pub async fn editor_keys(
        &self,
        session_id: u64,
        keys: &str,
    ) -> Result<EditorState, CallError> {
        self.send(|reply| RpcCommand::EditorKeys {
            session_id,
            keys: keys.into(),
            reply,
        })
        .await
    }

    // ── Tool Execution ───────────────────────────────────────────────────

    #[tracing::instrument(skip(self, params))]
    pub async fn execute_tool(&self, tool: &str, params: &str) -> Result<ToolResult, CallError> {
        self.send(|reply| RpcCommand::ExecuteTool {
            tool: tool.into(),
            params: params.into(),
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_tool_schemas(&self) -> Result<Vec<ToolSchema>, CallError> {
        self.send(|reply| RpcCommand::GetToolSchemas { reply })
            .await
    }

    #[tracing::instrument(skip(self, arguments))]
    pub async fn call_mcp_tool(
        &self,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> Result<McpToolResult, CallError> {
        self.send(|reply| RpcCommand::CallMcpTool {
            tool: tool.into(),
            arguments: arguments.clone(),
            reply,
        })
        .await
    }

    // ── MCP Resources ────────────────────────────────────────────────────

    #[tracing::instrument(skip(self))]
    pub async fn list_mcp_resources(&self, server: &str) -> Result<Vec<McpResource>, CallError> {
        self.send(|reply| RpcCommand::ListMcpResources {
            server: server.into(),
            reply,
        })
        .await
    }

    // ── LLM ──────────────────────────────────────────────────────────────

    #[tracing::instrument(skip(self, content))]
    pub async fn prompt(
        &self,
        content: &str,
        model: Option<&str>,
        context_id: ContextId,
    ) -> Result<String, CallError> {
        self.send(|reply| RpcCommand::Prompt {
            content: content.into(),
            model: model.map(String::from),
            context_id,
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn set_context_model(
        &self,
        context_id: ContextId,
        provider: &str,
        model: &str,
    ) -> Result<bool, CallError> {
        self.send(|reply| RpcCommand::ConfigureLlm {
            context_id,
            provider: provider.into(),
            model: model.into(),
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_llm_config(&self) -> Result<LlmConfigInfo, CallError> {
        self.send(|reply| RpcCommand::GetLlmConfig { reply }).await
    }

    /// Read a kernel-owned config file's content (e.g. `theme.toml`) over RPC.
    #[tracing::instrument(skip(self))]
    pub async fn get_config(&self, path: String) -> Result<String, CallError> {
        self.send(|reply| RpcCommand::GetConfig { path, reply }).await
    }

    #[tracing::instrument(skip(self))]
    pub async fn set_default_provider(&self, provider: &str) -> Result<bool, CallError> {
        self.send(|reply| RpcCommand::SetDefaultProvider {
            provider: provider.into(),
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn set_default_model(&self, provider: &str, model: &str) -> Result<bool, CallError> {
        self.send(|reply| RpcCommand::SetDefaultModel {
            provider: provider.into(),
            model: model.into(),
            reply,
        })
        .await
    }

    // ── Timeline ─────────────────────────────────────────────────────────

    #[tracing::instrument(skip(self))]
    pub async fn cherry_pick_block(
        &self,
        block_id: &BlockId,
        target_context: ContextId,
    ) -> Result<BlockId, CallError> {
        self.send(|reply| RpcCommand::CherryPickBlock {
            block_id: *block_id,
            target_context,
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_context_history(
        &self,
        context_id: ContextId,
        limit: u32,
    ) -> Result<Vec<VersionSnapshot>, CallError> {
        self.send(|reply| RpcCommand::GetContextHistory {
            context_id,
            limit,
            reply,
        })
        .await
    }

    // ── Kernel Info ──────────────────────────────────────────────────────

    #[tracing::instrument(skip(self))]
    pub async fn get_info(&self) -> Result<KernelInfo, CallError> {
        self.send(|reply| RpcCommand::GetInfo { reply }).await
    }

    // ── Interrupt ───────────────────────────────────────────────────────

    #[tracing::instrument(skip(self))]
    pub async fn interrupt_context(
        &self,
        context_id: ContextId,
        immediate: bool,
    ) -> Result<bool, CallError> {
        self.send(|reply| RpcCommand::InterruptContext {
            context_id,
            immediate,
            reply,
        })
        .await
    }

    pub async fn list_presets(&self) -> Result<Vec<crate::PresetInfo>, CallError> {
        self.send(|reply| RpcCommand::ListPresets { reply }).await
    }

    // ── Join Context ─────────────────────────────────────────────────────

    /// Join an existing context. Updates the actor's internal context so
    /// reconnects re-join the same context automatically.
    ///
    /// Note: the `instance` is fixed at actor construction; this method
    /// does NOT accept an `instance` argument anymore.
    #[tracing::instrument(skip(self))]
    pub async fn join_context(
        &self,
        context_id: ContextId,
    ) -> Result<ContextId, CallError> {
        self.send(|reply| RpcCommand::JoinContext {
            context_id,
            reply,
        })
        .await
    }

    /// Re-issue the block-events subscription, scoped to the actor's current
    /// context. Use to recover delivery the server may have reaped after a
    /// sustained callback stall (e.g. after an MCP shell call times out)
    /// without forcing a full reconnect. Best-effort: returns once the actor
    /// has dispatched the re-subscribe.
    #[tracing::instrument(skip(self))]
    pub async fn resubscribe_blocks(&self) -> Result<(), CallError> {
        self.send(|reply| RpcCommand::ResubscribeBlocks { reply })
            .await
    }

    // ── World-level ──────────────────────────────────────────────────────

    #[tracing::instrument(skip(self))]
    pub async fn whoami(&self) -> Result<Identity, CallError> {
        self.send(|reply| RpcCommand::Whoami { reply }).await
    }

    #[tracing::instrument(skip(self))]
    pub async fn list_kernels(&self) -> Result<Vec<KernelInfo>, CallError> {
        self.send(|reply| RpcCommand::ListKernels { reply }).await
    }

    // ── Peers ────────────────────────────────────────────────────────────

    #[tracing::instrument(skip(self, config, invocation_tx))]
    pub async fn attach_peer(
        &self,
        config: PeerConfig,
        invocation_tx: std::sync::mpsc::Sender<PeerInvocation>,
    ) -> Result<PeerAttachResult, CallError> {
        self.send(|reply| RpcCommand::AttachPeer {
            config,
            invocation_tx,
            reply,
        })
        .await
    }

    #[tracing::instrument(skip(self, params))]
    pub async fn invoke_peer(
        &self,
        nick: &str,
        action: &str,
        params: &[u8],
    ) -> Result<Vec<u8>, CallError> {
        self.send(|reply| RpcCommand::InvokePeer {
            nick: nick.to_string(),
            action: action.to_string(),
            params: params.to_vec(),
            reply,
        })
        .await
    }

    /// List all peers currently attached to the kernel.
    #[tracing::instrument(skip(self))]
    pub async fn list_peers(&self) -> Result<Vec<PeerInfo>, CallError> {
        self.send(|reply| RpcCommand::ListPeers { reply }).await
    }
}

// ────────────────────────────────────────────────────────────────────────────
// RpcActor (internal, !Send, runs in spawn_local)
// ────────────────────────────────────────────────────────────────────────────

/// Classify a Cap'n Proto error string as "the pipe is broken" vs "the call
/// failed but the pipe is fine." Used to decide whether an RPC error should
/// trigger a Closing transition.
fn is_disconnect_error(msg: &str) -> bool {
    // capnp::Error formats as "<kind>: <reason>". The Disconnected kind is
    // what we care about; "Peer disconnected" is the canonical wording.
    msg.contains("Disconnected") || msg.contains("disconnected")
}

/// Run a single RPC call against an EXPLICIT deadline, mapping the outcome
/// into `CallError`. On disconnect-class errors, signals `close_tx` so the
/// actor can transition to Closing.
///
/// [`run_rpc_call`] is the thin, common-case wrapper over this that supplies
/// the global [`RPC_CALL_TIMEOUT`]. This function exists so a command whose
/// RPC may reach a longer logical wait — today, a gated `shell_write` call
/// blocking on `kj::gate::run_gate` — can override the deadline without
/// touching every other command's behavior. See [`dispatch_deadline!`] and
/// `kaijutsu_types::timeout::gate`.
async fn run_rpc_call_with_deadline<T, F, E>(
    fut: F,
    close_tx: &mpsc::Sender<CloseCause>,
    deadline: Duration,
) -> Result<T, CallError>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    match tokio::time::timeout(deadline, fut).await {
        Ok(Ok(val)) => Ok(val),
        Ok(Err(e)) => {
            let msg = e.to_string();
            if is_disconnect_error(&msg) {
                // Coalesce: first close wins; subsequent in-flight failures
                // discover the actor is already Closing and just log.
                let _ = close_tx.try_send(CloseCause::RpcError(msg.clone()));
            }
            Err(CallError::Rpc(msg))
        }
        Err(_) => Err(CallError::Timeout(deadline)),
    }
}

/// Run a single RPC call with the global per-call deadline ([`RPC_CALL_TIMEOUT`]),
/// mapping the outcome into `CallError`. On disconnect-class errors, signals
/// `close_tx` so the actor can transition to Closing.
async fn run_rpc_call<T, F, E>(
    fut: F,
    close_tx: &mpsc::Sender<CloseCause>,
) -> Result<T, CallError>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    run_rpc_call_with_deadline(fut, close_tx, RPC_CALL_TIMEOUT).await
}

/// Dispatch macro that invokes `run_rpc_call` and forwards the result to the
/// command's oneshot reply.
macro_rules! dispatch {
    ($kernel:ident, $reply:ident, $close_tx:ident, $k:ident, $call:expr) => {{
        let $k = &$kernel;
        let result = run_rpc_call($call, &$close_tx).await;
        let _ = $reply.send(result);
    }};
}

/// Sibling of [`dispatch!`] for commands whose call may reach a gated tool —
/// `kj::gate::run_gate` blocking on a human answering from another surface.
/// The global [`RPC_CALL_TIMEOUT`] (request tier, 30s) is far shorter than
/// that wait, so a command on this path uses an explicit, longer deadline
/// instead — `kaijutsu_types::timeout::gate::CLIENT_CALL` is the one this
/// crate hands to the gate-capable commands today. Every OTHER command keeps
/// going through `dispatch!` unchanged: a genuinely wedged RPC must still
/// report back in `RPC_CALL_TIMEOUT`, not silently wait longer everywhere.
macro_rules! dispatch_deadline {
    ($kernel:ident, $reply:ident, $close_tx:ident, $k:ident, $deadline:expr, $call:expr) => {{
        let $k = &$kernel;
        let result = run_rpc_call_with_deadline($call, &$close_tx, $deadline).await;
        let _ = $reply.send(result);
    }};
}

/// The actor that holds !Send Cap'n Proto state and runs the FSM.
struct RpcActor {
    // ── configuration ──
    config: SshConfig,
    /// Stable per-actor UUID used for subscribe dedupe and `join_context`.
    /// Set once at construction; the server keys subscriptions on
    /// `(principal, instance)`.
    instance: String,

    // ── state (updated only inside `run`) ──
    state: ActorState,
    /// Server-bound kernel ID from the most recent successful handshake.
    /// `None` until the first Connected transition; mismatch on subsequent
    /// pings triggers a hard reconnect (kernel restart detected).
    bound_kernel_id: Option<KernelId>,
    /// Context the actor will re-join on every reconnect. Set by the
    /// `JoinContext` command and persisted across reconnects.
    context_id: Option<ContextId>,
    /// When true, block-event subscriptions are scoped to `context_id` (the
    /// single context this client cares about) instead of kernel-wide. Set by
    /// single-context clients like the MCP server, whose single-threaded RPC
    /// LocalSet is starved by foreign-context event volume. The multi-context
    /// app leaves this false — it routes every context's block events by
    /// `context_id` into a per-context `DocumentCache`, so it genuinely needs
    /// kernel-wide delivery.
    scope_blocks_to_context: bool,
    /// Context returned by the most recent `join_context`.
    joined_context_id: Option<ContextId>,
    /// Peer registration the actor re-establishes on every reconnect. Set by
    /// the `AttachPeer` command and persisted, mirroring `context_id` — the
    /// kernel's `PeerRegistry` resets on restart, so without this the app
    /// becomes uninvokable after a kernel cycle until it respawns
    /// (tech_debt_peer_reattach_on_reconnect). The `Sender` is cheap to clone
    /// and the capnp callback is rebuilt from it on each attach.
    peer_registration: Option<(PeerConfig, std::sync::mpsc::Sender<PeerInvocation>)>,
    /// The remembered peer intent changed after the current handshake took
    /// its reconnect snapshot. Replayed once on the next Connected edge.
    peer_attach_pending: bool,
    /// Requested tick interval for the VFS activity digest subscription
    /// (Lane K, FSN slice-1) — `None` until the first
    /// `SubscribeVfsActivity` command. Persisted like `peer_registration` so
    /// the actor best-effort re-subscribes on every reconnect (see
    /// `connect_handshake`); also doubles as the "already subscribed" guard
    /// so a second `SubscribeVfsActivity` call on a live connection is a
    /// no-op rather than stacking a duplicate bridge task server-side.
    vfs_activity_interval_ms: Option<u32>,
    /// Shared with `ActorHandle` and handed to every block-events forwarder
    /// this actor builds (`docs/midi-next.md` "SysEx: the exchange pattern").
    /// Rebuilding the forwarder on reconnect therefore never loses the
    /// installed sink.
    midi_exchange: Arc<crate::midi_exchange::MidiExchangeSlot>,
    /// Persistent sender behind `ActorHandle::subscribe_ledger_events`.
    /// `connect_handshake` builds a fresh `LedgerEventsForwarder` around a
    /// clone of this on every (re)connect — a `broadcast::Sender` clone can
    /// be handed straight to the forwarder (the same shape `event_tx` uses
    /// for `EditorEventsForwarder` et al.), no intermediate per-connect
    /// channel + forwarding task needed.
    ledger_tx: broadcast::Sender<i64>,

    /// Context change feeds this client wants, by context
    /// (docs/change-feed.md). The actor keeps the consumer's end of each feed
    /// so it can build a fresh observer and re-subscribe after a reconnect —
    /// the consumer's receiver survives, the wire capability does not.
    ///
    /// Intent, not connection state: a subscribe that arrives while
    /// disconnected is remembered and issued on the next Connected edge, the
    /// same way peer attachment is.
    context_feeds: HashMap<ContextId, mpsc::Sender<crate::context_feed::FeedEvent>>,

    /// Owned during `Connected`. Replaced atomically on successful handshake.
    connection: Option<ConnectionState>,
    /// Spawned during `Connected` to issue periodic pings; aborted on Closing.
    ping_task: Option<JoinHandle<()>>,
    /// Handshake task spawned during `Connecting`; the actor selects on it.
    connecting_task: Option<JoinHandle<ConnectOutcome>>,

    // ── signaling ──
    /// First-write-wins close signal. Capacity 1; senders use `try_send`.
    close_tx: mpsc::Sender<CloseCause>,
    close_rx: mpsc::Receiver<CloseCause>,
    /// Internal messages from spawned child tasks (e.g., join_context result).
    /// Unbounded so a slow loop doesn't block the spawned task.
    internal_tx: mpsc::UnboundedSender<InternalMsg>,
    internal_rx: mpsc::UnboundedReceiver<InternalMsg>,
    /// Inbound commands from `ActorHandle`.
    rx: mpsc::Receiver<ChannelCmd>,
    /// Outbound: server events.
    event_tx: broadcast::Sender<ServerEvent>,
    /// Outbound: connection status (transition stream).
    status_tx: broadcast::Sender<ConnectionStatus>,
    /// Outbound: connection status (latest-value mirror). Sent alongside
    /// `status_tx` so observers can read the current level without racing the
    /// broadcast's one-shot edges.
    status_watch_tx: watch::Sender<ConnectionStatus>,
}

impl RpcActor {
    /// Snapshot peer intent for a new handshake. Any later declaration sets
    /// `peer_attach_pending` again and is replayed on the Connected edge.
    fn peer_registration_for_handshake(
        &mut self,
    ) -> Option<(PeerConfig, std::sync::mpsc::Sender<PeerInvocation>)> {
        self.peer_attach_pending = false;
        self.peer_registration.clone()
    }

    /// Take a declaration that arrived after the handshake snapshot. Taking
    /// makes replay edge-triggered: one declaration produces one attach.
    fn take_pending_peer_registration(
        &mut self,
    ) -> Option<(PeerConfig, std::sync::mpsc::Sender<PeerInvocation>)> {
        if !self.peer_attach_pending {
            return None;
        }
        self.peer_attach_pending = false;
        self.peer_registration.clone()
    }

    // One argument per independent piece of actor construction state
    // (connection config, session identity, and every channel endpoint the
    // run loop owns) — the actor's whole raison d'être is holding these
    // together, so a params struct would just be this list with a name.
    #[allow(clippy::too_many_arguments)]
    fn new(
        config: SshConfig,
        context_id: Option<ContextId>,
        instance: String,
        scope_blocks_to_context: bool,
        rx: mpsc::Receiver<ChannelCmd>,
        event_tx: broadcast::Sender<ServerEvent>,
        status_tx: broadcast::Sender<ConnectionStatus>,
        status_watch_tx: watch::Sender<ConnectionStatus>,
        midi_exchange: Arc<crate::midi_exchange::MidiExchangeSlot>,
        ledger_tx: broadcast::Sender<i64>,
    ) -> Self {
        let (close_tx, close_rx) = mpsc::channel(1);
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        Self {
            config,
            instance,
            state: ActorState::Idle,
            bound_kernel_id: None,
            context_id,
            scope_blocks_to_context,
            joined_context_id: None,
            peer_registration: None,
            peer_attach_pending: false,
            vfs_activity_interval_ms: None,
            midi_exchange,
            ledger_tx,
            context_feeds: HashMap::new(),
            connection: None,
            ping_task: None,
            connecting_task: None,
            close_tx,
            close_rx,
            internal_tx,
            internal_rx,
            rx,
            event_tx,
            status_tx,
            status_watch_tx,
        }
    }

    /// Broadcast a `ConnectionStatus` matching the current state.
    fn broadcast_state(&self) {
        let status = match &self.state {
            ActorState::Idle => ConnectionStatus::Idle,
            ActorState::Connecting { attempt, .. } => {
                ConnectionStatus::Connecting { attempt: *attempt }
            }
            ActorState::Connected { since } => ConnectionStatus::Connected {
                kernel_id: self.bound_kernel_id.expect("bound_kernel_id set on Connected"),
                context_id: self.joined_context_id,
                since_ms: since.elapsed().as_millis() as u64,
            },
            ActorState::Closing { cause, .. } => ConnectionStatus::Closing {
                cause: cause.to_error_string(),
            },
            ActorState::Cooldown {
                next_attempt,
                until,
                last_error,
            } => {
                let until_ms = system_now_ms().saturating_add(
                    until.saturating_duration_since(Instant::now()).as_millis() as u64,
                );
                ConnectionStatus::Cooldown {
                    next_attempt: *next_attempt,
                    until_ms,
                    last_error: last_error.clone(),
                }
            }
            ActorState::Terminal { reason } => ConnectionStatus::Terminal {
                reason: reason.clone(),
            },
        };
        // Latest-value mirror first so a watcher woken by the broadcast (or a
        // reader racing this call) can't observe a stale level.
        let _ = self.status_watch_tx.send(status.clone());
        let _ = self.status_tx.send(status);
    }

    /// Transition to `Connecting` and spawn the handshake task.
    fn start_connecting(&mut self, attempt: u32) {
        log::info!(
            "Actor connecting to {}:{} (attempt {}, instance={})",
            self.config.host, self.config.port, attempt, self.instance
        );
        self.state = ActorState::Connecting {
            attempt,
            started_at: Instant::now(),
        };
        let peer_registration = self.peer_registration_for_handshake();
        // This handshake now owns the replay for the snapshot above. An
        // AttachPeer arriving while it runs sets this back to true, and the
        // Connected transition replays that newer intent.
        let task = spawn_handshake(
            self.config.clone(),
            self.context_id,
            self.instance.clone(),
            self.scope_blocks_to_context,
            self.event_tx.clone(),
            peer_registration,
            self.vfs_activity_interval_ms,
            self.midi_exchange.clone(),
            self.ledger_tx.clone(),
        );
        self.connecting_task = Some(task);
        self.broadcast_state();
    }

    /// Transition to `Connected` with a freshly-built connection.
    fn enter_connected(&mut self, built: BuiltConnection) {
        // Cancel any leftover handshake task (it succeeded — its task handle
        // is what produced `built` — but explicitly drop the handle to free
        // the slot).
        self.connecting_task = None;

        // A reconnect (not the first connect) means the block stream we just
        // re-subscribed missed everything the kernel published during the
        // outage. `bound_kernel_id` is `Some` iff we've connected before — it's
        // set on the first connect and never cleared on a drop — so it's the
        // exact reconnect signal, no extra bookkeeping. We emit `Reconnected`
        // below, once the new connection is fully in place.
        let is_reconnect = self.bound_kernel_id.is_some();

        self.bound_kernel_id = Some(built.kernel_id);
        self.joined_context_id = built.joined_context;
        self.connection = Some(ConnectionState {
            client: built.client,
            kernel: built.kernel.clone(),
        });
        self.state = ActorState::Connected {
            since: Instant::now(),
        };

        // Context feeds are intent, like peer attachment: the observer
        // capability died with the old connection, but the consumer's receiver
        // did not. Re-issue every feed on the new connection and tell each
        // consumer to refetch — nothing published during the outage is on the
        // feed, and a client that kept applying deltas across the gap would be
        // quietly missing whatever happened while it was away.
        self.resubscribe_context_feeds();

        // A one-shot caller may have declared peer intent while this
        // connection was still being built. Its request correctly received
        // NotReady, but the durable intent must not disappear with that
        // response. The in-flight handshake could not see it, so replay it
        // exactly once now.
        if let Some((config, invocation_tx)) = self.take_pending_peer_registration() {
            let kernel = built.kernel.clone();
            tokio::task::spawn_local(async move {
                match tokio::time::timeout(
                    RPC_CALL_TIMEOUT,
                    kernel.attach_peer(&config, invocation_tx),
                )
                .await
                {
                    Ok(Ok(_)) => log::info!(
                        "Attached peer '{}' after connect-time declaration",
                        config.nick
                    ),
                    Ok(Err(e)) => log::warn!("peer attach replay failed (non-fatal): {e}"),
                    Err(_) => log::warn!("peer attach replay timed out (non-fatal)"),
                }
            });
        }

        // Spawn the liveness pinger. It runs until aborted on Closing.
        let close_tx = self.close_tx.clone();
        let expected_kernel_id = built.kernel_id;
        let kernel = built.kernel;
        self.ping_task = Some(tokio::task::spawn_local(async move {
            run_ping_loop(kernel, expected_kernel_id, close_tx).await;
        }));

        log::info!(
            "Actor connected: kernel_id={} context={:?}",
            built.kernel_id, self.joined_context_id,
        );

        // Tell renderers a reconnect happened so they re-sync the view the
        // re-subscribed stream can't backfill. Best-effort: no subscribers
        // (e.g. a headless client) is fine.
        //
        // This send carries no state. `subscribe_context`'s change feed
        // already re-subscribes on every new connection and sends
        // `FeedEvent::Resubscribed` first (see its doc comment), which is the
        // consumer's cue to throw away its stale `ContextMirror` and refetch a
        // snapshot with `get_blocks_versioned`. `document_store.rs` already
        // implements that path.
        if is_reconnect {
            let _ = self.event_tx.send(ServerEvent::Reconnected);
        }

        self.broadcast_state();
    }

    /// Transition to `Closing` from any state where a connection might be live.
    fn start_closing(&mut self, cause: CloseCause) {
        log::warn!("Actor closing connection: {}", cause.to_error_string());
        // Carry the attempt count from the state we're leaving so the next
        // backoff climbs instead of resetting. `Connected` carries 0 (a fresh
        // drop → next reconnect is attempt 1); a mid-handshake close keeps the
        // in-flight attempt; a cooldown close keeps the pending one.
        let attempt = match &self.state {
            ActorState::Connecting { attempt, .. } => *attempt,
            ActorState::Cooldown { next_attempt, .. } => *next_attempt,
            _ => 0,
        };
        self.state = ActorState::Closing {
            cause: cause.clone(),
            attempt,
        };
        // Drop the live connection (this aborts the RpcSystem via
        // RpcSystemGuard and closes the SSH channels).
        self.connection = None;
        // Abort the ping task; if it was about to fire a duplicate close,
        // that signal is now redundant.
        if let Some(task) = self.ping_task.take() {
            task.abort();
        }
        // If a stray handshake task was still alive (e.g., we got a close
        // while still Connecting), abort it.
        if let Some(task) = self.connecting_task.take() {
            task.abort();
        }
        self.broadcast_state();
    }

    /// Transition out of `Closing` to either `Cooldown` or `Terminal`.
    fn finish_closing(&mut self) {
        let ActorState::Closing { cause, attempt } =
            std::mem::replace(&mut self.state, ActorState::Idle)
        else {
            // Defensive — we should only reach finish_closing from Closing.
            log::error!("finish_closing called from non-Closing state");
            return;
        };

        if cause.is_terminal() {
            self.state = ActorState::Terminal {
                reason: cause.to_error_string(),
            };
            self.broadcast_state();
            return;
        }

        // Compute backoff. The attempt count carries over from the state we
        // closed from (captured in `start_closing`); `self.state` is now the
        // Idle placeholder, so we must use the carried value, not re-read it.
        let next_attempt = attempt.saturating_add(1).max(1);
        let backoff = backoff_for_attempt(next_attempt);
        let until = Instant::now() + backoff;
        log::info!(
            "Actor entering cooldown for {:?} before attempt {}",
            backoff, next_attempt,
        );
        self.state = ActorState::Cooldown {
            next_attempt,
            until,
            last_error: cause.to_error_string(),
        };
        self.broadcast_state();
    }

    /// React to a successful handshake.
    fn on_connect_outcome(&mut self, outcome: ConnectOutcome) {
        let attempt = match &self.state {
            ActorState::Connecting { attempt, .. } => *attempt,
            _ => {
                log::error!(
                    "on_connect_outcome from state {} — ignoring",
                    self.state.name()
                );
                return;
            }
        };
        self.connecting_task = None;
        match outcome {
            ConnectOutcome::Ok(built) => self.enter_connected(built),
            ConnectOutcome::Transient(msg) => {
                let next_attempt = attempt.saturating_add(1);
                let backoff = backoff_for_attempt(next_attempt);
                let until = Instant::now() + backoff;
                log::warn!(
                    "Handshake failed (transient, attempt {}): {} — next attempt in {:?}",
                    attempt, msg, backoff,
                );
                self.state = ActorState::Cooldown {
                    next_attempt,
                    until,
                    last_error: msg,
                };
                self.broadcast_state();
            }
            ConnectOutcome::Permanent(msg) => {
                log::error!("Handshake permanently failed: {}", msg);
                self.state = ActorState::Terminal { reason: msg };
                self.broadcast_state();
            }
        }
    }

    /// Reject a command with the current state's `NotReady` reason.
    fn reject_not_ready(&mut self, cmd: RpcCommand) {
        if let RpcCommand::AttachPeer {
            config,
            invocation_tx,
            ..
        } = &cmd
        {
            self.peer_registration = Some((config.clone(), invocation_tx.clone()));
            self.peer_attach_pending = true;
        }
        let reason = match &self.state {
            ActorState::Idle => NotReadyReason::Idle,
            ActorState::Connecting { attempt, .. } => NotReadyReason::Connecting {
                attempt: *attempt,
            },
            ActorState::Cooldown {
                until, last_error, ..
            } => NotReadyReason::Cooldown {
                until_ms: system_now_ms().saturating_add(
                    until.saturating_duration_since(Instant::now()).as_millis() as u64,
                ),
                last_error: last_error.clone(),
            },
            ActorState::Closing { .. } => NotReadyReason::Closing,
            _ => {
                // Caller should not have reached reject_not_ready in
                // Connected/Terminal; if they did, surface as Rpc error.
                cmd.reply_err(CallError::Rpc(format!(
                    "internal: reject from state {}",
                    self.state.name()
                )));
                return;
            }
        };
        cmd.reply_err(CallError::NotReady(reason));
    }

    /// Reject a command with the Terminal reason.
    fn reject_terminal(&self, cmd: RpcCommand) {
        if let ActorState::Terminal { reason } = &self.state {
            cmd.reply_err(CallError::PermanentlyFailed(reason.clone()));
        } else {
            cmd.reply_err(CallError::Rpc("internal: reject_terminal off-state".into()));
        }
    }

    /// Dispatch a command in `Connected`.
    ///
    /// Every command — including `JoinContext` — is spawned as a child task
    /// so the actor loop can keep reacting to close signals while the call
    /// is in flight. `JoinContext` signals back via `internal_tx` so the
    /// loop can update the cached `context_id` without holding `&mut self`
    /// across an await.
    fn dispatch(
        &mut self,
        cmd: RpcCommand,
        close_tx: mpsc::Sender<CloseCause>,
        span: tracing::Span,
    ) {
        let conn = self
            .connection
            .as_ref()
            .expect("dispatch called without Connected connection");

        match cmd {
            RpcCommand::JoinContext { context_id, reply } => {
                let kernel = conn.kernel.clone();
                let instance = self.instance.clone();
                let internal_tx = self.internal_tx.clone();
                tokio::task::spawn_local(
                    async move {
                        let result =
                            run_rpc_call(kernel.join_context(context_id, &instance), &close_tx)
                                .await;
                        if result.is_ok() {
                            // Best-effort: if the actor is shutting down,
                            // the channel is closed and the state update
                            // doesn't matter anyway.
                            let _ = internal_tx.send(InternalMsg::JoinedContext(context_id));
                        }
                        let _ = reply.send(result);
                    }
                    .instrument(span),
                );
            }
            RpcCommand::SubscribeContext {
                context_id,
                sender,
                reply,
            } => {
                // Recorded as intent first, issued second: a subscribe that
                // arrives while disconnected is not an error, it is a feed that
                // starts at the next Connected edge. Re-subscribing an already
                // followed context replaces its sender rather than stacking a
                // second pump onto the same feed.
                self.context_feeds.insert(context_id, sender.clone());
                self.issue_context_feed(context_id, sender, false);
                let _ = reply.send(Ok(()));
            }
            RpcCommand::ResubscribeBlocks { reply } => {
                // Inline: uses the live connection's kernel via the actor's
                // own scoped re-subscribe helper. Fire-and-forget on the wire;
                // we ack the caller immediately (the subscription replaces the
                // prior one on the server by (principal, instance)).
                self.resubscribe_blocks();
                let _ = reply.send(Ok(()));
            }
            RpcCommand::SubscribeVfsActivity { interval_ms, reply } => {
                // Guard duplicate subscribes: only the first ask on a live
                // connection actually issues the RPC. There is no wire method
                // to "change interval" on an existing bridge, so a repeat
                // call is a harmless no-op rather than stacking a second
                // server-side timer task.
                if self.vfs_activity_interval_ms.is_some() {
                    let _ = reply.send(Ok(()));
                    return;
                }
                self.vfs_activity_interval_ms = Some(interval_ms);
                let kernel = conn.kernel.clone();
                let event_tx = self.event_tx.clone();
                tokio::task::spawn_local(
                    async move {
                        let forwarder = VfsActivityEventsForwarder { event_tx };
                        let client: crate::kaijutsu_capnp::vfs_activity_events::Client =
                            capnp_rpc::new_client(forwarder);
                        let result = run_rpc_call(
                            kernel.subscribe_vfs_activity(client, interval_ms),
                            &close_tx,
                        )
                        .await;
                        let _ = reply.send(result);
                    }
                    .instrument(span),
                );
            }
            RpcCommand::AttachPeer {
                config,
                invocation_tx,
                reply,
            } => {
                // Remember the registration so the actor re-attaches on every
                // reconnect (mirrors how `context_id` drives re-join). Clone for
                // storage, then dispatch the attach itself as usual. Clones of
                // `conn` are taken before the `self` mutation so the immutable
                // borrow is released first.
                let client = conn.client.clone();
                let kernel = conn.kernel.clone();
                self.peer_registration = Some((config.clone(), invocation_tx.clone()));
                self.peer_attach_pending = false;
                tokio::task::spawn_local(
                    dispatch_kernel_command(
                        RpcCommand::AttachPeer {
                            config,
                            invocation_tx,
                            reply,
                        },
                        client,
                        kernel,
                        close_tx,
                    )
                    .instrument(span),
                );
            }
            other => {
                let client = conn.client.clone();
                let kernel = conn.kernel.clone();
                tokio::task::spawn_local(
                    dispatch_kernel_command(other, client, kernel, close_tx).instrument(span),
                );
            }
        }
    }

    /// Apply an internal state-update message from a spawned child task.
    fn apply_internal(&mut self, msg: InternalMsg) {
        match msg {
            InternalMsg::JoinedContext(ctx) => {
                self.context_id = Some(ctx);
                self.joined_context_id = Some(ctx);
                // For single-context clients, re-scope block events to this
                // context now that we know it. The initial subscription (made
                // at connect, before any context existed) is kernel-wide;
                // leaving it unscoped floods the client with every other
                // context's block events. The re-subscribe carries the same
                // `instance`, so the server replaces the unscoped subscription
                // rather than stacking. Multi-context clients (the app) skip
                // this — they need kernel-wide delivery.
                if self.scope_blocks_to_context {
                    self.resubscribe_blocks();
                }
                self.broadcast_state();
            }
        }
    }

    /// (Re)issue the block-events subscription on the live connection, scoped
    /// to the actor's current `context_id`. Best-effort and fire-and-forget: a
    /// failure logs and leaves the prior subscription in place (the server
    /// keeps it until replaced or the connection drops). No-op when not
    /// Connected. Used both to re-scope after a `JoinContext` and to recover a
    /// subscription the server may have reaped after a sustained callback stall
    /// (the client-side half of the 2026-06-17 shell-timeout fix).
    /// Issue one context feed's `subscribeContext` on the live connection.
    ///
    /// Fire-and-forget on the wire, like the block re-subscribe beside it: a
    /// failure logs and leaves the consumer waiting, which its next reconnect
    /// repairs. No-op when not Connected — the intent stays in
    /// `context_feeds` and is replayed on the Connected edge.
    fn issue_context_feed(
        &self,
        context_id: ContextId,
        sender: mpsc::Sender<crate::context_feed::FeedEvent>,
        announce_resubscribe: bool,
    ) {
        let Some(conn) = self.connection.as_ref() else {
            return;
        };
        let kernel = conn.kernel.clone();
        tokio::task::spawn_local(async move {
            // Announced inside this task, before the subscribe, so the
            // consumer cannot receive a post-reconnect delta ahead of the
            // notice that it must rehydrate. Two spawned tasks would race.
            if announce_resubscribe
                && sender
                    .send(crate::context_feed::FeedEvent::Resubscribed)
                    .await
                    .is_err()
            {
                return;
            }
            let (observer, mut rx) = crate::context_feed::context_feed_channel(CONTEXT_FEED_QUEUE);
            match tokio::time::timeout(SUBSCRIBE_TIMEOUT, kernel.subscribe_context(context_id, observer))
                .await
            {
                Ok(Ok(())) => log::debug!("Subscribed to the change feed for {context_id}"),
                Ok(Err(e)) => {
                    log::warn!("Context feed subscribe failed for {context_id} (non-fatal): {e}");
                    return;
                }
                Err(_) => {
                    log::warn!("Context feed subscribe timed out for {context_id} (non-fatal)");
                    return;
                }
            }
            // Pump the observer's channel into the consumer's. Two channels
            // rather than one because the observer is rebuilt on every
            // reconnect while the consumer's receiver has to survive; the pump
            // ends when either side goes away.
            while let Some(event) = rx.recv().await {
                if sender.send(event).await.is_err() {
                    break;
                }
            }
        });
    }

    /// Re-issue every context feed on a new connection, telling each consumer
    /// to rehydrate first.
    fn resubscribe_context_feeds(&self) {
        for (context_id, sender) in &self.context_feeds {
            self.issue_context_feed(*context_id, sender.clone(), true);
        }
    }

    fn resubscribe_blocks(&self) {
        let Some(conn) = self.connection.as_ref() else {
            return;
        };
        let kernel = conn.kernel.clone();
        let event_tx = self.event_tx.clone();
        let instance = self.instance.clone();
        let midi_exchange = self.midi_exchange.clone();
        // Scope to the joined context only for single-context clients; a
        // kernel-wide client re-subscribes kernel-wide (None), matching its
        // handshake subscription.
        let context_id = if self.scope_blocks_to_context {
            self.context_id
        } else {
            None
        };
        tokio::task::spawn_local(async move {
            let (block_client, filter) =
                block_events_client_and_filter(&event_tx, context_id, midi_exchange);
            match tokio::time::timeout(
                SUBSCRIBE_TIMEOUT,
                kernel.subscribe_blocks_filtered(block_client, &filter, &instance),
            )
            .await
            {
                Ok(Ok(())) => {
                    log::debug!("Re-subscribed block events scoped to {context_id:?}")
                }
                Ok(Err(e)) => log::warn!("Block re-subscribe failed (non-fatal): {e}"),
                Err(_) => log::warn!("Block re-subscribe timed out (non-fatal)"),
            }
        });
    }

    /// Cancel any running ping/handshake tasks. Used during shutdown.
    fn abort_background_tasks(&mut self) {
        if let Some(t) = self.ping_task.take() {
            t.abort();
        }
        if let Some(t) = self.connecting_task.take() {
            t.abort();
        }
    }

    /// Main FSM loop.
    async fn run(mut self) {
        self.broadcast_state();

        loop {
            // Trace state transitions at debug level so reconnect dynamics
            // are visible in normal logs without enabling trace.
            log::debug!("actor state: {}", self.state.name());

            match &self.state {
                ActorState::Idle => {
                    // Eager connect: the actor dials the moment it starts
                    // running rather than waiting for a first command to kick
                    // it. A client should reach for the connection as soon as
                    // it can — that gives the user an early connected/failed
                    // signal, and a command arriving after startup usually
                    // finds us already Connected instead of eating a
                    // sacrificial NotReady("idle") round-trip. This is
                    // asynchronous: `start_connecting` spawns the handshake
                    // task and returns; commands that race the handshake are
                    // handled by the `Connecting` arm (rejected as
                    // NotReady(Connecting), or served once Connected). Shutdown
                    // (mpsc closed) is likewise observed there. Idle is now a
                    // transient bootstrap state the actor never rests in.
                    self.start_connecting(1);
                }

                ActorState::Cooldown { until, .. } => {
                    let next_attempt = match self.state {
                        ActorState::Cooldown { next_attempt, .. } => next_attempt,
                        _ => unreachable!(),
                    };
                    let sleep = tokio::time::sleep_until((*until).into());
                    tokio::pin!(sleep);
                    tokio::select! {
                        cmd = self.rx.recv() => {
                            let Some(envelope) = cmd else {
                                self.start_closing(CloseCause::Shutdown);
                                continue;
                            };
                            self.reject_not_ready(envelope.command);
                        }
                        _ = &mut sleep => {
                            self.start_connecting(next_attempt);
                        }
                    }
                }

                ActorState::Connecting { started_at, attempt } => {
                    let started_at = *started_at;
                    let attempt = *attempt;
                    let total_deadline =
                        tokio::time::Instant::from_std(started_at + CONNECT_TOTAL_BUDGET);
                    let total_sleep = tokio::time::sleep_until(total_deadline);
                    tokio::pin!(total_sleep);

                    let task = self
                        .connecting_task
                        .as_mut()
                        .expect("connecting_task set in Connecting");

                    enum ConnStep {
                        Reject(RpcCommand),
                        Shutdown,
                        Close(CloseCause),
                        Outcome(ConnectOutcome),
                        TotalBudget,
                    }
                    let step = tokio::select! {
                        cmd = self.rx.recv() => {
                            match cmd {
                                Some(c) => ConnStep::Reject(c.command),
                                None => ConnStep::Shutdown,
                            }
                        }
                        cause = self.close_rx.recv() => {
                            ConnStep::Close(cause.unwrap_or(CloseCause::Shutdown))
                        }
                        outcome = task => {
                            match outcome {
                                Ok(o) => ConnStep::Outcome(o),
                                Err(join_err) => ConnStep::Outcome(
                                    ConnectOutcome::Transient(format!(
                                        "handshake task: {}", join_err
                                    ))
                                ),
                            }
                        }
                        _ = &mut total_sleep => ConnStep::TotalBudget,
                    };
                    match step {
                        ConnStep::Reject(cmd) => self.reject_not_ready(cmd),
                        ConnStep::Shutdown => self.start_closing(CloseCause::Shutdown),
                        ConnStep::Close(cause) => self.start_closing(cause),
                        ConnStep::Outcome(o) => self.on_connect_outcome(o),
                        ConnStep::TotalBudget => {
                            log::warn!(
                                "Connect exceeded total budget {:?}; forcing cooldown",
                                CONNECT_TOTAL_BUDGET,
                            );
                            if let Some(t) = self.connecting_task.take() {
                                t.abort();
                            }
                            let next_attempt = attempt.saturating_add(1);
                            let backoff = backoff_for_attempt(next_attempt);
                            let until = Instant::now() + backoff;
                            self.state = ActorState::Cooldown {
                                next_attempt,
                                until,
                                last_error: format!(
                                    "connect exceeded total budget ({:?})",
                                    CONNECT_TOTAL_BUDGET
                                ),
                            };
                            self.broadcast_state();
                        }
                    }
                }

                ActorState::Connected { .. } => {
                    let close_tx = self.close_tx.clone();
                    tokio::select! {
                        // `biased` orders the branches deterministically:
                        // close > internal state updates > new commands.
                        // Without bias, a steady stream of commands could
                        // starve the close branch — i.e., we'd never notice
                        // the ping task signalled disconnect.
                        biased;
                        cause = self.close_rx.recv() => {
                            self.start_closing(cause.unwrap_or(CloseCause::Shutdown));
                        }
                        msg = self.internal_rx.recv() => {
                            if let Some(m) = msg {
                                self.apply_internal(m);
                            }
                        }
                        cmd = self.rx.recv() => {
                            match cmd {
                                Some(ChannelCmd { command, span }) => {
                                    self.dispatch(command, close_tx, span);
                                }
                                None => self.start_closing(CloseCause::Shutdown),
                            }
                        }
                    }
                }

                ActorState::Closing { .. } => {
                    // Connection already dropped in start_closing; nothing
                    // else to await here. Transition immediately.
                    self.finish_closing();
                }

                ActorState::Terminal { .. } => {
                    // Absorbing state. Reject all incoming commands.
                    tokio::select! {
                        cmd = self.rx.recv() => {
                            let Some(envelope) = cmd else {
                                // mpsc closed — done.
                                break;
                            };
                            self.reject_terminal(envelope.command);
                        }
                    }
                }
            }
        }

        self.abort_background_tasks();
        log::debug!("Actor shutting down: loop exited");
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Handshake task
// ────────────────────────────────────────────────────────────────────────────

/// Spawn the connect-handshake task. Returns a JoinHandle the actor can
/// select on. The task runs each step with its own per-phase deadline so
/// the failure mode names the slow phase.
#[allow(clippy::too_many_arguments)] // the handshake's inputs, one per subscription it re-establishes
fn spawn_handshake(
    config: SshConfig,
    context_id: Option<ContextId>,
    instance: String,
    scope_blocks_to_context: bool,
    event_tx: broadcast::Sender<ServerEvent>,
    peer_registration: Option<(PeerConfig, std::sync::mpsc::Sender<PeerInvocation>)>,
    vfs_activity_interval_ms: Option<u32>,
    midi_exchange: Arc<crate::midi_exchange::MidiExchangeSlot>,
    ledger_tx: broadcast::Sender<i64>,
) -> JoinHandle<ConnectOutcome> {
    tokio::task::spawn_local(async move {
        connect_handshake(
            config,
            context_id,
            instance,
            scope_blocks_to_context,
            event_tx,
            peer_registration,
            vfs_activity_interval_ms,
            midi_exchange,
            ledger_tx,
        )
        .await
    })
}

/// Build the block-events callback client + its filter, scoped to
/// `context_id` when known. Empty filter = kernel-wide delivery (every
/// context's block events), which floods a single-context client and can starve
/// its single-threaded RPC executor past the server's 5s callback deadline (the
/// 2026-06-17 MCP shell-timeout stall). Same `instance` on re-subscribe ⇒ the
/// server replaces the prior subscription for this (principal, instance) rather
/// than stacking, so re-scoping is safe.
fn block_events_client_and_filter(
    event_tx: &broadcast::Sender<ServerEvent>,
    context_id: Option<ContextId>,
    midi_exchange: Arc<crate::midi_exchange::MidiExchangeSlot>,
) -> (
    crate::kaijutsu_capnp::block_events::Client,
    kaijutsu_types::BlockEventFilter,
) {
    let block_fwd = BlockEventsForwarder {
        event_tx: event_tx.clone(),
        midi_exchange,
        last_ordered_seq: std::sync::atomic::AtomicU64::new(0),
        last_timing_seq: std::sync::atomic::AtomicU64::new(0),
    };
    let block_client: crate::kaijutsu_capnp::block_events::Client =
        capnp_rpc::new_client(block_fwd);
    let filter = context_id
        .map(|ctx| kaijutsu_types::BlockEventFilter {
            context_ids: vec![ctx],
            ..Default::default()
        })
        .unwrap_or_default();
    (block_client, filter)
}

#[allow(clippy::too_many_arguments)] // mirrors spawn_handshake's parameter list exactly
async fn connect_handshake(
    config: SshConfig,
    context_id: Option<ContextId>,
    instance: String,
    scope_blocks_to_context: bool,
    event_tx: broadcast::Sender<ServerEvent>,
    peer_registration: Option<(PeerConfig, std::sync::mpsc::Sender<PeerInvocation>)>,
    vfs_activity_interval_ms: Option<u32>,
    midi_exchange: Arc<crate::midi_exchange::MidiExchangeSlot>,
    ledger_tx: broadcast::Sender<i64>,
) -> ConnectOutcome {
    // 1. SSH dial + auth + channel open (with per-phase deadline).
    let client = match tokio::time::timeout(SSH_DIAL_TIMEOUT, connect_ssh(config)).await {
        Ok(Ok(c)) => c,
        Ok(Err(ConnectError::Ssh(e))) if e.is_permanent() => {
            return ConnectOutcome::Permanent(format!("ssh: {e}"));
        }
        Ok(Err(e)) => return ConnectOutcome::Transient(format!("ssh: {e}")),
        Err(_) => {
            return ConnectOutcome::Transient(format!(
                "ssh dial exceeded {:?}",
                SSH_DIAL_TIMEOUT
            ));
        }
    };

    // 2. bind_kernel — capability handout. Should be ~1ms.
    let (kernel, kernel_id) = match tokio::time::timeout(
        RPC_BIND_KERNEL_TIMEOUT,
        client.bind_kernel(),
    )
    .await
    {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            // Non-disconnect bind_kernel errors are server-side logic
            // failures (e.g., kernel state corrupt) — retrying won't help.
            let msg = format!("bind_kernel: {e}");
            return if is_disconnect_error(&msg) {
                ConnectOutcome::Transient(msg)
            } else {
                ConnectOutcome::Permanent(msg)
            };
        }
        Err(_) => {
            return ConnectOutcome::Transient(format!(
                "bind_kernel exceeded {:?}",
                RPC_BIND_KERNEL_TIMEOUT
            ));
        }
    };

    // 3. join_context if a context was specified. Optional.
    let joined_context = if let Some(ctx) = context_id {
        match tokio::time::timeout(
            RPC_JOIN_CONTEXT_TIMEOUT,
            kernel.join_context(ctx, &instance),
        )
        .await
        {
            Ok(Ok(c)) => Some(c),
            Ok(Err(e)) => {
                // join_context returns an application error when the context
                // does not exist (e.g., kernel restart with a fresh db, or
                // the context was deleted). Looping on that produces an
                // infinite reconnect — surface as Permanent so the actor
                // settles in Terminal. Disconnect errors stay Transient.
                let msg = format!("join_context: {e}");
                return if is_disconnect_error(&msg) {
                    ConnectOutcome::Transient(msg)
                } else {
                    ConnectOutcome::Permanent(msg)
                };
            }
            Err(_) => {
                return ConnectOutcome::Transient(format!(
                    "join_context exceeded {:?}",
                    RPC_JOIN_CONTEXT_TIMEOUT
                ));
            }
        }
    } else {
        None
    };

    // 3.5. Re-attach as a peer if a registration is remembered, so the kernel's
    //      PeerRegistry repopulates after a restart (the original
    //      tech_debt_peer_reattach_on_reconnect). Only fires on reconnects —
    //      `peer_registration` is None until the app's first `attach_peer`,
    //      which lands after the initial connect. Best-effort: a failure here
    //      must NOT abort an otherwise-healthy handshake (peers are a
    //      convenience, and the kernel may simply not be ready for the callback
    //      yet); we log and continue rather than forcing another reconnect.
    if let Some((cfg, inv_tx)) = &peer_registration {
        match tokio::time::timeout(RPC_CALL_TIMEOUT, kernel.attach_peer(cfg, inv_tx.clone())).await
        {
            Ok(Ok(_)) => log::info!("Re-attached peer '{}' on connect", cfg.nick),
            Ok(Err(e)) => log::warn!("peer re-attach failed (non-fatal): {e}"),
            Err(_) => log::warn!("peer re-attach timed out (non-fatal)"),
        }
    }

    // 3.6. Re-subscribe to the VFS activity digest stream if a caller had
    //      asked for one before this (re)connect (Lane K, FSN slice-1).
    //      BEST-EFFORT, same reasoning as the peer re-attach just above: heat
    //      is decorative world-rendering signal, never a control input, so a
    //      failure here must log-and-continue rather than fail the handshake
    //      or force another reconnect attempt.
    if let Some(interval_ms) = vfs_activity_interval_ms {
        let vfs_activity_fwd = VfsActivityEventsForwarder {
            event_tx: event_tx.clone(),
        };
        let vfs_activity_client: crate::kaijutsu_capnp::vfs_activity_events::Client =
            capnp_rpc::new_client(vfs_activity_fwd);
        match tokio::time::timeout(
            RPC_CALL_TIMEOUT,
            kernel.subscribe_vfs_activity(vfs_activity_client, interval_ms),
        )
        .await
        {
            Ok(Ok(())) => log::info!("Re-subscribed vfs activity on connect"),
            Ok(Err(e)) => log::warn!("vfs activity re-subscribe failed (non-fatal): {e}"),
            Err(_) => log::warn!("vfs activity re-subscribe timed out (non-fatal)"),
        }
    }

    // 3.7. Re-subscribe to the kernel-wide approval-ledger change stream.
    //      Unconditional on every (re)connect — no opt-in toggle to
    //      persist. `onChanged` expects no answer, so the forwarder can
    //      write straight into the persistent `ledger_tx`
    //      clone with no intermediate per-connect channel or forwarding
    //      task (the same shape `editor_fwd`/`vfs_activity_fwd` use with
    //      the shared `event_tx` below). BEST-EFFORT — a hint channel
    //      failing to (re)subscribe costs a subscriber a late poll, never a
    //      wrong answer (the ledger itself stays authoritative), so this
    //      must not abort an otherwise-healthy handshake.
    {
        let ledger_client: crate::kaijutsu_capnp::ledger_events::Client =
            capnp_rpc::new_client(LedgerEventsForwarder { tx: ledger_tx.clone() });
        match tokio::time::timeout(
            RPC_CALL_TIMEOUT,
            kernel.subscribe_ledger_events(ledger_client),
        )
        .await
        {
            Ok(Ok(())) => log::info!("Re-subscribed ledger events on connect"),
            Ok(Err(e)) => log::warn!("ledger events subscribe failed (non-fatal): {e}"),
            Err(_) => log::warn!("ledger events subscribe timed out (non-fatal)"),
        }
    }

    // 4. Subscribe to block + resource events in parallel under a single
    //    deadline. If either fails, the whole handshake fails — we don't
    //    want to enter Connected without subscriptions.
    //
    //    Scope block events to the joined context. An empty filter is
    //    kernel-wide delivery — every context's block events firehosed at a
    //    single-context client. On a single-threaded RPC LocalSet that
    //    foreign-context volume can starve the executor past the server's 5s
    //    callback deadline (the 2026-06-17 MCP "every shell call times out"
    //    stall). When no context is joined yet (first connect before
    //    register_session), we fall back to kernel-wide and re-scope on the
    //    JoinedContext that follows. Multi-context clients (the app) leave
    //    `scope_blocks_to_context` false and always subscribe kernel-wide.
    let filter_context = if scope_blocks_to_context {
        joined_context
    } else {
        None
    };
    let (block_client, filter) =
        block_events_client_and_filter(&event_tx, filter_context, midi_exchange);

    let resource_fwd = ResourceEventsForwarder {
        event_tx: event_tx.clone(),
    };
    let resource_client: crate::kaijutsu_capnp::resource_events::Client =
        capnp_rpc::new_client(resource_fwd);

    // Editor push events ride the same shared `event_tx` ServerEvent broadcast,
    // so `EditorStateChanged`/`EditorClosed` reach the app via the same stream
    // the renderer drains. The editor subscription is kernel-wide (session ids
    // are global; no per-context filter).
    let editor_fwd = EditorEventsForwarder {
        event_tx: event_tx.clone(),
    };
    let editor_client: crate::kaijutsu_capnp::editor_events::Client =
        capnp_rpc::new_client(editor_fwd);

    // Turn outcomes ride the same shared `event_tx`, alongside the editor and
    // block streams. Kernel-wide for the same reason the editor channel is: the
    // event names its own context. Part of the MANDATORY subscription set, not
    // an opt-in like the VFS digest — knowing a turn ended is not decorative,
    // and a client that entered Connected without it would be back to guessing
    // completion from block-status polling.
    let turn_fwd = TurnEventsForwarder {
        event_tx: event_tx.clone(),
    };
    let turn_client: crate::kaijutsu_capnp::turn_events::Client =
        capnp_rpc::new_client(turn_fwd);

    let subscribe_block = kernel.subscribe_blocks_filtered(block_client, &filter, &instance);
    let subscribe_resource = kernel.subscribe_mcp_resources(resource_client, &instance);
    let subscribe_editor = kernel.subscribe_editor(editor_client);
    let subscribe_turns = kernel.subscribe_turn_events(turn_client);

    // `try_join!` short-circuits: if any subscription fails, the others are
    // cancelled and we return immediately. `futures::future::join` would wait
    // for all, eating budget for nothing.
    let subscribe_both = async {
        tokio::try_join!(
            subscribe_block,
            subscribe_resource,
            subscribe_editor,
            subscribe_turns
        )
        .map(|_| ())
        .map_err(|e| format!("subscribe: {e}"))
    };

    match tokio::time::timeout(SUBSCRIBE_TIMEOUT, subscribe_both).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return ConnectOutcome::Transient(e),
        Err(_) => {
            return ConnectOutcome::Transient(format!(
                "subscribe exceeded {:?}",
                SUBSCRIBE_TIMEOUT
            ));
        }
    }

    ConnectOutcome::Ok(BuiltConnection {
        client,
        kernel: kernel.clone(),
        kernel_id,
        joined_context,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Liveness pinger
// ────────────────────────────────────────────────────────────────────────────

/// Run ping forever until aborted or ping fails. Signals `close_tx` on
/// failure (timeout, RPC error, or kernel ID mismatch).
async fn run_ping_loop(
    kernel: KernelHandle,
    expected_kernel_id: KernelId,
    close_tx: mpsc::Sender<CloseCause>,
) {
    let mut ticker = tokio::time::interval(PING_INTERVAL);
    // Skip the first immediate tick — we just connected, no need to ping
    // right away.
    ticker.tick().await;

    loop {
        ticker.tick().await;
        match tokio::time::timeout(PING_TIMEOUT, kernel.ping()).await {
            Ok(Ok((got_id, _server_ms))) => {
                if got_id != expected_kernel_id {
                    log::warn!(
                        "Ping returned kernel_id mismatch: expected {}, got {}",
                        expected_kernel_id, got_id
                    );
                    let _ = close_tx
                        .try_send(CloseCause::KernelIdChanged {
                            expected: expected_kernel_id,
                            got: got_id,
                        });
                    return;
                }
                log::trace!("ping ok for kernel_id={}", expected_kernel_id);
            }
            Ok(Err(e)) => {
                log::warn!("ping rpc error: {e}");
                let _ = close_tx.try_send(CloseCause::PingFailed(e.to_string()));
                return;
            }
            Err(_) => {
                log::warn!("ping exceeded {:?}", PING_TIMEOUT);
                let _ = close_tx.try_send(CloseCause::PingFailed(format!(
                    "timeout {:?}",
                    PING_TIMEOUT
                )));
                return;
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Kernel-level command dispatch (concurrent child tasks)
// ────────────────────────────────────────────────────────────────────────────

async fn dispatch_kernel_command(
    cmd: RpcCommand,
    client: RpcClient,
    kernel: KernelHandle,
    close_tx: mpsc::Sender<CloseCause>,
) {
    match cmd {
        // ── Drift ──
        RpcCommand::DriftQueue { reply } => {
            dispatch!(kernel, reply, close_tx, k, k.drift_queue());
        }
        RpcCommand::DriftCancel { staged_id, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.drift_cancel(staged_id));
        }

        // ── Context ──
        RpcCommand::GetContextId { reply } => {
            dispatch!(kernel, reply, close_tx, k, k.get_context_id());
        }
        RpcCommand::ListContexts { reply } => {
            dispatch!(kernel, reply, close_tx, k, k.list_contexts());
        }
        RpcCommand::ListTracks { reply } => {
            dispatch!(kernel, reply, close_tx, k, k.list_tracks());
        }
        RpcCommand::VfsSnapshot { path, depth, max_entries, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.vfs_snapshot(&path, depth, max_entries));
        }
        RpcCommand::Conclude { context_id, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.conclude(context_id));
        }
        RpcCommand::RenameContext { context_id, label, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.rename_context(context_id, &label));
        }
        RpcCommand::PromoteContext { context_id, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.promote_context(context_id));
        }
        RpcCommand::DemoteContext { context_id, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.demote_context(context_id));
        }
        RpcCommand::SetContextPaused { context_id, paused, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.set_context_paused(context_id, paused));
        }
        RpcCommand::AuthorBlock { req, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.author_block(&req));
        }
        RpcCommand::CompleteBlock { context_id, block_id, status, is_error, exit_code, reply } => {
            dispatch!(
                kernel,
                reply,
                close_tx,
                k,
                k.complete_block(context_id, &block_id, status, is_error, exit_code)
            );
        }
        RpcCommand::SetContextOriginHost { context_id, origin_host, reply } => {
            dispatch!(
                kernel,
                reply,
                close_tx,
                k,
                k.set_context_origin_host(context_id, &origin_host)
            );
        }
        RpcCommand::ArchiveContext { context_id, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.archive_context(context_id));
        }
        RpcCommand::SearchSimilar { query, k: topk, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.search_similar(&query, topk));
        }
        RpcCommand::GetNeighbors { context_id, k: topk, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.get_neighbors(context_id, topk));
        }
        RpcCommand::GetClusters { min_cluster_size, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.get_clusters(min_cluster_size));
        }
        RpcCommand::CreateContext {
            label,
            context_type,
            reply,
        } => {
            dispatch!(
                kernel,
                reply,
                close_tx,
                k,
                k.create_context_typed(&label, &context_type)
            );
        }
        RpcCommand::ResolveContextLabel { label, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.resolve_context_label(&label));
        }

        // ── Blocks / Change Feed ──
        RpcCommand::GetBlocks {
            context_id,
            query,
            reply,
        } => {
            dispatch!(kernel, reply, close_tx, k, k.get_blocks(context_id, &query));
        }
        RpcCommand::GetBlocksVersioned {
            context_id,
            query,
            reply,
        } => {
            dispatch!(
                kernel,
                reply,
                close_tx,
                k,
                k.get_blocks_versioned(context_id, &query)
            );
        }
        RpcCommand::GetContextVersion { context_id, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.get_context_version(context_id));
        }
        RpcCommand::CompactContext { context_id, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.compact_context(context_id));
        }

        // ── Shell / Execution ──
        RpcCommand::Execute { code, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.execute(&code));
        }
        RpcCommand::ShellExecute {
            code,
            context_id,
            user_initiated,
            reply,
        } => {
            dispatch!(
                kernel, reply, close_tx, k,
                k.shell_execute(&code, context_id, user_initiated)
            );
        }
        RpcCommand::SetBlockExcluded {
            context_id,
            block_id,
            excluded,
            reply,
        } => {
            dispatch!(
                kernel, reply, close_tx, k,
                k.set_block_excluded(context_id, &block_id, excluded)
            );
        }
        RpcCommand::Interrupt { exec_id, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.interrupt(exec_id));
        }
        RpcCommand::Complete {
            partial,
            cursor,
            reply,
        } => {
            dispatch!(kernel, reply, close_tx, k, k.complete(&partial, cursor));
        }
        RpcCommand::GetCommandHistory { limit, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.get_command_history(limit));
        }

        // ── Addressed Shell State ──
        RpcCommand::GetContextCwd { context_id, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.get_context_cwd(context_id));
        }
        RpcCommand::SetContextCwd { context_id, path, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.set_context_cwd(context_id, &path));
        }
        // `executeKj` runs the verb SYNCHRONOUSLY on the server
        // (`execute_kj_command` returns exit code and output, unlike
        // `shellExecute`, which spawns and hands back a block id), and some
        // `kj` verbs block on the approval gate — `kj cc send` today, plus
        // the six `Latch` producers Slice 5 migrates. So this reaches the
        // gate exactly the way `ExecuteTool` does, and needs the same
        // deadline. It is also the path ACP drives `kj` through.
        //
        // Applied to the whole verb surface rather than just the gated
        // ones: the client cannot tell a gated argv from an ungated one
        // without duplicating `is_gated_verb` here, and a second copy of
        // that policy would drift from the kernel's. The kernel is what
        // bounds the wait (`effective_gate_wait()`); this side's only job
        // is to not fire first.
        RpcCommand::ExecuteKj { context_id, argv, reply } => {
            dispatch_deadline!(
                kernel, reply, close_tx, k,
                kaijutsu_types::timeout::gate::CLIENT_CALL,
                k.execute_kj(context_id, &argv)
            );
        }
        RpcCommand::GetKjCommandCatalog { context_id, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.get_kj_command_catalog(context_id));
        }

        // ── Shell Variables ──
        RpcCommand::GetShellVar { name, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.get_shell_var(&name));
        }
        RpcCommand::SetShellVar { name, value, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.set_shell_var(&name, &value));
        }
        RpcCommand::ListShellVars { reply } => {
            dispatch!(kernel, reply, close_tx, k, k.list_shell_vars());
        }

        // ── Per-client durable view state ──
        RpcCommand::SetLastContext { client_id, context_id, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.set_last_context(&client_id, context_id));
        }
        RpcCommand::GetClientView { client_id, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.get_client_view(&client_id));
        }

        // ── Input Document ──
        RpcCommand::EditInput {
            context_id,
            pos,
            insert,
            delete,
            reply,
        } => {
            dispatch!(
                kernel, reply, close_tx, k,
                k.edit_input(context_id, pos, &insert, delete)
            );
        }
        RpcCommand::GetInputState { context_id, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.get_input_state(context_id));
        }
        RpcCommand::SubmitInput {
            context_id,
            is_shell,
            reply,
        } => {
            dispatch!(
                kernel, reply, close_tx, k,
                k.submit_input(context_id, is_shell)
            );
        }
        RpcCommand::ClearInput { context_id, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.clear_input(context_id));
        }
        RpcCommand::CommitCapture { context_id, mime, payload, reply } => {
            dispatch!(
                kernel, reply, close_tx, k,
                k.commit_capture(context_id, &mime, &payload)
            );
        }
        RpcCommand::ReportClockEstimate { context_id, beat, tempo_bps, epoch_ns, source, reply } => {
            dispatch!(
                kernel, reply, close_tx, k,
                k.report_clock_estimate(context_id, beat, tempo_bps, epoch_ns, &source)
            );
        }
        RpcCommand::ReportMidiPresence {
            device, present, backend, ports, epoch_ns, sink_host, reply
        } => {
            dispatch!(
                kernel, reply, close_tx, k,
                k.report_midi_presence(&device, present, &backend, &ports, epoch_ns, &sink_host)
            );
        }
        RpcCommand::VfsReadAll { path, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.vfs_read_all(&path));
        }

        // ── Editor (vi) ──
        RpcCommand::EditorKeys { session_id, keys, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.editor_keys(session_id, &keys));
        }

        // ── Tool Execution ──
        //
        // `ExecuteTool` and `CallMcpTool` both route (server-side, through
        // `dispatch_tool_via_broker` → `Broker::call_tool`) to whichever
        // instance the resolved tool name lands on — including
        // `builtin.shell_write`, the only BROKER instance that reaches
        // `kj::gate::run_gate` (`ExecuteKj` below reaches the same gate by
        // the other route, through kaish) (verified by reading
        // `kaijutsu-server::rpc::{execute_tool, call_mcp_tool}`). Both need
        // the gate ladder's client-side deadline instead of the generic
        // `RPC_CALL_TIMEOUT`, or the client gives up on an answerable gate
        // ask long before the gate itself would. `GetToolSchemas` and
        // `ListMcpResources` never dispatch a tool call, so they stay on
        // `dispatch!`.
        RpcCommand::ExecuteTool {
            tool, params, reply,
        } => {
            dispatch_deadline!(
                kernel, reply, close_tx, k,
                kaijutsu_types::timeout::gate::CLIENT_CALL,
                k.execute_tool(&tool, &params)
            );
        }
        RpcCommand::GetToolSchemas { reply } => {
            dispatch!(kernel, reply, close_tx, k, k.get_tool_schemas());
        }
        RpcCommand::CallMcpTool {
            tool, arguments, reply,
        } => {
            dispatch_deadline!(
                kernel, reply, close_tx, k,
                kaijutsu_types::timeout::gate::CLIENT_CALL,
                k.call_mcp_tool(&tool, &arguments)
            );
        }

        // ── MCP Resources ──
        RpcCommand::ListMcpResources { server, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.list_mcp_resources(&server));
        }

        // ── LLM ──
        RpcCommand::Prompt {
            content, model, context_id, reply,
        } => {
            dispatch!(
                kernel, reply, close_tx, k,
                k.prompt(&content, model.as_deref(), context_id)
            );
        }
        RpcCommand::ConfigureLlm {
            context_id, provider, model, reply,
        } => {
            dispatch!(
                kernel, reply, close_tx, k,
                k.set_context_model(context_id, &provider, &model)
            );
        }
        RpcCommand::GetLlmConfig { reply } => {
            dispatch!(kernel, reply, close_tx, k, k.get_llm_config());
        }
        RpcCommand::GetConfig { path, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.get_config(&path));
        }
        RpcCommand::SetDefaultProvider { provider, reply } => {
            dispatch!(kernel, reply, close_tx, k, k.set_default_provider(&provider));
        }
        RpcCommand::SetDefaultModel { provider, model, reply } => {
            dispatch!(
                kernel, reply, close_tx, k,
                k.set_default_model(&provider, &model)
            );
        }

        // ── Timeline ──
        RpcCommand::CherryPickBlock {
            block_id, target_context, reply,
        } => {
            dispatch!(
                kernel, reply, close_tx, k,
                k.cherry_pick_block(&block_id, target_context)
            );
        }
        RpcCommand::GetContextHistory {
            context_id, limit, reply,
        } => {
            dispatch!(
                kernel, reply, close_tx, k,
                k.get_context_history(context_id, limit)
            );
        }

        // ── Kernel Info ──
        RpcCommand::GetInfo { reply } => {
            dispatch!(kernel, reply, close_tx, k, k.get_info());
        }

        // ── Interrupt ──
        RpcCommand::InterruptContext {
            context_id, immediate, reply,
        } => {
            dispatch!(
                kernel, reply, close_tx, k,
                k.interrupt_context(context_id, immediate)
            );
        }
        RpcCommand::ListPresets { reply } => {
            dispatch!(kernel, reply, close_tx, k, k.list_presets());
        }

        // ── World-level (use client, not kernel) ──
        RpcCommand::Whoami { reply } => {
            let result = run_rpc_call(client.whoami(), &close_tx).await;
            let _ = reply.send(result);
        }
        RpcCommand::ListKernels { reply } => {
            let result = run_rpc_call(client.list_kernels(), &close_tx).await;
            let _ = reply.send(result);
        }
        // ── JoinContext handled inline by RpcActor::dispatch ──
        RpcCommand::JoinContext { reply, .. } => {
            let _ = reply.send(Err(CallError::Rpc(
                "join_context leaked into kernel dispatch (bug)".into(),
            )));
        }

        // ── SubscribeContext handled inline by RpcActor::dispatch ──
        // It has to be: the actor owns the feed registry that survives
        // reconnects, and this dispatcher only sees one live connection.
        RpcCommand::SubscribeContext { reply, .. } => {
            let _ = reply.send(Err(CallError::Rpc(
                "subscribe_context leaked into kernel dispatch (bug)".into(),
            )));
        }

        // ── ResubscribeBlocks handled inline by RpcActor::dispatch ──
        RpcCommand::ResubscribeBlocks { reply } => {
            let _ = reply.send(Err(CallError::Rpc(
                "resubscribe_blocks leaked into kernel dispatch (bug)".into(),
            )));
        }

        // ── SubscribeVfsActivity handled inline by RpcActor::dispatch (needs event_tx) ──
        RpcCommand::SubscribeVfsActivity { reply, .. } => {
            let _ = reply.send(Err(CallError::Rpc(
                "subscribe_vfs_activity leaked into kernel dispatch (bug)".into(),
            )));
        }

        // ── Peers ──
        RpcCommand::AttachPeer {
            config, invocation_tx, reply,
        } => {
            // attach_peer has its own bridge task; if it errors we still want
            // to surface disconnect to the actor.
            let result = match tokio::time::timeout(
                RPC_CALL_TIMEOUT,
                kernel.attach_peer(&config, invocation_tx),
            )
            .await
            {
                Ok(Ok(r)) => Ok(r),
                Ok(Err(e)) => {
                    let msg = e.to_string();
                    if is_disconnect_error(&msg) {
                        let _ = close_tx.try_send(CloseCause::RpcError(msg.clone()));
                    }
                    Err(CallError::Rpc(msg))
                }
                Err(_) => Err(CallError::Timeout(RPC_CALL_TIMEOUT)),
            };
            let _ = reply.send(result);
        }
        RpcCommand::InvokePeer {
            nick, action, params, reply,
        } => {
            dispatch!(
                kernel, reply, close_tx, k,
                k.invoke_peer(&nick, &action, &params)
            );
        }
        RpcCommand::ListPeers { reply } => {
            dispatch!(kernel, reply, close_tx, k, k.list_peers());
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

/// Full jitter applied under the exponential-capped envelope. A bounced
/// kernel is a many-clients event (every app window, every MCP session,
/// every peer dials back at once) — without jitter every one of them lands
/// on the same 1s/2s/4s/.../30s clock ticks and the reconnect wave itself
/// becomes a small thundering herd against the kernel that just came back up.
/// `JITTER_FLOOR` keeps a retry from ever collapsing to ~0s (which would
/// defeat backoff entirely): the realized delay is always at least this
/// fraction of the exponential-capped value, uniformly up to the full value.
const JITTER_FLOOR: f64 = 0.5;

fn backoff_for_attempt(attempt: u32) -> Duration {
    backoff_for_attempt_jittered(attempt, || {
        rand::Rng::gen_range(&mut rand::thread_rng(), 0.0..1.0)
    })
}

/// Same exponential-capped schedule as `backoff_for_attempt`, but the jitter
/// unit (expected in `[0.0, 1.0)`) comes from `jitter_source` instead of the
/// OS RNG. Lets tests assert exact bounds (e.g. `0.0` / `1.0` sources) rather
/// than asserting on random output — the RNG itself isn't what we're
/// verifying, the *shape* of the jitter envelope is.
fn backoff_for_attempt_jittered(attempt: u32, jitter_source: impl FnOnce() -> f64) -> Duration {
    let base = (BACKOFF_BASE.as_secs_f64() * 2.0_f64.powi(attempt.saturating_sub(1) as i32))
        .min(BACKOFF_MAX.as_secs_f64());
    let unit = jitter_source().clamp(0.0, 1.0);
    let factor = JITTER_FLOOR + unit * (1.0 - JITTER_FLOOR);
    Duration::from_secs_f64(base * factor)
}

fn system_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ────────────────────────────────────────────────────────────────────────────
// Public spawn function
// ────────────────────────────────────────────────────────────────────────────

/// Spawn an RPC actor in the current `LocalSet` context.
///
/// `instance` is a per-actor stable UUID — the server uses
/// `(principal, instance)` to dedupe subscriptions across reconnects, so
/// callers should NOT pass a fresh UUID on every spawn unless they want
/// the server to treat them as a brand-new participant.
///
/// `context_id` is the optional context to (re)join on every Connected
/// transition. If `None`, the actor connects but doesn't bind to a context;
/// later calls to `ActorHandle::join_context` set this and persist for
/// future reconnects.
///
/// `scope_blocks_to_context` makes block-event subscriptions track the joined
/// context instead of being kernel-wide. Single-context clients (e.g. the MCP
/// server) should pass `true` — kernel-wide delivery firehoses their
/// single-threaded RPC executor with foreign-context events. Multi-context
/// clients (the app, which routes every context's events into a per-context
/// cache) must pass `false`.
pub fn spawn_actor(
    config: SshConfig,
    context_id: Option<ContextId>,
    instance: String,
    scope_blocks_to_context: bool,
) -> ActorHandle {
    let (tx, rx) = mpsc::channel::<ChannelCmd>(CHANNEL_CAPACITY);
    let (event_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
    let (status_tx, _) = broadcast::channel(STATUS_BROADCAST_CAPACITY);
    // Seed the level mirror with Idle — the state the actor starts in, before
    // `run()` issues its first `broadcast_state`.
    let (status_watch_tx, status_watch_rx) = watch::channel(ConnectionStatus::Idle);

    // One slot, shared by the actor (which hands it to every block-events
    // forwarder it builds, reconnects included) and the handle (where a
    // MIDI-capable client installs its worker).
    let midi_exchange = crate::midi_exchange::MidiExchangeSlot::new();

    // Created once, outside the reconnect loop, same as `event_tx`/
    // `status_tx` above: `connect_handshake` rebuilds the kernel-side
    // subscription on every (re)connect, but every receiver this sender
    // ever hands out (via `ActorHandle::subscribe_ledger_events`) survives
    // a reconnect untouched.
    let (ledger_tx, _) = broadcast::channel::<i64>(LEDGER_BROADCAST_CAPACITY);

    let actor = RpcActor::new(
        config,
        context_id,
        instance,
        scope_blocks_to_context,
        rx,
        event_tx.clone(),
        status_tx.clone(),
        status_watch_tx,
        midi_exchange.clone(),
        ledger_tx.clone(),
    );
    tokio::task::spawn_local(actor.run());

    ActorHandle {
        tx,
        event_tx,
        status_tx,
        status_watch_rx,
        midi_exchange,
        ledger_tx,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The exponential-capped *shape* of the schedule, isolated from jitter
    /// by pinning the jitter source at its top (`1.0` ⇒ `factor == 1.0`, no
    /// reduction) so this test still pins exact values.
    #[test]
    fn backoff_curve_caps_at_max() {
        assert_eq!(backoff_for_attempt_jittered(1, || 1.0).as_secs(), 1);
        assert_eq!(backoff_for_attempt_jittered(2, || 1.0).as_secs(), 2);
        assert_eq!(backoff_for_attempt_jittered(3, || 1.0).as_secs(), 4);
        assert_eq!(backoff_for_attempt_jittered(4, || 1.0).as_secs(), 8);
        assert_eq!(backoff_for_attempt_jittered(5, || 1.0).as_secs(), 16);
        // 32s capped to 30s
        assert_eq!(backoff_for_attempt_jittered(6, || 1.0).as_secs(), 30);
        assert_eq!(backoff_for_attempt_jittered(20, || 1.0).as_secs(), 30);
    }

    /// Jitter never collapses a retry toward zero: the floor keeps every
    /// realized delay at least `JITTER_FLOOR` of the exponential-capped
    /// value, even when the jitter source returns the minimum unit.
    #[test]
    fn jitter_floor_bounds_the_minimum_delay() {
        let base = backoff_for_attempt_jittered(4, || 1.0); // 8s, no reduction
        let floored = backoff_for_attempt_jittered(4, || 0.0); // minimum jitter unit
        assert_eq!(floored.as_secs_f64(), base.as_secs_f64() * JITTER_FLOOR);
    }

    /// Jitter never exceeds the exponential-capped envelope — the unit source
    /// is clamped even if a (misbehaving) source returns something outside
    /// `[0.0, 1.0)`.
    #[test]
    fn jitter_never_exceeds_the_unjittered_envelope() {
        let base = backoff_for_attempt_jittered(6, || 1.0); // at the 30s cap
        let over = backoff_for_attempt_jittered(6, || 5.0); // out-of-range source
        assert_eq!(over.as_secs_f64(), base.as_secs_f64());
    }

    /// Two different jitter units at the same attempt produce two different
    /// delays — this is the actual thundering-herd fix: without jitter every
    /// client reconnecting to the same bounced kernel lands on the identical
    /// 1s/2s/4s/.../30s tick.
    #[test]
    fn jitter_spreads_same_attempt_across_a_range() {
        let a = backoff_for_attempt_jittered(5, || 0.0);
        let b = backoff_for_attempt_jittered(5, || 1.0);
        assert!(a < b, "jitter should spread delays at a fixed attempt: {a:?} vs {b:?}");
    }

    /// The production entry point (`backoff_for_attempt`, OS-RNG-backed)
    /// always stays within the `[floor, envelope]` bounds regardless of what
    /// the RNG draws — run enough samples that a bug producing an
    /// out-of-bounds factor would show up virtually every run.
    #[test]
    fn backoff_for_attempt_stays_within_jitter_bounds() {
        let envelope = backoff_for_attempt_jittered(4, || 1.0).as_secs_f64();
        let floor = envelope * JITTER_FLOOR;
        for _ in 0..200 {
            let d = backoff_for_attempt(4).as_secs_f64();
            assert!(
                (floor..=envelope).contains(&d),
                "backoff {d} outside [{floor}, {envelope}]"
            );
        }
    }

    #[test]
    fn is_disconnect_classifier_matches_capnp_kinds() {
        assert!(is_disconnect_error("Disconnected: Peer disconnected"));
        assert!(is_disconnect_error("disconnected from peer"));
        assert!(!is_disconnect_error("Failed: invalid context ID"));
        assert!(!is_disconnect_error("Overloaded: too many requests"));
    }

    /// The deadline `dispatch_deadline!` hands the gate-reaching commands
    /// must clear the broker's cap for the gate-capable instance, or the
    /// client would give up before the broker even has a chance to cancel —
    /// racing the broker's `Timeout` instead of reporting the gate's own
    /// reason. Enforced here rather than left to the shared
    /// `gate_ladder_fires_caller_first` test in `kaijutsu-types`, because
    /// THIS crate is the one that could silently pass the wrong constant to
    /// `dispatch_deadline!` at the call site — a typo'd `RPC_CALL_TIMEOUT`
    /// or `gate::BROKER_CALL` would still compile.
    #[test]
    fn gated_command_deadline_clears_the_broker_call_cap() {
        let deadline = kaijutsu_types::timeout::gate::CLIENT_CALL;
        assert!(
            deadline > kaijutsu_types::timeout::gate::BROKER_CALL,
            "the client's per-call deadline for a gate-capable command must \
             outlast the broker's cap ({:?}), or the client gives up first \
             and destroys the broker's/gate's honest error \
             (client deadline {:?})",
            kaijutsu_types::timeout::gate::BROKER_CALL,
            deadline
        );
        // A genuinely wedged RPC must still report far sooner than the gate
        // deadline would ever be reached — this isn't a blanket raise of
        // every command's timeout, just the two that can reach the gate.
        assert!(
            RPC_CALL_TIMEOUT < deadline,
            "the gated deadline should be strictly longer than the default \
             request-tier RPC_CALL_TIMEOUT, or there was no defect to fix"
        );
    }

    #[test]
    fn close_cause_terminal_distinguishes() {
        assert!(CloseCause::Shutdown.is_terminal());
        assert!(!CloseCause::RpcError("disc".into()).is_terminal());
        assert!(!CloseCause::PingFailed("timeout".into()).is_terminal());
        assert!(!CloseCause::KernelIdChanged {
            expected: KernelId::new(),
            got: KernelId::new(),
        }
        .is_terminal());
    }

    #[test]
    fn call_error_displays_helpfully() {
        let e = CallError::NotReady(NotReadyReason::Connecting { attempt: 3 });
        let s = e.to_string();
        assert!(s.contains("connecting"), "got: {s}");
        assert!(s.contains("3"), "got: {s}");
    }

    /// Build a bare `RpcActor` for state-machine unit tests. No network I/O:
    /// `RpcActor::new` only wires in-memory channels, so the state transition
    /// methods (`start_closing`/`finish_closing`/...) are exercisable without
    /// a live connection.
    fn test_actor() -> RpcActor {
        let (_tx, rx) = mpsc::channel(8);
        let (event_tx, _) = broadcast::channel(8);
        let (status_tx, _) = broadcast::channel(8);
        let (status_watch_tx, _) = watch::channel(ConnectionStatus::Idle);
        let (ledger_tx, _) = broadcast::channel(8);
        RpcActor::new(
            SshConfig::default(),
            None,
            "test-actor".to_string(),
            false,
            rx,
            event_tx,
            status_tx,
            status_watch_tx,
            crate::midi_exchange::MidiExchangeSlot::new(),
            ledger_tx,
        )
    }

    /// Regression test for the backoff reset bug: `finish_closing` used to
    /// read `self.state` *after* `mem::replace` had already swapped it to
    /// `Idle`, so the attempt count carried into `Closing` was always read
    /// back as 0 and backoff reset to attempt 1 on every post-connect
    /// failure. `start_closing` captures the leaving state's attempt count
    /// into `Closing::attempt`; `finish_closing` must carry that value
    /// through to the next `Cooldown::next_attempt`, not re-derive it from
    /// the (already-Idle) `self.state`.
    #[test]
    fn finish_closing_carries_attempt_count_from_connecting_through_cooldown() {
        let mut actor = test_actor();
        // Simulate the 3rd handshake attempt in flight when the pipe dies.
        actor.state = ActorState::Connecting {
            attempt: 3,
            started_at: Instant::now(),
        };
        actor.start_closing(CloseCause::RpcError("disconnected".into()));
        assert!(
            matches!(actor.state, ActorState::Closing { attempt: 3, .. }),
            "start_closing should capture the in-flight attempt: {:?}",
            actor.state
        );

        actor.finish_closing();
        match actor.state {
            ActorState::Cooldown { next_attempt, .. } => {
                assert_eq!(
                    next_attempt, 4,
                    "backoff should climb to attempt 4, not reset to 1"
                );
            }
            other => panic!("expected Cooldown, got {other:?}"),
        }
    }

    /// Same carry-through, but leaving from `Cooldown` (a reconnect attempt
    /// itself failed before ever reaching `Connecting` — e.g. the dial threw
    /// immediately). `next_attempt` on the Cooldown we're leaving must carry
    /// forward, not reset.
    #[test]
    fn finish_closing_carries_attempt_count_from_cooldown_through_cooldown() {
        let mut actor = test_actor();
        actor.state = ActorState::Cooldown {
            next_attempt: 5,
            until: Instant::now(),
            last_error: "prior failure".into(),
        };
        actor.start_closing(CloseCause::PingFailed("timeout".into()));
        assert!(
            matches!(actor.state, ActorState::Closing { attempt: 5, .. }),
            "start_closing should carry the pending Cooldown attempt: {:?}",
            actor.state
        );

        actor.finish_closing();
        match actor.state {
            ActorState::Cooldown { next_attempt, .. } => {
                assert_eq!(next_attempt, 6);
            }
            other => panic!("expected Cooldown, got {other:?}"),
        }
    }

    /// Closing from a healthy `Connected` carries attempt 0 — the next
    /// reconnect is attempt 1, not a continuation of some prior backoff.
    #[test]
    fn finish_closing_starts_fresh_backoff_after_a_healthy_connection_drops() {
        let mut actor = test_actor();
        actor.state = ActorState::Connected {
            since: Instant::now(),
        };
        actor.start_closing(CloseCause::RpcError("disconnected".into()));
        assert!(matches!(actor.state, ActorState::Closing { attempt: 0, .. }));

        actor.finish_closing();
        match actor.state {
            ActorState::Cooldown { next_attempt, .. } => assert_eq!(next_attempt, 1),
            other => panic!("expected Cooldown, got {other:?}"),
        }
    }

    /// A terminal cause (`Shutdown`) skips backoff entirely and settles
    /// `Terminal`, regardless of the carried attempt count.
    #[test]
    fn finish_closing_terminal_cause_ignores_attempt_count() {
        let mut actor = test_actor();
        actor.state = ActorState::Connecting {
            attempt: 7,
            started_at: Instant::now(),
        };
        actor.start_closing(CloseCause::Shutdown);
        actor.finish_closing();
        assert!(matches!(actor.state, ActorState::Terminal { .. }));
    }

    /// Reconnect must be indefinite: a still-down kernel (every handshake
    /// keeps returning `ConnectOutcome::Transient` — connection refused,
    /// exactly what `SshError::ConnectionFailed`/`Disconnected` produce
    /// during a rebuild window) never settles `Terminal` no matter how many
    /// attempts pile up. Only a `ConnectOutcome::Permanent` result (auth
    /// rejected, missing context) is allowed to give up — see
    /// `on_connect_outcome`. This drives 200 Connecting→Transient cycles
    /// (a kernel down for minutes at capped 30s backoff is ~4-5 attempts/min,
    /// so 200 covers well over half an hour) and asserts every one lands
    /// back in `Cooldown`, climbing `next_attempt`, never `Terminal`.
    #[test]
    fn reconnect_never_gives_up_while_the_kernel_stays_down() {
        let mut actor = test_actor();
        for i in 1..=200u32 {
            actor.state = ActorState::Connecting {
                attempt: i,
                started_at: Instant::now(),
            };
            actor.on_connect_outcome(ConnectOutcome::Transient("connection refused".into()));
            match &actor.state {
                ActorState::Cooldown { next_attempt, .. } => {
                    assert_eq!(
                        *next_attempt,
                        i + 1,
                        "attempt count should climb by exactly one per cycle"
                    );
                }
                other => panic!(
                    "reconnect gave up after {i} attempts against a still-down kernel: {other:?}"
                ),
            }
        }
    }

    /// Regression guard for the re-init contract `connect_handshake` relies
    /// on: it reads `peer_registration`, `vfs_activity_interval_ms`, and
    /// `context_id` off the actor to decide what to re-attach/re-subscribe/
    /// re-join on every (re)connect (see `connect_handshake` steps 3, 3.5,
    /// 3.6 and `tech_debt_peer_reattach_on_reconnect`). None of those fields
    /// is state a close→cooldown cycle should ever touch — if a future
    /// refactor accidentally cleared one in `start_closing`/`finish_closing`,
    /// the corresponding re-init would silently stop happening on the next
    /// successful handshake, with nothing failing loudly until a dev noticed
    /// their peer registration (or VFS heat, or joined context) gone after a
    /// kernel bounce. This can't exercise `connect_handshake` itself (it
    /// does real capnp RPC against a live kernel — no mock transport exists
    /// in this crate), so it pins the state-survival half of the contract at
    /// the FSM level, matching this file's existing no-I/O state tests.
    #[test]
    fn reconnect_state_survives_a_full_close_cooldown_cycle() {
        let mut actor = test_actor();
        let (peer_tx, _peer_rx) = std::sync::mpsc::channel();
        actor.peer_registration = Some((
            PeerConfig {
                nick: "amy".into(),
                instance: "inst-1".into(),
            },
            peer_tx,
        ));
        actor.vfs_activity_interval_ms = Some(500);
        actor.context_id = Some(ContextId::new());
        actor.state = ActorState::Connected {
            since: Instant::now(),
        };

        actor.start_closing(CloseCause::RpcError("disconnected".into()));
        actor.finish_closing();

        assert!(
            matches!(actor.state, ActorState::Cooldown { .. }),
            "expected Cooldown, got {:?}",
            actor.state
        );
        assert!(
            actor.peer_registration.is_some(),
            "peer registration must survive close→cooldown so connect_handshake re-attaches"
        );
        assert_eq!(actor.peer_registration.as_ref().unwrap().0.nick, "amy");
        assert_eq!(
            actor.vfs_activity_interval_ms,
            Some(500),
            "VFS activity subscription request must survive close→cooldown"
        );
        assert!(
            actor.context_id.is_some(),
            "context_id must survive close→cooldown so connect_handshake re-joins it"
        );
    }

    #[test]
    fn attach_peer_during_connecting_keeps_intent_and_replays_once() {
        let mut actor = test_actor();
        actor.state = ActorState::Connecting {
            attempt: 2,
            started_at: Instant::now(),
        };
        // The current handshake already took its snapshot, so this declaration
        // must be replayed on the Connected edge instead of waiting for yet
        // another reconnect.
        assert!(actor.peer_registration_for_handshake().is_none());

        let (peer_tx, peer_rx) = std::sync::mpsc::channel();
        let (reply_tx, reply_rx) = oneshot::channel();
        actor.reject_not_ready(RpcCommand::AttachPeer {
            config: PeerConfig {
                nick: "acp/toad".into(),
                instance: "toad-1".into(),
            },
            invocation_tx: peer_tx,
            reply: reply_tx,
        });

        assert!(matches!(
            reply_rx.blocking_recv(),
            Ok(Err(CallError::NotReady(NotReadyReason::Connecting { attempt: 2 })))
        ));
        let (config, replay_tx) = actor
            .take_pending_peer_registration()
            .expect("connect-time declaration should replay on Connected");
        assert_eq!(config.nick, "acp/toad");
        assert!(
            actor.take_pending_peer_registration().is_none(),
            "one declaration must not produce duplicate registrations"
        );

        let (invocation_reply, _invocation_result) = oneshot::channel();
        replay_tx
            .send(PeerInvocation {
                action: "ping".into(),
                params: vec![],
                reply: invocation_reply,
            })
            .expect("replay must retain the original invocation receiver");
        assert_eq!(peer_rx.recv().unwrap().action, "ping");
    }

    #[test]
    fn attach_peer_during_cooldown_is_owned_by_next_handshake() {
        let mut actor = test_actor();
        actor.state = ActorState::Cooldown {
            next_attempt: 4,
            until: Instant::now(),
            last_error: "kernel restarting".into(),
        };
        let (peer_tx, _peer_rx) = std::sync::mpsc::channel();
        let (reply_tx, reply_rx) = oneshot::channel();
        actor.reject_not_ready(RpcCommand::AttachPeer {
            config: PeerConfig {
                nick: "mcp".into(),
                instance: "mcp-1".into(),
            },
            invocation_tx: peer_tx,
            reply: reply_tx,
        });

        assert!(matches!(
            reply_rx.blocking_recv(),
            Ok(Err(CallError::NotReady(NotReadyReason::Cooldown {
                ref last_error,
                ..
            }))) if last_error == "kernel restarting"
        ));
        let (config, _) = actor
            .peer_registration_for_handshake()
            .expect("next handshake should own cooldown declaration");
        assert_eq!(config.nick, "mcp");
        assert!(actor.take_pending_peer_registration().is_none());
    }

    #[test]
    fn terminal_rejection_does_not_remember_peer_intent() {
        let mut actor = test_actor();
        actor.state = ActorState::Terminal {
            reason: "authentication rejected".into(),
        };
        let (peer_tx, _peer_rx) = std::sync::mpsc::channel();
        let (reply_tx, reply_rx) = oneshot::channel();
        actor.reject_terminal(RpcCommand::AttachPeer {
            config: PeerConfig {
                nick: "acp/toad".into(),
                instance: "toad-1".into(),
            },
            invocation_tx: peer_tx,
            reply: reply_tx,
        });

        assert!(matches!(
            reply_rx.blocking_recv(),
            Ok(Err(CallError::PermanentlyFailed(_)))
        ));
        assert!(actor.peer_registration.is_none());
        assert!(!actor.peer_attach_pending);
    }
}
