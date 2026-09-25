//! Unix socket listener for hook events.
//!
//! The MCP process opens a Unix socket as a secondary listener. Adapter
//! scripts (or `kaijutsu-mcp hook`) connect, send one JSON line, receive
//! one JSON response, and disconnect.
//!
//! On each event the listener:
//! 1. Creates kernel blocks in the shared store
//! 2. Pushes ops to the server (if remote)
//! 3. Checks for pending drift and injects it into the response

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use kaijutsu_types::{BlockKind, ContentType, ContextId, PrincipalId, Role, Status, ToolKind};
use kaijutsu_types::timeout::tiers;
use kaijutsu_kernel::SharedBlockStore;

use kaijutsu_client::{ActorHandle, AuthorBlock};

use crate::RemoteState;
use crate::hook_types::{
    HookEvent, HookResponse, KAIJUTSU_MCP_TOOLS, PingResponse, normalize_tool_name,
    short_session_suffix,
};

/// Connect timeout for the stale-socket sweep at serve startup
/// (`sweep_stale_sockets`). A listening socket accepts near-instantly; this
/// only bounds the worst case (e.g. a socket whose listener is wedged).
/// **Probe tier**, same reasoning.
const SWEEP_CONNECT_TIMEOUT: Duration = tiers::PROBE;

/// How long [`HookListener::bind_socket`] will wait for a `Live`/`Unknown`
/// socket path to clear before giving up. Successive `kaijutsu-mcp`
/// processes under one hosting-process PID compute the same socket path, so
/// a predecessor still finishing its own shutdown reads as `Live` for a
/// moment — a clean shutdown finishes well under this window.
const BIND_RETRY_WINDOW: Duration = Duration::from_secs(10);

/// Poll interval while `bind_socket` waits out a `Live`/`Unknown` path.
const BIND_RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// Maximum size of a block's content created from hook events.
const DEFAULT_MAX_BLOCK_SIZE: usize = 4096;

/// Bound on the archive RPC [`HookListener::archive_if_session_ended`] issues
/// at process shutdown. Not on the hook critical path — the hosting process
/// is already exiting — but still bounded so a wedged kernel connection
/// cannot hang that exit indefinitely. [`tiers::REQUEST`]: "a single call
/// that may do real work."
const ARCHIVE_ON_SHUTDOWN_TIMEOUT: Duration = tiers::REQUEST;

/// Run the hook's work under `budget`, degrading to a permissive response if
/// it overruns.
///
/// Extracted from `handle_connection` so the *degraded* branch is reachable
/// from a test — it is the branch that only fires when something is already
/// wrong, which is exactly the kind that rots untested.
///
/// On overrun the caller is told, in `context`, that the event may not have
/// been recorded. Saying so beats both alternatives: going silent would let a
/// missing block look like a normal turn, and denying would block the user's
/// action over an ambient mirror.
async fn with_hook_budget<F>(budget: Duration, event_name: &str, work: F) -> HookResponse
where
    F: std::future::Future<Output = HookResponse>,
{
    match tokio::time::timeout(budget, work).await {
        Ok(response) => response,
        Err(_) => {
            tracing::warn!(
                event = %event_name,
                ?budget,
                "hook path exceeded its budget — returning permissive response; \
                 this event's block may be missing or land late"
            );
            HookResponse::allow_with_context(format!(
                "kaijutsu: hook work exceeded {budget:?} and was cut short — this \
                 event may not have been recorded. The kernel is slow or \
                 mid-resync; your action was not blocked."
            ))
        }
    }
}

/// Guarantees a reserved `ToolCall` reaches a terminal state even if the task
/// authoring it is **cancelled** partway through.
///
/// Reserve-then-flow leaves a liveness residue: between `authorBlock(call)`
/// and `completeBlock`, the call sits at `Running` with its completion still
/// in the future. `insert_tool_blocks` handles the *error* form of that
/// (a failed result still completes the call at `Error`), but not the
/// *cancellation* form — and cancellation is not hypothetical here.
/// [`with_hook_budget`] is `tokio::time::timeout`, which DROPS the future
/// when the budget expires. A drop landing between those two RPCs means the
/// completion code simply never runs, and the block stays `Running` forever.
/// That is precisely the state slice 3 argued was legitimate *because it is
/// transient*; a leaked one is a lie in the log.
///
/// So the completion is attached to the reservation's lifetime rather than to
/// the code path. Dropping while armed spawns a detached `completeBlock` at
/// `Error`, which outlives the cancelled task. `disarm` is called only after
/// an explicit completion has actually succeeded — if it failed, the guard
/// stays armed and gets one more best-effort attempt on the way out.
struct CallReservation {
    actor: ActorHandle,
    context_id: ContextId,
    call_id: kaijutsu_types::BlockId,
    armed: bool,
}

impl CallReservation {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CallReservation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let actor = self.actor.clone();
        let context_id = self.context_id;
        let call_id = self.call_id;
        tracing::error!(
            %call_id,
            "hook tool authoring was cancelled or failed mid-sequence — completing the \
             reserved ToolCall at Error so it does not sit at Running forever"
        );
        tokio::spawn(async move {
            if let Err(e) = actor
                .complete_block(context_id, call_id, Status::Error, true, None)
                .await
            {
                tracing::error!(%call_id, "detached completeBlock also failed: {e}");
            }
        });
    }
}

/// Hook listener — receives events over a Unix socket and writes kernel blocks.
pub struct HookListener {
    /// Local-mode block store (in-process). `None` in remote mode, where blocks
    /// are authored by RPC through `RemoteState`'s actor — this process holds
    /// no replica of anything. (It once pushed into a local `SyncedDocument`;
    /// that path is gone, and so is the RPC it pushed through.)
    local_store: Option<SharedBlockStore>,
    /// Shared context ID — updated by register_session (None until then).
    shared_context_id: Arc<Mutex<Option<ContextId>>>,
    /// Fixed context ID for local mode (not shared).
    local_context_id: Option<ContextId>,
    /// Remote state for doc-task authoring + drift (None in local mode).
    remote: Option<RemoteState>,
    /// Max content size per block.
    max_block_size: usize,
    /// The session this listener serves: set at startup from the host's
    /// environment when it supplies one, otherwise by the first hook event
    /// that names it. See [`should_adopt_session_id`].
    session_id: Arc<Mutex<Option<String>>>,
    /// Shared hosting-agent name — bootstrapped from hook event `source` when
    /// startup detection could not identify the MCP host.
    agent_name: Arc<Mutex<Option<String>>>,
    /// Remote-only: the auto-register label's stable prefix
    /// (`auto_register_base` in `main.rs` — repo name, no launch
    /// timestamp), present only when the initial join used a placeholder
    /// label (session id wasn't known at register time). The first hook
    /// event that carries a `session_id` consumes it via `Mutex::take` and
    /// stabilizes onto `{base}-{sid8}` (`maybe_stabilize_label`) — `Some`
    /// means stabilization is still pending, `None` means it already
    /// happened or was never needed (manual `register_session`, local
    /// mode). Deliberately NOT gated to `session.start`: that event never
    /// fires again on a same-session MCP relaunch (`/mcp reconnect`, a
    /// kernel restart that killed the process) — exactly the case this
    /// exists to fix.
    pending_label_base: Mutex<Option<String>>,
    /// Guards `set_context_model` (from `session.start`'s `model` field) to
    /// at most one call per process.
    context_model_set: Mutex<bool>,
    /// A `session.end` recorded for this listener's own session
    /// ([`should_record_session_end`]), not yet confirmed by process exit.
    /// Cleared by ANY later hook event (proof the session kept going) or
    /// consumed by [`Self::archive_if_session_ended`] at real shutdown. See
    /// that method's doc comment for why `session.end` alone must not
    /// archive.
    session_end_recorded: Mutex<bool>,
}

/// Whether `incoming`, carried by a hook event named `event`, replaces the
/// current session id.
///
/// Only this host's hook events reach this listener (see
/// [`hook_client_socket_path`]), so an event-carried id is this host's. With
/// no id yet, the first one names the session. After that only
/// `session.start` renames it: `/clear` starts a new session id in the same
/// host process. The same id again is never an adoption.
pub fn should_adopt_session_id(current: Option<&str>, event: &str, incoming: &str) -> bool {
    match current {
        None => true,
        Some(cur) if cur == incoming => false,
        Some(_) => event == "session.start",
    }
}

/// Whether a `session.end` event should record this listener's joined
/// context as ended, pending confirmation by [`HookListener::
/// archive_if_session_ended`] that the process is really exiting.
/// `session.end` alone never archives (see that method's doc comment).
///
/// Requires the event's `session_id` to equal the stored one.
pub fn should_record_session_end(stored: Option<&str>, event_session_id: Option<&str>) -> bool {
    stored.is_some() && stored == event_session_id
}

/// Whether an event names a session other than the one this listener
/// serves. Such an event is refused before it writes anything. A listener
/// that has no session id yet, or an event that carries none, is not a
/// mismatch.
pub fn is_foreign_session(stored: Option<&str>, event_session_id: Option<&str>) -> bool {
    matches!((stored, event_session_id), (Some(s), Some(e)) if s != e)
}

impl HookListener {
    /// Get the current context ID (from shared or local).
    fn context_id(&self) -> Option<ContextId> {
        if let Some(id) = self.local_context_id {
            return Some(id);
        }
        self.shared_context_id.lock().ok().and_then(|g| *g)
    }

    /// Create a listener backed by a local-only store.
    pub fn local(store: SharedBlockStore, context_id: ContextId) -> Self {
        Self {
            local_store: Some(store),
            shared_context_id: Arc::new(Mutex::new(None)),
            local_context_id: Some(context_id),
            remote: None,
            max_block_size: DEFAULT_MAX_BLOCK_SIZE,
            session_id: Arc::new(Mutex::new(None)),
            agent_name: Arc::new(Mutex::new(None)),
            pending_label_base: Mutex::new(None),
            context_model_set: Mutex::new(false),
            session_end_recorded: Mutex::new(false),
        }
    }

    /// Create a listener backed by a remote connection.
    ///
    /// `shared_context_id` is updated by `register_session` when a context is joined.
    ///
    /// `pending_label_base`: `Some(base)` when the caller auto-registered
    /// with a placeholder label lacking a session-id suffix (session id
    /// unknown at register time) — the first hook event carrying a session
    /// id stabilizes the context onto `{base}-{first 8 chars}`
    /// (`maybe_stabilize_label`). Pass `None` when the label already
    /// carries a session id, or wasn't auto-generated.
    pub fn remote(
        remote: RemoteState,
        shared_context_id: Arc<Mutex<Option<ContextId>>>,
        session_id: Arc<Mutex<Option<String>>>,
        pending_label_base: Option<String>,
    ) -> Self {
        Self::remote_with_agent(
            remote,
            shared_context_id,
            session_id,
            Arc::new(Mutex::new(None)),
            pending_label_base,
        )
    }

