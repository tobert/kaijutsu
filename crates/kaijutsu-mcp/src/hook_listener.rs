//! Unix socket listener for hook events.
//!
//! The MCP process opens a Unix socket as a secondary listener. Adapter
//! scripts (or `kaijutsu-mcp hook`) connect, send one JSON line, receive
//! one JSON response, and disconnect.
//!
//! On each event the listener:
//! 1. Creates CRDT blocks in the shared store
//! 2. Pushes ops to the server (if remote)
//! 3. Checks for pending drift and injects it into the response

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex as TokioMutex;

use kaijutsu_crdt::{BlockKind, ContentType, ContextId, PrincipalId, Role, Status, ToolKind};
use kaijutsu_kernel::SharedBlockStore;

use crate::RemoteState;
use crate::hook_types::{
    HookEvent, HookResponse, KAIJUTSU_MCP_TOOLS, PingResponse, normalize_tool_name,
    short_session_suffix,
};

/// Per-candidate connect+ping timeout during hook socket resolution
/// (`resolve_hook_socket`). Short — dead/stale sockets must not add
/// meaningful latency to every hook call.
pub const PING_TIMEOUT: Duration = Duration::from_millis(200);

/// Connect timeout for the stale-socket sweep at serve startup
/// (`sweep_stale_sockets`). A listening socket accepts near-instantly; this
/// only bounds the worst case (e.g. a socket whose listener is wedged).
const SWEEP_CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

/// Maximum size of a block's content created from hook events.
const DEFAULT_MAX_BLOCK_SIZE: usize = 4096;

