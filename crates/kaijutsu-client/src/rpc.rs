//! Cap'n Proto RPC client for kaijutsu
//!
//! Provides typed interface to the World and Kernel capabilities.

use capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty};
use futures::AsyncReadExt;
use kaijutsu_crdt::{ContextId, KernelId};
use kaijutsu_types::{
    BlockFilter, BlockId, BlockKind, BlockQuery, BlockSnapshot, BlockSnapshotBuilder, ContentType,
    DriftKind, ErrorCategory, ErrorPayload, ErrorSeverity, ErrorSpan, PrincipalId, Role, Status,
    Tick, ToolKind, TrackId,
};
use russh::ChannelStream;
use russh::client::Msg;
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::kaijutsu_capnp::world;

/// Chunk size for [`RpcClient::vfs_read_all`]. Matches the SFTP READ window
/// and `VfsOps::STREAM_CHUNK_SIZE` so a wire-backed backend sees the same
/// cadence its own streaming path would drive.
const VFS_READ_CHUNK: u32 = 256 * 1024;

/// Aborts the Cap'n Proto RPC system task when the last reference is dropped.
///
/// Without this, `spawn_local(rpc_system)` runs forever — the task owns the
/// underlying SSH stream, so the server never sees a disconnect even after
/// the actor exits and drops `ConnectionState`.
#[derive(Clone)]
struct RpcSystemGuard(#[allow(dead_code)] std::rc::Rc<RpcSystemGuardInner>);

#[allow(dead_code)]
struct RpcSystemGuardInner(tokio::task::AbortHandle);

impl Drop for RpcSystemGuardInner {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One block to author over RPC (`authorBlock @106`).
///
/// A struct rather than fifteen positional arguments, and every optional
/// field is genuinely optional: `None` parent = root, `None` after = append,
/// `None` tool fields = not a tool block. Constructors cover the two shapes
/// that actually occur; build one by hand for anything else.
#[derive(Debug, Clone)]
pub struct AuthorBlock {
    pub context_id: ContextId,
    /// The authoring identity. Callers pass their agent-session principal
    /// (`PrincipalId::for_agent_session`) so a block names the agent that
    /// caused it, and keeps naming it across process restarts.
    pub principal_id: PrincipalId,
    pub role: Role,
    pub kind: BlockKind,
    pub status: Status,
    pub content: String,
    /// MIME hint; `None` = let the kernel's heuristic decide.
    pub content_type: Option<String>,
    /// For a ToolResult this is REQUIRED and is the ToolCall's id — the
    /// server refuses an unlinked result rather than orphaning the pair.
    pub parent_id: Option<BlockId>,
    pub after_id: Option<BlockId>,
    pub tool_name: Option<String>,
    pub tool_input: Option<serde_json::Value>,
    pub tool_kind: Option<ToolKind>,
}

impl AuthorBlock {
    /// A plain text block, appended.
    pub fn text(
        context_id: ContextId,
        principal_id: PrincipalId,
        role: Role,
        content: impl Into<String>,
    ) -> Self {
        Self {
            context_id,
            principal_id,
            role,
            kind: BlockKind::Text,
            status: Status::Done,
            content: content.into(),
            content_type: None,
            parent_id: None,
            after_id: None,
            tool_name: None,
            tool_input: None,
            tool_kind: None,
        }
    }

    /// A tool call, reserved at `Status::Running`.
    ///
    /// Running is the honest default: the tool has not finished, and under
    /// reserve-then-flow the result arrives later on its own schedule. A
    /// ToolCall sitting at Running is a pending state, not an orphan —
    /// completing it is [`RpcClient::complete_block`]'s job.
    pub fn tool_call(
        context_id: ContextId,
        principal_id: PrincipalId,
        tool_name: impl Into<String>,
        tool_input: serde_json::Value,
        tool_kind: Option<ToolKind>,
    ) -> Self {
        Self {
            context_id,
            principal_id,
            role: Role::Tool,
            kind: BlockKind::ToolCall,
            status: Status::Running,
            content: String::new(),
            content_type: None,
            parent_id: None,
            after_id: None,
            tool_name: Some(tool_name.into()),
            tool_input: Some(tool_input),
            tool_kind,
        }
    }

    /// A tool result, linked to the call it answers.
    pub fn tool_result(
        context_id: ContextId,
        principal_id: PrincipalId,
        call_id: BlockId,
        content: impl Into<String>,
        is_error: bool,
        tool_kind: Option<ToolKind>,
    ) -> Self {
        Self {
            context_id,
            principal_id,
            role: Role::Tool,
            kind: BlockKind::ToolResult,
            status: if is_error { Status::Error } else { Status::Done },
            content: content.into(),
            content_type: None,
            parent_id: Some(call_id),
            after_id: None,
            tool_name: None,
            tool_input: None,
            tool_kind,
        }
    }
}

fn role_to_capnp(role: Role) -> crate::kaijutsu_capnp::Role {
    match role {
        Role::User => crate::kaijutsu_capnp::Role::User,
        Role::Model => crate::kaijutsu_capnp::Role::Model,
        Role::System => crate::kaijutsu_capnp::Role::System,
        Role::Tool => crate::kaijutsu_capnp::Role::Tool,
        Role::Asset => crate::kaijutsu_capnp::Role::Asset,
    }
}

fn status_to_capnp(status: Status) -> crate::kaijutsu_capnp::Status {
    match status {
        Status::Pending => crate::kaijutsu_capnp::Status::Pending,
        Status::Running => crate::kaijutsu_capnp::Status::Running,
        Status::Done => crate::kaijutsu_capnp::Status::Done,
        Status::Error => crate::kaijutsu_capnp::Status::Error,
    }
}

fn tool_kind_to_capnp(tk: ToolKind) -> crate::kaijutsu_capnp::ToolKind {
    match tk {
        ToolKind::Shell => crate::kaijutsu_capnp::ToolKind::Shell,
        ToolKind::Mcp => crate::kaijutsu_capnp::ToolKind::Mcp,
        ToolKind::Builtin => crate::kaijutsu_capnp::ToolKind::Builtin,
    }
}

fn block_kind_to_capnp(kind: BlockKind) -> crate::kaijutsu_capnp::BlockKind {
    match kind {
        BlockKind::Text => crate::kaijutsu_capnp::BlockKind::Text,
        BlockKind::Thinking => crate::kaijutsu_capnp::BlockKind::Thinking,
        BlockKind::ToolCall => crate::kaijutsu_capnp::BlockKind::ToolCall,
        BlockKind::ToolResult => crate::kaijutsu_capnp::BlockKind::ToolResult,
        BlockKind::Drift => crate::kaijutsu_capnp::BlockKind::Drift,
        BlockKind::File => crate::kaijutsu_capnp::BlockKind::File,
        BlockKind::Error => crate::kaijutsu_capnp::BlockKind::Error,
        BlockKind::Notification => crate::kaijutsu_capnp::BlockKind::Notification,
        BlockKind::Resource => crate::kaijutsu_capnp::BlockKind::Resource,
        BlockKind::Trace => crate::kaijutsu_capnp::BlockKind::Trace,
        BlockKind::Task => crate::kaijutsu_capnp::BlockKind::Task,
    }
}

/// RPC client wrapper
///
/// Holds the World capability bootstrapped from the server.
///
/// IMPORTANT: Must be created and used within a `tokio::task::LocalSet` context
/// because capnp-rpc's RpcSystem is not Send.
#[derive(Clone)]
pub struct RpcClient {
    world: world::Client,
    /// Aborts the RPC system task when the last RpcClient clone is dropped.
    /// This closes the underlying stream, causing the server to detect
    /// the disconnect and stop its FlowBus bridge task.
    _rpc_guard: RpcSystemGuard,
    /// Retained SSH session for clean disconnect and keepalive.
    /// Without this, no SSH_MSG_DISCONNECT can be sent on shutdown.
    #[allow(dead_code)]
    ssh_session: Option<std::rc::Rc<std::cell::RefCell<crate::ssh::SshClient>>>,
}

impl RpcClient {
    /// Initialize RPC over an SSH channel stream
    ///
    /// MUST be called within a `tokio::task::LocalSet::run_until()` context.
    pub async fn new(channel_stream: ChannelStream<Msg>) -> Result<Self, RpcError> {
        let compat_stream = TokioAsyncReadCompatExt::compat(channel_stream);
        Self::from_stream(compat_stream).await
    }

    /// Initialize RPC from any AsyncRead+AsyncWrite stream
    ///
    /// Useful for testing with Unix sockets or in-memory streams.
    pub async fn from_stream<S>(stream: S) -> Result<Self, RpcError>
    where
        S: futures::AsyncRead + futures::AsyncWrite + Unpin + 'static,
    {
        let (reader, writer) = stream.split();

        let rpc_network = Box::new(twoparty::VatNetwork::new(
            futures::io::BufReader::new(reader),
            futures::io::BufWriter::new(writer),
            rpc_twoparty_capnp::Side::Client,
            Default::default(),
        ));

        let mut rpc_system = RpcSystem::new(rpc_network, None);
        let world: world::Client = rpc_system.bootstrap(rpc_twoparty_capnp::Side::Server);

        // Spawn the RPC system to run in the background (requires LocalSet).
        // Store the abort handle so we can cancel it when the connection drops.
        let handle = tokio::task::spawn_local(rpc_system);
        let rpc_guard = RpcSystemGuard(std::rc::Rc::new(RpcSystemGuardInner(handle.abort_handle())));

        Ok(Self {
            world,
            _rpc_guard: rpc_guard,
            ssh_session: None,
        })
    }

    /// Retain the SSH session handle for clean disconnect and keepalive.
    pub fn retain_ssh_session(&mut self, ssh: crate::ssh::SshClient) {
        self.ssh_session = Some(std::rc::Rc::new(std::cell::RefCell::new(ssh)));
    }

    /// Get current identity from the server
    #[tracing::instrument(skip(self), name = "rpc_client.whoami")]
    pub async fn whoami(&self) -> Result<Identity, RpcError> {
        let request = self.world.whoami_request();
        let response = request.send().promise.await?;
        let identity = response.get()?.get_identity()?;

        Ok(Identity {
            username: identity.get_username()?.to_string()?,
            display_name: identity.get_display_name()?.to_string()?,
            // The server always stamps principalId now; an empty/invalid one is
            // a protocol error, not a thing to paper over with a wrong default.
            principal_id: PrincipalId::try_from_slice(identity.get_principal_id()?).ok_or_else(
                || RpcError::ServerError("whoami: missing/invalid principalId".to_string()),
            )?,
        })
    }

    /// List available kernels
    #[tracing::instrument(skip(self), name = "rpc_client.list_kernels")]
    pub async fn list_kernels(&self) -> Result<Vec<KernelInfo>, RpcError> {
        let request = self.world.list_kernels_request();
        let response = request.send().promise.await?;
        let kernels = response.get()?.get_kernels()?;

        let mut result = Vec::with_capacity(kernels.len() as usize);
        for kernel in kernels.iter() {
            result.push(parse_kernel_info(&kernel)?);
        }
        Ok(result)
    }

    /// Bind to the server's kernel — handshake getter that returns the
    /// shared kernel capability and its server-assigned ID. Despite the
    /// historical name `attachKernel`, no per-client state is attached;
    /// this is purely a capability handout. Real attach lifecycle lives
    /// at the rc-verb layer (`kj attach <ctx>`) and at `attach_peer`.
    #[tracing::instrument(skip(self), name = "rpc_client.bind_kernel")]
    pub async fn bind_kernel(&self) -> Result<(KernelHandle, KernelId), RpcError> {
        let mut request = self.world.bind_kernel_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        let kernel = reader.get_kernel()?;
        let kernel_id = parse_kernel_id(reader.get_kernel_id()?)?;

        Ok((KernelHandle { kernel }, kernel_id))
    }
}

// ============================================================================
// Types
// ============================================================================

#[derive(Debug, Clone)]
pub struct Identity {
    pub username: String,
    pub display_name: String,
    /// The authenticated principal (server-stamped). Lets a client know *its
    /// own* PrincipalId — needed e.g. to register a peer under its principal
    /// for principal-scoped kernel→app addressing.
    pub principal_id: PrincipalId,
}

#[derive(Debug, Clone)]
pub struct KernelInfo {
    pub id: KernelId,
    pub name: String,
    pub user_count: u32,
    pub agent_count: u32,
    pub contexts: Vec<ContextInfo>,
}

/// An attached peer, as reported by `listPeers`.
///
/// Mirrors the wire `PeerInfo` struct (kaijutsu.capnp), which currently
/// carries only `nick`/`attachedAt` — no `instance`, so two windows sharing a
/// nick are indistinguishable here (docs/issues.md tracks the schema gap).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub nick: String,
    /// Unix timestamp ms when the peer attached.
    pub attached_at: u64,
}

// ============================================================================
// Context Membership
// ============================================================================

/// Lightweight context membership — tracks what context we joined and as whom.
#[derive(Debug, Clone)]
pub struct ContextMembership {
    pub context_id: ContextId,
    pub kernel_id: KernelId,
    pub nick: String,
    pub instance: String,
}

/// Context within a kernel (rich info from ContextHandleInfo wire type)
///
/// `PartialEq` only, not `Eq` — `context_used_pct` is an `Option<f32>` and
/// `f32` isn't `Eq` (NaN isn't reflexive under `==`); the wire never
/// produces NaN here (only the `-1.0` sentinel or a real division), so
/// `PartialEq` is exact in practice.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextInfo {
    pub id: ContextId,
    pub label: String,
    pub forked_from: Option<ContextId>,
    pub provider: String,
    pub model: String,
    pub created_at: u64,
    /// Long-running OTel trace ID for this context (16 bytes, or zeros if unavailable).
    pub trace_id: [u8; 16],
    /// How this context was forked (e.g. "full", "shallow", "compact", "subtree").
    pub fork_kind: Option<String>,
    /// rc bucket / mode bundle that drives lifecycle-script dispatch (e.g.
    /// "default", "coder"). Empty on the wire is normalized to "default".
    pub context_type: String,
    /// Whether this context has been archived.
    pub archived: bool,
    /// Unix-millis of the explicit `conclude` act, or `None` if still open.
    /// Drives the time-well's lifecycle banding (recent-concluded vs haystack).
    pub concluded_at: Option<u64>,
    /// Synthesis keywords (empty if not yet synthesized).
    pub keywords: Vec<String>,
    /// Preview of the most representative block (empty if none).
    pub top_block_preview: Option<String>,
    /// Live activity, derived kernel-side from the context's block statuses:
    /// `Running` = actively working, `Error` = its most recent turn failed, else
    /// `Pending` (idle). Drives the time-well card pulse. Defaults to `Pending`.
    pub live_status: Status,
    /// Unix-millis of the most recent block append/mutation, or `None` if
    /// never set (0 on the wire). Drives the time-well's activity-recency
    /// ordering and idle-age bands (Stage 1).
    pub last_activity_at: Option<u64>,
    /// The track this context is attached to (docs/tracks.md), or `None` when
    /// unattached (empty on the wire — TrackIds are never empty). Drives the
    /// time-well's track rays + per-card beat lanes (Stage 3).
    pub track_id: Option<String>,
    /// Unix-millis of the explicit ring-0 ("active") promote, or `None` if
    /// never promoted (0 on the wire). First-write-wins server-side, so this
    /// stays stable across re-promotes. Drives the time-well's ring-0
    /// hand-curated seat row.
    pub promoted_at: Option<u64>,
    /// Unix-millis of the explicit push to the demoted ring, or `None` if
    /// never demoted (0 on the wire).
    pub demoted_at: Option<u64>,
    /// Unix-millis of the explicit "suspend activity" flag, or `None` if not
    /// paused (0 on the wire). Design-only for now — no behavioral gating is
    /// wired yet.
    pub paused_at: Option<u64>,
    /// Configured context-window size for the model that served the last
    /// completed call, or `None` when unconfigured (0 on the wire) — never a
    /// guessed denominator standing in for an unknown one.
    pub context_window: Option<u64>,
    /// Tokens filled by the LAST completed call (input + output), NOT a
    /// running total across turns. `None` when this context has never
    /// completed an LLM call (0 on the wire).
    pub context_used_tokens: Option<u64>,
    /// Percentage of `context_window` used by the last call (0-100+, never
    /// clamped), kernel-derived via the same helper `kj context info --json`
    /// uses. `None` when the window is unconfigured OR there's no usage yet
    /// (wire sentinel -1.0) — decoded here so no code above this boundary
    /// has to remember what -1.0 means.
    pub context_used_pct: Option<f32>,
    /// Background processes (`kaijutsu_kernel::background_exec`) currently
    /// `Running` in this context. `0` when none — the honest "nothing
    /// running" state, not a sentinel (kernel:
    /// `BackgroundRegistry::summary_by_context`).
    pub background_running_count: u32,
    /// Unix-millis `started_at` of the longest-running currently-`Running`
    /// background process, or `None` when nothing is running (0 on the
    /// wire) — the elapsed-time anchor the dock's "how long" formatting
    /// subtracts `now` from.
    pub background_oldest_running_started_at: Option<u64>,
    /// Unix-millis the most-recently-finished background process ended, or
    /// `None` when nothing has finished yet — including one that finished
    /// more than the kernel's `DEFAULT_RETENTION` ago and has since been
    /// reaped (the registry's own forgetting is this field's natural TTL,
    /// no separate expiry tracked on the wire).
    pub background_last_finished_at: Option<u64>,
    /// `"exited"` or `"killed"` for the process `background_last_finished_at`
    /// describes; `None` when nothing has finished (empty string on the
    /// wire).
    pub background_last_finished_status: Option<String>,
    /// Exit code for an `"exited"` finish; `None` for a `"killed"` finish or
    /// when nothing has finished (-1 sentinel on the wire — real exit
    /// codes, including the `128 + signal` convention, are never negative).
    pub background_last_exit_code: Option<i32>,
    /// The named model ensemble this context plays under (Track D,
    /// 2026-08-03), resolved kernel-side from `ContextRow::cast_id` — the
    /// wire ships the label, never a bare id. `None` when uncast (empty on
    /// the wire), same falls-through-to-registry-default semantics as an
    /// absent `provider`/`model` override.
    pub cast_label: Option<String>,
    /// Advisory hostname the registering client self-reported at creation
    /// (`ContextRow::origin_host`), or `None` when unknown (old client, a
    /// creation path with nothing to report, or a pre-migration row). Set
    /// once via `setContextOriginHost`; never overwritten by a later
    /// resume/attach from a different machine.
    pub origin_host: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KjCommandInfo {
    pub name: String,
    pub description: String,
    pub input_hint: String,
    pub argv_prefix: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KjExecutionResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub command_block_id: BlockId,
    pub latch: Option<KjLatch>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KjLatch {
    pub command: String,
    pub target: String,
    pub message: String,
}

/// Live state of one track (wire `TrackInfo`; docs/tracks.md) — read from the
/// beat scheduler's in-memory truth via `listTracks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInfo {
    /// The track's name (`[a-z0-9_-]`, never empty).
    pub id: String,
    /// The track's score context — a real, browsable context.
    pub score_context_id: ContextId,
    /// Whether the clock is rolling.
    pub playing: bool,
    /// Musical time (event-counted; freezes on pause).
    pub playhead_tick: i64,
    /// Beat period (the tempo knob), microseconds.
    pub period_us: u64,
    pub beats_per_phrase: u64,
    /// Beats elapsed while playing (resets on kernel restart).
    pub beat_count: u64,
    /// Wall clock (ns) of the most recent beat; 0 = never fired.
    pub last_epoch_ns: u64,
    /// Clock driver discriminator (`"system"` today, `"modeled"` at M3).
    pub clock_kind: String,
    /// Contexts currently bound to this track.
    pub attached: Vec<ContextId>,
}

/// A context returned by semantic search (`search_similar`) or neighbor lookup
/// (`get_neighbors`).
#[derive(Debug, Clone, PartialEq)]
pub struct SimilarContext {
    pub context_id: ContextId,
    /// Cosine similarity in `[0.0, 1.0]`.
    pub score: f32,
    /// Context label, or empty string if the kernel had none.
    pub label: String,
}

/// A semantic cluster of contexts (`get_clusters`).
#[derive(Debug, Clone, PartialEq)]
pub struct ContextCluster {
    pub cluster_id: u32,
    pub context_ids: Vec<ContextId>,
    /// Kernel-synthesized label (top shared keyword), or empty string if none.
    pub label: String,
}

/// Preset template info from the server.
///
/// `provider`/`model` are RETIRED (Track D, 2026-08-03): a preset narrows to
/// a cast reference instead of pinning a provider/model pair itself. The
/// wire's `@3`/`@4` ordinals stay declared-but-unused (capnp ordinals are
/// dense and never renumbered) — this mirror type simply drops the two
/// dead fields and adds `cast_label`.
#[derive(Debug, Clone)]
pub struct PresetInfo {
    pub id: Vec<u8>,
    pub label: String,
    pub description: String,
    /// The cast this preset assigns at fork/apply time, resolved
    /// kernel-side from `PresetRow::cast_id` — the wire ships the label,
    /// never a bare id. Empty = the preset moves no model knobs (the three
    /// factory presets' shape).
    pub cast_label: String,
}

#[derive(Debug, Clone)]
pub struct KernelConfig {
    pub name: String,
    pub consent_mode: ConsentMode,
    pub mounts: Vec<MountSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConsentMode {
    #[default]
    Collaborative,
    Autonomous,
}

#[derive(Debug, Clone)]
pub struct MountSpec {
    pub path: String,
    pub source: String,
    pub writable: bool,
}

/// Handle to a bound kernel capability returned by `bind_kernel`.
#[derive(Clone)]
pub struct KernelHandle {
    kernel: crate::kaijutsu_capnp::kernel::Client,
}

impl KernelHandle {
    /// Get kernel info
    #[tracing::instrument(skip(self), name = "rpc_client.get_info")]
    pub async fn get_info(&self) -> Result<KernelInfo, RpcError> {
        let mut request = self.kernel.get_info_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let info = response.get()?.get_info()?;
        parse_kernel_info(&info)
    }

    /// Cheap liveness probe. Returns `(kernel_id, server_time_ms)`.
    ///
    /// Used by the reconnect FSM's background liveness pinger. The handler
    /// is documented as "must not take per-context locks" so a wedge in
    /// LLM / drift / context machinery doesn't mask itself as ping success.
    /// A `kernel_id` mismatch vs. what the actor bound to means the server
    /// restarted under us — the FSM treats that as a hard reconnect signal.
    #[tracing::instrument(skip(self), name = "rpc_client.ping")]
    pub async fn ping(&self) -> Result<(KernelId, u64), RpcError> {
        let mut request = self.kernel.ping_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        let kernel_id = parse_kernel_id(reader.get_kernel_id()?)?;
        let server_time_ms = reader.get_server_time_ms();
        Ok((kernel_id, server_time_ms))
    }

    // =========================================================================
    // Context management
    // =========================================================================

    /// List all contexts in this kernel (includes drift info).
    #[tracing::instrument(skip(self), name = "rpc_client.list_contexts")]
    pub async fn list_contexts(&self) -> Result<Vec<ContextInfo>, RpcError> {
        let mut request = self.kernel.list_contexts_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let contexts = response.get()?.get_contexts()?;

        let mut result = Vec::with_capacity(contexts.len() as usize);
        for ctx in contexts.iter() {
            result.push(parse_context_info(&ctx)?);
        }
        Ok(result)
    }

    /// List every track's live state (docs/tracks.md). Empty when no tracks
    /// exist or the kernel runs without a beat scheduler.
    #[tracing::instrument(skip(self), name = "rpc_client.list_tracks")]
    pub async fn list_tracks(&self) -> Result<Vec<TrackInfo>, RpcError> {
        let mut request = self.kernel.list_tracks_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let tracks = response.get()?.get_tracks()?;

        let mut result = Vec::with_capacity(tracks.len() as usize);
        for t in tracks.iter() {
            result.push(parse_track_info(&t)?);
        }
        Ok(result)
    }

    /// Semantic search: find contexts similar to a free-text query.
    ///
    /// Returns up to `k` results ranked by cosine similarity. Empty when the
    /// kernel has no semantic index.
    #[tracing::instrument(skip(self, query), name = "rpc_client.search_similar")]
    pub async fn search_similar(
        &self,
        query: &str,
        k: u32,
    ) -> Result<Vec<SimilarContext>, RpcError> {
        let mut request = self.kernel.search_similar_request();
        request.get().set_query(query);
        request.get().set_k(k);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let results = response.get()?.get_results()?;
        let mut out = Vec::with_capacity(results.len() as usize);
        for r in results.iter() {
            out.push(parse_similar_context(&r)?);
        }
        Ok(out)
    }

    /// Find contexts semantically similar to a given context.
    ///
    /// Returns up to `k` neighbors ranked by cosine similarity. Empty when the
    /// kernel has no semantic index.
    #[tracing::instrument(skip(self), name = "rpc_client.get_neighbors")]
    pub async fn get_neighbors(
        &self,
        context_id: ContextId,
        k: u32,
    ) -> Result<Vec<SimilarContext>, RpcError> {
        let mut request = self.kernel.get_neighbors_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_k(k);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let results = response.get()?.get_results()?;
        let mut out = Vec::with_capacity(results.len() as usize);
        for r in results.iter() {
            out.push(parse_similar_context(&r)?);
        }
        Ok(out)
    }

    /// Group contexts into semantic clusters.
    ///
    /// Only clusters with at least `min_cluster_size` members are returned; each
    /// carries a kernel-synthesized [`label`](ContextCluster::label). Empty when
    /// the kernel has no semantic index.
    #[tracing::instrument(skip(self), name = "rpc_client.get_clusters")]
    pub async fn get_clusters(
        &self,
        min_cluster_size: u32,
    ) -> Result<Vec<ContextCluster>, RpcError> {
        let mut request = self.kernel.get_clusters_request();
        request.get().set_min_cluster_size(min_cluster_size);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let clusters = response.get()?.get_clusters()?;
        let mut out = Vec::with_capacity(clusters.len() as usize);
        for c in clusters.iter() {
            out.push(parse_context_cluster(&c)?);
        }
        Ok(out)
    }

    /// Create a new context with an optional label.
    ///
    /// Returns the server-assigned ContextId. The context is born with the
    /// `"default"` context_type — use [`create_context_typed`] to pick a mode
    /// (e.g. `"coder"`, `"mcp"`) whose rc lifecycle then runs on creation.
    #[tracing::instrument(skip(self), name = "rpc_client.create_context")]
    pub async fn create_context(&self, label: &str) -> Result<ContextId, RpcError> {
        self.create_context_typed(label, "").await
    }

    /// Create a new context with a label and an explicit `context_type`.
    ///
    /// The type selects which `/etc/rc/<context_type>/create/*` lifecycle
    /// scripts run server-side. An empty `context_type` is treated as
    /// `"default"` by the server.
    #[tracing::instrument(skip(self), name = "rpc_client.create_context_typed")]
    pub async fn create_context_typed(
        &self,
        label: &str,
        context_type: &str,
    ) -> Result<ContextId, RpcError> {
        let mut request = self.kernel.create_context_request();
        request.get().set_label(label);
        request.get().set_context_type(context_type);
        let response = request.send().promise.await?;
        parse_context_id(response.get()?.get_id()?)
    }

    /// Resolve a label straight against `KernelDb` — bypassing the
    /// DriftRouter that `list_contexts` reads. `None` means no context
    /// currently holds this label. See the capnp doc comment on
    /// `resolveContextLabel` for why this exists as a separate call rather
    /// than reusing `list_contexts`: an indexed single-row lookup instead of
    /// a kernel-wide scan, and it also reaches the one real registry gap
    /// (an archived context whose restart-time registration recovery skips
    /// it) that `list_contexts` can miss.
    #[tracing::instrument(skip(self), name = "rpc_client.resolve_context_label")]
    pub async fn resolve_context_label(&self, label: &str) -> Result<Option<ContextInfo>, RpcError> {
        let mut request = self.kernel.resolve_context_label_request();
        request.get().set_label(label);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        if !reader.get_found() {
            return Ok(None);
        }
        let info = reader.get_info()?;
        Ok(Some(parse_context_info(&info)?))
    }

    /// Join a context by ID.
    ///
    /// Returns the document_id for the joined context. The `instance` param
    /// identifies which client connected (for logging/debugging).
    #[tracing::instrument(skip(self), name = "rpc_client.join_context")]
    pub async fn join_context(
        &self,
        context_id: ContextId,
        instance: &str,
    ) -> Result<ContextId, RpcError> {
        let mut request = self.kernel.join_context_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_instance(instance);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        parse_context_id(response.get()?.get_context_id()?)
    }

    // kaish execution methods

    /// Execute code in the kernel
    #[tracing::instrument(skip(self, code), name = "rpc_client.execute")]
    pub async fn execute(&self, code: &str) -> Result<u64, RpcError> {
        let mut request = self.kernel.execute_request();
        request.get().set_code(code);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_exec_id())
    }

    /// Execute shell command with block output (kaish REPL mode)
    ///
    /// Creates ShellCommand and ShellOutput blocks in the specified cell.
    /// Output is streamed via block events.
    /// Returns the BlockId of the command block.
    #[tracing::instrument(skip(self, code), name = "rpc_client.shell_execute")]
    pub async fn shell_execute(
        &self,
        code: &str,
        context_id: ContextId,
        user_initiated: bool,
    ) -> Result<BlockId, RpcError> {
        let mut request = self.kernel.shell_execute_request();
        request.get().set_code(code);
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_user_initiated(user_initiated);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let block_id = response.get()?.get_command_block_id()?;
        parse_block_id(&block_id)
    }

    /// Toggle block exclusion from conversation hydration.
    ///
    /// Excluded blocks are displayed but omitted from LLM context.
    #[tracing::instrument(skip(self), name = "rpc_client.set_block_excluded")]
    pub async fn set_block_excluded(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        excluded: bool,
    ) -> Result<u64, RpcError> {
        let mut request = self.kernel.set_block_excluded_request();
        request.get().set_context_id(context_id.as_bytes());
        set_block_id_builder(&mut request.get().init_block_id(), block_id);
        request.get().set_excluded(excluded);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_ack_version())
    }

    /// Move a block to a new position. `after` is the block id to land
    /// after; `None` parks the block at the document beginning. Returns
    /// the resulting context version (ack).
    #[tracing::instrument(skip(self), name = "rpc_client.move_block")]
    pub async fn move_block(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        after: Option<&BlockId>,
    ) -> Result<u64, RpcError> {
        let mut request = self.kernel.move_block_request();
        request.get().set_context_id(context_id.as_bytes());
        set_block_id_builder(&mut request.get().init_block_id(), block_id);
        match after {
            Some(a) => {
                request.get().set_has_after(true);
                set_block_id_builder(&mut request.get().init_after(), a);
            }
            None => {
                request.get().set_has_after(false);
            }
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_ack_version())
    }

    /// Subscribe to output events from `execute()` RPCs.
    ///
    /// Returns an unbounded receiver that yields stdout, stderr, and exit code
    /// events tagged with their exec_id. The subscription is persistent for
    /// the lifetime of the RPC connection.
    #[tracing::instrument(skip(self), name = "rpc_client.subscribe_output")]
    pub async fn subscribe_output(
        &self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<crate::subscriptions::OutputEvent>, RpcError>
    {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let forwarder = crate::subscriptions::KernelOutputForwarder { tx };
        let callback: crate::kaijutsu_capnp::kernel_output::Client =
            capnp_rpc::new_client(forwarder);
        let mut request = self.kernel.subscribe_output_request();
        request.get().set_callback(callback);
        request.send().promise.await?;
        Ok(rx)
    }

    /// Interrupt an execution
    #[tracing::instrument(skip(self), name = "rpc_client.interrupt")]
    pub async fn interrupt(&self, exec_id: u64) -> Result<(), RpcError> {
        let mut request = self.kernel.interrupt_request();
        request.get().set_exec_id(exec_id);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        request.send().promise.await?;
        Ok(())
    }

    /// Get completions
    #[tracing::instrument(skip(self, partial), name = "rpc_client.complete")]
    pub async fn complete(&self, partial: &str, cursor: u32) -> Result<Vec<Completion>, RpcError> {
        let mut request = self.kernel.complete_request();
        request.get().set_partial(partial);
        request.get().set_cursor(cursor);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let completions = response.get()?.get_completions()?;

        let mut result = Vec::with_capacity(completions.len() as usize);
        for c in completions.iter() {
            result.push(Completion {
                text: c.get_text()?.to_string()?,
                display_text: c.get_display_text()?.to_string()?,
                kind: CompletionKind::from_capnp(c.get_kind()?),
            });
        }
        Ok(result)
    }

    /// Get command history
    #[tracing::instrument(skip(self), name = "rpc_client.get_command_history")]
    pub async fn get_command_history(&self, limit: u32) -> Result<Vec<HistoryEntry>, RpcError> {
        let mut request = self.kernel.get_command_history_request();
        request.get().set_limit(limit);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let entries = response.get()?.get_entries()?;

        let mut result = Vec::with_capacity(entries.len() as usize);
        for e in entries.iter() {
            result.push(HistoryEntry {
                id: e.get_id(),
                code: e.get_code()?.to_string()?,
                timestamp: e.get_timestamp(),
            });
        }
        Ok(result)
    }

    /// Detach from the kernel
    #[tracing::instrument(skip(self), name = "rpc_client.detach")]
    pub async fn detach(self) -> Result<(), RpcError> {
        let request = self.kernel.detach_peer_request();
        request.send().promise.await?;
        Ok(())
    }

    // =========================================================================
    // Block-based CRDT sync methods
    // =========================================================================

    /// Get document state (blocks and CRDT oplog)
    /// Compact a document's oplog, returning new size and sync generation.
    #[tracing::instrument(skip(self), name = "rpc_client.compact_context")]
    pub async fn compact_context(&self, context_id: ContextId) -> Result<(u64, u64), RpcError> {
        let mut request = self.kernel.compact_context_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let r = response.get()?;
        Ok((r.get_new_size(), r.get_generation()))
    }

    // =========================================================================
    // Block Queries (getBlocks / getContextSync)
    // =========================================================================

    /// Fetch blocks by query (all, byIds, or byFilter).
    ///
    /// Drops the version the kernel answered with. A caller that is joining
    /// this snapshot to a live subscription must use
    /// [`Self::get_blocks_versioned`] instead — without the version it cannot
    /// tell whether a buffered append is already included here, and applying
    /// one twice corrupts the text (docs/change-feed.md rule 24).
    #[tracing::instrument(skip(self), name = "rpc_client.get_blocks")]
    pub async fn get_blocks(
        &self,
        context_id: ContextId,
        query: &BlockQuery,
    ) -> Result<Vec<BlockSnapshot>, RpcError> {
        self.get_blocks_versioned(context_id, query)
            .await
            .map(|(blocks, _version)| blocks)
    }

    /// Fetch blocks by query together with the context version they were read
    /// at — the snapshot half of the change feed's recovery protocol
    /// (docs/change-feed.md rules 21-26).
    #[tracing::instrument(skip(self), name = "rpc_client.get_blocks_versioned")]
    pub async fn get_blocks_versioned(
        &self,
        context_id: ContextId,
        query: &BlockQuery,
    ) -> Result<(Vec<BlockSnapshot>, u64), RpcError> {
        let mut request = self.kernel.get_blocks_request();
        request.get().set_context_id(context_id.as_bytes());
        set_block_query_builder(request.get().init_query(), query);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let r = response.get()?;
        let version = r.get_version();
        let blocks_reader = r.get_blocks()?;
        let mut blocks = Vec::with_capacity(blocks_reader.len() as usize);
        for block in blocks_reader.iter() {
            blocks.push(parse_block_snapshot(&block)?);
        }
        Ok((blocks, version))
    }

    /// Fetch CRDT sync state (ops + version) without blocks.
    #[tracing::instrument(skip(self), name = "rpc_client.get_context_sync")]
    pub async fn get_context_sync(&self, context_id: ContextId) -> Result<SyncState, RpcError> {
        let mut request = self.kernel.get_context_sync_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let r = response.get()?;
        let context_id = parse_context_id(r.get_context_id()?)?;
        let ops = r.get_ops().map(|d| d.to_vec()).unwrap_or_default();
        let version = r.get_version();
        Ok(SyncState {
            context_id,
            ops,
            version,
        })
    }

    /// Fetch just the projected revision of a context's block document — the
    /// semantic counterpart to `get_context_sync`'s `version` field, without
    /// the oplog bytes. For callers that only need staleness/gap detection
    /// and would otherwise decode a `SyncState` just to throw the ops away.
    #[tracing::instrument(skip(self), name = "rpc_client.get_context_version")]
    pub async fn get_context_version(&self, context_id: ContextId) -> Result<u64, RpcError> {
        let mut request = self.kernel.get_context_version_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_version())
    }

    // =========================================================================
    // LLM operations
    // =========================================================================

    /// Send a prompt to the server-side LLM
    ///
    /// Returns a prompt ID that can be used to track the response.
    /// The response will be streamed via block events if subscribed.
    #[tracing::instrument(skip(self, content), name = "rpc_client.prompt")]
    pub async fn prompt(
        &self,
        content: &str,
        model: Option<&str>,
        context_id: ContextId,
    ) -> Result<String, RpcError> {
        let mut request = self.kernel.prompt_request();
        {
            let mut req = request.get().init_request();
            req.set_content(content);
            if let Some(m) = model {
                req.set_model(m);
            }
            req.set_context_id(context_id.as_bytes());
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_prompt_id()?.to_string()?)
    }

    /// Subscribe to block events (for LLM streaming updates)
    ///
    /// The callback will receive block insertions, edits, and other events.
    #[tracing::instrument(skip(self, callback), name = "rpc_client.subscribe_blocks")]
    pub async fn subscribe_blocks(
        &self,
        callback: crate::kaijutsu_capnp::block_events::Client,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.subscribe_blocks_request();
        request.get().set_callback(callback);
        request.send().promise.await?;
        Ok(())
    }

    /// Subscribe to block events with server-side filtering.
    ///
    /// `instance` is the client's stable per-session UUID. The server uses
    /// `(principal, instance)` as a dedupe key — a new subscribe from the
    /// same instance replaces the prior live subscription instead of stacking.
    /// Passing an empty string disables instance-based dedupe (older callers).
    #[tracing::instrument(
        skip(self, callback, filter),
        name = "rpc_client.subscribe_blocks_filtered"
    )]
    pub async fn subscribe_blocks_filtered(
        &self,
        callback: crate::kaijutsu_capnp::block_events::Client,
        filter: &kaijutsu_types::BlockEventFilter,
        instance: &str,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.subscribe_blocks_filtered_request();
        {
            let mut params = request.get();
            params.set_callback(callback);
            let mut fb = params.reborrow().init_filter();
            set_block_event_filter_builder(&mut fb, filter);
            params.set_instance(instance);
        }
        request.send().promise.await?;
        Ok(())
    }

    /// Declare what this client's push subscriptions can handle.
    ///
    /// Call BEFORE subscribing — a server-side bridge reads the flags once,
    /// when it starts. Opt-in by design: a kernel that never hears from us
    /// sends the conservative event shapes, which is exactly what keeps an
    /// older binary working against a newer kernel and vice versa.
    ///
    /// This build implements both `onBlockTextOpsBatch` (coalesced text ops)
    /// and `onSubscriptionTerminated` (the lag kick), so it declares both.
    #[tracing::instrument(skip(self), name = "rpc_client.declare_event_capabilities")]
    pub async fn declare_event_capabilities(
        &self,
        text_ops_batch: bool,
        subscription_terminated: bool,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.declare_event_capabilities_request();
        {
            let mut params = request.get();
            params.set_text_ops_batch(text_ops_batch);
            params.set_subscription_terminated(subscription_terminated);
        }
        request.send().promise.await?;
        Ok(())
    }

    // =========================================================================
    // In-app editor sessions (the vi/edit builtin; see docs/vi.md)
    // =========================================================================

    /// Open an editor session on `path`, binding to the CRDT block that owns its
    /// text. Returns the initial state (carrying the new session id).
    #[tracing::instrument(skip(self), name = "rpc_client.editor_open")]
    pub async fn editor_open(&self, path: &str) -> Result<EditorState, RpcError> {
        let mut request = self.kernel.editor_open_request();
        request.get().set_path(path);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        parse_editor_state(response.get()?.get_state()?)
    }

    /// Feed vim-notation keys to an open session; returns the new state.
    #[tracing::instrument(skip(self), name = "rpc_client.editor_keys")]
    pub async fn editor_keys(&self, session_id: u64, keys: &str) -> Result<EditorState, RpcError> {
        let mut request = self.kernel.editor_keys_request();
        request.get().set_session_id(session_id);
        request.get().set_keys(keys);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        parse_editor_state(response.get()?.get_state()?)
    }

    /// Read the current state of an open session.
    #[tracing::instrument(skip(self), name = "rpc_client.editor_state")]
    pub async fn editor_state(&self, session_id: u64) -> Result<EditorState, RpcError> {
        let mut request = self.kernel.editor_state_request();
        request.get().set_session_id(session_id);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        parse_editor_state(response.get()?.get_state()?)
    }

    /// `ZZ` — checkpoint the session's buffer as saved; returns the clean state.
    #[tracing::instrument(skip(self), name = "rpc_client.editor_save")]
    pub async fn editor_save(&self, session_id: u64) -> Result<EditorState, RpcError> {
        let mut request = self.kernel.editor_save_request();
        request.get().set_session_id(session_id);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        parse_editor_state(response.get()?.get_state()?)
    }

    /// `ZQ` — roll the block back to the session's checkpoint and close it.
    #[tracing::instrument(skip(self), name = "rpc_client.editor_quit")]
    pub async fn editor_quit(&self, session_id: u64) -> Result<(), RpcError> {
        let mut request = self.kernel.editor_quit_request();
        request.get().set_session_id(session_id);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        request.send().promise.await?;
        Ok(())
    }

    /// Subscribe to editor-session state pushes (the in-app vi editor channel).
    /// The server streams `EditorState` changes (and future remote merges) to
    /// `callback` until the connection drops.
    #[tracing::instrument(skip(self, callback), name = "rpc_client.subscribe_editor")]
    pub async fn subscribe_editor(
        &self,
        callback: crate::kaijutsu_capnp::editor_events::Client,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.subscribe_editor_request();
        request.get().set_callback(callback);
        request.send().promise.await?;
        Ok(())
    }

    /// Subscribe to turn-outcome pushes — the server streams every turn's
    /// terminal event (completed, with its structured stop reason, or failed)
    /// to `callback` until the connection drops.
    ///
    /// Kernel-wide, following `subscribe_editor`: the event names its own
    /// context, so one subscription covers every context this client watches.
    /// See [`crate::subscriptions::turn_events_channel`] for building the
    /// callback client. This is what replaces inferring turn completion from
    /// block-status polling.
    #[tracing::instrument(skip(self, callback), name = "rpc_client.subscribe_turn_events")]
    pub async fn subscribe_turn_events(
        &self,
        callback: crate::kaijutsu_capnp::turn_events::Client,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.subscribe_turn_events_request();
        request.get().set_callback(callback);
        request.send().promise.await?;
        Ok(())
    }

    // =========================================================================
    // MCP Tool operations
    // =========================================================================

    /// Execute a tool via the kernel's tool registry.
    ///
    /// This is the general-purpose tool execution path (executeTool @16).
    /// Tools include git, drift, and any registered execution engines.
    #[tracing::instrument(skip(self, params), name = "rpc_client.execute_tool")]
    pub async fn execute_tool(&self, tool: &str, params: &str) -> Result<ToolResult, RpcError> {
        let mut request = self.kernel.execute_tool_request();
        {
            let mut call = request.get().init_call();
            call.set_tool(tool);
            call.set_params(params);
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let result = response.get()?.get_result()?;

        Ok(ToolResult {
            request_id: result.get_request_id()?.to_string()?,
            success: result.get_success(),
            output: result.get_output()?.to_string()?,
        })
    }

    /// Get schemas for all registered kernel tools.
    #[tracing::instrument(skip(self), name = "rpc_client.get_tool_schemas")]
    pub async fn get_tool_schemas(&self) -> Result<Vec<ToolSchema>, RpcError> {
        let mut request = self.kernel.get_tool_schemas_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let schemas = response.get()?.get_schemas()?;

        let mut result = Vec::with_capacity(schemas.len() as usize);
        for i in 0..schemas.len() {
            let s = schemas.get(i);
            result.push(ToolSchema {
                name: s.get_name()?.to_string()?,
                description: s.get_description()?.to_string()?,
                category: s.get_category()?.to_string()?,
                input_schema: s.get_input_schema()?.to_string()?,
            });
        }
        Ok(result)
    }

    /// Call an MCP tool.
    ///
    /// Tool name is resolved against the calling context's binding.
    #[tracing::instrument(skip(self, arguments), name = "rpc_client.call_mcp_tool")]
    pub async fn call_mcp_tool(
        &self,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> Result<McpToolResult, RpcError> {
        let mut request = self.kernel.call_mcp_tool_request();
        {
            let mut call = request.get().init_call();
            call.set_tool(tool);
            call.set_arguments(
                &serde_json::to_string(arguments).map_err(|e| {
                    RpcError::Other(format!("Failed to serialize MCP arguments: {e}"))
                })?,
            );
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let result = response.get()?.get_result()?;

        Ok(McpToolResult {
            content: result.get_content()?.to_string()?,
            is_error: result.get_is_error(),
        })
    }

    // =========================================================================
    // MCP Resource operations (push-first with caching)
    // =========================================================================

    /// List resources from an MCP server
    ///
    /// Returns a list of resources available from the specified server.
    /// Results may be cached on the server for efficiency.
    #[tracing::instrument(skip(self), name = "rpc_client.list_mcp_resources")]
    pub async fn list_mcp_resources(&self, server: &str) -> Result<Vec<McpResource>, RpcError> {
        let mut request = self.kernel.list_mcp_resources_request();
        request.get().set_server(server);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let resources = response.get()?.get_resources()?;

        let mut result = Vec::with_capacity(resources.len() as usize);
        for r in resources.iter() {
            result.push(McpResource {
                uri: r.get_uri()?.to_string()?,
                name: r.get_name()?.to_string()?,
                description: if r.get_has_description() {
                    Some(r.get_description()?.to_string()?)
                } else {
                    None
                },
                mime_type: if r.get_has_mime_type() {
                    Some(r.get_mime_type()?.to_string()?)
                } else {
                    None
                },
            });
        }
        Ok(result)
    }

    /// Subscribe to MCP resource events.
    ///
    /// `instance` is the client's stable per-session UUID — see
    /// `subscribe_blocks_filtered` for dedupe semantics.
    #[tracing::instrument(skip(self, callback), name = "rpc_client.subscribe_mcp_resources")]
    pub async fn subscribe_mcp_resources(
        &self,
        callback: crate::kaijutsu_capnp::resource_events::Client,
        instance: &str,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.subscribe_mcp_resources_request();
        {
            let mut params = request.get();
            params.set_callback(callback);
            params.set_instance(instance);
        }
        request.send().promise.await?;
        Ok(())
    }

    /// Subscribe to MCP elicitation requests.
    ///
    /// `instance` is the client's stable per-session UUID — see
    /// `subscribe_blocks_filtered` for dedupe semantics. Elicitation
    /// subscriptions are per-connection on the server today, so dedupe is
    /// a no-op there; the parameter exists for protocol uniformity.
    #[tracing::instrument(skip(self, callback), name = "rpc_client.subscribe_mcp_elicitations")]
    pub async fn subscribe_mcp_elicitations(
        &self,
        callback: crate::kaijutsu_capnp::elicitation_events::Client,
        instance: &str,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.subscribe_mcp_elicitations_request();
        {
            let mut params = request.get();
            params.set_callback(callback);
            params.set_instance(instance);
        }
        request.send().promise.await?;
        Ok(())
    }

    /// Subscribe to permission asks (`HookAction::Ask`, D-57, docs/acp.md
    /// gap #2).
    ///
    /// Kernel-wide, following `subscribe_turn_events` rather than the
    /// per-connection `subscribe_mcp_elicitations` model: an Ask can fire
    /// from any call path, not just the connection that triggered it, so
    /// there's no `instance` dedupe parameter — one subscription serves
    /// every context. The server calls `onAsk` on this callback and blocks
    /// the hooked call on the response, bounded by its own timeout
    /// (`kaijutsu-kernel`'s `mcp::permission` module); no subscriber
    /// attached (or no answer in time) fails the call closed on the
    /// server side, never hangs the client.
    #[tracing::instrument(skip(self, callback), name = "rpc_client.subscribe_permission_events")]
    pub async fn subscribe_permission_events(
        &self,
        callback: crate::kaijutsu_capnp::permission_events::Client,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.subscribe_permission_events_request();
        request.get().set_callback(callback);
        request.send().promise.await?;
        Ok(())
    }

    // =========================================================================
    // Timeline / Fork operations
    // =========================================================================

    /// Cherry-pick a block from one context into another.
    ///
    /// Returns the new block ID in the target context.
    #[tracing::instrument(skip(self), name = "rpc_client.cherry_pick_block")]
    pub async fn cherry_pick_block(
        &self,
        block_id: &BlockId,
        target_context: ContextId,
    ) -> Result<BlockId, RpcError> {
        let mut request = self.kernel.cherry_pick_block_request();
        {
            let mut params = request.get();
            let mut source = params.reborrow().init_source_block_id();
            source.set_context_id(block_id.context_id.as_bytes());
            source.set_principal_id(block_id.principal_id.as_bytes());
            source.set_seq(block_id.seq);
            params.set_target_context_id(target_context.as_bytes());
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let new_block = response.get()?.get_new_block_id()?;
        parse_block_id(&new_block)
    }

    /// Get document history (version snapshots).
    ///
    /// Returns a list of version snapshots for timeline navigation.
    #[tracing::instrument(skip(self), name = "rpc_client.get_context_history")]
    pub async fn get_context_history(
        &self,
        context_id: ContextId,
        limit: u32,
    ) -> Result<Vec<VersionSnapshot>, RpcError> {
        let mut request = self.kernel.get_context_history_request();
        {
            let mut params = request.get();
            params.set_context_id(context_id.as_bytes());
            params.set_limit(limit);
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let snapshots = response.get()?.get_snapshots()?;

        let mut result = Vec::with_capacity(snapshots.len() as usize);
        for snap in snapshots.iter() {
            result.push(VersionSnapshot {
                version: snap.get_version(),
                timestamp: snap.get_timestamp(),
                block_count: snap.get_block_count(),
                change_kind: match snap.get_change_kind()? {
                    crate::kaijutsu_capnp::ChangeKind::BlockAdded => "block_added".to_string(),
                    crate::kaijutsu_capnp::ChangeKind::BlockDeleted => "block_deleted".to_string(),
                    crate::kaijutsu_capnp::ChangeKind::Edit => "edit".to_string(),
                    crate::kaijutsu_capnp::ChangeKind::StatusChange => "status_change".to_string(),
                },
            });
        }
        Ok(result)
    }

    // ========================================================================
    // Drift: Cross-Context Communication
    // ========================================================================

    /// Get this kernel's context ID and label.
    #[tracing::instrument(skip(self), name = "rpc_client.get_context_id")]
    pub async fn get_context_id(&self) -> Result<(ContextId, String), RpcError> {
        let mut request = self.kernel.get_context_id_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        let id = parse_context_id(reader.get_id()?)?;
        let label = reader.get_label()?.to_string()?;
        Ok((id, label))
    }

    /// Set the LLM provider and model for a specific context.
    #[tracing::instrument(skip(self), name = "rpc_client.set_context_model")]
    pub async fn set_context_model(
        &self,
        context_id: ContextId,
        provider: &str,
        model: &str,
    ) -> Result<bool, RpcError> {
        let mut request = self.kernel.configure_llm_request();
        {
            let mut params = request.get();
            params.set_provider(provider);
            params.set_model(model);
            params.set_context_id(context_id.as_bytes());
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        if reader.get_success() {
            Ok(true)
        } else {
            let error = reader.get_error()?.to_string()?;
            Err(RpcError::ServerError(error))
        }
    }

    /// View the drift staging queue.
    #[tracing::instrument(skip(self), name = "rpc_client.drift_queue")]
    pub async fn drift_queue(&self) -> Result<Vec<StagedDriftInfo>, RpcError> {
        let request = self.kernel.drift_queue_request();
        let response = request.send().promise.await?;
        let staged = response.get()?.get_staged()?;

        let mut result = Vec::with_capacity(staged.len() as usize);
        for entry in staged.iter() {
            let dk = match entry.get_drift_kind()? {
                crate::kaijutsu_capnp::DriftKind::Push => DriftKind::Push,
                crate::kaijutsu_capnp::DriftKind::Pull => DriftKind::Pull,
                crate::kaijutsu_capnp::DriftKind::Merge => DriftKind::Merge,
                crate::kaijutsu_capnp::DriftKind::Distill => DriftKind::Distill,
                crate::kaijutsu_capnp::DriftKind::Notification => DriftKind::Notification,
                crate::kaijutsu_capnp::DriftKind::Fork => DriftKind::Fork,
            };
            result.push(StagedDriftInfo {
                id: entry.get_id(),
                source_ctx: parse_context_id(entry.get_source_ctx()?)?,
                target_ctx: parse_context_id(entry.get_target_ctx()?)?,
                content: entry.get_content()?.to_string()?,
                source_model: entry.get_source_model()?.to_string()?,
                drift_kind: dk,
                created_at: entry.get_created_at(),
            });
        }
        Ok(result)
    }

    /// Cancel a staged drift.
    #[tracing::instrument(skip(self), name = "rpc_client.drift_cancel")]
    pub async fn drift_cancel(&self, staged_id: u64) -> Result<bool, RpcError> {
        let mut request = self.kernel.drift_cancel_request();
        request.get().set_staged_id(staged_id);
        let response = request.send().promise.await?;
        Ok(response.get()?.get_success())
    }

    /// Rename a context's human-friendly label.
    #[tracing::instrument(skip(self), name = "rpc_client.rename_context")]
    pub async fn rename_context(&self, context_id: ContextId, label: &str) -> Result<(), RpcError> {
        let mut request = self.kernel.rename_context_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_label(label);
        request.send().promise.await?;
        Ok(())
    }

    /// Conclude a context — the explicit "this work is done" act. Sets the
    /// context to the `concluded` lifecycle state and stamps `concludedAt`
    /// server-side. Idempotent (re-concluding succeeds without restamping).
    /// Returns a [`RpcError::ServerError`] with the server's message when the
    /// context can't be concluded (unknown / archived).
    #[tracing::instrument(skip(self), name = "rpc_client.conclude")]
    pub async fn conclude(&self, context_id: ContextId) -> Result<(), RpcError> {
        let mut request = self.kernel.conclude_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        if reader.get_success() {
            Ok(())
        } else {
            let msg = reader.get_error()?.to_str().unwrap_or("conclude failed");
            Err(RpcError::ServerError(msg.to_string()))
        }
    }

    /// Promote a context into ring 0 ("active"). First-write-wins server-side
    /// — re-promoting an already-promoted context is a no-op success.
    /// Promoting an ARCHIVED context resurrects it (unarchives + seats it;
    /// promote is the resurrection door). Returns a
    /// [`RpcError::ServerError`] when the active ring is full (10
    /// seats) — ring 0 is a hand-curated row, so seats never appear or
    /// vanish without an explicit act.
    #[tracing::instrument(skip(self), name = "rpc_client.promote_context")]
    pub async fn promote_context(&self, context_id: ContextId) -> Result<(), RpcError> {
        let mut request = self.kernel.promote_context_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        if reader.get_success() {
            Ok(())
        } else {
            let msg = reader
                .get_error()?
                .to_str()
                .unwrap_or("promote_context failed");
            Err(RpcError::ServerError(msg.to_string()))
        }
    }

    /// Push a context outward one step on the demote ladder (kernel-owned
    /// policy): promoted → unpromoted (automatic placement); neither
    /// promoted nor demoted → demoted; already demoted → archived (single
    /// context, no subtree recursion, no latch).
    #[tracing::instrument(skip(self), name = "rpc_client.demote_context")]
    pub async fn demote_context(&self, context_id: ContextId) -> Result<(), RpcError> {
        let mut request = self.kernel.demote_context_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        if reader.get_success() {
            Ok(())
        } else {
            let msg = reader
                .get_error()?
                .to_str()
                .unwrap_or("demote_context failed");
            Err(RpcError::ServerError(msg.to_string()))
        }
    }

    /// Set or clear a context's "suspend activity" flag. Design-only for now
    /// — persisted and exposed on the wire, but not yet wired to any
    /// behavioral gating.
    #[tracing::instrument(skip(self), name = "rpc_client.set_context_paused")]
    pub async fn set_context_paused(
        &self,
        context_id: ContextId,
        paused: bool,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.set_context_paused_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_paused(paused);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        if reader.get_success() {
            Ok(())
        } else {
            let msg = reader
                .get_error()?
                .to_str()
                .unwrap_or("set_context_paused failed");
            Err(RpcError::ServerError(msg.to_string()))
        }
    }

    /// Set (or clear, on `""`) a context's advisory `origin_host` — the
    /// registering client's own hostname. Called once, right after context
    /// creation, by a client that knows its own host (e.g.
    /// `register_session` on the kaijutsu-mcp side); every other creation
    /// path (fork, genesis bootstrap, the app's `create_context_typed`) is
    /// unaffected and simply never calls this. Shared-trust model: advisory
    /// metadata, not auth.
    #[tracing::instrument(skip(self), name = "rpc_client.set_context_origin_host")]
    pub async fn set_context_origin_host(
        &self,
        context_id: ContextId,
        origin_host: &str,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.set_context_origin_host_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_origin_host(origin_host);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        if reader.get_success() {
            Ok(())
        } else {
            let msg = reader
                .get_error()?
                .to_str()
                .unwrap_or("set_context_origin_host failed");
            Err(RpcError::ServerError(msg.to_string()))
        }
    }

    /// Author one block over RPC — no CRDT replication required.
    ///
    /// The client half of migration step 3
    /// (`docs/crdt-position-2026-08.md`). See [`AuthorBlock`] for the field
    /// meanings; the reservation/flow split is documented on the schema.
    #[tracing::instrument(skip(self, req), name = "rpc_client.author_block")]
    pub async fn author_block(&self, req: &AuthorBlock) -> Result<BlockId, RpcError> {
        let mut request = self.kernel.author_block_request();
        {
            let mut b = request.get();
            b.set_context_id(req.context_id.as_bytes());
            b.set_principal_id(req.principal_id.as_bytes());
            b.set_role(role_to_capnp(req.role));
            b.set_kind(block_kind_to_capnp(req.kind));
            b.set_status(status_to_capnp(req.status));
            b.set_content(&req.content);
            b.set_content_type(req.content_type.as_deref().unwrap_or(""));
            b.set_tool_name(req.tool_name.as_deref().unwrap_or(""));
            let tool_input = req
                .tool_input
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default();
            b.set_tool_input(&tool_input);
            b.set_has_tool_kind(req.tool_kind.is_some());
            if let Some(tk) = req.tool_kind {
                b.set_tool_kind(tool_kind_to_capnp(tk));
            }
            b.set_has_parent_id(req.parent_id.is_some());
            if let Some(ref id) = req.parent_id {
                let mut p = b.reborrow().init_parent_id();
                p.set_context_id(id.context_id.as_bytes());
                p.set_principal_id(id.principal_id.as_bytes());
                p.set_seq(id.seq);
            }
            b.set_has_after_id(req.after_id.is_some());
            if let Some(ref id) = req.after_id {
                let mut a = b.reborrow().init_after_id();
                a.set_context_id(id.context_id.as_bytes());
                a.set_principal_id(id.principal_id.as_bytes());
                a.set_seq(id.seq);
            }
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = b.init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        let err = reader.get_error()?.to_str().unwrap_or("");
        if !err.is_empty() {
            return Err(RpcError::ServerError(err.to_string()));
        }
        let id = reader.get_block_id()?;
        Ok(BlockId {
            context_id: ContextId::try_from_slice(id.get_context_id()?)
                .ok_or_else(|| RpcError::ServerError("invalid context_id".into()))?,
            principal_id: PrincipalId::try_from_slice(id.get_principal_id()?)
                .ok_or_else(|| RpcError::ServerError("invalid principal_id".into()))?,
            seq: id.get_seq(),
        })
    }

    /// Move an already-authored block to a terminal state — the flow half of
    /// reserve-then-flow.
    #[tracing::instrument(skip(self), name = "rpc_client.complete_block")]
    pub async fn complete_block(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        status: Status,
        is_error: bool,
        exit_code: Option<i32>,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.complete_block_request();
        {
            let mut b = request.get();
            b.set_context_id(context_id.as_bytes());
            b.set_status(status_to_capnp(status));
            b.set_is_error(is_error);
            b.set_has_exit_code(exit_code.is_some());
            b.set_exit_code(exit_code.unwrap_or(0));
            {
                let mut id = b.reborrow().init_block_id();
                id.set_context_id(block_id.context_id.as_bytes());
                id.set_principal_id(block_id.principal_id.as_bytes());
                id.set_seq(block_id.seq);
            }
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = b.init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        if reader.get_success() {
            Ok(())
        } else {
            let msg = reader.get_error()?.to_str().unwrap_or("complete_block failed");
            Err(RpcError::ServerError(msg.to_string()))
        }
    }

    /// Archive a single context — the well's single-keystroke archive
    /// action. Unlike the `kj context archive` builtin (latched, recurses
    /// into structural children), this is single-context, not latched, no
    /// subtree recursion. Idempotent: archiving an already-archived context
    /// succeeds.
    #[tracing::instrument(skip(self), name = "rpc_client.archive_context")]
    pub async fn archive_context(&self, context_id: ContextId) -> Result<(), RpcError> {
        let mut request = self.kernel.archive_context_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        if reader.get_success() {
            Ok(())
        } else {
            let msg = reader
                .get_error()?
                .to_str()
                .unwrap_or("archive_context failed");
            Err(RpcError::ServerError(msg.to_string()))
        }
    }

    // ========================================================================
    // LLM Configuration
    // ========================================================================

    /// Get current LLM configuration
    #[tracing::instrument(skip(self), name = "rpc_client.get_llm_config")]
    pub async fn get_llm_config(&self) -> Result<LlmConfigInfo, RpcError> {
        let mut request = self.kernel.get_llm_config_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let config = response.get()?.get_config()?;

        let providers_reader = config.get_providers()?;
        let mut providers = Vec::with_capacity(providers_reader.len() as usize);
        for p in providers_reader.iter() {
            let models: Vec<String> = if p.has_models() {
                p.get_models()?
                    .iter()
                    .filter_map(|m| m.ok().and_then(|s| s.to_string().ok()))
                    .collect()
            } else {
                Vec::new()
            };
            providers.push(LlmProviderInfo {
                name: p.get_name()?.to_string()?,
                default_model: p.get_default_model()?.to_string()?,
                available: p.get_available(),
                models,
            });
        }

        Ok(LlmConfigInfo {
            default_provider: config.get_default_provider()?.to_string()?,
            default_model: config.get_default_model()?.to_string()?,
            providers,
        })
    }

    /// Read a CRDT-owned config file's content (e.g. `theme.toml`). The kernel
    /// is the sole owner; this is how out-of-kernel surfaces (the app) read
    /// config without touching a host file. Returns the content on success or a
    /// `ServerError` carrying the kernel's message.
    #[tracing::instrument(skip(self), name = "rpc_client.get_config")]
    pub async fn get_config(&self, path: &str) -> Result<String, RpcError> {
        let mut request = self.kernel.get_config_request();
        request.get().set_path(path);
        let response = request.send().promise.await?;
        let reader = response.get()?;
        let error = reader.get_error()?.to_str().unwrap_or("");
        if !error.is_empty() {
            return Err(RpcError::ServerError(error.to_string()));
        }
        Ok(reader.get_content()?.to_string()?)
    }

    /// Set the default LLM provider
    #[tracing::instrument(skip(self), name = "rpc_client.set_default_provider")]
    pub async fn set_default_provider(&self, provider: &str) -> Result<bool, RpcError> {
        let mut request = self.kernel.set_default_provider_request();
        request.get().set_provider(provider);
        let response = request.send().promise.await?;
        let result = response.get()?;
        if !result.get_success() {
            let error = result.get_error()?.to_str()?;
            if !error.is_empty() {
                return Err(RpcError::ServerError(error.to_string()));
            }
        }
        Ok(result.get_success())
    }

    /// Set the default model for a provider
    #[tracing::instrument(skip(self), name = "rpc_client.set_default_model")]
    pub async fn set_default_model(&self, provider: &str, model: &str) -> Result<bool, RpcError> {
        let mut request = self.kernel.set_default_model_request();
        request.get().set_provider(provider);
        request.get().set_model(model);
        let response = request.send().promise.await?;
        let result = response.get()?;
        if !result.get_success() {
            let error = result.get_error()?.to_str()?;
            if !error.is_empty() {
                return Err(RpcError::ServerError(error.to_string()));
            }
        }
        Ok(result.get_success())
    }

    // =========================================================================
    // Context Interrupt
    // =========================================================================

    /// Interrupt a running LLM stream or shell jobs for a context.
    ///
    /// `immediate=false` → soft interrupt (stop after current tool turn).
    /// `immediate=true`  → hard interrupt (abort stream + kill kaish jobs).
    /// Returns `false` when the context has no active stream (no-op).
    #[tracing::instrument(skip(self), name = "rpc_client.interrupt_context")]
    pub async fn interrupt_context(
        &self,
        context_id: ContextId,
        immediate: bool,
    ) -> Result<bool, RpcError> {
        let mut request = self.kernel.interrupt_context_request();
        {
            let mut params = request.get();
            params.set_context_id(context_id.as_bytes());
            params.set_immediate(immediate);
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_success())
    }

    /// List all presets for this kernel.
    pub async fn list_presets(&self) -> Result<Vec<PresetInfo>, RpcError> {
        let mut request = self.kernel.list_presets_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let presets = response.get()?.get_presets()?;

        let mut result = Vec::with_capacity(presets.len() as usize);
        for p in presets.iter() {
            result.push(PresetInfo {
                id: p.get_id()?.to_vec(),
                label: p.get_label()?.to_string()?,
                description: p.get_description()?.to_string()?,
                cast_label: p.get_cast_label()?.to_string()?,
            });
        }
        Ok(result)
    }

    // =========================================================================
    // Peer Invocation
    // =========================================================================

    /// Attach as a peer with a commands callback.
    ///
    /// The `invocation_tx` sender receives incoming invocations from the
    /// kernel. Use `std::sync::mpsc` so the receiver can be polled from
    /// any executor (including Bevy's non-tokio task pool).
    #[tracing::instrument(skip(self, config, invocation_tx), name = "rpc_client.attach_peer")]
    pub async fn attach_peer(
        &self,
        config: &crate::actor::PeerConfig,
        invocation_tx: std::sync::mpsc::Sender<crate::actor::PeerInvocation>,
    ) -> Result<crate::actor::PeerAttachResult, RpcError> {
        // Create capnp server for the callback
        let commands_impl = PeerCommandsImpl { tx: invocation_tx };
        let commands_client: crate::kaijutsu_capnp::peer_commands::Client =
            capnp_rpc::new_client(commands_impl);

        let mut request = self.kernel.attach_peer_request();
        {
            let mut cfg = request.get().init_config();
            cfg.set_nick(&config.nick);
            cfg.set_instance(&config.instance);
            request.get().set_commands(commands_client);
        }

        let response = request.send().promise.await?;
        let info = response.get()?.get_info()?;
        let result = crate::actor::PeerAttachResult {
            nick: info.get_nick()?.to_string()?,
        };

        Ok(result)
    }

    /// List all peers currently attached to this kernel.
    #[tracing::instrument(skip(self), name = "rpc_client.list_peers")]
    pub async fn list_peers(&self) -> Result<Vec<PeerInfo>, RpcError> {
        let request = self.kernel.list_peers_request();
        let response = request.send().promise.await?;
        let peers = response.get()?.get_peers()?;

        let mut result = Vec::with_capacity(peers.len() as usize);
        for p in peers.iter() {
            result.push(parse_peer_info(&p)?);
        }
        Ok(result)
    }

    /// Invoke another peer through the kernel.
    #[tracing::instrument(skip(self, params), name = "rpc_client.invoke_peer")]
    pub async fn invoke_peer(
        &self,
        nick: &str,
        action: &str,
        params: &[u8],
    ) -> Result<Vec<u8>, RpcError> {
        let mut request = self.kernel.invoke_peer_request();
        {
            let mut p = request.get();
            p.set_nick(nick);
            p.set_action(action);
            p.set_params(params);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_result()?.to_vec())
    }

    // =========================================================================
    // Addressed Shell State / Shell Variable Introspection
    // =========================================================================

    /// Read the durable cwd of `context_id`, independent of this connection's
    /// currently joined context.
    #[tracing::instrument(skip(self), name = "rpc_client.get_context_cwd")]
    pub async fn get_context_cwd(&self, context_id: ContextId) -> Result<Option<String>, RpcError> {
        let mut request = self.kernel.get_context_cwd_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let result = response.get()?;
        if result.get_found() {
            Ok(Some(result.get_path()?.to_string()?))
        } else {
            Ok(None)
        }
    }

    /// Validate and persist `path` as `context_id`'s durable cwd, independent
    /// of this connection's currently joined context.
    #[tracing::instrument(skip(self, path), name = "rpc_client.set_context_cwd")]
    pub async fn set_context_cwd(
        &self,
        context_id: ContextId,
        path: &str,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.set_context_cwd_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_path(path);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let result = response.get()?;
        if !result.get_success() {
            return Err(RpcError::ServerError(result.get_error()?.to_string()?));
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, argv), name = "rpc_client.execute_kj")]
    pub async fn execute_kj(
        &self,
        context_id: ContextId,
        argv: &[String],
    ) -> Result<KjExecutionResult, RpcError> {
        let mut request = self.kernel.execute_kj_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let mut wire_argv = request.get().init_argv(argv.len() as u32);
            for (i, arg) in argv.iter().enumerate() {
                wire_argv.set(i as u32, arg);
            }
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let result = response.get()?;
        let latch = if result.get_has_latch() {
            Some(KjLatch {
                command: result.get_latch_command()?.to_string()?,
                target: result.get_latch_target()?.to_string()?,
                message: result.get_latch_message()?.to_string()?,
            })
        } else { None };
        Ok(KjExecutionResult {
            exit_code: result.get_exit_code(),
            stdout: result.get_stdout()?.to_string()?,
            stderr: result.get_stderr()?.to_string()?,
            command_block_id: parse_block_id(&result.get_command_block_id()?)?,
            latch,
        })
    }

    #[tracing::instrument(skip(self), name = "rpc_client.get_kj_command_catalog")]
    pub async fn get_kj_command_catalog(
        &self,
        context_id: ContextId,
    ) -> Result<Vec<KjCommandInfo>, RpcError> {
        let mut request = self.kernel.get_kj_command_catalog_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        response.get()?.get_commands()?.iter().map(|item| {
            Ok(KjCommandInfo {
                name: item.get_name()?.to_string()?,
                description: item.get_description()?.to_string()?,
                input_hint: item.get_input_hint()?.to_string()?,
                argv_prefix: item.get_argv_prefix()?.iter()
                    .map(|s| Ok(s?.to_string()?)).collect::<Result<Vec<_>, RpcError>>()?,
            })
        }).collect()
    }

    /// Get a shell variable by name.
    #[tracing::instrument(skip(self), name = "rpc_client.get_shell_var")]
    pub async fn get_shell_var(&self, name: &str) -> Result<(Option<ShellValue>, bool), RpcError> {
        let mut request = self.kernel.get_shell_var_request();
        request.get().set_name(name);
        let response = request.send().promise.await?;
        let result = response.get()?;
        let found = result.get_found();
        if found {
            let value = read_shell_value(result.get_value()?)?;
            Ok((Some(value), true))
        } else {
            Ok((None, false))
        }
    }

    /// Set a shell variable.
    #[tracing::instrument(skip(self, value), name = "rpc_client.set_shell_var")]
    pub async fn set_shell_var(&self, name: &str, value: &ShellValue) -> Result<(), RpcError> {
        let mut request = self.kernel.set_shell_var_request();
        request.get().set_name(name);
        write_shell_value(request.get().init_value(), value);
        let response = request.send().promise.await?;
        let result = response.get()?;
        if !result.get_success() {
            let error = result.get_error()?.to_str()?;
            if !error.is_empty() {
                return Err(RpcError::ServerError(error.to_string()));
            }
        }
        Ok(())
    }

    /// List all shell variables with their values.
    #[tracing::instrument(skip(self), name = "rpc_client.list_shell_vars")]
    pub async fn list_shell_vars(&self) -> Result<Vec<(String, ShellValue)>, RpcError> {
        let request = self.kernel.list_shell_vars_request();
        let response = request.send().promise.await?;
        let vars = response.get()?.get_vars()?;

        let mut result = Vec::with_capacity(vars.len() as usize);
        for var in vars.iter() {
            let name = var.get_name()?.to_string()?;
            let value = read_shell_value(var.get_value()?)?;
            result.push((name, value));
        }
        Ok(result)
    }

    // =========================================================================
    // Per-client durable view state (docs/shared-state.md "Retiring KV")
    // =========================================================================

    /// Record `context_id` as the last-viewed context for `client_id` (upsert).
    #[tracing::instrument(skip(self), name = "rpc_client.set_last_context")]
    pub async fn set_last_context(
        &self,
        client_id: &str,
        context_id: ContextId,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.set_last_context_request();
        request.get().set_client_id(client_id);
        request.get().set_context_id(context_id.to_string());
        request.send().promise.await?;
        Ok(())
    }

    /// Read back the last-viewed context for `client_id`. Returns `None` if
    /// this client has never recorded one.
    #[tracing::instrument(skip(self), name = "rpc_client.get_client_view")]
    pub async fn get_client_view(&self, client_id: &str) -> Result<Option<ContextId>, RpcError> {
        let mut request = self.kernel.get_client_view_request();
        request.get().set_client_id(client_id);
        let response = request.send().promise.await?;
        let result = response.get()?;
        if !result.get_found() {
            return Ok(None);
        }
        let context_id_str = result.get_context_id()?.to_str()?;
        let context_id = ContextId::parse(context_id_str)
            .map_err(|e| RpcError::ServerError(format!("invalid context id: {}", e)))?;
        Ok(Some(context_id))
    }

    // =========================================================================
    // Input Document (CRDT compose scratchpad)
    // =========================================================================

    /// High-level edit on the input document: insert text at position, delete characters.
    ///
    /// Returns the acknowledged version.
    #[tracing::instrument(skip(self, insert), name = "rpc_client.edit_input")]
    pub async fn edit_input(
        &self,
        context_id: ContextId,
        pos: u64,
        insert: &str,
        delete: u64,
    ) -> Result<u64, RpcError> {
        let mut request = self.kernel.edit_input_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_pos(pos);
        request.get().set_insert(insert);
        request.get().set_delete(delete);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_ack_version())
    }

    /// Get the full input document state for a context.
    ///
    /// Returns the current content, CRDT oplog, and version.
    #[tracing::instrument(skip(self), name = "rpc_client.get_input_state")]
    pub async fn get_input_state(&self, context_id: ContextId) -> Result<InputState, RpcError> {
        let mut request = self.kernel.get_input_state_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let result = response.get()?;
        Ok(InputState {
            content: result.get_content()?.to_string()?,
            ops: result.get_ops().map(|d| d.to_vec()).unwrap_or_default(),
            version: result.get_version(),
        })
    }

    /// Submit the input document: snapshot to conversation block and clear.
    ///
    /// `is_shell` selects the routing mode (shell command vs chat prompt).
    #[tracing::instrument(skip(self), name = "rpc_client.submit_input")]
    pub async fn submit_input(
        &self,
        context_id: ContextId,
        is_shell: bool,
    ) -> Result<SubmitResult, RpcError> {
        let mut request = self.kernel.submit_input_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_mode(if is_shell {
            crate::kaijutsu_capnp::InputMode::Shell
        } else {
            crate::kaijutsu_capnp::InputMode::Chat
        });
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let result = response.get()?;
        let block_id = parse_block_id(&result.get_command_block_id()?)?;
        Ok(SubmitResult { block_id })
    }

    /// Commit a captured-MIDI batch (`docs/midi.md` M2 — the ear's push half,
    /// the structural reverse of a `RenderCue`). `payload` is a
    /// `MIDI_CAPTURE_MIME` JSON batch record; the kernel quantizes it to the
    /// grid of whatever track `context_id` is attached to and returns the
    /// score-context block id it landed. The CAS payload arm is reserved
    /// (`casHash` rides empty until the client→kernel CAS write surface
    /// lands — docs/issues.md).
    #[tracing::instrument(skip(self, payload), name = "rpc_client.commit_capture")]
    pub async fn commit_capture(
        &self,
        context_id: ContextId,
        mime: &str,
        payload: &[u8],
    ) -> Result<BlockId, RpcError> {
        let mut request = self.kernel.commit_capture_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_mime(mime);
        request.get().set_payload(payload);
        request.get().set_cas_hash("");
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let result = response.get()?;
        parse_block_id(&result.get_block_id()?)
    }

    /// Ship one clock reference from the local observer (`docs/midi.md` M3
    /// — the reverse of the `BeatSync` push). Fire-and-forget semantics on
    /// a ~2 Hz stream; the kernel slaves the sender's track only when its
    /// clock is `modeled`.
    #[tracing::instrument(skip(self), name = "rpc_client.report_clock_estimate")]
    pub async fn report_clock_estimate(
        &self,
        context_id: ContextId,
        beat: f64,
        tempo_bps: f64,
        epoch_ns: u64,
        source: &str,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.report_clock_estimate_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_beat(beat);
        request.get().set_tempo_bps(tempo_bps);
        request.get().set_epoch_ns(epoch_ns);
        request.get().set_source(source);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        request.send().promise.await?;
        Ok(())
    }

    /// Report one profile-matched device's presence to the kernel
    /// (`docs/midi-next.md` "Presence is sink-fed" — the app matches, the
    /// kernel records). Not context-scoped: presence is a fact about the rig.
    /// `present = false` is a first-class report (unplug), never an omission.
    #[tracing::instrument(skip(self, ports), name = "rpc_client.report_midi_presence")]
    pub async fn report_midi_presence(
        &self,
        device: &str,
        present: bool,
        backend: &str,
        ports: &[(String, String)],
        epoch_ns: u64,
        sink_host: &str,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.report_midi_presence_request();
        {
            let mut p = request.get();
            p.set_device(device);
            p.set_present(present);
            p.set_backend(backend);
            p.set_epoch_ns(epoch_ns);
            // Provenance for display ("live on moltar"), not identity: the
            // kernel keys reaping on the connection this call rides.
            p.set_sink_host(sink_host);
            let mut list = p.init_ports(ports.len() as u32);
            for (i, (name, address)) in ports.iter().enumerate() {
                let mut entry = list.reborrow().get(i as u32);
                entry.set_name(name);
                entry.set_address(address);
            }
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        request.send().promise.await?;
        Ok(())
    }

    /// Clear the input document for a context (discard draft).
    ///
    /// The server clears the CRDT input doc and emits `InputCleared` to all
    /// subscribers. Use this for Escape×3 (discard draft) — `submit_input`
    /// already clears internally.
    #[tracing::instrument(skip(self), name = "rpc_client.clear_input")]
    pub async fn clear_input(&self, context_id: ContextId) -> Result<(), RpcError> {
        let mut request = self.kernel.clear_input_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        request.send().promise.await?;
        Ok(())
    }

    // =========================================================================
    // VFS (FSN world stage-0/1 plumbing, docs/scenes/vfs.md)
    // =========================================================================

    /// Recursive snapshot listing with generation stamps. All walking,
    /// depth/cap clamping, generation stamping, and gitignore classification
    /// happen kernel-side (thin client, smart kernel) — this returns the
    /// finished tree. `depth`/`max_entries` are server-clamped regardless of
    /// what's asked (see the kernel's `MountTable::snapshot` doc for the
    /// exact ceilings and the generation/`ignored` policy).
    #[tracing::instrument(skip(self), name = "rpc_client.vfs_snapshot")]
    pub async fn vfs_snapshot(
        &self,
        path: &str,
        depth: u32,
        max_entries: u32,
    ) -> Result<SnapshotResult, RpcError> {
        let vfs_response = self.kernel.vfs_request().send().promise.await?;
        let vfs = vfs_response.get()?.get_vfs()?;

        let mut request = vfs.snapshot_request();
        {
            let mut p = request.get();
            p.set_path(path);
            p.set_depth(depth);
            p.set_max_entries(max_entries);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        let root = parse_snapshot_node(reader.get_root()?)?;
        Ok(SnapshotResult {
            root,
            generation: reader.get_generation(),
            truncated: reader.get_truncated(),
        })
    }

    /// Read a whole VFS file over the existing `Vfs` capability — no new wire
    /// method. Chunked at [`VFS_READ_CHUNK`] and stopping on the documented
    /// zero-length-read EOF signal, so it works against every backend
    /// (`ConfigCrdtFs`, `MemoryBackend`, `MidiPresenceFs`, a share) without
    /// asking any of them for a size first.
    ///
    /// The app's device-profile fetch rides this: a sink reads
    /// `/etc/midi/devices/<name>` for its match strings the same way any
    /// other config reaches a client (`docs/midi-next.md` "Presence is
    /// sink-fed": *profiles reach the app the same way any config does*).
    #[tracing::instrument(skip(self), name = "rpc_client.vfs_read_all")]
    pub async fn vfs_read_all(&self, path: &str) -> Result<Vec<u8>, RpcError> {
        let vfs_response = self.kernel.vfs_request().send().promise.await?;
        let vfs = vfs_response.get()?.get_vfs()?;

        let mut out: Vec<u8> = Vec::new();
        loop {
            let mut request = vfs.read_request();
            {
                let mut p = request.get();
                p.set_path(path);
                p.set_offset(out.len() as u64);
                p.set_size(VFS_READ_CHUNK);
            }
            let response = request.send().promise.await?;
            let chunk = response.get()?.get_data()?;
            if chunk.is_empty() {
                // Zero-length read is EOF (the VfsOps read contract). A SHORT
                // read is not: the next request just resumes at the advanced
                // offset.
                break;
            }
            out.extend_from_slice(chunk);
        }
        Ok(out)
    }

    /// Thin wrapper over `Vfs.create` — needed by the vfs-activity e2e test
    /// to mint a file under a mounted path before writing to it. The
    /// resulting `FileAttr` is discarded (nothing here needs it yet); callers
    /// that do can extend this later.
    #[tracing::instrument(skip(self), name = "rpc_client.vfs_create")]
    pub async fn vfs_create(&self, path: &str, mode: u32) -> Result<(), RpcError> {
        let vfs_response = self.kernel.vfs_request().send().promise.await?;
        let vfs = vfs_response.get()?.get_vfs()?;
        let mut request = vfs.create_request();
        {
            let mut p = request.get();
            p.set_path(path);
            p.set_mode(mode);
        }
        request.send().promise.await?;
        Ok(())
    }

    /// Thin wrapper over `Vfs.write` — needed by the vfs-activity e2e test to
    /// generate content-mutation heat (`MountTable::bump_activity`) without a
    /// full CRDT edit path. Returns the byte count the backend reports written.
    #[tracing::instrument(skip(self, data), name = "rpc_client.vfs_write")]
    pub async fn vfs_write(&self, path: &str, offset: u64, data: &[u8]) -> Result<u32, RpcError> {
        let vfs_response = self.kernel.vfs_request().send().promise.await?;
        let vfs = vfs_response.get()?.get_vfs()?;
        let mut request = vfs.write_request();
        {
            let mut p = request.get();
            p.set_path(path);
            p.set_offset(offset);
            p.set_data(data);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_written())
    }

    /// Subscribe to the VFS activity digest push channel (Lane K, FSN
    /// slice-1, `docs/scenes/vfs.md`). `interval_ms = 0` requests the
    /// server's default tick period; the server floors anything requested
    /// below its minimum rather than rejecting it. The server streams
    /// `onActivityDigest` callbacks to `callback` until the connection drops
    /// — see [`crate::subscriptions::vfs_activity_events_channel`] for
    /// building the callback client.
    #[tracing::instrument(skip(self, callback), name = "rpc_client.subscribe_vfs_activity")]
    pub async fn subscribe_vfs_activity(
        &self,
        callback: crate::kaijutsu_capnp::vfs_activity_events::Client,
        interval_ms: u32,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.subscribe_vfs_activity_request();
        {
            let mut p = request.get();
            p.set_callback(callback);
            p.set_interval_ms(interval_ms);
        }
        request.send().promise.await?;
        Ok(())
    }
}

// ============================================================================
// VFS types
// ============================================================================

/// File type of a [`SnapshotNode`] — client-owned mirror of the wire
/// `FileType` enum (kaijutsu-client deliberately doesn't depend on
/// kaijutsu-kernel; see the crate layering note in CLAUDE.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsFileType {
    File,
    Directory,
    Symlink,
}

/// One node of a `Vfs.snapshot` reply — owned, recursive mirror of the wire
/// `SnapshotNode` struct. See `MountTable::snapshot`'s doc comment
/// (kaijutsu-kernel) for the full field semantics (generation policy,
/// `ignored` classification, truncation).
#[derive(Debug, Clone)]
pub struct SnapshotNode {
    pub name: String,
    pub kind: VfsFileType,
    pub size: u64,
    pub mtime_secs: u64,
    pub child_count: u32,
    pub ignored: bool,
    pub generation: u64,
    pub children: Vec<SnapshotNode>,
    pub truncated_here: bool,
    /// The walk was refused here (permission denied) — a fact about the
    /// tree, rendered as a seam; distinct from `truncated_here` (the
    /// walker's own budget cut).
    pub denied: bool,
}

/// Result of [`KernelHandle::vfs_snapshot`].
#[derive(Debug, Clone)]
pub struct SnapshotResult {
    pub root: SnapshotNode,
    /// Mirrors `root.generation` — a quick staleness check without reading
    /// into the node.
    pub generation: u64,
    /// `true` iff any node in the tree has `truncated_here` set.
    pub truncated: bool,
}

/// Recursively parse a capnp `SnapshotNode` reader into the owned client
/// type.
fn parse_snapshot_node(
    reader: crate::kaijutsu_capnp::snapshot_node::Reader<'_>,
) -> Result<SnapshotNode, RpcError> {
    let name = reader.get_name()?.to_string()?;
    let kind = match reader.get_kind()? {
        crate::kaijutsu_capnp::FileType::File => VfsFileType::File,
        crate::kaijutsu_capnp::FileType::Directory => VfsFileType::Directory,
        crate::kaijutsu_capnp::FileType::Symlink => VfsFileType::Symlink,
    };
    let mut children = Vec::new();
    for child in reader.get_children()?.iter() {
        children.push(parse_snapshot_node(child)?);
    }
    Ok(SnapshotNode {
        name,
        kind,
        size: reader.get_size(),
        mtime_secs: reader.get_mtime_secs(),
        child_count: reader.get_child_count(),
        ignored: reader.get_ignored(),
        generation: reader.get_generation(),
        children,
        truncated_here: reader.get_truncated_here(),
        denied: reader.get_denied(),
    })
}

/// One directory's entry in a VFS activity digest tick (Lane K, FSN slice-1,
/// `docs/scenes/vfs.md`). `total` is an ABSOLUTE monotonic count since kernel
/// boot, never a delta — see `kaijutsu-kernel::vfs::activity` for the
/// lossy-safe reasoning. `generation` mirrors the directory's current
/// listing-generation (same field `SnapshotNode` carries) so a consumer can
/// tell whether its cached listing for this path is stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfsActivityEntry {
    pub path: String,
    pub total: u64,
    pub generation: u64,
}

/// Parse a capnp `VfsActivityEntry` reader into the owned client struct.
/// Shared by the push forwarder (`subscriptions.rs`).
pub(crate) fn parse_vfs_activity_entry(
    r: crate::kaijutsu_capnp::vfs_activity_entry::Reader<'_>,
) -> Result<VfsActivityEntry, RpcError> {
    Ok(VfsActivityEntry {
        path: r.get_path()?.to_string()?,
        total: r.get_total(),
        generation: r.get_generation(),
    })
}

/// Read a `ShellValue` from a Cap'n Proto reader.
fn read_shell_value(
    reader: crate::kaijutsu_capnp::shell_value::Reader<'_>,
) -> Result<ShellValue, RpcError> {
    use crate::kaijutsu_capnp::shell_value;
    match reader.which().map_err(|e| RpcError::Capnp(e.into()))? {
        shell_value::Null(()) => Ok(ShellValue::Null),
        shell_value::Bool(b) => Ok(ShellValue::Bool(b)),
        shell_value::Int(i) => Ok(ShellValue::Int(i)),
        shell_value::Float(f) => Ok(ShellValue::Float(f)),
        shell_value::String(s) => Ok(ShellValue::String(s?.to_string()?)),
        shell_value::Json(j) => {
            let json_str = j?.to_str()?;
            let parsed: serde_json::Value = serde_json::from_str(json_str)
                .map_err(|e| RpcError::ServerError(format!("invalid JSON: {}", e)))?;
            Ok(ShellValue::Json(parsed))
        }
        shell_value::Bytes(b) => Ok(ShellValue::Bytes(b?.to_vec())),
        // LEGACY: kaish 0.9 dropped Value::Blob; a `blob` only arrives from an
        // older peer and carries a path string, so surface it as a String.
        shell_value::Blob(b) => Ok(ShellValue::String(b?.to_string()?)),
    }
}

/// Write a `ShellValue` into a Cap'n Proto builder.
fn write_shell_value(
    mut builder: crate::kaijutsu_capnp::shell_value::Builder<'_>,
    value: &ShellValue,
) {
    match value {
        ShellValue::Null => builder.set_null(()),
        ShellValue::Bool(b) => builder.set_bool(*b),
        ShellValue::Int(i) => builder.set_int(*i),
        ShellValue::Float(f) => builder.set_float(*f),
        ShellValue::String(s) => builder.set_string(s),
        ShellValue::Json(j) => builder.set_json(serde_json::to_string(j).unwrap_or_default()),
        ShellValue::Bytes(b) => builder.set_bytes(b),
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

// ============================================================================
// Block Query Builder Helpers
// ============================================================================

fn set_block_query_builder(
    mut builder: crate::kaijutsu_capnp::block_query::Builder<'_>,
    query: &BlockQuery,
) {
    match query {
        BlockQuery::All => builder.set_all(()),
        BlockQuery::ByIds(ids) => {
            let mut list = builder.init_by_ids(ids.len() as u32);
            for (i, id) in ids.iter().enumerate() {
                let mut b = list.reborrow().get(i as u32);
                set_block_id_builder(&mut b, id);
            }
        }
        BlockQuery::ByFilter(filter) => {
            let fb = builder.init_by_filter();
            set_block_filter_builder(fb, filter);
        }
    }
}

fn set_block_filter_builder(
    mut builder: crate::kaijutsu_capnp::block_filter::Builder<'_>,
    filter: &BlockFilter,
) {
    if !filter.kinds.is_empty() {
        builder.set_has_kinds(true);
        let mut list = builder.reborrow().init_kinds(filter.kinds.len() as u32);
        for (i, kind) in filter.kinds.iter().enumerate() {
            list.set(
                i as u32,
                match kind {
                    BlockKind::Text => crate::kaijutsu_capnp::BlockKind::Text,
                    BlockKind::Thinking => crate::kaijutsu_capnp::BlockKind::Thinking,
                    BlockKind::ToolCall => crate::kaijutsu_capnp::BlockKind::ToolCall,
                    BlockKind::ToolResult => crate::kaijutsu_capnp::BlockKind::ToolResult,
                    BlockKind::Drift => crate::kaijutsu_capnp::BlockKind::Drift,
                    BlockKind::File => crate::kaijutsu_capnp::BlockKind::File,
                    BlockKind::Error => crate::kaijutsu_capnp::BlockKind::Error,
                    BlockKind::Notification => crate::kaijutsu_capnp::BlockKind::Notification,
                    BlockKind::Resource => crate::kaijutsu_capnp::BlockKind::Resource,
                    BlockKind::Trace => crate::kaijutsu_capnp::BlockKind::Trace,
                    BlockKind::Task => crate::kaijutsu_capnp::BlockKind::Task,
                },
            );
        }
    }

    if !filter.roles.is_empty() {
        builder.set_has_roles(true);
        let mut list = builder.reborrow().init_roles(filter.roles.len() as u32);
        for (i, role) in filter.roles.iter().enumerate() {
            list.set(
                i as u32,
                match role {
                    Role::User => crate::kaijutsu_capnp::Role::User,
                    Role::Model => crate::kaijutsu_capnp::Role::Model,
                    Role::System => crate::kaijutsu_capnp::Role::System,
                    Role::Tool => crate::kaijutsu_capnp::Role::Tool,
                    Role::Asset => crate::kaijutsu_capnp::Role::Asset,
                },
            );
        }
    }

    if !filter.statuses.is_empty() {
        builder.set_has_statuses(true);
        let mut list = builder
            .reborrow()
            .init_statuses(filter.statuses.len() as u32);
        for (i, status) in filter.statuses.iter().enumerate() {
            list.set(
                i as u32,
                match status {
                    Status::Pending => crate::kaijutsu_capnp::Status::Pending,
                    Status::Running => crate::kaijutsu_capnp::Status::Running,
                    Status::Done => crate::kaijutsu_capnp::Status::Done,
                    Status::Error => crate::kaijutsu_capnp::Status::Error,
                },
            );
        }
    }

    builder.set_limit(filter.limit);
    builder.set_max_depth(filter.max_depth);

    if let Some(ref parent_id) = filter.parent_id {
        builder.set_has_parent_id(true);
        let mut pid = builder.reborrow().init_parent_id();
        set_block_id_builder(&mut pid, parent_id);
    }
}

fn set_block_event_filter_builder(
    builder: &mut crate::kaijutsu_capnp::block_event_filter::Builder<'_>,
    filter: &kaijutsu_types::BlockEventFilter,
) {
    if !filter.context_ids.is_empty() {
        builder.set_has_context_ids(true);
        let mut list = builder
            .reborrow()
            .init_context_ids(filter.context_ids.len() as u32);
        for (i, ctx_id) in filter.context_ids.iter().enumerate() {
            list.set(i as u32, ctx_id.as_bytes());
        }
    }

    if !filter.event_types.is_empty() {
        builder.set_has_event_types(true);
        let mut list = builder
            .reborrow()
            .init_event_types(filter.event_types.len() as u32);
        for (i, kind) in filter.event_types.iter().enumerate() {
            list.set(
                i as u32,
                match kind {
                    kaijutsu_types::BlockFlowKind::Inserted => {
                        crate::kaijutsu_capnp::BlockFlowKind::Inserted
                    }
                    kaijutsu_types::BlockFlowKind::TextOps => {
                        crate::kaijutsu_capnp::BlockFlowKind::TextOps
                    }
                    kaijutsu_types::BlockFlowKind::TextAppended => {
                        crate::kaijutsu_capnp::BlockFlowKind::TextAppended
                    }
                    kaijutsu_types::BlockFlowKind::TextReplaced => {
                        crate::kaijutsu_capnp::BlockFlowKind::TextReplaced
                    }
                    kaijutsu_types::BlockFlowKind::Deleted => {
                        crate::kaijutsu_capnp::BlockFlowKind::Deleted
                    }
                    kaijutsu_types::BlockFlowKind::StatusChanged => {
                        crate::kaijutsu_capnp::BlockFlowKind::StatusChanged
                    }
                    kaijutsu_types::BlockFlowKind::CollapsedChanged => {
                        crate::kaijutsu_capnp::BlockFlowKind::CollapsedChanged
                    }
                    kaijutsu_types::BlockFlowKind::ExcludedChanged => {
                        crate::kaijutsu_capnp::BlockFlowKind::ExcludedChanged
                    }
                    kaijutsu_types::BlockFlowKind::Moved => {
                        crate::kaijutsu_capnp::BlockFlowKind::Moved
                    }
                    kaijutsu_types::BlockFlowKind::SyncReset => {
                        crate::kaijutsu_capnp::BlockFlowKind::SyncReset
                    }
                    kaijutsu_types::BlockFlowKind::OutputChanged => {
                        crate::kaijutsu_capnp::BlockFlowKind::OutputChanged
                    }
                    kaijutsu_types::BlockFlowKind::MetadataChanged => {
                        crate::kaijutsu_capnp::BlockFlowKind::MetadataChanged
                    }
                    kaijutsu_types::BlockFlowKind::ContextSwitched => {
                        crate::kaijutsu_capnp::BlockFlowKind::ContextSwitched
                    }
                    kaijutsu_types::BlockFlowKind::RenderCue => {
                        crate::kaijutsu_capnp::BlockFlowKind::RenderCue
                    }
                    kaijutsu_types::BlockFlowKind::BeatSync => {
                        crate::kaijutsu_capnp::BlockFlowKind::BeatSync
                    }
                },
            );
        }
    }

    if !filter.block_kinds.is_empty() {
        builder.set_has_block_kinds(true);
        let mut list = builder
            .reborrow()
            .init_block_kinds(filter.block_kinds.len() as u32);
        for (i, kind) in filter.block_kinds.iter().enumerate() {
            list.set(
                i as u32,
                match kind {
                    BlockKind::Text => crate::kaijutsu_capnp::BlockKind::Text,
                    BlockKind::Thinking => crate::kaijutsu_capnp::BlockKind::Thinking,
                    BlockKind::ToolCall => crate::kaijutsu_capnp::BlockKind::ToolCall,
                    BlockKind::ToolResult => crate::kaijutsu_capnp::BlockKind::ToolResult,
                    BlockKind::Drift => crate::kaijutsu_capnp::BlockKind::Drift,
                    BlockKind::File => crate::kaijutsu_capnp::BlockKind::File,
                    BlockKind::Error => crate::kaijutsu_capnp::BlockKind::Error,
                    BlockKind::Notification => crate::kaijutsu_capnp::BlockKind::Notification,
                    BlockKind::Resource => crate::kaijutsu_capnp::BlockKind::Resource,
                    BlockKind::Trace => crate::kaijutsu_capnp::BlockKind::Trace,
                    BlockKind::Task => crate::kaijutsu_capnp::BlockKind::Task,
                },
            );
        }
    }
}

fn set_block_id_builder(builder: &mut crate::kaijutsu_capnp::block_id::Builder, id: &BlockId) {
    builder.set_context_id(id.context_id.as_bytes());
    builder.set_principal_id(id.principal_id.as_bytes());
    builder.set_seq(id.seq);
}

fn entry_type_from_capnp(et: crate::kaijutsu_capnp::EntryType) -> kaijutsu_types::OutputEntryType {
    use crate::kaijutsu_capnp::EntryType;
    use kaijutsu_types::OutputEntryType;
    match et {
        EntryType::Text => OutputEntryType::Text,
        EntryType::File => OutputEntryType::File,
        EntryType::Directory => OutputEntryType::Directory,
        EntryType::Executable => OutputEntryType::Executable,
        EntryType::Symlink => OutputEntryType::Symlink,
    }
}

fn parse_output_node(
    reader: crate::kaijutsu_capnp::output_node::Reader<'_>,
) -> Result<kaijutsu_types::OutputNode, capnp::Error> {
    let name = reader.get_name()?.to_str()?.to_owned();
    let entry_type = entry_type_from_capnp(reader.get_entry_type()?);
    let text = if reader.get_has_text() {
        Some(reader.get_text()?.to_str()?.to_owned())
    } else {
        None
    };
    let cells_reader = reader.get_cells()?;
    let mut cells = Vec::with_capacity(cells_reader.len() as usize);
    for i in 0..cells_reader.len() {
        cells.push(cells_reader.get(i)?.to_str()?.to_owned());
    }
    let children_reader = reader.get_children()?;
    let mut children = Vec::with_capacity(children_reader.len() as usize);
    for i in 0..children_reader.len() {
        children.push(parse_output_node(children_reader.get(i))?);
    }
    Ok(kaijutsu_types::OutputNode {
        name,
        entry_type,
        text,
        cells,
        children,
    })
}

/// Parse a Cap'n Proto `BlockMetadata` into the typed struct.
///
/// Lenient: malformed text fields degrade to defaults rather than erroring —
/// a metadata event is advisory, not load-bearing for protocol correctness.
pub(crate) fn parse_block_metadata(
    reader: crate::kaijutsu_capnp::block_metadata::Reader<'_>,
) -> kaijutsu_types::BlockMetadata {
    let content_type = reader
        .get_content_type()
        .ok()
        .and_then(|t| t.to_str().ok())
        .map(kaijutsu_types::ContentType::from_mime)
        .unwrap_or_default();
    let tool_use_id = reader
        .get_tool_use_id()
        .ok()
        .and_then(|t| t.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned());
    let stderr = if reader.get_has_stderr() {
        reader
            .get_stderr()
            .ok()
            .and_then(|t| t.to_str().ok())
            .map(|s| s.to_owned())
    } else {
        None
    };
    let task_status = reader
        .get_task_status()
        .ok()
        .and_then(|t| t.to_str().ok())
        .and_then(kaijutsu_types::TaskStatus::from_str)
        .unwrap_or_default();
    kaijutsu_types::BlockMetadata {
        exit_code: reader.get_has_exit_code().then(|| reader.get_exit_code()),
        is_error: reader.get_is_error(),
        content_type,
        ephemeral: reader.get_ephemeral(),
        tool_use_id,
        stderr,
        task_status,
    }
}

pub(crate) fn parse_output_data(
    reader: crate::kaijutsu_capnp::output_data::Reader<'_>,
) -> Result<kaijutsu_types::OutputData, capnp::Error> {
    let headers = if reader.get_has_headers() {
        let hlist = reader.get_headers()?;
        let mut v = Vec::with_capacity(hlist.len() as usize);
        for i in 0..hlist.len() {
            v.push(hlist.get(i)?.to_str()?.to_owned());
        }
        Some(v)
    } else {
        None
    };
    let root_reader = reader.get_root()?;
    let mut root = Vec::with_capacity(root_reader.len() as usize);
    for i in 0..root_reader.len() {
        root.push(parse_output_node(root_reader.get(i))?);
    }
    // `OutputData` is `#[non_exhaustive]` upstream — build it through the
    // constructors rather than a struct literal. `nodes` sets `rich_json:
    // None`; headers are attached only when present.
    let mut data = kaijutsu_types::OutputData::nodes(root);
    if let Some(headers) = headers {
        data = data.with_headers(headers);
    }
    // richJson is "empty string = none" on the wire (see kaijutsu.capnp).
    // The server is the sole encoder, so malformed JSON here means
    // corruption or version-skew, not a routine condition — log loud, but
    // still leave rich_json unset rather than failing the whole snapshot
    // parse over a display-hint field.
    let rich_json_text = reader.get_rich_json()?.to_str()?;
    if !rich_json_text.is_empty() {
        match serde_json::from_str::<serde_json::Value>(rich_json_text) {
            Ok(v) => data = data.with_rich_json(v),
            Err(e) => {
                log::error!("parse_output_data: rich_json did not parse as JSON: {e}")
            }
        }
    }
    Ok(data)
}

fn parse_context_id(data: &[u8]) -> Result<ContextId, RpcError> {
    ContextId::try_from_slice(data).ok_or_else(|| {
        RpcError::ServerError(format!(
            "invalid context ID: expected 16 bytes, got {}",
            data.len()
        ))
    })
}

/// Parse 16-byte Data into KernelId.
fn parse_kernel_id(data: &[u8]) -> Result<KernelId, RpcError> {
    KernelId::try_from_slice(data).ok_or_else(|| {
        RpcError::ServerError(format!(
            "invalid kernel ID: expected 16 bytes, got {}",
            data.len()
        ))
    })
}

/// Parse a `SimilarContext` from the wire (search/neighbor results).
fn parse_similar_context(
    reader: &crate::kaijutsu_capnp::similar_context::Reader<'_>,
) -> Result<SimilarContext, RpcError> {
    Ok(SimilarContext {
        context_id: parse_context_id(reader.get_context_id()?)?,
        score: reader.get_score(),
        label: reader.get_label()?.to_string()?,
    })
}

/// Parse a `ContextCluster` from the wire (`get_clusters`).
fn parse_context_cluster(
    reader: &crate::kaijutsu_capnp::context_cluster::Reader<'_>,
) -> Result<ContextCluster, RpcError> {
    let ids = reader.get_context_ids()?;
    let mut context_ids = Vec::with_capacity(ids.len() as usize);
    for id in ids.iter() {
        context_ids.push(parse_context_id(id?)?);
    }
    Ok(ContextCluster {
        cluster_id: reader.get_cluster_id(),
        context_ids,
        label: reader.get_label()?.to_string()?,
    })
}

/// Helper to parse ContextInfo from Cap'n Proto ContextHandleInfo.
fn parse_context_info(
    reader: &crate::kaijutsu_capnp::context_handle_info::Reader<'_>,
) -> Result<ContextInfo, RpcError> {
    let id = parse_context_id(reader.get_id()?)?;
    let label = reader.get_label()?.to_string()?;
    // Wire field is still named `parentId` — Rust side renamed to `forked_from`
    let parent_data = reader.get_parent_id()?;
    let forked_from = if parent_data.len() == 16 {
        let pid = ContextId::try_from_slice(parent_data);
        pid.filter(|id| !id.is_nil())
    } else {
        None
    };

    let trace_data = reader.get_trace_id()?;
    let trace_id = if trace_data.len() == 16 {
        let mut buf = [0u8; 16];
        buf.copy_from_slice(trace_data);
        buf
    } else {
        [0u8; 16]
    };

    let fork_kind_str = reader.get_fork_kind()?.to_str().unwrap_or("");
    let fork_kind = if fork_kind_str.is_empty() {
        None
    } else {
        Some(fork_kind_str.to_string())
    };
    let archived = reader.get_archived_at() > 0;
    let concluded_at = match reader.get_concluded_at() {
        0 => None,
        ts => Some(ts),
    };

    let context_type_str = reader.get_context_type()?.to_str().unwrap_or("");
    let context_type = if context_type_str.is_empty() {
        "default".to_string()
    } else {
        context_type_str.to_string()
    };

    // Parse synthesis keywords
    let keywords = if reader.has_keywords() {
        reader
            .get_keywords()?
            .into_iter()
            .filter_map(|k| k.ok().map(|s| s.to_string().unwrap_or_default()))
            .collect()
    } else {
        Vec::new()
    };

    let top_block_preview = if reader.has_top_block_preview() {
        let s = reader.get_top_block_preview()?.to_str().unwrap_or("");
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    } else {
        None
    };

    let live_status = match reader.get_live_status()? {
        crate::kaijutsu_capnp::Status::Pending => Status::Pending,
        crate::kaijutsu_capnp::Status::Running => Status::Running,
        crate::kaijutsu_capnp::Status::Done => Status::Done,
        crate::kaijutsu_capnp::Status::Error => Status::Error,
    };

    let last_activity_at = match reader.get_last_activity_at() {
        0 => None,
        ts => Some(ts),
    };

    // Empty on the wire = unattached (TrackIds are never empty by construction).
    let track_id = if reader.has_track_id() {
        let s = reader.get_track_id()?.to_str().unwrap_or("");
        if s.is_empty() { None } else { Some(s.to_string()) }
    } else {
        None
    };

    let promoted_at = match reader.get_promoted_at() {
        0 => None,
        ts => Some(ts),
    };
    let demoted_at = match reader.get_demoted_at() {
        0 => None,
        ts => Some(ts),
    };
    let paused_at = match reader.get_paused_at() {
        0 => None,
        ts => Some(ts),
    };

    let context_window = match reader.get_context_window() {
        0 => None,
        w => Some(w),
    };
    let context_used_tokens = match reader.get_context_used_tokens() {
        0 => None,
        t => Some(t),
    };
    // -1.0 is the dedicated "unknown" sentinel (0.0 is a legitimate value —
    // a fresh context is genuinely 0% used — so it can't double as the
    // sentinel the way 0 does for the fields above). Decoded once here so
    // no caller above this boundary has to remember the wire convention.
    let context_used_pct = match reader.get_context_used_pct() {
        pct if pct < 0.0 => None,
        pct => Some(pct),
    };

    let background_running_count = reader.get_background_running_count();
    let background_oldest_running_started_at =
        match reader.get_background_oldest_running_started_at() {
            0 => None,
            ts => Some(ts),
        };
    let background_last_finished_at = match reader.get_background_last_finished_at() {
        0 => None,
        ts => Some(ts),
    };
    let background_last_finished_status_str =
        reader.get_background_last_finished_status()?.to_str().unwrap_or("");
    let background_last_finished_status = if background_last_finished_status_str.is_empty() {
        None
    } else {
        Some(background_last_finished_status_str.to_string())
    };
    // -1 is the dedicated "no exit code" sentinel (a killed process, or
    // nothing finished yet) — real exit codes (including the `128 + signal`
    // convention `background_exec.rs` uses) are never negative, so there is
    // no collision the way there would be with a plain 0-means-none scheme.
    let background_last_exit_code = match reader.get_background_last_exit_code() {
        -1 => None,
        code => Some(code),
    };

    // Empty = uncast (Track D) — same "absence is the wire sentinel"
    // convention as `context_type`/`track_id` above.
    let cast_label_str = reader.get_cast_label()?.to_str().unwrap_or("");
    let cast_label = if cast_label_str.is_empty() {
        None
    } else {
        Some(cast_label_str.to_string())
    };

    // Empty = unknown (old client, no client to ask, or pre-migration row) —
    // same "absence is the wire sentinel" convention as `cast_label` above.
    let origin_host_str = reader.get_origin_host()?.to_str().unwrap_or("");
    let origin_host = if origin_host_str.is_empty() {
        None
    } else {
        Some(origin_host_str.to_string())
    };

    Ok(ContextInfo {
        id,
        label,
        forked_from,
        provider: reader.get_provider()?.to_string()?,
        model: reader.get_model()?.to_string()?,
        created_at: reader.get_created_at(),
        trace_id,
        fork_kind,
        context_type,
        archived,
        concluded_at,
        keywords,
        top_block_preview,
        live_status,
        last_activity_at,
        track_id,
        promoted_at,
        demoted_at,
        paused_at,
        context_window,
        context_used_tokens,
        context_used_pct,
        background_running_count,
        background_oldest_running_started_at,
        background_last_finished_at,
        background_last_finished_status,
        background_last_exit_code,
        cast_label,
        origin_host,
    })
}

/// Helper to parse a TrackInfo from Cap'n Proto.
fn parse_track_info(
    reader: &crate::kaijutsu_capnp::track_info::Reader<'_>,
) -> Result<TrackInfo, RpcError> {
    let score_context_id = ContextId::try_from_slice(reader.get_score_context_id()?)
        .ok_or_else(|| RpcError::Other("invalid score context id in TrackInfo".into()))?;
    let attached_reader = reader.get_attached()?;
    let mut attached = Vec::with_capacity(attached_reader.len() as usize);
    for ctx in attached_reader.iter() {
        attached.push(
            ContextId::try_from_slice(ctx?)
                .ok_or_else(|| RpcError::Other("invalid attached context id in TrackInfo".into()))?,
        );
    }
    Ok(TrackInfo {
        id: reader.get_id()?.to_string()?,
        score_context_id,
        playing: reader.get_playing(),
        playhead_tick: reader.get_playhead_tick(),
        period_us: reader.get_period_us(),
        beats_per_phrase: reader.get_beats_per_phrase(),
        beat_count: reader.get_beat_count(),
        last_epoch_ns: reader.get_last_epoch_ns(),
        clock_kind: reader.get_clock_kind()?.to_string()?,
        attached,
    })
}

/// Helper to parse KernelInfo from Cap'n Proto
fn parse_kernel_info(
    reader: &crate::kaijutsu_capnp::kernel_info::Reader<'_>,
) -> Result<KernelInfo, RpcError> {
    let id = parse_kernel_id(reader.get_id()?)?;
    let contexts_reader = reader.get_contexts()?;
    let mut contexts = Vec::with_capacity(contexts_reader.len() as usize);
    for ctx in contexts_reader.iter() {
        contexts.push(parse_context_info(&ctx)?);
    }

    Ok(KernelInfo {
        id,
        name: reader.get_name()?.to_string()?,
        user_count: reader.get_user_count(),
        agent_count: reader.get_agent_count(),
        contexts,
    })
}

/// Parse a `PeerInfo` from Cap'n Proto — shared by `list_peers` decode and
/// the roundtrip test below.
fn parse_peer_info(
    reader: &crate::kaijutsu_capnp::peer_info::Reader<'_>,
) -> Result<PeerInfo, RpcError> {
    Ok(PeerInfo {
        nick: reader.get_nick()?.to_string()?,
        attached_at: reader.get_attached_at(),
    })
}

/// Helper to parse block ID from Cap'n Proto (binary 16-byte UUIDs).
pub(crate) fn parse_block_id(
    reader: &crate::kaijutsu_capnp::block_id::Reader<'_>,
) -> Result<BlockId, RpcError> {
    let context_id = ContextId::try_from_slice(reader.get_context_id()?)
        .ok_or_else(|| RpcError::ServerError("invalid context_id in BlockId".into()))?;
    let principal_id = PrincipalId::try_from_slice(reader.get_principal_id()?)
        .ok_or_else(|| RpcError::ServerError("invalid principal_id in BlockId".into()))?;
    Ok(BlockId::new(context_id, principal_id, reader.get_seq()))
}

/// Helper to parse a flat BlockSnapshot from Cap'n Proto using BlockSnapshotBuilder.
pub(crate) fn parse_block_snapshot(
    reader: &crate::kaijutsu_capnp::block_snapshot::Reader<'_>,
) -> Result<BlockSnapshot, RpcError> {
    // Parse block ID
    let id = parse_block_id(&reader.get_id()?)?;

    // Parse kind (10 variants)
    let kind = match reader.get_kind()? {
        crate::kaijutsu_capnp::BlockKind::Text => BlockKind::Text,
        crate::kaijutsu_capnp::BlockKind::Thinking => BlockKind::Thinking,
        crate::kaijutsu_capnp::BlockKind::ToolCall => BlockKind::ToolCall,
        crate::kaijutsu_capnp::BlockKind::ToolResult => BlockKind::ToolResult,
        crate::kaijutsu_capnp::BlockKind::Drift => BlockKind::Drift,
        crate::kaijutsu_capnp::BlockKind::File => BlockKind::File,
        crate::kaijutsu_capnp::BlockKind::Error => BlockKind::Error,
        crate::kaijutsu_capnp::BlockKind::Notification => BlockKind::Notification,
        crate::kaijutsu_capnp::BlockKind::Resource => BlockKind::Resource,
        crate::kaijutsu_capnp::BlockKind::Trace => BlockKind::Trace,
        crate::kaijutsu_capnp::BlockKind::Task => BlockKind::Task,
    };

    let mut builder = BlockSnapshotBuilder::new(id, kind);

    // Parse parent_id if present
    if reader.get_has_parent_id() {
        builder = builder.parent_id(parse_block_id(&reader.get_parent_id()?)?);
    }

    // Parse role
    let role = match reader.get_role()? {
        crate::kaijutsu_capnp::Role::User => Role::User,
        crate::kaijutsu_capnp::Role::Model => Role::Model,
        crate::kaijutsu_capnp::Role::System => Role::System,
        crate::kaijutsu_capnp::Role::Tool => Role::Tool,
        crate::kaijutsu_capnp::Role::Asset => Role::Asset,
    };
    builder = builder.role(role);

    // Parse status
    let status = match reader.get_status()? {
        crate::kaijutsu_capnp::Status::Pending => Status::Pending,
        crate::kaijutsu_capnp::Status::Running => Status::Running,
        crate::kaijutsu_capnp::Status::Done => Status::Done,
        crate::kaijutsu_capnp::Status::Error => Status::Error,
    };
    builder = builder.status(status);

    // Content
    builder = builder.content(reader.get_content()?.to_str()?);
    builder = builder.collapsed(reader.get_collapsed());

    // created_at (kaijutsu.capnp:207, "Unix timestamp in milliseconds").
    // BlockSnapshotBuilder::new() defaults this to now_millis(), correct for
    // a block being authored locally right now — but wrong for one being
    // reconstructed off the wire, where the server always sends the real
    // creation time (kaijutsu-server/src/rpc.rs set_block_snapshot,
    // unconditional builder.set_created_at). We propagate the wire value
    // faithfully, including 0, rather than treating 0 as "unset" and
    // falling back to now_millis(): the server never omits this field, so a
    // 0 here means the sender's own created_at was genuinely 0 (a bug
    // upstream, or a malformed/ancient peer) — and an obviously-bogus
    // 1970 timestamp is far easier to notice and debug (e.g. a context
    // parked at the extreme end of the time well) than silently
    // substituting "now", which would make a real upstream defect
    // indistinguishable from a correctly-timestamped fresh block. That
    // masking is exactly the shape of bug this fix exists to close.
    builder = builder.created_at(reader.get_created_at());

    // Tool-specific fields
    if reader.has_tool_name() {
        let name = reader.get_tool_name()?.to_str()?;
        if !name.is_empty() {
            builder = builder.tool_name(name);
        }
    }

    if reader.has_tool_input() {
        let input = reader.get_tool_input()?.to_str()?;
        if !input.is_empty() {
            builder = builder.tool_input(input);
        }
    }

    if reader.has_tool_call_id() {
        builder = builder.tool_call_id(parse_block_id(&reader.get_tool_call_id()?)?);
    }

    if reader.get_has_exit_code() {
        builder = builder.exit_code(reader.get_exit_code());
    }

    if reader.get_is_error() {
        builder = builder.is_error(true);
    }

    if reader.get_has_stderr()
        && let Ok(s) = reader.get_stderr()
        && let Ok(s) = s.to_str()
    {
        builder = builder.stderr(s);
    }

    if reader.get_has_signature()
        && let Ok(s) = reader.get_signature()
        && let Ok(s) = s.to_str()
    {
        builder = builder.signature(s);
    }

    // Structured output data. A kj block's OutputData is exactly root-empty +
    // headers-none + rich_json-some — without the rich_json arm here that
    // shape was silently dropped (see parse_block_snapshot_attaches_rich_json_only_output).
    if let Ok(output_data_reader) = reader.get_output_data()
        && let Ok(data) = parse_output_data(output_data_reader)
        && (!data.root.is_empty() || data.headers.is_some() || data.rich_json.is_some())
    {
        builder = builder.output(data);
    }

    // Drift-specific fields — source_context is now binary Data (16-byte ContextId)
    let source_data = reader.get_source_context()?;
    if source_data.len() == 16
        && let Some(ctx) = ContextId::try_from_slice(source_data)
        && !ctx.is_nil()
    {
        builder = builder.source_context(ctx);
    }

    if reader.has_source_model()
        && let Ok(model) = reader.get_source_model()
        && let Ok(s) = model.to_str()
        && !s.is_empty()
    {
        builder = builder.source_model(s);
    }

    // DriftKind — wire is now an enum, not a string
    if reader.get_has_drift_kind()
        && let Ok(dk) = reader.get_drift_kind()
    {
        let drift_kind = match dk {
            crate::kaijutsu_capnp::DriftKind::Push => DriftKind::Push,
            crate::kaijutsu_capnp::DriftKind::Pull => DriftKind::Pull,
            crate::kaijutsu_capnp::DriftKind::Merge => DriftKind::Merge,
            crate::kaijutsu_capnp::DriftKind::Distill => DriftKind::Distill,
            crate::kaijutsu_capnp::DriftKind::Notification => DriftKind::Notification,
            crate::kaijutsu_capnp::DriftKind::Fork => DriftKind::Fork,
        };
        builder = builder.drift_kind(drift_kind);
    }

    // ToolKind — wire enum
    if reader.get_has_tool_kind()
        && let Ok(tk) = reader.get_tool_kind()
    {
        let tool_kind = match tk {
            crate::kaijutsu_capnp::ToolKind::Shell => ToolKind::Shell,
            crate::kaijutsu_capnp::ToolKind::Mcp => ToolKind::Mcp,
            crate::kaijutsu_capnp::ToolKind::Builtin => ToolKind::Builtin,
        };
        builder = builder.tool_kind(tool_kind);
    }

    // tool_use_id (LLM-assigned tool invocation ID)
    if reader.has_tool_use_id()
        && let Ok(tui) = reader.get_tool_use_id()
        && let Ok(s) = tui.to_str()
        && !s.is_empty()
    {
        builder = builder.tool_use_id(s);
    }

    // File path (for BlockKind::File blocks)
    if reader.has_file_path()
        && let Ok(path) = reader.get_file_path()
        && let Ok(s) = path.to_str()
        && !s.is_empty()
    {
        builder = builder.file_path(s);
    }

    // Content type hint (MIME type)
    if reader.has_content_type()
        && let Ok(ct) = reader.get_content_type()
        && let Ok(s) = ct.to_str()
        && !s.is_empty()
    {
        builder = builder.content_type(ContentType::from_mime(s));
    }

    // Task lifecycle status (BlockKind::Task; "" falls back to the builder's
    // default Open, same "empty = default" convention as content_type)
    if reader.has_task_status()
        && let Ok(ts) = reader.get_task_status()
        && let Ok(s) = ts.to_str()
        && let Some(status) = kaijutsu_types::TaskStatus::from_str(s)
    {
        builder = builder.task_status(status);
    }

    // Ephemeral flag (human-only, excluded from LLM hydration)
    if reader.get_ephemeral() {
        builder = builder.ephemeral(true);
    }

    // Excluded flag (user-curated staging curation, toggled by `block exclude`;
    // decides whether the block is dropped at the next conversation hydrate
    // boundary). Unlike `ephemeral` above (system-managed, always hidden from
    // hydration), `excluded` is an explicit user decision — see
    // kaijutsu.capnp:215-219.
    if reader.get_excluded() {
        builder = builder.excluded(true);
    }

    // Hyoushigi timeline coordinate (Some only for materialized timeline cells)
    if reader.get_has_tick() {
        builder = builder.tick(Tick::new(reader.get_tick()));
    }

    // Hyoushigi track / lane identity (Some only for materialized timeline cells).
    // A malformed track string on the wire is corruption: warn loudly and drop to
    // None rather than fabricate a lane (no silent normalization). hasTrack=false
    // ⇔ None — old writers leave it false.
    if reader.get_has_track()
        && let Ok(s) = reader.get_track()
        && let Ok(s) = s.to_str()
    {
        match TrackId::new(s) {
            Ok(track) => builder = builder.track(track),
            Err(e) => log::warn!("parse_block_snapshot: invalid track {s:?} on wire: {e}"),
        }
    }

    // Error payload (for Error blocks)
    if reader.get_has_error_payload()
        && let Ok(ep) = reader.get_error_payload()
    {
        let category = match ep.get_category()? {
            crate::kaijutsu_capnp::ErrorCategory::Tool => ErrorCategory::Tool,
            crate::kaijutsu_capnp::ErrorCategory::Stream => ErrorCategory::Stream,
            crate::kaijutsu_capnp::ErrorCategory::Rpc => ErrorCategory::Rpc,
            crate::kaijutsu_capnp::ErrorCategory::Render => ErrorCategory::Render,
            crate::kaijutsu_capnp::ErrorCategory::Parse => ErrorCategory::Parse,
            crate::kaijutsu_capnp::ErrorCategory::Validation => ErrorCategory::Validation,
            crate::kaijutsu_capnp::ErrorCategory::Kernel => ErrorCategory::Kernel,
        };
        let severity = match ep.get_severity()? {
            crate::kaijutsu_capnp::ErrorSeverity::Warning => ErrorSeverity::Warning,
            crate::kaijutsu_capnp::ErrorSeverity::Error => ErrorSeverity::Error,
            crate::kaijutsu_capnp::ErrorSeverity::Fatal => ErrorSeverity::Fatal,
        };
        let code = ep.get_code().ok()
            .and_then(|s| s.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let detail = ep.get_detail().ok()
            .and_then(|s| s.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let span = if ep.get_has_span() {
            Some(ErrorSpan {
                line: ep.get_span_line(),
                column: ep.get_span_column(),
                length: ep.get_span_length(),
            })
        } else {
            None
        };
        let source_kind = if ep.get_has_source_kind() {
            ep.get_source_kind().ok().map(|sk| match sk {
                crate::kaijutsu_capnp::BlockKind::Text => BlockKind::Text,
                crate::kaijutsu_capnp::BlockKind::Thinking => BlockKind::Thinking,
                crate::kaijutsu_capnp::BlockKind::ToolCall => BlockKind::ToolCall,
                crate::kaijutsu_capnp::BlockKind::ToolResult => BlockKind::ToolResult,
                crate::kaijutsu_capnp::BlockKind::Drift => BlockKind::Drift,
                crate::kaijutsu_capnp::BlockKind::File => BlockKind::File,
                crate::kaijutsu_capnp::BlockKind::Error => BlockKind::Error,
                crate::kaijutsu_capnp::BlockKind::Notification => BlockKind::Notification,
                crate::kaijutsu_capnp::BlockKind::Resource => BlockKind::Resource,
                crate::kaijutsu_capnp::BlockKind::Trace => BlockKind::Trace,
                crate::kaijutsu_capnp::BlockKind::Task => BlockKind::Task,
            })
        } else {
            None
        };
        builder = builder.error_payload(ErrorPayload {
            category,
            severity,
            code,
            detail,
            span,
            source_kind,
        });
    }

    // Notification payload (for Notification blocks)
    if reader.get_has_notification_payload()
        && let Ok(np) = reader.get_notification_payload()
    {
        let instance = np.get_instance().ok()
            .and_then(|s| s.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let kind = match np.get_kind()? {
            crate::kaijutsu_capnp::NotificationKind::ToolAdded => {
                kaijutsu_types::NotificationKind::ToolAdded
            }
            crate::kaijutsu_capnp::NotificationKind::ToolRemoved => {
                kaijutsu_types::NotificationKind::ToolRemoved
            }
            crate::kaijutsu_capnp::NotificationKind::Log => {
                kaijutsu_types::NotificationKind::Log
            }
            crate::kaijutsu_capnp::NotificationKind::PromptsChanged => {
                kaijutsu_types::NotificationKind::PromptsChanged
            }
            crate::kaijutsu_capnp::NotificationKind::Coalesced => {
                kaijutsu_types::NotificationKind::Coalesced
            }
        };
        let level = if np.get_has_level() {
            np.get_level().ok().map(|l| match l {
                crate::kaijutsu_capnp::LogLevel::Trace => kaijutsu_types::LogLevel::Trace,
                crate::kaijutsu_capnp::LogLevel::Debug => kaijutsu_types::LogLevel::Debug,
                crate::kaijutsu_capnp::LogLevel::Info => kaijutsu_types::LogLevel::Info,
                crate::kaijutsu_capnp::LogLevel::Warn => kaijutsu_types::LogLevel::Warn,
                crate::kaijutsu_capnp::LogLevel::Error => kaijutsu_types::LogLevel::Error,
            })
        } else {
            None
        };
        let tools: Vec<String> = np
            .get_tools()
            .ok()
            .map(|list| {
                (0..list.len())
                    .filter_map(|i| list.get(i).ok().and_then(|s| s.to_str().ok()).map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let count = if np.get_has_count() {
            Some(np.get_count() as usize)
        } else {
            None
        };
        let detail = np.get_detail().ok()
            .and_then(|s| s.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        builder = builder.notification_payload(kaijutsu_types::NotificationPayload {
            instance,
            kind,
            level,
            tools,
            count,
            detail,
        });
    }

    // Resource payload (for Resource blocks)
    if reader.get_has_resource_payload()
        && let Ok(rp) = reader.get_resource_payload()
    {
        let instance = rp.get_instance().ok()
            .and_then(|s| s.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let uri = rp.get_uri().ok()
            .and_then(|s| s.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let mime_type = if rp.get_has_mime_type() {
            rp.get_mime_type().ok()
                .and_then(|s| s.to_str().ok())
                .map(|s| s.to_string())
        } else {
            None
        };
        let size = if rp.get_has_size() {
            Some(rp.get_size())
        } else {
            None
        };
        let text = if rp.get_has_text() {
            rp.get_text().ok()
                .and_then(|s| s.to_str().ok())
                .map(|s| s.to_string())
        } else {
            None
        };
        let blob_base64 = if rp.get_has_blob() {
            rp.get_blob_base64().ok()
                .and_then(|s| s.to_str().ok())
                .map(|s| s.to_string())
        } else {
            None
        };
        let parent_resource_block_id = if rp.get_has_parent_resource_block_id() {
            rp.get_parent_resource_block_id().ok()
                .and_then(|r| parse_block_id(&r).ok())
        } else {
            None
        };
        builder = builder.resource_payload(kaijutsu_types::ResourcePayload {
            instance,
            uri,
            mime_type,
            size,
            text,
            blob_base64,
            parent_resource_block_id,
        });
    }

    Ok(builder.build())
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    pub display_text: String,
    pub kind: CompletionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    Path,
    Variable,
    Keyword,
}

impl CompletionKind {
    fn from_capnp(kind: crate::kaijutsu_capnp::CompletionKind) -> Self {
        match kind {
            crate::kaijutsu_capnp::CompletionKind::Command => CompletionKind::Command,
            crate::kaijutsu_capnp::CompletionKind::Path => CompletionKind::Path,
            crate::kaijutsu_capnp::CompletionKind::Variable => CompletionKind::Variable,
            crate::kaijutsu_capnp::CompletionKind::Keyword => CompletionKind::Keyword,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub id: u64,
    pub code: String,
    pub timestamp: u64,
}

// ============================================================================
// Timeline Types
// ============================================================================

/// A version snapshot in document history.
#[derive(Debug, Clone)]
pub struct VersionSnapshot {
    /// Version number
    pub version: u64,
    /// Unix timestamp (ms) when this version was created
    pub timestamp: u64,
    /// Number of blocks at this version
    pub block_count: u32,
    /// What changed ("block_added", "block_edited", etc.)
    pub change_kind: String,
}

// ============================================================================
// Drift Types
// ============================================================================

/// Info about a staged drift operation.
#[derive(Debug, Clone)]
pub struct StagedDriftInfo {
    pub id: u64,
    pub source_ctx: ContextId,
    pub target_ctx: ContextId,
    pub content: String,
    pub source_model: String,
    pub drift_kind: DriftKind,
    pub created_at: u64,
}

// ============================================================================
// LLM Configuration Types
// ============================================================================

/// Information about a single LLM provider
#[derive(Debug, Clone)]
pub struct LlmProviderInfo {
    pub name: String,
    pub default_model: String,
    pub available: bool,
    /// All available model IDs for this provider (from aliases + default).
    pub models: Vec<String>,
}

/// Current LLM configuration for a kernel
#[derive(Debug, Clone)]
pub struct LlmConfigInfo {
    pub default_provider: String,
    pub default_model: String,
    pub providers: Vec<LlmProviderInfo>,
}

/// Shell variable value (mirrors kaish `ast::Value`).
#[derive(Debug, Clone, PartialEq)]
pub enum ShellValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    /// JSON-serialized structured data.
    Json(serde_json::Value),
    /// Inline binary data (kaish 0.9 `Value::Bytes`).
    Bytes(Vec<u8>),
}

// ============================================================================
// Block Types
// ============================================================================

/// CRDT sync state (ops + version, no blocks).
///
/// Used by `get_context_sync` for lightweight CRDT bootstrapping and resync.
#[derive(Debug, Clone)]
pub struct SyncState {
    pub context_id: ContextId,
    pub ops: Vec<u8>,
    pub version: u64,
}

/// Result from submitting the input document (submitInput @78).
#[derive(Debug, Clone)]
pub struct SubmitResult {
    pub block_id: BlockId,
}

/// Full input document state for a context.
#[derive(Debug, Clone)]
pub struct InputState {
    pub content: String,
    pub ops: Vec<u8>,
    pub version: u64,
}

/// Renderer-facing snapshot of an in-app editor session (the vi/edit builtin).
/// The client form of the kernel's `EditorState` (see docs/vi.md). `mode` is
/// `None` when the wire `mode` is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorState {
    pub session: u64,
    pub text: String,
    pub cursor: u64,
    pub mode: Option<String>,
    pub dirty: bool,
    /// The `:`-line a renderer draws while command mode is active (`":wq"`);
    /// `None` when the bar is unfocused. (docs/vi.md → Command mode.)
    pub command_line: Option<String>,
    /// A transient status/error line (vim `E492`), e.g. an unknown `:command` or
    /// a bad `:s` regex; `None` when there's nothing to report. Drawn read-only.
    pub message: Option<String>,
}

/// Parse a capnp `EditorState` reader into the client struct. Shared by the
/// `editor_*` RPC methods and the `EditorEvents` push forwarder.
pub(crate) fn parse_editor_state(
    r: crate::kaijutsu_capnp::editor_state::Reader<'_>,
) -> Result<EditorState, RpcError> {
    let mode = r.get_mode()?.to_string()?;
    let command_line = r.get_command_line()?.to_string()?;
    let message = r.get_message()?.to_string()?;
    Ok(EditorState {
        session: r.get_session(),
        text: r.get_text()?.to_string()?,
        cursor: r.get_cursor(),
        mode: if mode.is_empty() { None } else { Some(mode) },
        dirty: r.get_dirty(),
        command_line: if command_line.is_empty() {
            None
        } else {
            Some(command_line)
        },
        message: if message.is_empty() {
            None
        } else {
            Some(message)
        },
    })
}

/// Result from a kernel tool execution (executeTool @16).
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub request_id: String,
    pub success: bool,
    pub output: String,
}

/// Schema for a kernel tool (getToolSchemas @11).
#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub category: String,
    pub input_schema: String,
}

/// Result from an MCP tool call
#[derive(Debug, Clone)]
pub struct McpToolResult {
    pub content: String,
    pub is_error: bool,
}

/// Information about an MCP resource
#[derive(Debug, Clone)]
pub struct McpResource {
    /// Resource URI (e.g., "file:///path/to/file")
    pub uri: String,
    /// Resource name
    pub name: String,
    /// Optional description
    pub description: Option<String>,
    /// Optional MIME type
    pub mime_type: Option<String>,
}

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("Cap'n Proto error: {0}")]
    Capnp(#[from] capnp::Error),
    #[error("Not in schema: {0}")]
    NotInSchema(#[from] capnp::NotInSchema),
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("Not connected to server")]
    NotConnected,
    #[error("Capability no longer valid")]
    CapabilityLost,
    #[error("Server error: {0}")]
    ServerError(String),
    #[error("{0}")]
    Other(String),
}

// ============================================================================
// PeerCommands capnp server (client-side callback)
// ============================================================================

/// Implements the `PeerCommands` Cap'n Proto interface on the client side.
///
/// Lives in `spawn_local` (is `!Send`). Forwards invocations to the caller
/// via an mpsc channel so they can be processed on any thread.
struct PeerCommandsImpl {
    tx: std::sync::mpsc::Sender<crate::actor::PeerInvocation>,
}

impl crate::kaijutsu_capnp::peer_commands::Server for PeerCommandsImpl {
    async fn invoke(
        self: capnp::capability::Rc<Self>,
        params: crate::kaijutsu_capnp::peer_commands::InvokeParams,
        mut results: crate::kaijutsu_capnp::peer_commands::InvokeResults,
    ) -> Result<(), capnp::Error> {
        let action = params
            .get()?
            .get_action()?
            .to_str()
            .unwrap_or_default()
            .to_string();
        let invoke_params = params.get()?.get_params()?.to_vec();

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let invocation = crate::actor::PeerInvocation {
            action,
            params: invoke_params,
            reply: reply_tx,
        };

        // std::sync::mpsc::Sender::send is non-blocking and works from any executor
        self.tx
            .send(invocation)
            .map_err(|_| capnp::Error::failed("peer handler disconnected".into()))?;

        // Await the reply with timeout — prevents indefinite hang if the app
        // stalls or crashes. 15s is generous for a frame-rate-driven poll loop.
        let response = tokio::time::timeout(
            crate::constants::PEER_INVOCATION_TIMEOUT,
            reply_rx,
        )
        .await
        .map_err(|_| {
            capnp::Error::failed(format!(
                "peer invocation timed out after {}s waiting for app dispatch",
                crate::constants::PEER_INVOCATION_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|_| capnp::Error::failed("peer handler dropped reply".into()))?;

        match response {
            Ok(data) => {
                results.get().set_result(&data);
                Ok(())
            }
            Err(e) => Err(capnp::Error::failed(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capnp::message::Builder as MessageBuilder;

    /// `write_shell_value` → `read_shell_value` round-trip — the client mirror of
    /// the kaish 0.9 `Value::Blob` → `Value::Bytes` migration. Locks in that
    /// inline binary survives the wire byte-for-byte and a legacy `blob` from an
    /// older peer decodes to a String path. (Server-side has the symmetric test.)
    fn roundtrip_shell_value(value: &ShellValue) -> ShellValue {
        let mut message = MessageBuilder::new_default();
        write_shell_value(
            message.init_root::<crate::kaijutsu_capnp::shell_value::Builder>(),
            value,
        );
        let reader = message
            .get_root_as_reader::<crate::kaijutsu_capnp::shell_value::Reader>()
            .expect("read back shell_value");
        read_shell_value(reader).expect("decode shell_value")
    }

    #[test]
    fn shell_value_bytes_round_trip_is_byte_exact() {
        // NUL + non-UTF-8 byte: exactly what a String-decode or base64 fudge corrupts.
        let original = ShellValue::Bytes(vec![0x00, 0x01, 0xFF, 0xFE, b'h', b'i']);
        assert_eq!(roundtrip_shell_value(&original), original);
        assert_eq!(
            roundtrip_shell_value(&ShellValue::Bytes(vec![])),
            ShellValue::Bytes(vec![])
        );
    }

    #[test]
    fn shell_value_legacy_blob_decodes_to_string() {
        // kaish 0.9 never produces `blob`; an older peer's path string must
        // survive as a String, never mis-typed as binary or dropped.
        let mut message = MessageBuilder::new_default();
        message
            .init_root::<crate::kaijutsu_capnp::shell_value::Builder>()
            .set_blob("/v/cas/deadbeef");
        let reader = message
            .get_root_as_reader::<crate::kaijutsu_capnp::shell_value::Reader>()
            .unwrap();
        assert_eq!(
            read_shell_value(reader).unwrap(),
            ShellValue::String("/v/cas/deadbeef".into())
        );
    }

    /// `OutputData::rich_json` — the kj / builtin structured-payload channel
    /// wired through in the OutputData capnp round-trip fix — must survive
    /// build → wire → `parse_output_data` intact. Manually sets the fields
    /// `build_output_data` (kaijutsu-server/src/rpc.rs, private to that
    /// crate) would set, then decodes through the real production
    /// `parse_output_data`, mirroring `roundtrip_snapshot` below.
    #[test]
    fn parse_output_data_rich_json_round_trip() {
        let mut message = MessageBuilder::new_default();
        let mut builder = message.init_root::<crate::kaijutsu_capnp::output_data::Builder>();
        let rich = serde_json::json!(["a", "b"]);
        builder.set_rich_json(serde_json::to_string(&rich).unwrap().as_str());
        // root left empty — a rich_json-only payload (kj's shape).
        builder.reborrow().init_root(0);

        let reader = message
            .get_root_as_reader::<crate::kaijutsu_capnp::output_data::Reader>()
            .unwrap();
        let data = parse_output_data(reader).expect("parse_output_data");

        assert_eq!(
            data.rich_json,
            Some(rich),
            "rich_json must survive the capnp round trip intact"
        );
        assert!(data.root.is_empty(), "root was never set on the wire");
    }

    /// The "empty string = none" wire convention (kaijutsu.capnp comment on
    /// `richJson`): a message that never touched `richJson` must decode to
    /// `rich_json: None`, not `Some("")`.
    #[test]
    fn parse_output_data_no_rich_json_stays_none() {
        let mut message = MessageBuilder::new_default();
        let mut builder = message.init_root::<crate::kaijutsu_capnp::output_data::Builder>();
        builder.reborrow().init_root(0);

        let reader = message
            .get_root_as_reader::<crate::kaijutsu_capnp::output_data::Reader>()
            .unwrap();
        let data = parse_output_data(reader).expect("parse_output_data");

        assert_eq!(data.rich_json, None);
    }

    /// Helper: build a BlockSnapshot capnp message, set fields, then parse it back
    /// through `parse_block_snapshot` to verify roundtrip fidelity.
    fn roundtrip_snapshot(snap: &BlockSnapshot) -> BlockSnapshot {
        let mut message = MessageBuilder::new_default();
        let mut builder = message.init_root::<crate::kaijutsu_capnp::block_snapshot::Builder>();

        // Set ID
        {
            let mut id = builder.reborrow().init_id();
            id.set_context_id(snap.id.context_id.as_bytes());
            id.set_principal_id(snap.id.principal_id.as_bytes());
            id.set_seq(snap.id.seq);
        }

        // Set parent_id if present (DAG edge — also the subtask edge for
        // BlockKind::Task; no dedicated wire field needed).
        if let Some(ref parent) = snap.parent_id {
            builder.set_has_parent_id(true);
            let mut pid = builder.reborrow().init_parent_id();
            pid.set_context_id(parent.context_id.as_bytes());
            pid.set_principal_id(parent.principal_id.as_bytes());
            pid.set_seq(parent.seq);
        } else {
            builder.set_has_parent_id(false);
        }

        // Set kind
        builder.set_kind(match snap.kind {
            BlockKind::Text => crate::kaijutsu_capnp::BlockKind::Text,
            BlockKind::Thinking => crate::kaijutsu_capnp::BlockKind::Thinking,
            BlockKind::ToolCall => crate::kaijutsu_capnp::BlockKind::ToolCall,
            BlockKind::ToolResult => crate::kaijutsu_capnp::BlockKind::ToolResult,
            BlockKind::Drift => crate::kaijutsu_capnp::BlockKind::Drift,
            BlockKind::File => crate::kaijutsu_capnp::BlockKind::File,
            BlockKind::Error => crate::kaijutsu_capnp::BlockKind::Error,
            BlockKind::Notification => crate::kaijutsu_capnp::BlockKind::Notification,
            BlockKind::Resource => crate::kaijutsu_capnp::BlockKind::Resource,
            BlockKind::Trace => crate::kaijutsu_capnp::BlockKind::Trace,
            BlockKind::Task => crate::kaijutsu_capnp::BlockKind::Task,
        });

        // Set role
        builder.set_role(match snap.role {
            Role::User => crate::kaijutsu_capnp::Role::User,
            Role::Model => crate::kaijutsu_capnp::Role::Model,
            Role::System => crate::kaijutsu_capnp::Role::System,
            Role::Tool => crate::kaijutsu_capnp::Role::Tool,
            Role::Asset => crate::kaijutsu_capnp::Role::Asset,
        });

        // Set status
        builder.set_status(match snap.status {
            Status::Pending => crate::kaijutsu_capnp::Status::Pending,
            Status::Running => crate::kaijutsu_capnp::Status::Running,
            Status::Done => crate::kaijutsu_capnp::Status::Done,
            Status::Error => crate::kaijutsu_capnp::Status::Error,
        });

        builder.set_content(&snap.content);
        builder.set_collapsed(snap.collapsed);
        builder.set_ephemeral(snap.ephemeral);
        builder.set_excluded(snap.excluded);
        builder.set_created_at(snap.created_at);

        // Set file_path if present
        if let Some(ref path) = snap.file_path {
            builder.set_file_path(path);
        }

        // Set tool_kind if present
        if let Some(tk) = snap.tool_kind {
            builder.set_has_tool_kind(true);
            builder.set_tool_kind(match tk {
                ToolKind::Shell => crate::kaijutsu_capnp::ToolKind::Shell,
                ToolKind::Mcp => crate::kaijutsu_capnp::ToolKind::Mcp,
                ToolKind::Builtin => crate::kaijutsu_capnp::ToolKind::Builtin,
            });
        }

        // Set notification payload if present (D-36).
        if let Some(ref payload) = snap.notification {
            builder.set_has_notification_payload(true);
            let mut np = builder.reborrow().init_notification_payload();
            np.set_instance(&payload.instance);
            np.set_kind(match payload.kind {
                kaijutsu_types::NotificationKind::ToolAdded => {
                    crate::kaijutsu_capnp::NotificationKind::ToolAdded
                }
                kaijutsu_types::NotificationKind::ToolRemoved => {
                    crate::kaijutsu_capnp::NotificationKind::ToolRemoved
                }
                kaijutsu_types::NotificationKind::Log => {
                    crate::kaijutsu_capnp::NotificationKind::Log
                }
                kaijutsu_types::NotificationKind::PromptsChanged => {
                    crate::kaijutsu_capnp::NotificationKind::PromptsChanged
                }
                kaijutsu_types::NotificationKind::Coalesced => {
                    crate::kaijutsu_capnp::NotificationKind::Coalesced
                }
            });
            if let Some(level) = payload.level {
                np.set_has_level(true);
                np.set_level(match level {
                    kaijutsu_types::LogLevel::Trace => crate::kaijutsu_capnp::LogLevel::Trace,
                    kaijutsu_types::LogLevel::Debug => crate::kaijutsu_capnp::LogLevel::Debug,
                    kaijutsu_types::LogLevel::Info => crate::kaijutsu_capnp::LogLevel::Info,
                    kaijutsu_types::LogLevel::Warn => crate::kaijutsu_capnp::LogLevel::Warn,
                    kaijutsu_types::LogLevel::Error => crate::kaijutsu_capnp::LogLevel::Error,
                });
            }
            if !payload.tools.is_empty() {
                let mut tools_builder = np
                    .reborrow()
                    .init_tools(payload.tools.len() as u32);
                for (i, name) in payload.tools.iter().enumerate() {
                    tools_builder.set(i as u32, name);
                }
            }
            if let Some(count) = payload.count {
                np.set_has_count(true);
                np.set_count(count as u32);
            }
            if let Some(ref detail) = payload.detail {
                np.set_detail(detail);
            }
        }

        // Set resource payload if present (Phase 3 — D-42).
        if let Some(ref payload) = snap.resource {
            builder.set_has_resource_payload(true);
            let mut rp = builder.reborrow().init_resource_payload();
            rp.set_instance(&payload.instance);
            rp.set_uri(&payload.uri);
            if let Some(ref mime) = payload.mime_type {
                rp.set_has_mime_type(true);
                rp.set_mime_type(mime);
            }
            if let Some(size) = payload.size {
                rp.set_has_size(true);
                rp.set_size(size);
            }
            if let Some(ref text) = payload.text {
                rp.set_has_text(true);
                rp.set_text(text);
            }
            if let Some(ref blob) = payload.blob_base64 {
                rp.set_has_blob(true);
                rp.set_blob_base64(blob);
            }
            if let Some(ref parent) = payload.parent_resource_block_id {
                rp.set_has_parent_resource_block_id(true);
                let mut pid = rp.reborrow().init_parent_resource_block_id();
                pid.set_context_id(parent.context_id.as_bytes());
                pid.set_principal_id(parent.principal_id.as_bytes());
                pid.set_seq(parent.seq);
            }
        }

        if let Some(tick) = snap.tick {
            builder.set_has_tick(true);
            builder.set_tick(tick.get());
        }

        if let Some(ref track) = snap.track {
            builder.set_has_track(true);
            builder.set_track(track.as_str());
        }

        if let Some(ref signature) = snap.signature {
            builder.set_has_signature(true);
            builder.set_signature(signature);
        }

        // Task lifecycle status ("" on the wire means Open/default, same
        // convention as content_type/Plain — see kaijutsu-server's
        // set_block_snapshot).
        if snap.task_status != kaijutsu_types::TaskStatus::default() {
            builder.set_task_status(snap.task_status.as_str());
        }

        // Parse back
        let reader = message
            .get_root_as_reader::<crate::kaijutsu_capnp::block_snapshot::Reader>()
            .unwrap();
        parse_block_snapshot(&reader).unwrap()
    }

    /// `roundtrip_snapshot` never sets `output_data` (no `snap.output` handling),
    /// so this builds a `block_snapshot` capnp message directly — mirroring
    /// `roundtrip_snapshot`'s field setup plus `parse_output_data_rich_json_round_trip`'s
    /// output_data setup — to exercise `parse_block_snapshot`'s output-attach
    /// guard with a rich_json-only `OutputData` (root empty, headers none): a kj
    /// block's exact shape. The guard used to require `!root.is_empty() ||
    /// headers.is_some()`, dropping this payload — the app store (populated from
    /// getBlocks / onBlockInserted full snapshots, and any reconnect re-fetch)
    /// lost it; it only survived via the live onBlockOutputChanged path.
    #[test]
    fn parse_block_snapshot_attaches_rich_json_only_output() {
        let mut message = MessageBuilder::new_default();
        let mut builder = message.init_root::<crate::kaijutsu_capnp::block_snapshot::Builder>();

        let id = BlockId {
            context_id: ContextId::new(),
            principal_id: PrincipalId::new(),
            seq: 1,
        };
        {
            let mut id_builder = builder.reborrow().init_id();
            id_builder.set_context_id(id.context_id.as_bytes());
            id_builder.set_principal_id(id.principal_id.as_bytes());
            id_builder.set_seq(id.seq);
        }
        builder.set_kind(crate::kaijutsu_capnp::BlockKind::ToolResult);
        builder.set_role(crate::kaijutsu_capnp::Role::Tool);
        builder.set_status(crate::kaijutsu_capnp::Status::Done);
        builder.set_content("bass\nbassline");

        let rich = serde_json::json!(["bass", "bassline"]);
        {
            let mut od = builder.reborrow().init_output_data();
            od.set_rich_json(serde_json::to_string(&rich).unwrap().as_str());
            od.reborrow().init_root(0);
        }

        let reader = message
            .get_root_as_reader::<crate::kaijutsu_capnp::block_snapshot::Reader>()
            .unwrap();
        let parsed = parse_block_snapshot(&reader).expect("parse_block_snapshot");

        let output = parsed.output.expect(
            "a rich_json-only OutputData must still attach — it is exactly a kj block's shape",
        );
        assert_eq!(output.rich_json, Some(rich), "rich_json must survive the round trip");
        assert!(output.root.is_empty(), "test premise: no node tree on the wire");
    }

    #[test]
    fn test_parse_block_snapshot_tick_roundtrip() {
        let id = BlockId {
            context_id: ContextId::new(),
            principal_id: PrincipalId::new(),
            seq: 1,
        };
        // A materialized timeline cell carries its tick across the wire.
        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .tick(Tick::new(42))
            .build();
        assert_eq!(roundtrip_snapshot(&snap).tick, Some(Tick::new(42)));

        // An ordinary block has no tick, and absence roundtrips as None.
        let plain = BlockSnapshotBuilder::new(id, BlockKind::Text).build();
        assert_eq!(roundtrip_snapshot(&plain).tick, None);
    }

    /// T18 (design §8 Phase 5) — track survives Rust→capnp→Rust; hasTrack=false ⇔
    /// None. A materialized block's lane identity crosses the wire verbatim; an
    /// ordinary block's absent track stays None (old writers leave hasTrack=false).
    #[test]
    fn track_capnp_roundtrip() {
        let id = BlockId {
            context_id: ContextId::new(),
            principal_id: PrincipalId::new(),
            seq: 1,
        };
        // Some(track) survives.
        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .track(TrackId::new("bass").unwrap())
            .build();
        assert_eq!(
            roundtrip_snapshot(&snap).track,
            Some(TrackId::new("bass").unwrap())
        );

        // None ⇔ hasTrack=false: an ordinary block roundtrips with track absent.
        let plain = BlockSnapshotBuilder::new(id, BlockKind::Text).build();
        assert_eq!(roundtrip_snapshot(&plain).track, None);
    }

    #[test]
    fn test_parse_block_snapshot_signature_roundtrip() {
        let id = BlockId {
            context_id: ContextId::new(),
            principal_id: PrincipalId::new(),
            seq: 1,
        };
        // A signed Thinking block carries its reasoning-continuity token across
        // the wire verbatim.
        let snap = BlockSnapshotBuilder::new(id, BlockKind::Thinking)
            .signature("sig_xyz")
            .build();
        assert_eq!(
            roundtrip_snapshot(&snap).signature.as_deref(),
            Some("sig_xyz")
        );

        // A block with no signature roundtrips as None (hasSignature=false).
        let plain = BlockSnapshotBuilder::new(id, BlockKind::Text).build();
        assert_eq!(roundtrip_snapshot(&plain).signature, None);
    }

    /// `excluded` (kaijutsu.capnp:219) is the user-curated staging-exclusion
    /// flag `block exclude` toggles — distinct from `ephemeral` (system-managed,
    /// hidden from LLM hydration). The server always writes it
    /// (`set_block_snapshot`, kaijutsu-server/src/rpc.rs), but until this fix
    /// `parse_block_snapshot` never called `get_excluded()`, so every
    /// capnp-decoded block silently reported `excluded = false` regardless of
    /// its real value — a latent bug that becomes real data loss once clients
    /// migrate from `getContextSync` (CBOR, unaffected) onto projected
    /// `getBlocks` queries.
    #[test]
    fn test_parse_block_snapshot_excluded_roundtrip() {
        let id = BlockId {
            context_id: ContextId::new(),
            principal_id: PrincipalId::new(),
            seq: 1,
        };

        // A user-excluded block carries excluded=true across the wire.
        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .excluded(true)
            .build();
        assert!(
            roundtrip_snapshot(&snap).excluded,
            "excluded=true must survive the capnp round trip"
        );

        // An ordinary (non-excluded) block roundtrips with excluded=false.
        let plain = BlockSnapshotBuilder::new(id, BlockKind::Text).build();
        assert!(
            !roundtrip_snapshot(&plain).excluded,
            "excluded=false must survive the capnp round trip"
        );
    }

    /// Pins the wire-carried / CRDT-internal split documented at
    /// `kaijutsu-types/src/block.rs:1578`: `order_key`, `updated_at`, and the
    /// six per-field LWW Lamport timestamps (`status_at`, `collapsed_at`,
    /// `ephemeral_at`, `excluded_at`, `tool_meta_at`, `content_type_at`,
    /// `task_status_at`) are deliberately absent from `kaijutsu.capnp` — they
    /// are CRDT-internal bookkeeping, not conversation-visible state, and
    /// `parse_block_snapshot` has no wire field to read them from. That is
    /// by design, not a gap to "fix" the way `excluded` above was. Wire-carried
    /// flags (`excluded`, `ephemeral`, `tick`) must still survive the same
    /// round trip that drops the CRDT-internal ones.
    #[test]
    fn test_parse_block_snapshot_drops_crdt_internal_fields_by_design() {
        let id = BlockId {
            context_id: ContextId::new(),
            principal_id: PrincipalId::new(),
            seq: 1,
        };
        let mut snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .excluded(true)
            .ephemeral(true)
            .tick(Tick::new(7))
            .build();
        // CRDT-internal bookkeeping — never on the wire.
        snap.order_key = Some("a0".to_string());
        snap.updated_at = 999;
        snap.status_at = 999;
        snap.collapsed_at = 999;
        snap.ephemeral_at = 999;
        snap.excluded_at = 999;
        snap.tool_meta_at = 999;
        snap.content_type_at = 999;
        snap.task_status_at = 999;

        let round_tripped = roundtrip_snapshot(&snap);

        // Wire-carried fields survive.
        assert!(round_tripped.excluded, "excluded is on the wire");
        assert!(round_tripped.ephemeral, "ephemeral is on the wire");
        assert_eq!(round_tripped.tick, Some(Tick::new(7)), "tick is on the wire");

        // CRDT-internal fields are intentionally absent from the wire and
        // must decode to their defaults, not silently carry the sender's
        // in-memory values.
        assert_eq!(round_tripped.order_key, None, "order_key is not on the wire");
        assert_eq!(round_tripped.updated_at, 0, "updated_at is not on the wire");
        assert_eq!(round_tripped.status_at, 0, "status_at is not on the wire");
        assert_eq!(round_tripped.collapsed_at, 0, "collapsed_at is not on the wire");
        assert_eq!(round_tripped.ephemeral_at, 0, "ephemeral_at is not on the wire");
        assert_eq!(round_tripped.excluded_at, 0, "excluded_at is not on the wire");
        assert_eq!(round_tripped.tool_meta_at, 0, "tool_meta_at is not on the wire");
        assert_eq!(round_tripped.content_type_at, 0, "content_type_at is not on the wire");
        assert_eq!(round_tripped.task_status_at, 0, "task_status_at is not on the wire");
    }

    /// `created_at` (kaijutsu.capnp:207) is written unconditionally by the
    /// server (`set_block_snapshot`, kaijutsu-server/src/rpc.rs) but until
    /// this fix `parse_block_snapshot` never called `get_created_at()`, so
    /// `BlockSnapshotBuilder::new`'s `now_millis()` default silently stood in
    /// for every capnp-decoded block's real creation time. Pins a fixed,
    /// unmistakably-not-"now" timestamp so a regression back to the
    /// `now_millis()` default fails loudly rather than plausibly (a
    /// regression that swapped in the current wallclock would otherwise
    /// still look like "some recent timestamp" and could slip past a casual
    /// read of a failing assertion).
    #[test]
    fn test_parse_block_snapshot_created_at_roundtrip() {
        let id = BlockId {
            context_id: ContextId::new(),
            principal_id: PrincipalId::new(),
            seq: 1,
        };

        // 2000-01-01T00:00:00Z in ms — decades before "now" by construction,
        // so a fallback to now_millis() cannot masquerade as this value.
        const FIXED_PAST_MS: u64 = 946_684_800_000;

        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .created_at(FIXED_PAST_MS)
            .build();
        assert_eq!(
            roundtrip_snapshot(&snap).created_at,
            FIXED_PAST_MS,
            "created_at must survive the capnp round trip, not fall back to now_millis()"
        );
    }

    /// Zero-value decision for `created_at`: the server writes it
    /// unconditionally (never omits the field), so a wire value of 0 means
    /// the *sender's* `created_at` was genuinely 0 — a bug upstream, or a
    /// malformed/ancient peer — not "field not set". We propagate 0
    /// faithfully rather than treating it as a sentinel and substituting
    /// `now_millis()`: an obviously-bogus 1970 timestamp is easy to notice
    /// and debug downstream (e.g. a context parked at the extreme end of
    /// the time well's idle-age ring), whereas silently substituting "now"
    /// would make a real upstream defect indistinguishable from a
    /// correctly-timestamped fresh block — the exact silent-fallback shape
    /// this fix exists to close.
    #[test]
    fn test_parse_block_snapshot_created_at_zero_propagates_faithfully() {
        let id = BlockId {
            context_id: ContextId::new(),
            principal_id: PrincipalId::new(),
            seq: 1,
        };

        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .created_at(0)
            .build();
        assert_eq!(
            roundtrip_snapshot(&snap).created_at,
            0,
            "created_at=0 must propagate faithfully, not be masked by a now_millis() fallback"
        );
    }

    /// Task block round-trip (household-agent arc, docs/tasks.md): create →
    /// capnp serialize → deserialize → same `kind`/`task_status`/`content`/
    /// `parent_id`. Covers every non-default `TaskStatus` (the default,
    /// Open, is covered by the "plain block" empty-string-fallback case
    /// below — the wire convention `"" == Open` needs its own assertion,
    /// not just "whatever the enum's Default happens to be").
    #[test]
    fn test_task_block_capnp_roundtrip() {
        let ctx = ContextId::new();
        let principal = PrincipalId::new();
        let id = BlockId {
            context_id: ctx,
            principal_id: principal,
            seq: 1,
        };

        for status in [
            kaijutsu_types::TaskStatus::Open,
            kaijutsu_types::TaskStatus::InProgress,
            kaijutsu_types::TaskStatus::Done,
            kaijutsu_types::TaskStatus::Cancelled,
        ] {
            let snap = kaijutsu_types::BlockSnapshotBuilder::new(id, BlockKind::Task)
                .role(Role::Tool)
                .content("Buy milk")
                .task_status(status)
                .build();
            let round_tripped = roundtrip_snapshot(&snap);
            assert_eq!(round_tripped.kind, BlockKind::Task, "status={status:?}");
            assert_eq!(round_tripped.task_status, status, "status={status:?}");
            assert_eq!(round_tripped.content, "Buy milk", "status={status:?}");
        }

        // Subtask: parent_id round-trips too (the ordinary DAG edge, no
        // task-specific wire field needed).
        let parent_id = id;
        let child_id = BlockId {
            context_id: ctx,
            principal_id: principal,
            seq: 2,
        };
        let subtask = kaijutsu_types::BlockSnapshotBuilder::new(child_id, BlockKind::Task)
            .role(Role::Tool)
            .content("Buy oat milk")
            .task_status(kaijutsu_types::TaskStatus::Open)
            .parent_id(parent_id)
            .build();
        let round_tripped = roundtrip_snapshot(&subtask);
        assert_eq!(round_tripped.parent_id, Some(parent_id));

        // "" on the wire falls back to Open — an ordinary (non-task) block
        // never wrote task_status, so it must decode as the default, not an
        // error or garbage value.
        let plain = BlockSnapshotBuilder::new(id, BlockKind::Text).build();
        assert_eq!(
            roundtrip_snapshot(&plain).task_status,
            kaijutsu_types::TaskStatus::Open
        );
    }

    #[test]
    fn test_parse_block_snapshot_file_path_roundtrip() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let id = BlockId {
            context_id: ctx,
            principal_id: agent,
            seq: 1,
        };

        let snap = BlockSnapshotBuilder::new(id, BlockKind::File)
            .role(Role::Asset)
            .status(Status::Done)
            .content("file content here")
            .file_path("/src/main.rs")
            .build();

        let parsed = roundtrip_snapshot(&snap);

        assert_eq!(parsed.file_path.as_deref(), Some("/src/main.rs"));
        assert_eq!(parsed.kind, BlockKind::File);
        assert_eq!(parsed.role, Role::Asset);
    }

    #[test]
    fn test_parse_block_snapshot_no_file_path() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let id = BlockId {
            context_id: ctx,
            principal_id: agent,
            seq: 2,
        };

        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .role(Role::User)
            .status(Status::Done)
            .content("hello")
            .build();

        let parsed = roundtrip_snapshot(&snap);

        assert_eq!(parsed.file_path, None);
        assert_eq!(parsed.kind, BlockKind::Text);
    }

    #[test]
    fn test_parse_block_snapshot_tool_kind_roundtrip() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let id = BlockId {
            context_id: ctx,
            principal_id: agent,
            seq: 3,
        };

        let snap = BlockSnapshotBuilder::new(id, BlockKind::ToolCall)
            .role(Role::Model)
            .status(Status::Done)
            .content("{}")
            .tool_kind(ToolKind::Mcp)
            .build();

        let parsed = roundtrip_snapshot(&snap);

        assert_eq!(parsed.tool_kind, Some(ToolKind::Mcp));
    }

    // ── Phase 2: NotificationPayload capnp roundtrip (D-36) ───────────────
    //
    // Capnp wire maps use two fragile patterns for optional fields:
    //   (a) explicit `has_<field>` flags paired with a value slot
    //       (level, count) — forgetting the flag-setter on encode silently
    //       drops the value; forgetting the flag-check on decode reads
    //       whatever garbage was in the slot.
    //   (b) empty-string sentinels for Option<String> (tool, detail) —
    //       the encoder omits a None, the decoder filters "" back to None.
    //
    // A pure-Rust unit test can't catch either failure. These roundtrips
    // encode into a capnp builder, re-read through `parse_block_snapshot`,
    // and compare the payload structurally. Coverage split across three
    // tests so a failure pinpoints which axis broke (kind variants, flag
    // pairs, or None-sentinel handling) instead of failing one fat assert.
    //
    // `roundtrip_snapshot` was extended above to include the notification
    // encoder branch; these tests exercise that branch + the decoder in
    // `parse_block_snapshot`.

    fn notif_ctx_id() -> BlockId {
        BlockId {
            context_id: ContextId::new(),
            principal_id: PrincipalId::system(),
            seq: 42,
        }
    }

    #[test]
    fn test_notification_payload_capnp_roundtrip_full() {
        // Exercise every field populated at once — the "happy path" for a
        // Log notification (most populated kind).
        let id = notif_ctx_id();
        let payload = kaijutsu_types::NotificationPayload {
            instance: "gpal".into(),
            kind: kaijutsu_types::NotificationKind::Log,
            level: Some(kaijutsu_types::LogLevel::Warn),
            tools: vec!["consult_gemini".into()],
            count: Some(3),
            detail: Some("upstream timeout; retrying".into()),
        };
        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .role(Role::System)
            .content("[gpal] warn: upstream timeout; retrying")
            .notification_payload(payload.clone())
            .build();

        let parsed = roundtrip_snapshot(&snap);

        assert_eq!(parsed.kind, BlockKind::Notification);
        assert_eq!(parsed.notification, Some(payload));
    }

    #[test]
    fn test_notification_payload_capnp_roundtrip_minimal() {
        // Minimal payload: only `instance` + `kind` populated. This locks
        // the `has_*` flag / empty-string-sentinel discipline: a None on
        // encode must round-trip back to None, not Some("") or Some(0).
        let id = notif_ctx_id();
        let payload = kaijutsu_types::NotificationPayload {
            instance: "builtin.block".into(),
            kind: kaijutsu_types::NotificationKind::PromptsChanged,
            level: None,
            tools: Vec::new(),
            count: None,
            detail: None,
        };
        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .role(Role::System)
            .notification_payload(payload.clone())
            .build();

        let parsed = roundtrip_snapshot(&snap);

        let parsed_payload = parsed.notification.expect("notification must survive");
        assert_eq!(parsed_payload.instance, "builtin.block");
        assert_eq!(
            parsed_payload.kind,
            kaijutsu_types::NotificationKind::PromptsChanged
        );
        assert_eq!(parsed_payload.level, None, "has_level=false must yield None");
        assert!(
            parsed_payload.tools.is_empty(),
            "empty tools list must roundtrip empty"
        );
        assert_eq!(parsed_payload.count, None, "has_count=false must yield None");
        assert_eq!(parsed_payload.detail, None, "empty detail must yield None");
    }

    #[test]
    fn test_notification_payload_capnp_roundtrip_all_kind_variants() {
        // One test per NotificationKind variant — catches a mis-ordered
        // capnp enum ordinal (e.g. `toolAdded @1; toolRemoved @0` swap)
        // which would silently alias ToolAdded ↔ ToolRemoved on the wire.
        let id = notif_ctx_id();
        let kinds = [
            kaijutsu_types::NotificationKind::ToolAdded,
            kaijutsu_types::NotificationKind::ToolRemoved,
            kaijutsu_types::NotificationKind::Log,
            kaijutsu_types::NotificationKind::PromptsChanged,
            kaijutsu_types::NotificationKind::Coalesced,
        ];
        for kind in kinds {
            let payload = kaijutsu_types::NotificationPayload {
                instance: "svc".into(),
                kind,
                level: None,
                tools: Vec::new(),
                count: None,
                detail: None,
            };
            let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
                .role(Role::System)
                .notification_payload(payload.clone())
                .build();
            let parsed = roundtrip_snapshot(&snap);
            assert_eq!(
                parsed.notification.map(|p| p.kind),
                Some(kind),
                "NotificationKind::{:?} did not roundtrip through capnp",
                kind,
            );
        }
    }

    #[test]
    fn test_resource_payload_capnp_roundtrip_text_full() {
        // Full text-variant ResourcePayload with every optional field set.
        let id = notif_ctx_id();
        let parent_id = BlockId {
            context_id: id.context_id,
            principal_id: id.principal_id,
            seq: 42,
        };
        let payload = kaijutsu_types::ResourcePayload {
            instance: "gpal".into(),
            uri: "file:///tmp/note.md".into(),
            mime_type: Some("text/markdown".into()),
            size: Some(1337),
            text: Some("# hello\nworld".into()),
            blob_base64: None,
            parent_resource_block_id: Some(parent_id),
        };
        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .role(Role::System)
            .content("[gpal] file:///tmp/note.md (text/markdown)")
            .resource_payload(payload.clone())
            .build();

        let parsed = roundtrip_snapshot(&snap);

        assert_eq!(parsed.kind, BlockKind::Resource);
        assert_eq!(parsed.resource, Some(payload));
    }

    #[test]
    fn test_resource_payload_capnp_roundtrip_blob_minimal() {
        // Blob variant with the minimum that still distinguishes it from
        // text (hasBlob=true, hasText=false). Locks the has_* flag discipline
        // for the text/blob exclusion contract.
        let id = notif_ctx_id();
        let payload = kaijutsu_types::ResourcePayload {
            instance: "bevy_brp".into(),
            uri: "screen://capture/0".into(),
            mime_type: None,
            size: None,
            text: None,
            blob_base64: Some("AAAA".into()),
            parent_resource_block_id: None,
        };
        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .role(Role::System)
            .resource_payload(payload.clone())
            .build();

        let parsed = roundtrip_snapshot(&snap);

        let parsed_payload = parsed.resource.expect("resource must survive");
        assert_eq!(parsed_payload.instance, "bevy_brp");
        assert_eq!(parsed_payload.uri, "screen://capture/0");
        assert_eq!(parsed_payload.mime_type, None, "has_mime_type=false must yield None");
        assert_eq!(parsed_payload.size, None, "has_size=false must yield None");
        assert_eq!(parsed_payload.text, None, "has_text=false must yield None");
        assert_eq!(parsed_payload.blob_base64, Some("AAAA".into()));
        assert_eq!(parsed_payload.parent_resource_block_id, None);
    }

    #[test]
    fn test_notification_payload_capnp_roundtrip_all_log_levels() {
        // Same ordinal-aliasing risk for LogLevel. One test per variant.
        let id = notif_ctx_id();
        let levels = [
            kaijutsu_types::LogLevel::Trace,
            kaijutsu_types::LogLevel::Debug,
            kaijutsu_types::LogLevel::Info,
            kaijutsu_types::LogLevel::Warn,
            kaijutsu_types::LogLevel::Error,
        ];
        for level in levels {
            let payload = kaijutsu_types::NotificationPayload {
                instance: "svc".into(),
                kind: kaijutsu_types::NotificationKind::Log,
                level: Some(level),
                tools: Vec::new(),
                count: None,
                detail: Some("m".into()),
            };
            let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
                .role(Role::System)
                .notification_payload(payload.clone())
                .build();
            let parsed = roundtrip_snapshot(&snap);
            assert_eq!(
                parsed.notification.and_then(|p| p.level),
                Some(level),
                "LogLevel::{:?} did not roundtrip through capnp",
                level,
            );
        }
    }

    /// Stage 1 (time-well) wire spine: `lastActivityAt` on `ContextHandleInfo`
    /// round-trips through `parse_context_info`, and an unset field (0 on the
    /// wire) normalizes to `None` — same sentinel convention as `concluded_at`.
    #[test]
    fn test_parse_context_info_last_activity_at_roundtrip() {
        let mut message = MessageBuilder::new_default();
        let mut builder =
            message.init_root::<crate::kaijutsu_capnp::context_handle_info::Builder>();
        builder.set_id(&[7u8; 16]);
        builder.set_last_activity_at(1234);
        let reader = message
            .get_root_as_reader::<crate::kaijutsu_capnp::context_handle_info::Reader>()
            .unwrap();
        let parsed = parse_context_info(&reader).unwrap();
        assert_eq!(parsed.last_activity_at, Some(1234));

        // Unset (default 0) must normalize to None, not Some(0).
        let mut message2 = MessageBuilder::new_default();
        let mut builder2 =
            message2.init_root::<crate::kaijutsu_capnp::context_handle_info::Builder>();
        builder2.set_id(&[7u8; 16]);
        let reader2 = message2
            .get_root_as_reader::<crate::kaijutsu_capnp::context_handle_info::Reader>()
            .unwrap();
        let parsed2 = parse_context_info(&reader2).unwrap();
        assert_eq!(parsed2.last_activity_at, None);
    }

    /// Bottom-dock gauge wire spine: `contextWindow` / `contextUsedTokens` /
    /// `contextUsedPct` on `ContextHandleInfo` round-trip through
    /// `parse_context_info`. The whole point of the `-1.0` sentinel is that
    /// it must NOT collide with a real 0% — this pins both directions: an
    /// unset/unknown window decodes as `None`, and a KNOWN window with a
    /// genuinely empty context (0 tokens used, 0.0%) decodes as `Some(0.0)`,
    /// never confused with "unknown". If someone "simplified" the sentinel
    /// back to a plain 0-means-unknown convention, the second block here
    /// would start failing.
    #[test]
    fn test_parse_context_info_usage_sentinel_roundtrip() {
        // Unknown window: nothing set on the wire (old server, or the
        // model has no configured window) — contextUsedPct's schema
        // default (-1.0) must decode as None, never Some(0.0).
        let mut message = MessageBuilder::new_default();
        let mut builder =
            message.init_root::<crate::kaijutsu_capnp::context_handle_info::Builder>();
        builder.set_id(&[9u8; 16]);
        let reader = message
            .get_root_as_reader::<crate::kaijutsu_capnp::context_handle_info::Reader>()
            .unwrap();
        let parsed = parse_context_info(&reader).unwrap();
        assert_eq!(parsed.context_window, None);
        assert_eq!(parsed.context_used_tokens, None);
        assert_eq!(
            parsed.context_used_pct, None,
            "unset contextUsedPct must decode as unknown, not Some(0.0)"
        );

        // Known window, freshly-used context: a real Some(0.0) must
        // survive intact.
        let mut message2 = MessageBuilder::new_default();
        let mut builder2 =
            message2.init_root::<crate::kaijutsu_capnp::context_handle_info::Builder>();
        builder2.set_id(&[9u8; 16]);
        builder2.set_context_window(200_000);
        builder2.set_context_used_pct(0.0);
        let reader2 = message2
            .get_root_as_reader::<crate::kaijutsu_capnp::context_handle_info::Reader>()
            .unwrap();
        let parsed2 = parse_context_info(&reader2).unwrap();
        assert_eq!(parsed2.context_window, Some(200_000));
        assert_eq!(
            parsed2.context_used_pct,
            Some(0.0),
            "a genuinely 0%-used known window must NOT be confused with unknown"
        );

        // A real mid-range usage value round-trips exactly too.
        let mut message3 = MessageBuilder::new_default();
        let mut builder3 =
            message3.init_root::<crate::kaijutsu_capnp::context_handle_info::Builder>();
        builder3.set_id(&[9u8; 16]);
        builder3.set_context_window(200_000);
        builder3.set_context_used_tokens(50_000);
        builder3.set_context_used_pct(25.0);
        let reader3 = message3
            .get_root_as_reader::<crate::kaijutsu_capnp::context_handle_info::Reader>()
            .unwrap();
        let parsed3 = parse_context_info(&reader3).unwrap();
        assert_eq!(parsed3.context_window, Some(200_000));
        assert_eq!(parsed3.context_used_tokens, Some(50_000));
        assert_eq!(parsed3.context_used_pct, Some(25.0));
    }

    /// Background-process ambient-state wire spine (`ContextHandleInfo`
    /// `background*` fields @23-@27, `kaijutsu_kernel::background_exec`).
    /// Two sentinels to pin down here, mirroring the `contextUsedPct`
    /// coverage above: (1) an unset/no-history context decodes every field
    /// as `None`/`0`, never a fabricated "0 running"-that-looks-intentional;
    /// (2) a genuinely successful `exited(0)` must survive as `Some(0)`,
    /// never collapsing into the `-1` "no exit code" sentinel a killed
    /// process uses.
    #[test]
    fn test_parse_context_info_background_sentinel_roundtrip() {
        // Nothing ever backgrounded in this context: every field must
        // decode as the honest "none" sentinel.
        let mut message = MessageBuilder::new_default();
        let mut builder =
            message.init_root::<crate::kaijutsu_capnp::context_handle_info::Builder>();
        builder.set_id(&[11u8; 16]);
        let reader = message
            .get_root_as_reader::<crate::kaijutsu_capnp::context_handle_info::Reader>()
            .unwrap();
        let parsed = parse_context_info(&reader).unwrap();
        assert_eq!(parsed.background_running_count, 0);
        assert_eq!(parsed.background_oldest_running_started_at, None);
        assert_eq!(parsed.background_last_finished_at, None);
        assert_eq!(parsed.background_last_finished_status, None);
        assert_eq!(
            parsed.background_last_exit_code, None,
            "unset backgroundLastExitCode (-1 schema default) must decode as unknown, not Some(-1)"
        );

        // Two still running, anchored on the oldest one's start time.
        let mut message2 = MessageBuilder::new_default();
        let mut builder2 =
            message2.init_root::<crate::kaijutsu_capnp::context_handle_info::Builder>();
        builder2.set_id(&[11u8; 16]);
        builder2.set_background_running_count(2);
        builder2.set_background_oldest_running_started_at(1_000);
        let reader2 = message2
            .get_root_as_reader::<crate::kaijutsu_capnp::context_handle_info::Reader>()
            .unwrap();
        let parsed2 = parse_context_info(&reader2).unwrap();
        assert_eq!(parsed2.background_running_count, 2);
        assert_eq!(parsed2.background_oldest_running_started_at, Some(1_000));
        assert_eq!(parsed2.background_last_finished_at, None);

        // A genuinely successful exit(0) must round-trip as Some(0), never
        // confused with "killed"/"no exit code" (-1).
        let mut message3 = MessageBuilder::new_default();
        let mut builder3 =
            message3.init_root::<crate::kaijutsu_capnp::context_handle_info::Builder>();
        builder3.set_id(&[11u8; 16]);
        builder3.set_background_last_finished_at(5_000);
        builder3.set_background_last_finished_status("exited");
        builder3.set_background_last_exit_code(0);
        let reader3 = message3
            .get_root_as_reader::<crate::kaijutsu_capnp::context_handle_info::Reader>()
            .unwrap();
        let parsed3 = parse_context_info(&reader3).unwrap();
        assert_eq!(parsed3.background_last_finished_at, Some(5_000));
        assert_eq!(parsed3.background_last_finished_status, Some("exited".to_string()));
        assert_eq!(
            parsed3.background_last_exit_code,
            Some(0),
            "a genuinely successful exit(0) must NOT be confused with the -1 'no exit code' sentinel"
        );

        // A killed process: no exit code at all — must decode as None, not
        // a fabricated 0 or the -1 wire sentinel leaking through.
        let mut message4 = MessageBuilder::new_default();
        let mut builder4 =
            message4.init_root::<crate::kaijutsu_capnp::context_handle_info::Builder>();
        builder4.set_id(&[11u8; 16]);
        builder4.set_background_last_finished_at(6_000);
        builder4.set_background_last_finished_status("killed");
        builder4.set_background_last_exit_code(-1);
        let reader4 = message4
            .get_root_as_reader::<crate::kaijutsu_capnp::context_handle_info::Reader>()
            .unwrap();
        let parsed4 = parse_context_info(&reader4).unwrap();
        assert_eq!(parsed4.background_last_finished_status, Some("killed".to_string()));
        assert_eq!(parsed4.background_last_exit_code, None);
    }

    /// Stage 3 (time-well) wire spine: `trackId` on `ContextHandleInfo`
    /// round-trips, and unset/empty normalizes to `None` (TrackIds are never
    /// empty by construction, so the empty-string sentinel is unambiguous).
    #[test]
    fn test_parse_context_info_track_id_roundtrip() {
        let mut message = MessageBuilder::new_default();
        let mut builder =
            message.init_root::<crate::kaijutsu_capnp::context_handle_info::Builder>();
        builder.set_id(&[7u8; 16]);
        builder.set_track_id("bass");
        let reader = message
            .get_root_as_reader::<crate::kaijutsu_capnp::context_handle_info::Reader>()
            .unwrap();
        let parsed = parse_context_info(&reader).unwrap();
        assert_eq!(parsed.track_id.as_deref(), Some("bass"));

        // Unset (old wire / unattached) must normalize to None.
        let mut message2 = MessageBuilder::new_default();
        let mut builder2 =
            message2.init_root::<crate::kaijutsu_capnp::context_handle_info::Builder>();
        builder2.set_id(&[7u8; 16]);
        let reader2 = message2
            .get_root_as_reader::<crate::kaijutsu_capnp::context_handle_info::Reader>()
            .unwrap();
        let parsed2 = parse_context_info(&reader2).unwrap();
        assert_eq!(parsed2.track_id, None);
    }

    /// `TrackInfo` round-trips through `parse_track_info` — every field,
    /// including the attached-context list.
    #[test]
    fn test_parse_track_info_roundtrip() {
        let mut message = MessageBuilder::new_default();
        let mut builder = message.init_root::<crate::kaijutsu_capnp::track_info::Builder>();
        builder.set_id("bass");
        builder.set_score_context_id(&[3u8; 16]);
        builder.set_playing(true);
        builder.set_playhead_tick(1234);
        builder.set_period_us(500_000);
        builder.set_beats_per_phrase(32);
        builder.set_beat_count(99);
        builder.set_last_epoch_ns(1_700_000_000_000_000_000);
        builder.set_clock_kind("system");
        {
            let mut att = builder.reborrow().init_attached(2);
            att.set(0, &[1u8; 16]);
            att.set(1, &[2u8; 16]);
        }
        let reader = message
            .get_root_as_reader::<crate::kaijutsu_capnp::track_info::Reader>()
            .unwrap();
        let parsed = parse_track_info(&reader).unwrap();
        assert_eq!(parsed.id, "bass");
        assert_eq!(parsed.score_context_id, ContextId::from_bytes([3u8; 16]));
        assert!(parsed.playing);
        assert_eq!(parsed.playhead_tick, 1234);
        assert_eq!(parsed.period_us, 500_000);
        assert_eq!(parsed.beats_per_phrase, 32);
        assert_eq!(parsed.beat_count, 99);
        assert_eq!(parsed.last_epoch_ns, 1_700_000_000_000_000_000);
        assert_eq!(parsed.clock_kind, "system");
        assert_eq!(
            parsed.attached,
            vec![ContextId::from_bytes([1u8; 16]), ContextId::from_bytes([2u8; 16])]
        );
    }

    /// `PeerInfo` round-trips through `parse_peer_info` — the decode helper
    /// `list_peers` uses. No live kernel needed: builds the capnp message
    /// directly, same shape `set_peer_info` (kaijutsu-server/src/rpc.rs)
    /// would produce.
    #[test]
    fn test_parse_peer_info_roundtrip() {
        let mut message = MessageBuilder::new_default();
        let mut builder = message.init_root::<crate::kaijutsu_capnp::peer_info::Builder>();
        builder.set_nick("mcp/toad");
        builder.set_attached_at(1_754_800_000_000);

        let reader = message
            .get_root_as_reader::<crate::kaijutsu_capnp::peer_info::Reader>()
            .unwrap();
        let parsed = parse_peer_info(&reader).unwrap();

        assert_eq!(parsed.nick, "mcp/toad");
        assert_eq!(parsed.attached_at, 1_754_800_000_000);
    }
}