    /// Create a remote listener sharing both session and host-agent identity
    /// with the MCP tool surface.
    pub fn remote_with_agent(
        remote: RemoteState,
        shared_context_id: Arc<Mutex<Option<ContextId>>>,
        session_id: Arc<Mutex<Option<String>>>,
        agent_name: Arc<Mutex<Option<String>>>,
        pending_label_base: Option<String>,
    ) -> Self {
        Self {
            local_store: None,
            shared_context_id,
            local_context_id: None,
            remote: Some(remote),
            max_block_size: DEFAULT_MAX_BLOCK_SIZE,
            session_id,
            agent_name,
            pending_label_base: Mutex::new(pending_label_base),
            context_model_set: Mutex::new(false),
            session_end_recorded: Mutex::new(false),
        }
    }

    /// Bind and start listening on a Unix socket, then serve forever.
    /// Convenience wrapper over [`Self::bind_socket`] + [`Self::serve`] for
    /// callers that don't need to observe bind success separately from serve
    /// (tests, and any future caller that doesn't need ownership tracking —
    /// see `main.rs`'s `run_serve`, which calls the two steps separately so
    /// it only unlinks the socket on exit if THIS process actually bound
    /// it).
    pub async fn start(self: Arc<Self>, socket_path: PathBuf) -> anyhow::Result<()> {
        let listener = Self::bind_socket(&socket_path).await?;
        self.serve(listener).await
    }

    /// Bind the hook Unix socket, refusing to steal a path a live listener
    /// still owns.
    ///
    /// Successive MCP processes under one Claude Code PPID compute the same
    /// path (`default_socket_path`) and each used to unlink-then-rebind it
    /// unconditionally. If the OLD process was still alive when a NEW one
    /// bound the path, the new bind silently replaced the socket special
    /// file out from under it: the old listener kept running and accepting
    /// on its already-open fd, but nothing could reach it by that path any
    /// more (observed live: "Hook socket listening" logged three times for
    /// one path). A socket special file left behind by an UNCLEAN exit is
    /// the one case safe to reclaim — `ECONNREFUSED` on connect proves
    /// nothing is listening.
    ///
    /// A `Live`/`Unknown` verdict is not necessarily a competing listener —
    /// the path is stable across reconnects, so a predecessor still
    /// finishing its own shutdown reads the same way for a moment. Rather
    /// than refuse on the first probe, wait out [`BIND_RETRY_WINDOW`]
    /// (polling every [`BIND_RETRY_INTERVAL`]) for the path to go `Stale`
    /// or disappear. Only a path still `Live`/`Unknown` at the end of that
    /// window is treated as a genuine competing listener: the caller
    /// decides whether to run without a hook socket rather than corrupt
    /// another listener's.
    pub async fn bind_socket(socket_path: &Path) -> anyhow::Result<UnixListener> {
        Self::bind_socket_with_retry(socket_path, BIND_RETRY_INTERVAL, BIND_RETRY_WINDOW).await
    }

    /// [`Self::bind_socket`] with the poll interval and wait window as
    /// parameters instead of the fixed [`BIND_RETRY_INTERVAL`] /
    /// [`BIND_RETRY_WINDOW`]. `bind_socket` is the constant-parameter
    /// production entry point; this split exists so a test can exercise the
    /// same refuse-at-deadline path in real wall-clock time without waiting
    /// out the production 10-second window.
    async fn bind_socket_with_retry(
        socket_path: &Path,
        interval: Duration,
        window: Duration,
    ) -> anyhow::Result<UnixListener> {
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        if socket_path.exists() {
            // Each call already carries its own Stale-confirmation
            // (`confirm_stale`): a raw `Stale` from `probe_socket_or_gone`
            // is re-checked once before this closure ever returns it, so
            // the polling loop below sees only confirmed verdicts.
            let probe = || confirm_stale(|| probe_socket_or_gone(socket_path), interval);

            let mut verdict = probe().await;
            if matches!(verdict, ExistingSocket::Live | ExistingSocket::Unknown) {
                tracing::info!(
                    path = %socket_path.display(),
                    window_ms = window.as_millis(),
                    "hook socket path still appears owned by another listener; waiting for \
                     it to clear before binding"
                );
                verdict = wait_for_socket_to_clear(probe, interval, window).await;
            }

            match verdict {
                ExistingSocket::Stale => {
                    // The path may already be gone (the "disappeared
                    // mid-wait" case above) — that is success, not a
                    // failure to report.
                    if let Err(e) = tokio::fs::remove_file(socket_path).await {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            return Err(e.into());
                        }
                    }
                }
                ExistingSocket::Live | ExistingSocket::Unknown => {
                    anyhow::bail!(
                        "refusing to bind hook socket {}: another listener appears to still \
                         own it (or its liveness could not be confirmed) — stealing the path \
                         would leave that listener unreachable while it keeps running",
                        socket_path.display()
                    );
                }
            }
        }