/// Hook listener — receives events over a Unix socket and writes CRDT blocks.
pub struct HookListener {
    /// Local-mode block store (in-process). `None` in remote mode, where blocks
    /// are authored into the `RemoteState`'s `SyncedDocument` and pushed up.
    local_store: Option<SharedBlockStore>,
    /// Shared context ID — updated by register_session (None until then).
    shared_context_id: Arc<Mutex<Option<ContextId>>>,
    /// Fixed context ID for local mode (not shared).
    local_context_id: Option<ContextId>,
    /// Remote state for push_ops + drift (None in local mode).
    remote: Option<RemoteState>,
    /// Max content size per block.
    max_block_size: usize,
    /// Serializes push_ops to avoid concurrent pushes sending duplicate ops.
    push_lock: TokioMutex<()>,
    /// Shared session ID — updated from hook events when detected.
    session_id: Arc<Mutex<Option<String>>>,
    /// Remote-only: the auto-register label, present only when it was
    /// generated without a session-id suffix (session id wasn't known at
    /// register time). `session.start`'s handler consumes it via
    /// `Mutex::take` — `Some` means a rename is still pending, `None` means
    /// it already happened or was never needed (manual `register_session`,
    /// local mode).
    pending_label_rename: Mutex<Option<String>>,
    /// Guards `set_context_model` (from `session.start`'s `model` field) to
    /// at most one call per process.
    context_model_set: Mutex<bool>,
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
            push_lock: TokioMutex::new(()),
            session_id: Arc::new(Mutex::new(None)),
            pending_label_rename: Mutex::new(None),
            context_model_set: Mutex::new(false),
        }
    }

    /// Create a listener backed by a remote connection.
    ///
    /// `shared_context_id` is updated by `register_session` when a context is joined.
    ///
    /// `pending_label_rename`: `Some(label)` when the caller auto-registered
    /// with a label that lacks a session-id suffix (session id unknown at
    /// register time) — the first `session.start` with a session id renames
    /// the context to `{label}-{first 8 chars}`. Pass `None` when the label
    /// already carries a session id, or wasn't auto-generated.
    pub fn remote(
        remote: RemoteState,
        shared_context_id: Arc<Mutex<Option<ContextId>>>,
        session_id: Arc<Mutex<Option<String>>>,
        pending_label_rename: Option<String>,
    ) -> Self {
        Self {
            local_store: None,
            shared_context_id,
            local_context_id: None,
            remote: Some(remote),
            max_block_size: DEFAULT_MAX_BLOCK_SIZE,
            push_lock: TokioMutex::new(()),
            session_id,
            pending_label_rename: Mutex::new(pending_label_rename),
            context_model_set: Mutex::new(false),
        }
    }

    /// Start listening on a Unix socket. Runs until the socket is closed or
    /// the task is cancelled. Spawns a tokio task per connection.
    pub async fn start(self: Arc<Self>, socket_path: PathBuf) -> anyhow::Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Remove stale socket
        if socket_path.exists() {
            tokio::fs::remove_file(&socket_path).await?;
        }

        let listener = UnixListener::bind(&socket_path)?;
        tracing::info!(path = %socket_path.display(), "Hook socket listening");

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

        // Capture session_id from hook events. `session.start` is
        // authoritative and OVERWRITES: startup agent detection can report a
        // previous session's id (stale transcript scrape), and `/clear`
        // starts a new session id on the same process — in both cases the
        // stored id must follow the event or ping-based socket resolution
        // matches the wrong session forever. Other events only fill a void.
        if let Some(ref event_session_id) = event.session_id
            && let Ok(mut guard) = self.session_id.lock()
        {
            let stale = guard.as_deref().is_some_and(|cur| cur != event_session_id.as_str());
            if guard.is_none() || (event.event == "session.start" && stale) {
                tracing::info!(
                    session_id = %event_session_id,
                    replaced = %stale,
                    "Captured session ID from hook event"
                );
                *guard = Some(event_session_id.clone());
            }
        }

        let response = self.process_event(&event).await;

        let json = serde_json::to_string(&response).unwrap_or_default();
        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;

        Ok(())
    }

    /// Process a hook event: create blocks, push ops, check drift.
    async fn process_event(&self, event: &HookEvent) -> HookResponse {
        // 1. Filter self-referential kaijutsu MCP tools. Claude Code reports
        // MCP tool calls as `mcp__<server>__<tool>`, not the bare name.
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

        // 2. Create blocks based on event type
        match event.event.as_str() {
            "session.start" => {
                let model_info = event.model.as_deref().unwrap_or("unknown");
                let sid = event.session_id.as_deref().unwrap_or("unknown");
                let content = format!(
                    "Session started: {}, model: {}, session: {}",
                    event.source, model_info, sid
                );
                self.insert_text_block(Role::System, &content);

                // Remote-only follow-ups, each at most once per process: tell
                // the kernel which model this context is talking to, and
                // rename the auto-generated label once the session id is
                // known (it wasn't yet when register_session ran).
                if let Some(ref remote) = self.remote
                    && let Some(ctx_id) = self.context_id()
                {
                    if let Some(model) = event.model.as_deref() {
                        // Recover a poisoned lock (rather than `.ok()`-skip
                        // like `session_id` below) — this flag exists to
                        // stop a *duplicate* RPC, so losing track of "already
                        // called" under poisoning is the wrong failure mode.
                        let already_set = {
                            let mut guard =
                                self.context_model_set.lock().unwrap_or_else(|e| e.into_inner());
                            std::mem::replace(&mut *guard, true)
                        };
                        if !already_set {
                            // Only adapter today is claude-code (docs/hooks.md) — its
                            // models are always served by the "anthropic" provider.
                            match remote.actor.set_context_model(ctx_id, "anthropic", model).await
                            {
                                Ok(_) => {
                                    tracing::info!(model, "Set context model from session.start")
                                }
                                Err(e) => tracing::warn!("Failed to set context model: {e}"),
                            }
                        }
                    }

                    if let Some(session_id) = event.session_id.as_deref() {
                        // Recover a poisoned lock like `context_model_set`
                        // above: skipping here would leave the context's
                        // auto-generated label permanently suffix-less.
                        let pending = self
                            .pending_label_rename
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .take();
                        if let Some(label) = pending {
                            let suffix = short_session_suffix(session_id);
                            let renamed = format!("{label}-{suffix}");
                            match remote.actor.rename_context(ctx_id, &renamed).await {
                                Ok(_) => tracing::info!(
                                    label = %renamed,
                                    "Renamed context with session suffix"
                                ),
                                Err(e) => tracing::warn!(
                                    "Failed to rename context with session suffix: {e}"
                                ),
                            }
                        }
                    }
                }
            }

            "session.end" => {
                let content = match event.reason.as_deref() {
                    Some(reason) => format!("Session ended: {reason}"),
                    None => "Session ended".to_string(),
                };
                self.insert_text_block(Role::System, &content);
            }

            "prompt.submit" => {
                if let Some(ref prompt) = event.prompt {
                    let truncated = truncate(prompt, self.max_block_size);
                    self.insert_text_block(Role::User, &truncated);
                }
            }

            "tool.after" => {
                if let Some(ref tool) = event.tool {
                    self.insert_tool_blocks(tool, false);
                }
            }

            "tool.error" => {
                if let Some(ref tool) = event.tool {
                    self.insert_tool_blocks(tool, true);
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
                    self.insert_text_block(Role::Model, &truncated);
                }
            }

            "agent.compact" => {
                let content = match event.trigger.as_deref() {
                    Some(trigger) => format!("Context compaction ({trigger})"),
                    None => "Context compaction".to_string(),
                };
                self.insert_text_block(Role::System, &content);
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
                    self.insert_text_block(Role::Tool, &content);
                }
            }

            "subagent.start" => {
                let agent = event.principal_id.as_deref().unwrap_or("unknown");
                let kind = event.agent_type.as_deref().unwrap_or("subagent");
                self.insert_text_block(
                    Role::System,
                    &format!("Subagent started: {agent} ({kind})"),
                );
            }

            "subagent.stop" => {
                let agent = event.principal_id.as_deref().unwrap_or("unknown");
                self.insert_text_block(Role::System, &format!("Subagent stopped: {agent}"));
            }

            // tool.before — no block
            _ => {}
        }

        // 3. Push ops to server (serialized to avoid concurrent duplicate pushes)
        if let Some(ref remote) = self.remote {
            let _guard = self.push_lock.lock().await;
            if let Err(e) = push_ops(remote).await {
                tracing::warn!("Hook push_ops error: {e}");
            }
        }

        // 4. Check for pending drift
        self.maybe_inject_drift().await
    }

    // -- Block insertion helpers --

    fn insert_text_block(&self, role: Role, content: &str) {
        let Some(ctx_id) = self.context_id() else {
            tracing::debug!("Hook insert_text_block: no context yet (register_session not called)");
            return;
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
                Some(PrincipalId::system()),
            ) {
                tracing::warn!("Hook insert_block error: {e}");
            }
            return;
        }
        // Remote mode: author into the joined SyncedDocument, then wake waiters.
        if let Some(remote) = &self.remote {
            let mut guard = remote.synced.lock();
            if let Some(doc) = guard.as_mut()
                && let Err(e) = doc.doc_mut().insert_block(
                    None,
                    None,
                    role,
                    BlockKind::Text,
                    content,
                    Status::Done,
                    ContentType::Plain,
                )
            {
                tracing::warn!("Hook insert_block error: {e}");
            }
            drop(guard);
            remote.change.send_modify(|g| *g = g.wrapping_add(1));
        }
    }

    fn insert_tool_blocks(&self, tool: &crate::hook_types::ToolInfo, is_error: bool) {
        let Some(ctx_id) = self.context_id() else {
            tracing::debug!(
                "Hook insert_tool_blocks: no context yet (register_session not called)"
            );
            return;
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
                Some(PrincipalId::system()),
                None,
                None,
            ) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!("Hook insert_tool_call error: {e}");
                    return;
                }
            };
            if let Err(e) = store.insert_tool_result_as(
                ctx_id,
                &call_id,
                None,
                &truncated,
                is_error,
                None,
                Some(ToolKind::Mcp),
                Some(PrincipalId::system()),
                None,
            ) {
                tracing::warn!("Hook insert_tool_result error: {e}");
            }
            // The call block is inserted Status::Running (see
            // kaijutsu_crdt::block_store::BlockStore::insert_tool_call) and
            // only the LLM streaming path transitioned it to Done/Error —
            // hook-authored calls never did, so they stayed "running"
            // forever in the UI. Mirror the LLM path: complete the call once
            // its result has landed.
            let final_status = if is_error { Status::Error } else { Status::Done };
            if let Err(e) = store.set_status(ctx_id, &call_id, final_status) {
                tracing::warn!("Hook set_status (tool call) error: {e}");
            }
            return;
        }
        // Remote mode: author the call + result into the SyncedDocument under
        // one lock (sequentially, so the result's parent exists), then wake.
        if let Some(remote) = &self.remote {
            let mut guard = remote.synced.lock();
            if let Some(doc) = guard.as_mut() {
                let call_id = match doc.doc_mut().insert_tool_call(
                    None,
                    None,
                    &tool.name,
                    input,
                    Some(ToolKind::Mcp),
                    None,
                ) {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::warn!("Hook insert_tool_call error: {e}");
                        return;
                    }
                };
                if let Err(e) = doc.doc_mut().insert_tool_result_block(
                    &call_id,
                    None,
                    &truncated,
                    is_error,
                    None,
                    Some(ToolKind::Mcp),
                ) {
                    tracing::warn!("Hook insert_tool_result error: {e}");
                }
                // See the local-mode branch above: the call block defaults
                // to Status::Running and must be explicitly completed.
                let final_status = if is_error { Status::Error } else { Status::Done };
                if let Err(e) = doc.doc_mut().set_status(&call_id, final_status) {
                    tracing::warn!("Hook set_status (tool call) error: {e}");
                }
            }
            drop(guard);
            remote.change.send_modify(|g| *g = g.wrapping_add(1));
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

