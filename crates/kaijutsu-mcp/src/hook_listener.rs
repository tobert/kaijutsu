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
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex as TokioMutex;

use kaijutsu_crdt::{BlockKind, Role};
use kaijutsu_kernel::SharedBlockStore;

use crate::hook_types::{HookEvent, HookResponse, PingResponse, KAIJUTSU_MCP_TOOLS};
use crate::RemoteState;

/// Maximum size of a block's content created from hook events.
const DEFAULT_MAX_BLOCK_SIZE: usize = 4096;

/// Hook listener — receives events over a Unix socket and writes CRDT blocks.
pub struct HookListener {
    /// Block store — shared with the MCP server.
    store: SharedBlockStore,
    /// Document ID for the active context.
    document_id: String,
    /// Remote state for push_ops + drift (None in local mode).
    remote: Option<RemoteState>,
    /// Max content size per block.
    max_block_size: usize,
    /// Context name for ping responses.
    context_name: String,
    /// Serializes push_ops to avoid concurrent pushes sending duplicate ops.
    push_lock: TokioMutex<()>,
}

impl HookListener {
    /// Create a listener backed by a local-only store.
    pub fn local(store: SharedBlockStore, document_id: String) -> Self {
        Self {
            store,
            document_id,
            remote: None,
            max_block_size: DEFAULT_MAX_BLOCK_SIZE,
            context_name: "local".to_string(),
            push_lock: TokioMutex::new(()),
        }
    }

    /// Create a listener backed by a remote connection.
    pub fn remote(remote: RemoteState, context_name: String) -> Self {
        Self {
            store: remote.store.clone(),
            document_id: remote.document_id.clone(),
            remote: Some(remote),
            max_block_size: DEFAULT_MAX_BLOCK_SIZE,
            context_name,
            push_lock: TokioMutex::new(()),
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
            let ping = PingResponse {
                status: "ok".to_string(),
                pid: std::process::id(),
                cwd: std::env::current_dir().ok().map(|p| p.display().to_string()),
                context_name: Some(self.context_name.clone()),
                document_id: Some(self.document_id.clone()),
                pending_drifts: pending,
            };
            let json = serde_json::to_string(&ping).unwrap_or_default();
            writer.write_all(json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            return Ok(());
        }

        let response = self.process_event(&event).await;

        let json = serde_json::to_string(&response).unwrap_or_default();
        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;

        Ok(())
    }

    /// Process a hook event: create blocks, push ops, check drift.
    async fn process_event(&self, event: &HookEvent) -> HookResponse {
        // 1. Filter self-referential kaijutsu MCP tools
        if let Some(ref tool) = event.tool
            && KAIJUTSU_MCP_TOOLS.iter().any(|t| tool.name.eq_ignore_ascii_case(t))
        {
            // MCP server already recorded this — just check drift
            return self.maybe_inject_drift().await;
        }

        // 2. Create blocks based on event type
        match event.event.as_str() {
            "session.start" => {
                let model_info = event.model.as_deref().unwrap_or("unknown");
                let content = format!("Session started: {}, model: {}", event.source, model_info);
                self.insert_text_block(Role::System, &content);
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
                if let Some(ref response) = event.response {
                    let truncated = truncate(response, self.max_block_size);
                    self.insert_text_block(Role::Model, &truncated);
                }
            }

            "file.edit" => {
                if let Some(ref file) = event.file {
                    let edit_count = file.edits.as_ref().map(|e| e.len()).unwrap_or(0);
                    let content = if edit_count > 0 {
                        format!("File edited: {} ({} edit{})", file.path, edit_count, if edit_count == 1 { "" } else { "s" })
                    } else {
                        format!("File edited: {}", file.path)
                    };
                    self.insert_text_block(Role::Tool, &content);
                }
            }

            "subagent.start" => {
                let agent = event.agent_id.as_deref().unwrap_or("unknown");
                let kind = event.agent_type.as_deref().unwrap_or("subagent");
                self.insert_text_block(Role::System, &format!("Subagent started: {agent} ({kind})"));
            }

            "subagent.stop" => {
                let agent = event.agent_id.as_deref().unwrap_or("unknown");
                self.insert_text_block(Role::System, &format!("Subagent stopped: {agent}"));
            }

            // tool.before, agent.compact — no block
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
        if let Err(e) = self.store.insert_block(
            &self.document_id,
            None,  // parent
            None,  // after (append)
            role,
            BlockKind::Text,
            content,
        ) {
            tracing::warn!("Hook insert_block error: {e}");
        }
    }

    fn insert_tool_blocks(&self, tool: &crate::hook_types::ToolInfo, is_error: bool) {
        // Insert tool call block
        let input = tool.input.clone();
        let call_id = match self.store.insert_tool_call(
            &self.document_id,
            None,
            None,
            &tool.name,
            input,
        ) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("Hook insert_tool_call error: {e}");
                return;
            }
        };

        // Insert tool result block
        let content = if is_error {
            tool.error.as_deref().unwrap_or("(error)")
        } else {
            tool.output.as_deref().unwrap_or("(no output)")
        };
        let truncated = truncate(content, self.max_block_size);

        if let Err(e) = self.store.insert_tool_result(
            &self.document_id,
            &call_id,
            None,
            &truncated,
            is_error,
            None,
        ) {
            tracing::warn!("Hook insert_tool_result error: {e}");
        }
    }