        let listener = UnixListener::bind(socket_path)?;
        tracing::info!(path = %socket_path.display(), "Hook socket listening");
        Ok(listener)
    }

    /// Serve hook connections on an already-bound socket. Runs until the
    /// socket is closed or the task is cancelled. Spawns a tokio task per
    /// connection.
    pub async fn serve(self: Arc<Self>, listener: UnixListener) -> anyhow::Result<()> {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let this = Arc::clone(&self);
                    tokio::spawn(async move {
                        if let Err(e) = this.handle_connection(stream).await {
                            tracing::debug!("Hook connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("Hook accept error: {e}");
                }
            }
        }
    }

    /// Handle a single connection: read one JSON line, process, respond, close.
    async fn handle_connection(&self, stream: tokio::net::UnixStream) -> anyhow::Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();

        buf_reader.read_line(&mut line).await?;
        let line = line.trim();

        if line.is_empty() {
            return Ok(());
        }

        let event: HookEvent = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(e) => {
                let err = serde_json::json!({"error": format!("Invalid JSON: {e}")});
                writer.write_all(err.to_string().as_bytes()).await?;
                writer.write_all(b"\n").await?;
                return Ok(());
            }
        };

        // Handle ping — return status without creating blocks
        if event.event == "ping" {
            let pending = self.pending_drift_count().await;
            let session_id = self.session_id.lock().ok().and_then(|g| g.clone());
            let ping = PingResponse {
                status: "ok".to_string(),
                pid: std::process::id(),
                cwd: std::env::current_dir()
                    .ok()
                    .map(|p| p.display().to_string()),
                context_name: self.context_id().map(|id| id.short()),
                document_id: self.context_id().map(|id| id.to_hex()),
                session_id,
                pending_drifts: pending,
            };
            let json = serde_json::to_string(&ping).unwrap_or_default();
            writer.write_all(json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            return Ok(());
        }

        // Capture session_id from hook events: the first id fills a void,
        // and `session.start` renames (`/clear` starts a new session id in
        // the same host process). See `should_adopt_session_id`.
        if let Some(ref event_session_id) = event.session_id
            && let Ok(mut guard) = self.session_id.lock()
            && should_adopt_session_id(guard.as_deref(), &event.event, event_session_id)
        {
            tracing::info!(
                session_id = %event_session_id,
                replaced = ?guard.as_deref(),
                "Captured session ID from hook event"
            );
            *guard = Some(event_session_id.clone());
        }

        // Refuse an event that names another session, before it writes
        // anything. Routing delivers only this host's events, so this fires
        // only on a routing bug; say so loudly.
        let stored = self.session_id.lock().ok().and_then(|g| g.clone());
        if is_foreign_session(stored.as_deref(), event.session_id.as_deref()) {
            tracing::warn!(
                event = %event.event,
                stored_session_id = ?stored,
                event_session_id = ?event.session_id,
                "refused a hook event from another session"
            );
            let json = serde_json::to_string(&HookResponse::allow()).unwrap_or_default();
            writer.write_all(json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            return Ok(());
        }

        // Hook source is authoritative when startup agent detection was not
        // available. SessionStart may also replace stale startup metadata.
        if !event.source.is_empty()
            && let Ok(mut guard) = self.agent_name.lock()
            && (guard.is_none() || event.event == "session.start")
        {
            *guard = Some(event.source.clone());
        }

        // Everything from here to the response is on the source agent's hook
        // critical path, bounded by `tiers::HOOK_PATH`, which fits under every
        // host's ceiling.
        //
        // Not hypothetical headroom. Every RPC on this path carries the
        // request tier, and authoring a tool pair is three of them
        // (reserve, result, complete) on top of `maybe_stabilize_label`'s —
        // so a slow or reconnecting kernel can hold the hook far past the
        // source agent's patience.
        //
        // Degrading here is consistent with what this path already promises
        // for *failure*: the mirror is ambient, so its slowness must not
        // block the user's action any more than its errors do. We return the
        // permissive response and say what happened in `context` rather than
        // going silent.
        let hook_work = async {
            // Stabilize the placeholder label onto a source-session-stable one
            // now that we (may) know the true session id — BEFORE authoring
            // this event's own block, so a relaunch-reattach switch lands it in
            // the right context. One-shot per process (`pending_label_base`'s
            // `Mutex::take`); a no-op on every event after the first that finds
            // a session id.
            if let Some(sid) = self.session_id.lock().ok().and_then(|g| g.clone()) {
                self.maybe_stabilize_label(&sid).await;
            }
            self.process_event(&event).await
        };

        let response = with_hook_budget(tiers::HOOK_PATH, &event.event, hook_work).await;

        let json = serde_json::to_string(&response).unwrap_or_default();
        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;

        Ok(())
    }

    /// Process a hook event: create blocks, check drift.
    ///
    /// In remote mode, block authoring is now direct RPC (`authorBlock` /
    /// `completeBlock`) awaited before this returns — the kernel is the
    /// writer, and this process no longer replicates. `author_error`
    /// accumulates the first authoring failure across the event (there's at
    /// most one insertion per event today, but this stays correct if that
    /// changes): on failure we still return ALLOW — recording what happened
    /// must not block the user's action — but make it visible by folding an
    /// error note into the response's `context` field alongside (or instead
    /// of) any drift.
    async fn process_event(&self, event: &HookEvent) -> HookResponse {
        // 0. Any event other than `session.end` proves this session is still
        // alive: clear a previously recorded pending archive rather than let
        // a later real shutdown archive a context that kept working in
        // between (`should_record_session_end`, `archive_if_session_ended`).
        // Checked before the self-referential-tool filter below — that
        // filter still means a genuine event reached this listener.
        if event.event != "session.end"
            && std::mem::take(
                &mut *self
                    .session_end_recorded
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            )
        {
            tracing::info!(
                event = %event.event,
                "hook event arrived after a reported session end — the \
                 session is still alive; clearing the pending archive"
            );
        }

        // 1. Filter self-referential kaijutsu MCP tools. Adapters normalize
        // source-specific tool names before they reach this listener.
        if let Some(ref tool) = event.tool {
            let normalized = normalize_tool_name(&tool.name);
            if KAIJUTSU_MCP_TOOLS
                .iter()
                .any(|t| normalized.eq_ignore_ascii_case(t))
            {
                // MCP server already recorded this — just check drift
                return self.maybe_inject_drift().await;
            }
        }

        let mut author_error: Option<String> = None;

        // 2. Create blocks based on event type
        match event.event.as_str() {
            "session.start" => {
                let model_info = event.model.as_deref().unwrap_or("unknown");
                let sid = event.session_id.as_deref().unwrap_or("unknown");
                let content = format!(
                    "Session started: {}, model: {}, session: {}",
                    event.source, model_info, sid
                );
                if let Err(e) = self.insert_text_block(Role::System, &content).await {
                    author_error = Some(e);
                }

                // Remote-only follow-up, at most once per process: tell the
                // kernel which model this context is talking to. Label
                // stabilization now happens generally in `handle_connection`
                // (`maybe_stabilize_label`), not gated to this event type —
                // see that method's doc comment for why.
                if let Some(ref remote) = self.remote
                    && let Some(ctx_id) = self.context_id()
                    && let Some(model) = event.model.as_deref()
                    && let Some(provider) = provider_for_source(&event.source)
                {
                    // Recover a poisoned lock (rather than `.ok()`-skip like
                    // `session_id` elsewhere) — this flag exists to stop a
                    // *duplicate* RPC, so losing track of "already called"
                    // under poisoning is the wrong failure mode.
                    let already_set = {
                        let mut guard =
                            self.context_model_set.lock().unwrap_or_else(|e| e.into_inner());
                        std::mem::replace(&mut *guard, true)
                    };
                    if !already_set {
                        match remote.actor.set_context_model(ctx_id, provider, model).await {
                            Ok(_) => {
                                tracing::info!(source = %event.source, provider, model, "Set context model from session.start")
                            }
                            Err(e) => tracing::warn!("Failed to set context model: {e}"),
                        }
                    }
                }
            }

            "session.end" => {
                let content = match event.reason.as_deref() {
                    Some(reason) => format!("Session ended: {reason}"),
                    None => "Session ended".to_string(),
                };
                if let Err(e) = self.insert_text_block(Role::System, &content).await {
                    author_error.get_or_insert(e);
                }

                // Record the end — but do NOT archive yet, and only when this
                // event really belongs to the session we serve. A
                // `session.end` hook event can fire while the hosting
                // process keeps right on running (observed live:
                // `docs/issues.md`, "A `session.end` hook archived a live
                // session's context"); archiving on the event alone trapped
                // that still-running session on a dead context —
                // `register_session` answered `already_registered` with the
                // archived id, `shell` was refused, and only `/mcp` (a fresh
                // process) recovered. `archive_if_session_ended`, called once
                // stdio actually closes, performs the real archive; any later
                // hook event clears this recording (see the top of this
                // function). `should_record_session_end` also requires the
                // event's session id to match the one this listener serves.
                // Ordering matters: the "Session ended" block above is
                // written first so the record is complete before archiving
                // can ever freeze it.
                //
                // Archiving — not concluding. `list_active_contexts` filters on
                // `archived_at IS NULL` only, and `conclude_context` never
                // touches `archived_at`, so concluding would leave the label
                // still competing for resolution and fix nothing. Archive is
                // also not "trash": archived contexts are retained work, kept
                // for referential integrity, later search, and research. It
                // frees the *name*, not the content.
                //
                // The name is deliberately left as-is (no rename, no suffix) —
                // the label leaves the active set intact so it stays meaningful
                // to the indexing work this is feeding.
                //
                // Why this matters: without it, `cc-*` contexts accumulate
                // forever and drift addressing degrades without bound. Drift
                // resolves label *prefixes*, so once many contexts share
                // `cc-<project>`, that prefix resolves to nothing usable.
                //
                // A session that keeps going after this (e.g. `/clear`
                // re-keying a live process) is safe: `register_session` never
                // resurrects an archived context, it mints a fresh
                // suffixed-label one.
                if self.remote.is_some()
                    && let Some(ctx_id) = self.context_id()
                {
                    let stored = self.session_id.lock().ok().and_then(|g| g.clone());
                    if should_record_session_end(stored.as_deref(), event.session_id.as_deref()) {
                        // Recover a poisoned lock rather than `.ok()`-skip —
                        // same reasoning as `context_model_set` above: losing
                        // track of "a session.end was recorded" under
                        // poisoning is the wrong failure mode.
                        *self
                            .session_end_recorded
                            .lock()
                            .unwrap_or_else(|e| e.into_inner()) = true;
                        tracing::info!(
                            context = %ctx_id.short(),
                            "recorded session.end — archiving deferred until the \
                             process actually exits (archive_if_session_ended)"
                        );
                    } else {
                        // Loud, not silent: a `session.end` this listener
                        // cannot attribute to its own session must never
                        // archive its context.
                        tracing::warn!(
                            context = %ctx_id.short(),
                            stored_session_id = ?stored,
                            event_session_id = ?event.session_id,
                            "session.end did not match this listener's owned session — \
                             not recording it for archive"
                        );
                    }
                }
            }

            "prompt.submit" => {
                if let Some(ref prompt) = event.prompt {
                    let truncated = truncate(prompt, self.max_block_size);
                    if let Err(e) = self.insert_text_block(Role::User, &truncated).await {
                        author_error.get_or_insert(e);
                    }
                }
            }

            "tool.after" => {
                if let Some(ref tool) = event.tool
                    && let Err(e) = self.insert_tool_blocks(tool, false).await
                {
                    author_error.get_or_insert(e);
                }
            }

            "tool.error" => {
                if let Some(ref tool) = event.tool
                    && let Err(e) = self.insert_tool_blocks(tool, true).await
                {
                    author_error.get_or_insert(e);
                }
            }

            "agent.stop" => {
                // Claude Code's Stop hook payload carries no response text —
                // only a transcript path. Fall back to the last assistant
                // message in the JSONL transcript when `response` is absent.
                let text = match event.response.as_deref() {
                    Some(r) => Some(r.to_string()),
                    None => match event.transcript_path.as_deref() {
                        Some(path) => match tokio::fs::read_to_string(path).await {
                            Ok(jsonl) => last_assistant_text(&jsonl),
                            Err(e) => {
                                tracing::debug!(
                                    path,
                                    "Failed to read transcript for agent.stop: {e}"
                                );
                                None
                            }
                        },
                        None => None,
                    },
                };
                if let Some(text) = text {
                    let truncated = truncate(&text, self.max_block_size);
                    if let Err(e) = self.insert_text_block(Role::Model, &truncated).await {
                        author_error.get_or_insert(e);
                    }
                }
            }

            "agent.compact" => {
                let content = match event.trigger.as_deref() {
                    Some(trigger) => format!("Context compaction ({trigger})"),
                    None => "Context compaction".to_string(),
                };
                if let Err(e) = self.insert_text_block(Role::System, &content).await {
                    author_error.get_or_insert(e);
                }
            }

            "file.edit" => {
                if let Some(ref file) = event.file {
                    let edit_count = file.edits.as_ref().map(|e| e.len()).unwrap_or(0);
                    let content = if edit_count > 0 {
                        format!(
                            "File edited: {} ({} edit{})",
                            file.path,
                            edit_count,
                            if edit_count == 1 { "" } else { "s" }
                        )
                    } else {
                        format!("File edited: {}", file.path)
                    };
                    if let Err(e) = self.insert_text_block(Role::Tool, &content).await {
                        author_error.get_or_insert(e);
                    }
                }
            }

            "subagent.start" => {
                let agent = event.principal_id.as_deref().unwrap_or("unknown");
                let kind = event.agent_type.as_deref().unwrap_or("subagent");
                if let Err(e) = self
                    .insert_text_block(Role::System, &format!("Subagent started: {agent} ({kind})"))
                    .await
                {
                    author_error.get_or_insert(e);
                }
            }

            "subagent.stop" => {
                let agent = event.principal_id.as_deref().unwrap_or("unknown");
                if let Err(e) = self
                    .insert_text_block(Role::System, &format!("Subagent stopped: {agent}"))
                    .await
                {
                    author_error.get_or_insert(e);
                }
            }

            "tool.before" => {
                // No block: the pair is authored on `tool.after`, which
                // carries the output too. What happens here instead is the
                // dry run — the kernel's PreCall hooks score a command that
                // is about to run in ANOTHER harness, so the ledger learns
                // from it (`docs/kaish-integration.md`).
                //
                // Detached and never awaited. This path must not add
                // latency to the harness's own reply, and must not fail it:
                // a kernel that is down, slow, or refusing changes nothing
                // about the response below.
                self.spawn_shell_dry_run(event);
            }

            _ => {}
        }

        // 3. Check for pending drift, then fold in any authoring failure —
        // LOUD (the caller already `tracing::error!`'d it) and visible
        // however the hook reply protocol permits: the `context` field,
        // alongside any real drift.
        let response = self.maybe_inject_drift().await;
        match author_error {
            Some(err) => {
                let note = format!("[kaijutsu-mcp mirror error] {err}");
                let context = match response.context {
                    Some(existing) => format!("{existing}\n\n{note}"),
                    None => note,
                };
                HookResponse::allow_with_context(context)
            }
            None => response,
        }
    }

    /// The Bash command a `tool.before` event carries, or `None`.
    ///
    /// Narrow on purpose: `Bash` by exact name, and a `command` that is
    /// really a string. Every other tool the harness runs (a file edit, a
    /// search) is not a shell submission and has no clause for the kernel's
    /// hooks to score.
    fn bash_command(event: &HookEvent) -> Option<&str> {
        let tool = event.tool.as_ref()?;
        if tool.name != "Bash" {
            return None;
        }
        tool.input.get("command")?.as_str()
    }

    /// Send one `tool.before` Bash command to the kernel for a dry-run
    /// PreCall evaluation, and return immediately.
    ///
    /// Detached by construction: the reply this listener is about to write
    /// must never wait on the kernel. Every failure — no connection, no
    /// joined context, a kernel that is down, an RPC that errors — is a
    /// `debug!` and nothing more, because a scoring path that could break
    /// the harness it observes would be worse than one that goes quiet.
    fn spawn_shell_dry_run(&self, event: &HookEvent) {
        let Some(command) = Self::bash_command(event) else {
            return;
        };
        let Some(remote) = self.remote.as_ref() else {
            return;
        };
        let Some(context_id) = self.context_id() else {
            return;
        };
        let actor = remote.actor.clone();
        let command = command.to_string();
        tokio::spawn(async move {
            match actor.shell_dry_run(context_id, command).await {
                Ok(report) => tracing::debug!(
                    outcome = ?report.outcome,
                    hook_id = ?report.hook_id,
                    "shell dry run reported"
                ),
                Err(e) => tracing::debug!("shell dry run did not complete: {e}"),
            }
        });
    }

    // -- Block insertion helpers --
    //
    // Remote mode authors over RPC (`authorBlock` / `completeBlock`) — the
    // kernel is the writer, and every reader here reads the kernel back
    // directly (there is no local mirror). On failure: still LOUD
    // (`tracing::error!`, not `warn!`; the caller folds the returned message
    // into the hook's `context` field) but never fails the hook call itself —
    // recording an event must not block the user's actual action. Local mode
    // is unchanged (no RPC there; it writes the in-process `SharedBlockStore`
    // directly, as before).

    /// The principal hook-authored blocks belong to: the **agent session**,
    /// derived deterministically from the Claude Code session id.
    ///
    /// This is the one identity question the hook path has to answer, and it
    /// used to have two wrong answers. Local mode stamped
    /// `PrincipalId::system()`, which claims the kernel wrote the block and
    /// erases the agent. Remote mode carried a `PrincipalId::new()` minted
    /// once per MCP *process*, so one Claude Code session authored under a
    /// different principal after every `/mcp reconnect` and a single context
    /// filled up with blocks from N anonymous principals that were in fact
    /// the same agent. Same family as the drift/rc identity smear fixed in
    /// `b356fc45`: a block whose author does not answer "who did this".
    ///
    /// Safe to call at authoring time because `handle_connection` captures
    /// the session id from the event **before** dispatching to
    /// `process_event` — the same ordering that lets `maybe_stabilize_label`
    /// run before this event's own block is authored.
    ///
    /// The fallback is `system()` and it is a real (if rare) loss of
    /// attribution — an event carrying no session id at all, before any
    /// event has carried one — so it warns rather than passing silently.
    fn author_principal(&self) -> PrincipalId {
        match self.session_id.lock().ok().and_then(|g| g.clone()) {
            Some(sid) => PrincipalId::for_agent_session(&sid),
            None => {
                tracing::warn!(
                    "hook authoring before any session id was seen — attributing to \
                     system(); this block will not name the agent that caused it"
                );
                PrincipalId::system()
            }
        }
    }

    async fn insert_text_block(&self, role: Role, content: &str) -> Result<(), String> {
        let Some(ctx_id) = self.context_id() else {
            tracing::debug!("Hook insert_text_block: no context yet (register_session not called)");
            return Ok(());
        };
        if let Some(store) = &self.local_store {
            if let Err(e) = store.insert_block_as(
                ctx_id,
                None, // parent
                None, // after (append)
                role,
                BlockKind::Text,
                content,
                Status::Done,
                ContentType::Plain,
                Some(self.author_principal()),
            ) {
                tracing::warn!("Hook insert_block error: {e}");
            }
            return Ok(());
        }
        let Some(remote) = &self.remote else {
            return Ok(());
        };
        match remote
            .actor
            .author_block(AuthorBlock::text(
                ctx_id,
                self.author_principal(),
                role,
                content,
            ))
            .await
        {
            Ok(_id) => Ok(()),
            Err(e) => {
                tracing::error!("Hook insert_text_block: authorBlock failed: {e}");
                Err(format!("failed to author text block: {e}"))
            }
        }
    }

    async fn insert_tool_blocks(
        &self,
        tool: &crate::hook_types::ToolInfo,
        is_error: bool,
    ) -> Result<(), String> {
        let Some(ctx_id) = self.context_id() else {
            tracing::debug!(
                "Hook insert_tool_blocks: no context yet (register_session not called)"
            );
            return Ok(());
        };
        let input = tool.input.clone();
        let content = if is_error {
            tool.error.as_deref().unwrap_or("(error)")
        } else {
            tool.output.as_deref().unwrap_or("(no output)")
        };
        let truncated = truncate(content, self.max_block_size);

        if let Some(store) = &self.local_store {
            // Insert tool call block
            let call_id = match store.insert_tool_call_as(
                ctx_id,
                None,
                None,
                &tool.name,
                input,
                Some(ToolKind::Mcp),
                Some(self.author_principal()),
                None,
                None,
            ) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!("Hook insert_tool_call error: {e}");
                    return Ok(());
                }
            };
            if let Err(e) = store.insert_tool_result_as(
                ctx_id,
                &call_id,
                None,
                &truncated,
                if is_error { Status::Error } else { Status::Done },
                None,
                Some(ToolKind::Mcp),
                Some(self.author_principal()),
                None,
            ) {
                tracing::warn!("Hook insert_tool_result error: {e}");
            }
            // The call block is inserted Status::Running (see
            // kaijutsu_types::block_store::BlockStore::insert_tool_call) and
            // only the LLM streaming path transitioned it to Done/Error —
            // hook-authored calls never did, so they stayed "running"
            // forever in the UI. Mirror the LLM path: complete the call once
            // its result has landed.
            let final_status = if is_error { Status::Error } else { Status::Done };
            if let Err(e) = store.set_status(ctx_id, &call_id, final_status) {
                tracing::warn!("Hook set_status (tool call) error: {e}");
            }
            return Ok(());
        }
        let Some(remote) = &self.remote else {
            return Ok(());
        };
        let principal = self.author_principal();

        // Reserve, then flow. Three RPCs rather than one, deliberately: the
        // call is reserved at Running, the result arrives parented to it,
        // and only then does the call move to its terminal state.
        //
        // A `tool.after` hook already knows the outcome, so we could author
        // the call straight to Done and save a round trip on a path that
        // answers inside `tiers::HOOK_PATH`. We don't, because of what a
        // reader sees BETWEEN the two writes: a Done ToolCall with no result
        // yet looks like the result was lost, while a Running one reads as
        // exactly what it is. Both intermediate states are transient; only
        // one of them is honest.
        let call_id = match remote
            .actor
            .author_block(AuthorBlock::tool_call(
                ctx_id,
                principal,
                &tool.name,
                input,
                Some(ToolKind::Mcp),
            ))
            .await
        {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Hook insert_tool_blocks: authorBlock(call) failed: {e}");
                return Err(format!("failed to author tool call: {e}"));
            }
        };

        // From here on the reservation exists and MUST reach a terminal
        // state, including if this task is cancelled mid-flight — see
        // `CallReservation`.
        let mut reservation = CallReservation {
            actor: remote.actor.clone(),
            context_id: ctx_id,
            call_id,
            armed: true,
        };

        let result_err = remote
            .actor
            .author_block(AuthorBlock::tool_result(
                ctx_id,
                principal,
                call_id,
                truncated,
                is_error,
                Some(ToolKind::Mcp),
            ))
            .await
            .err();

        // Complete the call either way. If the RESULT failed to author, the
        // reservation is out there at Running with nothing coming for it.
        // Landing it at Error is the truth we actually have: the call
        // happened, and its result did not reach the kernel.
        let errored = is_error || result_err.is_some();
        let final_status = if errored { Status::Error } else { Status::Done };
        if let Some(ref e) = result_err {
            tracing::error!("Hook insert_tool_blocks: authorBlock(result) failed: {e}");
        }

        // `is_error` must agree with `final_status` — the server refuses the
        // pair when they contradict. Deriving both from `errored` is what
        // keeps them in step; passing the raw `is_error` here was a real bug
        // (a tool that SUCCEEDED but whose result failed to author sends
        // status=Error with is_error=false, which the server rejects).
        match remote
            .actor
            .complete_block(ctx_id, call_id, final_status, errored, None)
            .await
        {
            // Only now is the guard unnecessary: the block is terminal.
            Ok(()) => reservation.disarm(),
            Err(e) => {
                // Leave it armed — the drop guard gets one more detached
                // attempt, which is strictly better than returning here and
                // leaving the call at Running.
                tracing::error!("Hook insert_tool_blocks: completeBlock failed: {e}");
                // Report BOTH failures when both happened; returning only
                // "left pending" would hide that the result never landed
                // either, which is the more consequential of the two.
                return Err(match result_err {
                    Some(re) => format!("tool result failed to author ({re}) AND the call \
                                         could not be completed ({e})"),
                    None => format!("tool call left pending: {e}"),
                });
            }
        }

        match result_err {
            None => Ok(()),
            Some(e) => Err(format!("failed to author tool result: {e}")),
        }
    }

    /// Archive this listener's joined context if a `session.end` was
    /// recorded ([`should_record_session_end`]) and no later hook event
    /// cleared it (see the top of [`Self::process_event`]).
    ///
    /// Call exactly once, from the stdio server's own shutdown path (the
    /// hosting process's stdin/stdout actually closing) — never from the
    /// `session.end` hook event itself. The event alone is not proof the
    /// session ended: Claude Code can deliver `SessionEnd` while its process
    /// keeps running (observed live, `docs/issues.md`, "A `session.end` hook
    /// archived a live session's context"), and archiving on that event
    /// trapped the still-running session on a dead context (`shell` refused,
    /// `register_session` stuck on `already_registered`). Stdio actually
    /// closing is the one signal that is never a false positive.
    ///
    /// Bounded by [`ARCHIVE_ON_SHUTDOWN_TIMEOUT`] so a wedged kernel
    /// connection cannot hang process exit; a failure or timeout is logged
    /// loudly rather than swallowed, matching the old inline archive's
    /// failure handling.
    pub async fn archive_if_session_ended(&self) {
        let recorded = std::mem::take(
            &mut *self
                .session_end_recorded
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        if !recorded {
            return;
        }
        let Some(ref remote) = self.remote else { return };
        let Some(ctx_id) = self.context_id() else { return };
        match tokio::time::timeout(
            ARCHIVE_ON_SHUTDOWN_TIMEOUT,
            remote.actor.archive_context(ctx_id),
        )
        .await
        {
            Ok(Ok(())) => tracing::info!(
                context = %ctx_id.short(),
                "archived context: the session actually ended (stdio closed)"
            ),
            // Loud, not swallowed: a failure here means the label keeps
            // competing for drift resolution, which is the whole problem
            // archiving exists to close.
            Ok(Err(e)) => tracing::warn!(
                context = %ctx_id.short(),
                "failed to archive context on session end, its label stays \
                 in the active set: {e}"
            ),
            Err(_) => tracing::warn!(
                context = %ctx_id.short(),
                timeout = ?ARCHIVE_ON_SHUTDOWN_TIMEOUT,
                "archiving on session end timed out at process shutdown, \
                 its label stays in the active set"
            ),
        }
    }

    /// Stabilize the auto-registered placeholder context onto a label that's
    /// stable for the whole Claude Code session, once a hook event reveals
    /// the true session id. See `pending_label_base`'s doc comment for why
    /// this isn't gated to `session.start`.
    ///
    /// Delegates the actual label resolution (rename in place vs. switch to
    /// a prior process's context) to `crate::stabilize_context_label` — the
    /// same upsert machinery `register_session` uses, so a relaunch that
    /// lands here re-attaches to the SAME context an earlier process in
    /// this session already stabilized, instead of minting another
    /// placeholder.
    async fn maybe_stabilize_label(&self, session_id: &str) {
        let Some(ref remote) = self.remote else { return };

        // One-shot: `Mutex::take` leaves `None` behind, so every later call
        // (from later events) is a no-op regardless of outcome below.
        let base = {
            let mut guard =
                self.pending_label_base.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        let Some(base) = base else { return };

        let Some(current_context_id) = self.context_id() else {
            // Shouldn't happen — auto-register sets shared_context_id before
            // the listener starts accepting connections. Put the base back
            // rather than lose it silently; the next event with a session id
            // retries.
            let mut guard =
                self.pending_label_base.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(base);
            return;
        };

        let suffix = short_session_suffix(session_id);
        let stable_label = format!("{base}-{suffix}");
        match crate::stabilize_context_label(remote, current_context_id, stable_label.clone())
            .await
        {
            Ok(outcome) => tracing::info!(
                label = %outcome.label,
                context_id = %outcome.context_id,
                switched = outcome.context_id != current_context_id,
                "Stabilized auto-registered context onto session-derived label",
            ),
            Err(e) => tracing::warn!(
                label = %stable_label,
                "Failed to stabilize context label: {e}"
            ),
        }
    }

    // -- Drift injection --

    async fn pending_drift_count(&self) -> u32 {
        let Some(ref remote) = self.remote else {
            return 0;
        };
        let Some(ctx_id) = self.context_id() else {
            return 0;
        };
        match remote.actor.drift_queue().await {
            Ok(queue) => queue.iter().filter(|d| d.target_ctx == ctx_id).count() as u32,
            Err(_) => 0,
        }
    }

    async fn maybe_inject_drift(&self) -> HookResponse {
        let Some(ref remote) = self.remote else {
            return HookResponse::allow();
        };
        let Some(ctx_id) = self.context_id() else {
            return HookResponse::allow();
        };

        // Check for drifts targeted at our context
        let queue = match remote.actor.drift_queue().await {
            Ok(q) => q,
            Err(_) => return HookResponse::allow(),
        };

        let our_drifts: Vec<_> = queue.iter().filter(|d| d.target_ctx == ctx_id).collect();

        if our_drifts.is_empty() {
            return HookResponse::allow();
        }

        // Build context string from drifts targeted at us
        let context: String = our_drifts
            .iter()
            .map(|d| format!("[Drift from {}]: {}", d.source_ctx.short(), d.content))
            .collect::<Vec<_>>()
            .join("\n\n");

        // Cancel individual drifts we've consumed — avoids global flush which
        // would silently deliver other contexts' drifts without notifying them.
        for drift in &our_drifts {
            if let Err(e) = remote.actor.drift_cancel(drift.id).await {
                tracing::warn!(drift_id = drift.id, "Failed to cancel consumed drift: {e}");
            }
        }

        tracing::info!(
            count = our_drifts.len(),
            "Injected drift context via hook response"
        );

        HookResponse::allow_with_context(context)
    }
}

/// Map a normalized hook source to the kernel provider which serves its model.
///
/// A source without an explicit mapping must not pin a context to an arbitrary
/// provider: its model identifier may be meaningful only to that source.
fn provider_for_source(source: &str) -> Option<&'static str> {
    match source {
        "claude-code" => Some("anthropic"),
        "codex" => Some("codex-app"),
        _ => None,
    }
}