/// Push local ops to the server. Mirrors `KaijutsuMcp::push_to_server()`.
async fn push_ops(remote: &RemoteState) -> anyhow::Result<()> {
    let context_id = {
        let guard = remote.joined.read().await;
        guard
            .as_ref()
            .map(|j| j.context_id)
            .ok_or_else(|| anyhow::anyhow!("No active context — register_session not called"))?
    };
    // Shared with resync's flush, so the two paths can't diverge.
    crate::flush_local_ops(&remote.actor, &remote.synced, context_id).await;
    Ok(())
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

/// Compute the default socket path for hook communication.
///
/// Uses `$XDG_RUNTIME_DIR/kaijutsu/hook-{ppid}.sock` where PPID is the
/// parent process ID. Both the MCP server and adapter scripts are children
/// of the same agent process (Claude Code), so they independently compute
/// the same path.
///
/// Returns `None` if `$XDG_RUNTIME_DIR` is not set — we don't fall back to
/// `/tmp` to avoid socket permission issues on shared systems.
pub fn default_socket_path() -> Option<PathBuf> {
    let ppid = std::os::unix::process::parent_id();
    let runtime_dir = dirs::runtime_dir()?;
    Some(
        runtime_dir
            .join("kaijutsu")
            .join(format!("hook-{ppid}.sock")),
    )
}

/// List `hook-*.sock` entries directly under `dir` (non-recursive). Shared by
/// `discover_sockets` (the real runtime dir) and `sweep_stale_sockets` (which
/// takes an explicit dir so it's testable against a tempdir). Returns empty
/// if `dir` doesn't exist.
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

/// Discover hook sockets by scanning the runtime directory.
///
/// Used as a fallback when the PPID-based path doesn't exist (e.g.,
/// when an intermediate shell layer changes the PPID).
/// Returns empty if `$XDG_RUNTIME_DIR` is not set.
pub fn discover_sockets() -> Vec<PathBuf> {
    let Some(runtime_dir) = dirs::runtime_dir() else {
        return Vec::new();
    };
    list_hook_sockets_in(&runtime_dir.join("kaijutsu"))
}

/// Merge candidate socket paths in priority order, de-duplicated (first
/// occurrence wins). Pure — the I/O-touching lookups (`default_socket_path`,
/// `discover_sockets`) are gathered by the caller.
fn merge_candidates(
    explicit: Option<PathBuf>,
    default: Option<PathBuf>,
    discovered: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> =
        explicit.into_iter().chain(default).chain(discovered).collect();
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|p| seen.insert(p.clone()));
    candidates
}