    // -- Drift injection --

    async fn pending_drift_count(&self) -> u32 {
        let Some(ref remote) = self.remote else { return 0 };
        match remote.actor.drift_queue().await {
            Ok(queue) => queue.iter()
                .filter(|d| d.target_ctx == self.context_name)
                .count() as u32,
            Err(_) => 0,
        }
    }

    async fn maybe_inject_drift(&self) -> HookResponse {
        let Some(ref remote) = self.remote else {
            return HookResponse::allow();
        };

        // Check for drifts targeted at our context
        let queue = match remote.actor.drift_queue().await {
            Ok(q) => q,
            Err(_) => return HookResponse::allow(),
        };

        let our_drifts: Vec<_> = queue.iter()
            .filter(|d| d.target_ctx == self.context_name)
            .collect();

        if our_drifts.is_empty() {
            return HookResponse::allow();
        }

        // Build context string from drifts targeted at us
        let context: String = our_drifts.iter()
            .map(|d| format!("[Drift from {}]: {}", d.source_ctx, d.content))
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
    let frontier = {
        let sync = remote.sync.lock()
            .map_err(|e| anyhow::anyhow!("Lock error: {e}"))?;
        sync.frontier().cloned().unwrap_or_default()
    };

    let ops = remote.store.ops_since(&remote.document_id, &frontier)
        .map_err(|e| anyhow::anyhow!(e))?;

    let ops_bytes = serde_json::to_vec(&ops)
        .map_err(|e| anyhow::anyhow!("Serialize error: {e}"))?;

    if ops_bytes.len() <= 2 {
        return Ok(()); // No ops
    }

    remote.actor.push_ops(&remote.document_id, &ops_bytes).await
        .map_err(|e| anyhow::anyhow!("Push ops: {e}"))?;

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
/// of the same agent process (Claude/Gemini), so they independently compute
/// the same path.
///
/// Returns `None` if `$XDG_RUNTIME_DIR` is not set — we don't fall back to
/// `/tmp` to avoid socket permission issues on shared systems.
pub fn default_socket_path() -> Option<PathBuf> {
    let ppid = std::os::unix::process::parent_id();
    let runtime_dir = dirs::runtime_dir()?;
    Some(runtime_dir.join("kaijutsu").join(format!("hook-{ppid}.sock")))
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
    let kj_dir = runtime_dir.join("kaijutsu");

    let Ok(entries) = std::fs::read_dir(&kj_dir) else {
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