/// Truncate a string to `max_len` bytes at a char boundary.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let mut end = max_len;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        let mut result = s[..end].to_string();
        result.push_str("\n... (truncated)");
        result
    }
}

/// The hook socket for the host process `host_pid`:
/// `{runtime_dir}/kaijutsu/hook-{host_pid}.sock`.
fn hook_socket_in(runtime_dir: &Path, host_pid: u32) -> PathBuf {
    runtime_dir.join("kaijutsu").join(format!("hook-{host_pid}.sock"))
}

/// The socket this MCP server listens on, named for its parent: the host
/// process (Claude Code, Codex) that spawned it.
///
/// Returns `None` if no user runtime directory exists — we don't fall back
/// to `/tmp` to avoid socket permission issues on shared systems.
pub fn default_socket_path() -> Option<PathBuf> {
    Some(hook_socket_in(&hook_runtime_dir()?, std::os::unix::process::parent_id()))
}

/// The one socket a hook client may deliver to: the listener of the MCP
/// server that its own host process spawned. An explicit `--socket` wins,
/// then the host pid in `CLAUDE_PID`, then the client's parent pid. A host
/// spawns its hooks directly, so the parent pid names the same process.
///
/// There is no other candidate. An event whose own listener is absent is
/// dropped, never handed to another session's listener, which would adopt
/// its session id and could archive its own context on that session's end.
///
/// `None` (drop the event) if `CLAUDE_PID` is not a pid or no runtime
/// directory exists.
pub fn hook_client_socket_path(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if explicit.is_some() {
        return explicit;
    }
    let host_pid = host_pid_for_hook(
        std::env::var("CLAUDE_PID").ok().as_deref(),
        std::os::unix::process::parent_id(),
    )?;
    Some(hook_socket_in(&hook_runtime_dir()?, host_pid))
}