/// Build the ordered candidate list for hook socket resolution: an explicit
/// `--socket` (if given), then the PPID default, then every socket
/// `discover_sockets()` finds — de-duplicated, priority order preserved.
/// Existence isn't checked here; `resolve_hook_socket` filters and pings.
pub fn candidate_sockets(explicit: Option<PathBuf>) -> Vec<PathBuf> {
    merge_candidates(explicit, default_socket_path(), discover_sockets())
}

/// Ping a single hook socket and parse its `PingResponse`.
///
/// `None` on any failure (missing socket, connect/IO error, malformed JSON)
/// — resolution treats that candidate as dead, never as a match.
async fn ping_socket(path: &Path) -> Option<PingResponse> {
    let response = send_hook_event(path, r#"{"event":"ping","source":"kaijutsu-mcp-hook"}"#)
        .await
        .ok()
        .flatten()?;
    serde_json::from_str(response.trim()).ok()
}

/// Resolve which hook socket to send a real event to.
///
/// Every existing candidate is pinged concurrently (each bounded by
/// `ping_timeout`, so dead/stale sockets add negligible latency). Selection:
///
/// 1. An `explicit` candidate that answered wins outright. The adapter
///    derives it from its own PPID, so an ANSWERING server there is bound
///    by our own process tree — pid uniqueness makes a wrong answer require
///    pid-recycling coincidences. It outranks session match because a
///    server's stored session id CAN be wrong: startup detection scrapes
///    the newest transcript, which may belong to a previous or sibling
///    session (observed live).
/// 2. Otherwise a candidate whose ping `session_id` matches
///    `event_session_id` — this is what rescues events when an intermediate
///    shell skewed the adapter's PPID (explicit path dead).
/// 3. Otherwise, if exactly one candidate answered at all, use it — no
///    session to disambiguate by, but no ambiguity either.
/// 4. Otherwise `None` (fail open): nothing answered, or several answered
///    with nothing to break the tie — guessing would cross-wire sessions.
pub async fn resolve_hook_socket(
    candidates: Vec<PathBuf>,
    explicit: Option<&Path>,
    event_session_id: Option<&str>,
    ping_timeout: Duration,
) -> Option<PathBuf> {
    let pings = futures::future::join_all(candidates.into_iter().map(|path| async move {
        if !path.exists() {
            return None;
        }
        match tokio::time::timeout(ping_timeout, ping_socket(&path)).await {
            Ok(Some(resp)) => Some((path, resp)),
            _ => None,
        }
    }))
    .await;

    let answered: Vec<(PathBuf, PingResponse)> = pings.into_iter().flatten().collect();

    if let Some(explicit) = explicit
        && answered.iter().any(|(p, _)| p == explicit)
    {
        return Some(explicit.to_path_buf());
    }

    if let Some(sid) = event_session_id
        && let Some((path, _)) = answered.iter().find(|(_, r)| r.session_id.as_deref() == Some(sid))
    {
        return Some(path.clone());
    }

    // Exactly one responder that hasn't identified its session yet: a server
    // whose detection failed and which no hook event has reached. An event
    // that matched no identified server most plausibly belongs to it — and
    // routing it there is also what bootstraps that server's session id
    // (session.start capture/overwrite).
    if event_session_id.is_some() {
        let mut unknown = answered.iter().filter(|(_, r)| r.session_id.is_none());
        if let (Some((path, _)), None) = (unknown.next(), unknown.next()) {
            return Some(path.clone());
        }
    }

    match answered.len() {
        1 => Some(answered[0].0.clone()),
        _ => None,
    }
}

/// Scan `dir` for `hook-*.sock` files other than `keep` and unlink any that
/// refuse connections (`ECONNREFUSED` — the listener process is gone but the
/// socket special file outlives it; nothing cleans these up on an unclean
/// exit). Never touches a socket that accepts a connection, and never
/// touches `keep` (the path we're about to bind ourselves). Returns the
/// number removed.
pub async fn sweep_stale_sockets(dir: &Path, keep: &Path) -> usize {
    let mut removed = 0;
    for path in list_hook_sockets_in(dir) {
        if path == keep {
            continue;
        }
        match tokio::time::timeout(SWEEP_CONNECT_TIMEOUT, tokio::net::UnixStream::connect(&path))
            .await
        {
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                match tokio::fs::remove_file(&path).await {
                    Ok(()) => removed += 1,
                    Err(e) => tracing::debug!(
                        path = %path.display(),
                        "Failed to unlink stale hook socket: {e}"
                    ),
                }
            }
            // Accepted (someone's listening), some other error, or timed
            // out — leave it alone rather than guess.
            _ => {}
        }
    }
    removed
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
    use crate::hook_types::ToolInfo;

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

    // -- item 2: candidate list + ping-based resolution --

    #[test]
    fn merge_candidates_dedups_preserving_priority_order() {
        let explicit = PathBuf::from("/tmp/a.sock");
        let default = PathBuf::from("/tmp/b.sock");
        let discovered = vec![PathBuf::from("/tmp/b.sock"), PathBuf::from("/tmp/c.sock")];
        let merged = merge_candidates(Some(explicit.clone()), Some(default.clone()), discovered);
        assert_eq!(merged, vec![explicit, default, PathBuf::from("/tmp/c.sock")]);
    }

    #[test]
    fn merge_candidates_handles_all_none() {
        assert!(merge_candidates(None, None, vec![]).is_empty());
    }

    /// A minimal fake hook socket that only answers `ping` with a canned
    /// `PingResponse` — enough to drive `resolve_hook_socket` without
    /// standing up a real `HookListener`/kernel.
    async fn spawn_fake_ping_server(path: PathBuf, session_id: Option<String>) {
        let listener = UnixListener::bind(&path).expect("bind fake ping server");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _addr)) = listener.accept().await else {
                    break;
                };
                let session_id = session_id.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut buf_reader = BufReader::new(reader);
                    let mut line = String::new();
                    let _ = buf_reader.read_line(&mut line).await;
                    let resp = PingResponse {
                        status: "ok".to_string(),
                        pid: std::process::id(),
                        cwd: None,
                        context_name: None,
                        document_id: None,
                        session_id,
                        pending_drifts: 0,
                    };
                    let json = serde_json::to_string(&resp).unwrap();
                    let _ = writer.write_all(json.as_bytes()).await;
                    let _ = writer.write_all(b"\n").await;
                });
            }
        });
    }

    #[tokio::test]
    async fn resolve_picks_matching_session() {
        let dir = unique_temp_dir("resolve-match");
        let path_a = dir.join("hook-a.sock");
        let path_b = dir.join("hook-b.sock");
        spawn_fake_ping_server(path_a.clone(), Some("session-aaa".to_string())).await;
        spawn_fake_ping_server(path_b.clone(), Some("session-bbb".to_string())).await;

        let resolved = resolve_hook_socket(
            vec![path_a, path_b.clone()],
            None,
            Some("session-bbb"),
            PING_TIMEOUT,
        )
        .await;
        assert_eq!(resolved, Some(path_b));
    }

    #[tokio::test]
    async fn resolve_explicit_outranks_session_match() {
        // Another server can hold OUR session id (stale startup detection
        // scraped our transcript at its spawn) — but an answering server at
        // the adapter's own PPID-derived path is structurally ours. The
        // explicit socket must win or events cross-wire to the impostor.
        let dir = unique_temp_dir("resolve-explicit-vs-match");
        let path_a = dir.join("hook-a.sock");
        let path_b = dir.join("hook-b.sock");
        spawn_fake_ping_server(path_a.clone(), Some("session-aaa".to_string())).await;
        spawn_fake_ping_server(path_b.clone(), Some("session-bbb".to_string())).await;

        let resolved = resolve_hook_socket(
            vec![path_a.clone(), path_b.clone()],
            Some(&path_a),
            Some("session-bbb"),
            PING_TIMEOUT,
        )
        .await;
        assert_eq!(resolved, Some(path_a));
    }

    #[tokio::test]
    async fn resolve_falls_back_to_sole_responder_when_no_session_match() {
        let dir = unique_temp_dir("resolve-sole");
        let path_a = dir.join("hook-a.sock");
        spawn_fake_ping_server(path_a.clone(), Some("session-aaa".to_string())).await;

        // event_session_id doesn't match anything, but exactly one candidate
        // answered — use it rather than fail open.
        let resolved =
            resolve_hook_socket(vec![path_a.clone()], None, Some("session-zzz"), PING_TIMEOUT)
                .await;
        assert_eq!(resolved, Some(path_a));
    }

    #[tokio::test]
    async fn resolve_explicit_answering_beats_ambiguity() {
        // Two candidates answer, neither matches the event's session id
        // (both servers hold stale detected ids) — the adapter's explicit
        // PPID-derived socket answering the ping resolves the tie instead
        // of dropping the event.
        let dir = unique_temp_dir("resolve-explicit");
        let path_a = dir.join("hook-a.sock");
        let path_b = dir.join("hook-b.sock");
        spawn_fake_ping_server(path_a.clone(), Some("session-aaa".to_string())).await;
        spawn_fake_ping_server(path_b.clone(), Some("session-bbb".to_string())).await;

        let resolved = resolve_hook_socket(
            vec![path_a.clone(), path_b.clone()],
            Some(&path_a),
            Some("session-zzz"),
            PING_TIMEOUT,
        )
        .await;
        assert_eq!(resolved, Some(path_a));
    }

    #[tokio::test]
    async fn resolve_prefers_sole_unidentified_responder() {
        // Two servers answer; one has a (possibly stale) identity that
        // doesn't match, the other never learned its session (detection
        // failed, no event reached it). The event most plausibly belongs to
        // the unidentified one — and routing there is what bootstraps its
        // session id.
        let dir = unique_temp_dir("resolve-unidentified");
        let path_a = dir.join("hook-a.sock");
        let path_b = dir.join("hook-b.sock");
        spawn_fake_ping_server(path_a.clone(), Some("session-aaa".to_string())).await;
        spawn_fake_ping_server(path_b.clone(), None).await;

        let resolved = resolve_hook_socket(
            vec![path_a.clone(), path_b.clone()],
            None,
            Some("session-zzz"),
            PING_TIMEOUT,
        )
        .await;
        assert_eq!(resolved, Some(path_b));
    }

    #[tokio::test]
    async fn resolve_fails_open_when_multiple_answer_and_none_match() {
        let dir = unique_temp_dir("resolve-ambiguous");
        let path_a = dir.join("hook-a.sock");
        let path_b = dir.join("hook-b.sock");
        spawn_fake_ping_server(path_a.clone(), Some("session-aaa".to_string())).await;
        spawn_fake_ping_server(path_b.clone(), Some("session-bbb".to_string())).await;

        // No explicit socket (or a dead one) and no session match: guessing
        // between two live sessions would cross-wire events — fail open.
        let resolved =
            resolve_hook_socket(vec![path_a, path_b], None, Some("session-zzz"), PING_TIMEOUT)
                .await;
        assert_eq!(resolved, None);
    }

    #[tokio::test]
    async fn resolve_fails_open_when_nothing_answers() {
        let dir = unique_temp_dir("resolve-none");
        let ghost = dir.join("hook-ghost.sock");
        let resolved =
            resolve_hook_socket(vec![ghost.clone()], Some(&ghost), Some("whatever"), PING_TIMEOUT)
                .await;
        assert_eq!(resolved, None);
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
}