/// The host process a hook client belongs to: `CLAUDE_PID` when the host
/// sets it, otherwise the client's parent. A `CLAUDE_PID` that is not a pid
/// is `None`, not a guess.
fn host_pid_for_hook(claude_pid: Option<&str>, ppid: u32) -> Option<u32> {
    let Some(value) = claude_pid else {
        return Some(ppid);
    };
    match value.trim().parse() {
        Ok(pid) => Some(pid),
        Err(_) => {
            tracing::warn!(claude_pid = %value, "CLAUDE_PID is not a pid; dropping the hook event");
            None
        }
    }
}

/// List `hook-*.sock` entries directly under `dir` (non-recursive), for
/// `sweep_stale_sockets`. Returns empty if `dir` doesn't exist.
fn list_hook_sockets_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().map(|e| e == "sock").unwrap_or(false)
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("hook-"))
                    .unwrap_or(false)
        })
        .collect()
}

/// Runtime directory for hook transport. Command-hook hosts sometimes pass a
/// minimal environment, so on Linux recover the conventional user runtime
/// directory without requiring a shell wrapper to export XDG_RUNTIME_DIR.
fn hook_runtime_dir() -> Option<PathBuf> {
    if let Some(path) = dirs::runtime_dir() { return Some(path); }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let uid = std::fs::metadata("/proc/self").ok()?.uid();
        let candidate = PathBuf::from(format!("/run/user/{uid}"));
        if candidate.is_dir() { return Some(candidate); }
    }
    None
}

/// Scan `dir` for `hook-*.sock` files other than `keep` and unlink any that
/// refuse connections (`ECONNREFUSED` — the listener process is gone but the
/// socket special file outlives it; nothing cleans these up on an unclean
/// exit). A refusal is confirmed once (see [`confirm_stale`]) before it
/// drives an unlink. Never touches a socket that accepts a connection, and
/// never touches `keep` (the path we're about to bind ourselves). Returns
/// the number removed.
pub async fn sweep_stale_sockets(dir: &Path, keep: &Path) -> usize {
    let mut removed = 0;
    for path in list_hook_sockets_in(dir) {
        if path == keep {
            continue;
        }
        let verdict = confirm_stale(|| probe_socket_or_gone(&path), BIND_RETRY_INTERVAL).await;
        if let ExistingSocket::Stale = verdict {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => removed += 1,
                Err(e) => tracing::debug!(
                    path = %path.display(),
                    "Failed to unlink stale hook socket: {e}"
                ),
            }
        }
        // Live or Unknown (accepted, some other error, or timed out) —
        // leave it alone rather than guess.
    }
    removed
}

/// The bind-time liveness verdict for an existing socket special file.
enum ExistingSocket {
    /// Nothing answered — `ECONNREFUSED` proves the listener process is
    /// gone and the file is an unclean exit's leftover. Safe to unlink.
    Stale,
    /// A connection was accepted — a listener is still actually there.
    Live,
    /// Neither confirmed: some other I/O error, or the probe timed out.
    /// Treated the same as `Live` — refusing to touch it is the safe
    /// failure; silently reclaiming it is not.
    Unknown,
}

/// Probe whether something is listening at `path` by connecting to it.
/// Shared by [`sweep_stale_sockets`] (removes only `Stale`) and
/// [`HookListener::bind_socket`] (refuses to bind over anything but
/// `Stale`).
async fn probe_existing_socket(path: &Path) -> ExistingSocket {
    match tokio::time::timeout(SWEEP_CONNECT_TIMEOUT, tokio::net::UnixStream::connect(path)).await
    {
        Ok(Ok(_stream)) => ExistingSocket::Live,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => ExistingSocket::Stale,
        _ => ExistingSocket::Unknown,
    }
}

/// [`probe_existing_socket`], treating an already-vanished special file as
/// `Stale` (nothing left to steal) rather than the conservative `Unknown`
/// a connect against a missing path produces (`NotFound`, not
/// `ConnectionRefused`) — a predecessor's clean shutdown can remove the
/// path mid-check.
async fn probe_socket_or_gone(path: &Path) -> ExistingSocket {
    if !path.exists() { ExistingSocket::Stale } else { probe_existing_socket(path).await }
}

/// A single `Stale` verdict is provisional, never final grounds to unlink:
/// on BSD-derived systems (macOS included) `connect()` to a live
/// `AF_UNIX` listener whose accept backlog is momentarily full also
/// returns `ECONNREFUSED` — the same error a genuinely dead listener
/// produces (Linux returns `EAGAIN` for a full backlog instead, already
/// classified `Unknown`, never `Stale`). Trust a `Stale` verdict only
/// after a second, confirming call: wait one `interval`, then probe
/// again. `Stale` stands only if that second call agrees; any other
/// verdict overrides the first, since unlinking on an unconfirmed `Stale`
/// is the exact socket theft this mechanism exists to prevent. Shared by
/// [`HookListener::bind_socket`] and [`sweep_stale_sockets`] so the rule
/// lives once.
async fn confirm_stale<F, Fut>(mut probe: F, interval: Duration) -> ExistingSocket
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ExistingSocket>,
{
    let first = probe().await;
    if !matches!(first, ExistingSocket::Stale) {
        return first;
    }
    tokio::time::sleep(interval).await;
    probe().await
}

/// Poll `probe` every `interval` until it stops reporting `Live`/`Unknown`
/// or `window` elapses, whichever comes first. Returns the last verdict —
/// `Stale` (or an `Unknown` that never resolved) once seen, otherwise the
/// verdict standing when the window runs out.
///
/// Takes the probe as a closure so [`HookListener::bind_socket`]'s retry
/// policy is exercisable without a real socket or a real 10-second wait: a
/// test injects a probe that flips from `Live` to `Stale` after a fixed
/// number of calls, plus a millisecond-scale `interval`/`window`.
async fn wait_for_socket_to_clear<F, Fut>(
    mut probe: F,
    interval: Duration,
    window: Duration,
) -> ExistingSocket
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ExistingSocket>,
{
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let verdict = probe().await;
        if !matches!(verdict, ExistingSocket::Live | ExistingSocket::Unknown) {
            return verdict;
        }
        if tokio::time::Instant::now() >= deadline {
            return verdict;
        }
        tokio::time::sleep(interval).await;
    }
}

/// Extract the last assistant message's text from a Claude Code transcript
/// (JSONL, one JSON object per line).
///
/// Assistant entries look like `{"type":"assistant","message":{"content":
/// [{"type":"text","text":"..."}, ...]}}`; other line types (`user`,
/// `system`, `summary`, tool results nested in `user` entries) and
/// malformed lines are skipped. An assistant entry with only `tool_use`
/// content (no `text` parts) doesn't count as "the last one" — we want the
/// last assistant entry that actually said something. Multiple text parts
/// in one entry are concatenated in order.
fn last_assistant_text(jsonl: &str) -> Option<String> {
    let mut last: Option<String> = None;
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let Some(content) = value.pointer("/message/content").and_then(|v| v.as_array()) else {
            continue;
        };
        let text: String = content
            .iter()
            .filter(|c| c.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
            .collect();
        if !text.is_empty() {
            last = Some(text);
        }
    }
    last
}

/// Connect to a hook socket, send an event, and return the response.
///
/// This is the client side — used by `kaijutsu-mcp hook` subcommand.
/// Fail-open: returns `Ok(None)` if the socket doesn't exist.
pub async fn send_hook_event(
    socket_path: &Path,
    event_json: &str,
) -> anyhow::Result<Option<String>> {
    if !socket_path.exists() {
        return Ok(None);
    }

    let stream = tokio::net::UnixStream::connect(socket_path).await?;
    let (reader, mut writer) = stream.into_split();

    // Send event as a single JSON line
    writer.write_all(event_json.as_bytes()).await?;
    if !event_json.ends_with('\n') {
        writer.write_all(b"\n").await?;
    }
    writer.shutdown().await?;

    // Read response
    let mut buf_reader = BufReader::new(reader);
    let mut response = String::new();
    buf_reader.read_line(&mut response).await?;

    Ok(Some(response))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use kaijutsu_types::DocKind;

    use super::*;

    // -- session id policy --

    #[test]
    fn the_first_event_carried_id_names_the_session() {
        assert!(should_adopt_session_id(None, "tool.before", "83768815-this"));
    }

    /// Once the session has an id, only `session.start` renames it.
    #[test]
    fn a_named_session_is_renamed_only_by_session_start() {
        assert!(!should_adopt_session_id(Some("83768815-this"), "tool.before", "other-session"));
        assert!(should_adopt_session_id(Some("83768815-this"), "session.start", "4e1d0c2a-next"));
    }

    #[test]
    fn the_same_session_id_is_not_adopted_again() {
        assert!(!should_adopt_session_id(Some("83768815-this"), "session.start", "83768815-this"));
        assert!(!should_adopt_session_id(Some("83768815-this"), "tool.before", "83768815-this"));
    }

    /// A host-supplied id is as good as an event-carried one: its own
    /// `session.end` records.
    #[test]
    fn a_matching_session_end_records() {
        assert!(should_record_session_end(Some("83768815-this"), Some("83768815-this")));
    }

    #[test]
    fn another_sessions_end_does_not_record() {
        assert!(!should_record_session_end(Some("83768815-this"), Some("other-session")));
    }

    #[test]
    fn a_session_end_without_ids_does_not_record() {
        assert!(!should_record_session_end(None, Some("some-session")));
        assert!(!should_record_session_end(Some("83768815-this"), None));
        assert!(!should_record_session_end(None, None));
    }

    #[test]
    fn only_two_different_ids_make_an_event_foreign() {
        assert!(is_foreign_session(Some("83768815-this"), Some("other-session")));
        assert!(!is_foreign_session(Some("83768815-this"), Some("83768815-this")));
        assert!(!is_foreign_session(None, Some("other-session")));
        assert!(!is_foreign_session(Some("83768815-this"), None));
    }

    /// Serve `listener` on a fresh socket under a temp dir and wait until
    /// the socket file exists.
    async fn serve_on_temp_socket(listener: HookListener, tag: &str) -> PathBuf {
        let socket_path = unique_temp_dir(tag).join("hook-test.sock");
        let listener = Arc::new(listener);
        let bg_path = socket_path.clone();
        tokio::spawn(async move {
            let _ = listener.start(bg_path).await;
        });
        for _ in 0..100 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(socket_path.exists(), "hook socket never bound");
        socket_path
    }

    fn bash_after_json(session_id: &str) -> String {
        serde_json::json!({
            "event": "tool.after",
            "source": "claude-code",
            "session_id": session_id,
            "tool": {"name": "Bash", "input": {"command": "ls"}, "output": "total 0"},
        })
        .to_string()
    }

    /// An event that names another session writes nothing into this
    /// listener's context.
    #[tokio::test]
    async fn an_event_from_another_session_writes_nothing() {
        let (listener, store, ctx_id) = local_listener_with_context();
        *listener.session_id.lock().unwrap() = Some("70b2c659-this".to_string());
        let socket_path = serve_on_temp_socket(listener, "foreign-event").await;

        send_hook_event(&socket_path, &bash_after_json("baefd2dd-other")).await.unwrap();
        let tool_calls = |store: &SharedBlockStore| {
            store
                .block_snapshots(ctx_id)
                .unwrap()
                .iter()
                .filter(|b| b.kind == BlockKind::ToolCall)
                .count()
        };
        assert_eq!(tool_calls(&store), 0, "another session's event must not be recorded");

        send_hook_event(&socket_path, &bash_after_json("70b2c659-this")).await.unwrap();
        assert_eq!(tool_calls(&store), 1, "this session's own event is recorded");
    }

    /// The session id the host handed this process is a fact, and ping
    /// reports it before any hook event arrives.
    #[tokio::test]
    async fn ping_reports_the_host_session_id() {
        let (listener, _store, _ctx_id) = local_listener_with_context();
        *listener.session_id.lock().unwrap() = Some("70b2c659-this".to_string());
        let socket_path = serve_on_temp_socket(listener, "ping-host-id").await;

        let response = send_hook_event(
            &socket_path,
            r#"{"event":"ping","source":"kaijutsu-mcp-hook"}"#,
        )
        .await
        .unwrap()
        .expect("ping must get a response");
        let ping: PingResponse = serde_json::from_str(response.trim()).unwrap();
        assert_eq!(ping.session_id.as_deref(), Some("70b2c659-this"));
    }

    /// `/clear` gives the same host process a new session id, announced by
    /// `session.start`. Only this host's events reach this socket, so the
    /// listener follows the rename.
    #[tokio::test]
    async fn session_start_renames_the_session() {
        let (listener, _store, _ctx_id) = local_listener_with_context();
        *listener.session_id.lock().unwrap() = Some("70b2c659-before".to_string());
        let listener = Arc::new(listener);
        let socket_path = unique_temp_dir("session-start-rename").join("hook-test.sock");
        let bg_listener = Arc::clone(&listener);
        let bg_path = socket_path.clone();
        tokio::spawn(async move {
            let _ = bg_listener.start(bg_path).await;
        });
        for _ in 0..100 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        send_hook_event(
            &socket_path,
            r#"{"event":"session.start","source":"claude-code","session_id":"4e1d0c2a-after"}"#,
        )
        .await
        .unwrap();
        assert_eq!(
            listener.session_id.lock().unwrap().as_deref(),
            Some("4e1d0c2a-after")
        );
    }

    use crate::hook_types::ToolInfo;

    /// The hook must answer before Claude Code gives up. When the work
    /// overruns, the response is permissive and *says so* — a silently
    /// missing block would look like an ordinary turn, and denying would
    /// block the user's action over a mirror that is ambient by design.
    #[tokio::test]
    async fn hook_budget_degrades_to_a_permissive_response_and_explains() {
        let never = std::future::pending::<HookResponse>();
        let response =
            with_hook_budget(Duration::from_millis(10), "tool.after", never).await;

        assert_eq!(
            response.block, "allow",
            "an overrun must never block the user's action"
        );
        let context = response
            .context
            .expect("an overrun must be visible, not silent");
        assert!(
            context.contains("may not have been recorded"),
            "the note must warn that the event may be missing, got: {context}"
        );
    }

    /// Non-vacuity guard for the test above: work that finishes inside the
    /// budget passes its own response through untouched, so the assertion
    /// there is pinning the overrun branch rather than the wrapper always
    /// returning the same thing.
    #[tokio::test]
    async fn hook_budget_passes_fast_work_through_unchanged() {
        let quick = async { HookResponse::allow_with_context("drift note") };
        let response = with_hook_budget(Duration::from_secs(5), "tool.after", quick).await;

        assert_eq!(response.block, "allow");
        assert_eq!(
            response.context.as_deref(),
            Some("drift note"),
            "a response produced inside the budget must not be replaced"
        );
    }

    /// A fresh, empty temp directory for this test, never reused across
    /// tests or runs.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kaijutsu-mcp-test-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn empty_hook_event(event: &str) -> HookEvent {
        HookEvent {
            event: event.to_string(),
            source: "claude-code".to_string(),
            session_id: None,
            timestamp: None,
            cwd: None,
            model: None,
            transcript_path: None,
            tool: None,
            file: None,
            prompt: None,
            response: None,
            reason: None,
            principal_id: None,
            agent_type: None,
            trigger: None,
        }
    }

    // -- tool.before: the dry run must never be on the reply path --

    fn bash_before(command: &str) -> HookEvent {
        let mut event = empty_hook_event("tool.before");
        event.tool = Some(ToolInfo {
            name: "Bash".to_string(),
            input: serde_json::json!({ "command": command }),
            output: None,
            error: None,
            duration_ms: None,
        });
        event
    }

    /// Only a Bash call with a real `command` string is a shell submission
    /// the kernel's hooks have anything to say about.
    #[test]
    fn only_a_bash_command_is_forwarded_for_a_dry_run() {
        assert_eq!(
            HookListener::bash_command(&bash_before("ls -la")),
            Some("ls -la")
        );

        let mut no_command = bash_before("x");
        no_command.tool.as_mut().unwrap().input = serde_json::json!({ "description": "no cmd" });
        assert_eq!(HookListener::bash_command(&no_command), None);

        let mut not_a_string = bash_before("x");
        not_a_string.tool.as_mut().unwrap().input = serde_json::json!({ "command": 7 });
        assert_eq!(HookListener::bash_command(&not_a_string), None);

        let mut other_tool = bash_before("ls");
        other_tool.tool.as_mut().unwrap().name = "Edit".to_string();
        assert_eq!(HookListener::bash_command(&other_tool), None);

        assert_eq!(HookListener::bash_command(&empty_hook_event("tool.before")), None);
    }

    /// A remote listener whose kernel accepts every call and answers none.
    /// The returned receiver has to be held: dropping it closes the channel
    /// and every call fails fast, which would make the timing assertions
    /// below vacuous.
    fn listener_with_a_kernel_that_never_answers(
    ) -> (HookListener, ContextId, kaijutsu_client::UnansweredCommands) {
        let (actor, held) = kaijutsu_client::ActorHandle::never_answers_for_test();
        let context_id = ContextId::new();
        let shared_context_id = Arc::new(Mutex::new(Some(context_id)));
        let (change, _change_rx) = tokio::sync::watch::channel(0u64);
        let remote = crate::RemoteState {
            actor,
            change,
            joined: Arc::new(tokio::sync::RwLock::new(None)),
            shared_context_id: shared_context_id.clone(),
        };
        let listener = HookListener::remote(
            remote,
            shared_context_id,
            Arc::new(Mutex::new(None)),
            None,
        );
        (listener, context_id, held)
    }

    /// The reverted first cut's lesson: the dry run is issued and NOT
    /// awaited. Against a kernel that accepts the call and never answers,
    /// the listener returns anyway, and the call really did go out — a
    /// refused connection would return just as fast while proving nothing.
    ///
    /// `process_event` as a whole is not the subject here: it already awaits
    /// the kernel for drift, and `with_hook_budget` is what bounds that.
    /// What this pins is that the dry run adds no second wait beside it.
    #[tokio::test]
    async fn a_tool_before_dry_run_is_issued_and_never_awaited() {
        let (listener, _context_id, mut held) = listener_with_a_kernel_that_never_answers();
        let event = bash_before("cargo build --release");

        let returned = tokio::time::timeout(Duration::from_millis(500), async {
            listener.spawn_shell_dry_run(&event);
        })
        .await;
        assert!(
            returned.is_ok(),
            "the listener must not wait on the kernel for a dry run"
        );

        // Non-vacuity: the call was really issued, and is still unanswered —
        // holding the command keeps its reply channel open.
        let issued = tokio::time::timeout(Duration::from_secs(5), held.recv())
            .await
            .expect("the dry run must reach the kernel, not be dropped");
        assert!(
            issued.is_some(),
            "the command channel must still be open; a closed one proves nothing"
        );
    }

    /// A listener with no kernel connection at all issues nothing and still
    /// returns — local mode is the ordinary case for a `tool.before` event
    /// there, not an error.
    #[tokio::test]
    async fn a_local_listener_issues_no_dry_run() {
        let (listener, _store, _ctx_id) = local_listener_with_context();
        let event = bash_before("ls");
        let response = listener.process_event(&event).await;
        assert_eq!(response.block, "allow");
    }

    #[test]
    fn session_model_provider_is_explicitly_mapped_by_source() {
        assert_eq!(provider_for_source("claude-code"), Some("anthropic"));
        assert_eq!(provider_for_source("codex"), Some("codex-app"));
        assert_eq!(provider_for_source("unrecognized-agent"), None);
    }

    fn local_listener_with_context() -> (HookListener, SharedBlockStore, ContextId) {
        let store = kaijutsu_kernel::shared_block_store(PrincipalId::new());
        let ctx_id = ContextId::new();
        store.create_document(ctx_id, DocKind::Conversation, None).unwrap();
        let listener = HookListener::local(store.clone(), ctx_id);
        (listener, store, ctx_id)
    }

    // -- item 8: hook-authored tool_call must complete, not stay Running --

    #[tokio::test]
    async fn tool_after_completes_the_call_block() {
        let (listener, store, ctx_id) = local_listener_with_context();
        let mut event = empty_hook_event("tool.after");
        event.tool = Some(ToolInfo {
            name: "Bash".to_string(),
            input: serde_json::json!({"command": "ls"}),
            output: Some("total 0".to_string()),
            error: None,
            duration_ms: Some(12),
        });

        listener.process_event(&event).await;

        let snapshots = store.block_snapshots(ctx_id).unwrap();
        let call = snapshots
            .iter()
            .find(|b| b.kind == BlockKind::ToolCall)
            .expect("tool call block inserted");
        assert_eq!(
            call.status,
            Status::Done,
            "hook-authored tool call must complete, not stay Running"
        );
    }

    #[tokio::test]
    async fn tool_error_completes_the_call_block_as_error() {
        let (listener, store, ctx_id) = local_listener_with_context();
        let mut event = empty_hook_event("tool.error");
        event.tool = Some(ToolInfo {
            name: "Bash".to_string(),
            input: serde_json::json!({"command": "false"}),
            output: None,
            error: Some("exit 1".to_string()),
            duration_ms: Some(3),
        });

        listener.process_event(&event).await;

        let snapshots = store.block_snapshots(ctx_id).unwrap();
        let call = snapshots
            .iter()
            .find(|b| b.kind == BlockKind::ToolCall)
            .expect("tool call block inserted");
        assert_eq!(call.status, Status::Error);
    }

    // -- item 9: agent.compact --

    #[tokio::test]
    async fn agent_compact_inserts_system_block_with_trigger() {
        let (listener, store, ctx_id) = local_listener_with_context();
        let mut event = empty_hook_event("agent.compact");
        event.trigger = Some("auto".to_string());

        listener.process_event(&event).await;

        let snapshots = store.block_snapshots(ctx_id).unwrap();
        let block = snapshots
            .iter()
            .find(|b| b.role == Role::System && b.content.contains("compaction"))
            .expect("compaction block inserted");
        assert_eq!(block.content, "Context compaction (auto)");
    }

    #[tokio::test]
    async fn agent_compact_without_trigger_omits_parens() {
        let (listener, store, ctx_id) = local_listener_with_context();
        let event = empty_hook_event("agent.compact");

        listener.process_event(&event).await;

        let snapshots = store.block_snapshots(ctx_id).unwrap();
        let block = snapshots
            .iter()
            .find(|b| b.role == Role::System && b.content.contains("compaction"))
            .expect("compaction block inserted");
        assert_eq!(block.content, "Context compaction");
    }

    // -- item 7: agent.stop transcript fallback --

    #[test]
    fn last_assistant_text_finds_the_final_assistant_message() {
        let jsonl = concat!(
            r#"{"type":"system","message":"init"}"#, "\n",
            r#"{"type":"user","message":{"content":[{"type":"text","text":"do the thing"}]}}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Sure, "},{"type":"text","text":"let me look."}]}}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"1","name":"Bash","input":{}}]}}"#, "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"1","content":"ok"}]}}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Done!"}]}}"#, "\n",
            r#"{"type":"summary","summary":"conversation summary"}"#, "\n",
        );
        assert_eq!(last_assistant_text(jsonl).as_deref(), Some("Done!"));
    }

    #[test]
    fn last_assistant_text_concatenates_multiple_text_parts() {
        let jsonl = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Sure, "},{"type":"text","text":"let me look."}]}}"#;
        assert_eq!(last_assistant_text(jsonl).as_deref(), Some("Sure, let me look."));
    }

    #[test]
    fn last_assistant_text_skips_trailing_tool_use_only_entry() {
        // The transcript ENDS on an assistant entry that has no text parts
        // (just a tool_use) — the last *text-bearing* assistant entry should
        // still win, not `None`.
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"earlier answer"}]}}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"2","name":"Bash","input":{}}]}}"#, "\n",
        );
        assert_eq!(last_assistant_text(jsonl).as_deref(), Some("earlier answer"));
    }

    #[test]
    fn last_assistant_text_skips_malformed_lines() {
        let jsonl = "not json\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\n{{{broken\n";
        assert_eq!(last_assistant_text(jsonl).as_deref(), Some("ok"));
    }

    #[test]
    fn last_assistant_text_returns_none_when_no_assistant_entries() {
        let jsonl = r#"{"type":"user","message":{"content":[{"type":"text","text":"hi"}]}}"#;
        assert_eq!(last_assistant_text(jsonl), None);
    }

    #[test]
    fn last_assistant_text_handles_empty_input() {
        assert_eq!(last_assistant_text(""), None);
    }

    #[tokio::test]
    async fn agent_stop_reads_last_assistant_text_from_transcript_file() {
        let dir = unique_temp_dir("transcript");
        let transcript_path = dir.join("transcript.jsonl");
        tokio::fs::write(
            &transcript_path,
            concat!(
                r#"{"type":"user","message":{"content":[{"type":"text","text":"hi"}]}}"#, "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello back"}]}}"#, "\n",
            ),
        )
        .await
        .unwrap();

        let (listener, store, ctx_id) = local_listener_with_context();
        let mut event = empty_hook_event("agent.stop");
        event.transcript_path = Some(transcript_path.display().to_string());
        // response absent — this is the real Claude Code Stop payload shape.

        listener.process_event(&event).await;

        let snapshots = store.block_snapshots(ctx_id).unwrap();
        let block = snapshots
            .iter()
            .find(|b| b.role == Role::Model)
            .expect("model block inserted from transcript fallback");
        assert_eq!(block.content, "hello back");
    }

    #[tokio::test]
    async fn agent_stop_prefers_response_field_over_transcript() {
        let (listener, store, ctx_id) = local_listener_with_context();
        let mut event = empty_hook_event("agent.stop");
        event.response = Some("direct response".to_string());
        event.transcript_path = Some("/nonexistent/transcript.jsonl".to_string());

        listener.process_event(&event).await;

        let snapshots = store.block_snapshots(ctx_id).unwrap();
        let block = snapshots
            .iter()
            .find(|b| b.role == Role::Model)
            .expect("model block inserted");
        assert_eq!(block.content, "direct response");
    }

    // -- hook client routing: its own host's listener or nothing --

    #[test]
    fn claude_pid_names_the_host() {
        assert_eq!(host_pid_for_hook(Some("564812"), 1), Some(564812));
    }

    #[test]
    fn the_parent_pid_names_the_host_without_claude_pid() {
        assert_eq!(host_pid_for_hook(None, 564812), Some(564812));
    }

    #[test]
    fn a_claude_pid_that_is_not_a_pid_drops_the_event() {
        assert_eq!(host_pid_for_hook(Some("not-a-pid"), 564812), None);
    }

    #[test]
    fn a_host_socket_lives_under_the_runtime_dir() {
        assert_eq!(
            hook_socket_in(Path::new("/run/user/1000"), 564812),
            PathBuf::from("/run/user/1000/kaijutsu/hook-564812.sock")
        );
    }

    /// A session's own listener is not bound yet while another host's
    /// listener is live. The event is dropped and the other listener hears
    /// nothing.
    #[tokio::test]
    async fn an_event_never_reaches_another_hosts_listener() {
        let runtime_dir = unique_temp_dir("route-own-only");
        let other = hook_socket_in(&runtime_dir, 111);
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        let heard = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let other_listener = UnixListener::bind(&other).unwrap();
        let heard_bg = Arc::clone(&heard);
        tokio::spawn(async move {
            while let Ok((_stream, _)) = other_listener.accept().await {
                heard_bg.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let own = hook_socket_in(&runtime_dir, host_pid_for_hook(Some("222"), 333).unwrap());
        let delivered = send_hook_event(&own, &bash_after_json("83768815-this")).await.unwrap();
        assert!(delivered.is_none(), "no listener of our own: the event is dropped");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(heard.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    // -- item 3: stale socket sweep --

    #[tokio::test]
    async fn sweep_removes_stale_but_keeps_live_and_excluded() {
        let dir = unique_temp_dir("sweep");
        let stale = dir.join("hook-stale.sock");
        let live = dir.join("hook-live.sock");
        let keep = dir.join("hook-keep.sock");

        // A "stale" socket: bind, then drop without unlinking. The special
        // file outlives the listener — an unclean exit leaves exactly this
        // behind — and a subsequent connect() gets ECONNREFUSED.
        {
            let _listener = UnixListener::bind(&stale).unwrap();
        }
        assert!(stale.exists(), "socket file must outlive the dropped listener");

        // A live socket — keep the listener alive across the sweep.
        let _live_listener = UnixListener::bind(&live).unwrap();

        // Also stale-looking, but excluded by path — must never be touched.
        {
            let _listener = UnixListener::bind(&keep).unwrap();
        }

        let removed = sweep_stale_sockets(&dir, &keep).await;

        assert_eq!(removed, 1);
        assert!(!stale.exists(), "stale socket must be unlinked");
        assert!(live.exists(), "live socket must survive the sweep");
        assert!(keep.exists(), "keep path must never be touched, even if stale");
    }

    #[tokio::test]
    async fn sweep_on_missing_dir_is_a_noop() {
        let dir = unique_temp_dir("sweep-missing").join("does-not-exist");
        let removed = sweep_stale_sockets(&dir, Path::new("/nonexistent/keep.sock")).await;
        assert_eq!(removed, 0);
    }

    // -- item 3: bind_socket must not steal a live listener's path --

    #[tokio::test]
    async fn bind_socket_succeeds_on_a_fresh_path() {
        let dir = unique_temp_dir("bind-fresh");
        let path = dir.join("hook-fresh.sock");
        let listener = HookListener::bind_socket(&path).await.expect("fresh bind must succeed");
        assert!(path.exists());
        drop(listener);
    }

    #[tokio::test]
    async fn bind_socket_reclaims_a_stale_socket_file() {
        let dir = unique_temp_dir("bind-stale");
        let path = dir.join("hook-stale.sock");
        {
            // Bind and drop without unlinking — the special file outlives
            // the dropped listener, exactly like an unclean exit.
            let _listener = UnixListener::bind(&path).unwrap();
        }
        assert!(path.exists());

        let listener = HookListener::bind_socket(&path)
            .await
            .expect("a stale socket file must be reclaimed, not refused");
        assert!(path.exists());
        drop(listener);
    }

    /// The defect this closes: two successive processes computing the same
    /// PPID-derived path used to unlink-then-rebind unconditionally, so a
    /// still-alive predecessor's socket special file was silently replaced
    /// out from under it — the predecessor kept running but nothing could
    /// reach it by that path any more. `bind_socket` must refuse instead.
    #[tokio::test]
    async fn bind_socket_refuses_to_steal_a_live_listeners_path() {
        let dir = unique_temp_dir("bind-live");
        let path = dir.join("hook-live.sock");
        let live_listener = UnixListener::bind(&path).unwrap();

        // Real bind_socket() would wait out the full production
        // BIND_RETRY_WINDOW (10s) before giving up — same code path,
        // exercised here with a millisecond-scale window so the test stays
        // fast.
        let result = HookListener::bind_socket_with_retry(
            &path,
            Duration::from_millis(5),
            Duration::from_millis(30),
        )
        .await;
        assert!(
            result.is_err(),
            "binding over a live listener's path must fail loudly, not silently rebind"
        );

        // The original listener must still be reachable at the same path —
        // proof nothing stole it.
        let connect = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::net::UnixStream::connect(&path),
        )
        .await;
        assert!(
            connect.is_ok() && connect.unwrap().is_ok(),
            "the original live listener must still be reachable after the refused bind"
        );
        drop(live_listener);
    }

    /// The defect this closes: the hook socket path is per hosting-process
    /// PID and stable across `kaijutsu-mcp` reconnects, so a predecessor
    /// still finishing its own shutdown reads as `Live`/`Unknown` for a
    /// moment. `bind_socket` used to refuse permanently on that first
    /// probe; it must instead wait for the path to clear and bind as soon
    /// as it does, within `BIND_RETRY_WINDOW`.
    #[tokio::test]
    async fn bind_socket_binds_once_a_live_predecessor_exits_within_the_window() {
        let dir = unique_temp_dir("bind-retry-clears");
        let path = dir.join("hook-retry-clears.sock");
        let live_listener = UnixListener::bind(&path).unwrap();

        let path_for_task = path.clone();
        let unlink_after_a_beat = tokio::spawn(async move {
            // Simulate the predecessor's shutdown completing shortly after
            // the successor starts probing.
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(live_listener);
            tokio::fs::remove_file(&path_for_task).await.unwrap();
        });

        let listener = HookListener::bind_socket_with_retry(
            &path,
            Duration::from_millis(5),
            Duration::from_millis(500),
        )
        .await
        .expect("a predecessor that clears within the window must be waited out, not refused");
        assert!(path.exists());

        unlink_after_a_beat.await.unwrap();
        drop(listener);
    }

    // -- wait_for_socket_to_clear: the pure retry loop bind_socket delegates to --

    #[tokio::test]
    async fn wait_for_socket_to_clear_binds_once_probe_goes_stale_within_window() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let probe = || {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                if n < 2 { ExistingSocket::Live } else { ExistingSocket::Stale }
            }
        };

        let verdict =
            wait_for_socket_to_clear(probe, Duration::from_millis(5), Duration::from_millis(500))
                .await;

        assert!(
            matches!(verdict, ExistingSocket::Stale),
            "must resolve Stale once the probe reports it within the window"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "must stop polling as soon as the path clears, not keep going to the deadline"
        );
    }

    #[tokio::test]
    async fn wait_for_socket_to_clear_refuses_when_live_for_the_whole_window() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let probe = || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { ExistingSocket::Live }
        };

        let verdict =
            wait_for_socket_to_clear(probe, Duration::from_millis(5), Duration::from_millis(30))
                .await;

        assert!(
            matches!(verdict, ExistingSocket::Live),
            "a path Live for the whole window must come back Live, not silently resolve"
        );
        assert!(
            calls.load(std::sync::atomic::Ordering::SeqCst) > 1,
            "must have actually polled more than once across the window"
        );
    }

    #[tokio::test]
    async fn wait_for_socket_to_clear_returns_immediately_when_already_stale() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let probe = || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { ExistingSocket::Stale }
        };

        let start = tokio::time::Instant::now();
        let verdict = wait_for_socket_to_clear(
            probe,
            Duration::from_millis(5),
            Duration::from_secs(10),
        )
        .await;
        let elapsed = start.elapsed();

        assert!(matches!(verdict, ExistingSocket::Stale));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1, "must probe exactly once");
        assert!(
            elapsed < Duration::from_millis(200),
            "an already-stale path must bind at once, not wait out any part of a 10s window \
             (elapsed: {elapsed:?})"
        );
    }

    // -- confirm_stale: a raw Stale verdict is provisional, not final --

    #[tokio::test]
    async fn confirm_stale_does_not_confirm_when_the_second_probe_says_live() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let probe = || {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { if n == 0 { ExistingSocket::Stale } else { ExistingSocket::Live } }
        };

        let verdict = confirm_stale(probe, Duration::from_millis(5)).await;

        assert!(
            matches!(verdict, ExistingSocket::Live),
            "a momentarily-refused live listener (a full accept backlog reads ECONNREFUSED \
             on BSD-derived systems, same as a dead listener) must not be confirmed stale"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "must re-probe exactly once before trusting a Stale verdict"
        );
    }

    #[tokio::test]
    async fn confirm_stale_confirms_when_the_second_probe_agrees() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let probe = || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { ExistingSocket::Stale }
        };

        let verdict = confirm_stale(probe, Duration::from_millis(5)).await;

        assert!(
            matches!(verdict, ExistingSocket::Stale),
            "two agreeing Stale probes must confirm the verdict"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "must probe exactly twice: once, then once more to confirm"
        );
    }

    #[tokio::test]
    async fn confirm_stale_does_not_wait_when_the_first_probe_is_not_stale() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let probe = || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { ExistingSocket::Live }
        };

        let start = tokio::time::Instant::now();
        let verdict = confirm_stale(probe, Duration::from_secs(10)).await;
        let elapsed = start.elapsed();

        assert!(matches!(verdict, ExistingSocket::Live));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1, "must probe exactly once");
        assert!(
            elapsed < Duration::from_millis(200),
            "a non-Stale first verdict must return at once, never wait out the confirmation \
             interval (elapsed: {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn probe_socket_or_gone_treats_a_missing_path_as_stale() {
        let dir = unique_temp_dir("probe-gone");
        let path = dir.join("hook-never-existed.sock");
        assert!(!path.exists());

        let verdict = probe_socket_or_gone(&path).await;

        assert!(
            matches!(verdict, ExistingSocket::Stale),
            "a path that was never bound (or already vanished) has nothing left to steal"
        );
    }
}
