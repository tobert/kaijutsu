//! `ExternalMcpServer` — wraps an rmcp subprocess behind `McpServerLike`.
//!
//! Phase 1 covers:
//! - subprocess / HTTP transport spawn + handshake (lifted from
//!   `mcp_pool.rs`)
//! - `_meta` propagation per §5.4 / D-11 (`io.kaijutsu.v1.*`)
//! - health flipping to `Down` on transport error; reconnect is a follow-up
//! - minimal `ClientHandler` that surfaces rmcp notifications as
//!   `ServerNotification` on the broker-visible broadcast channel (nothing
//!   subscribes yet — D-32)

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use parking_lot::RwLock as PlRwLock;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo, ContentBlock,
    ProgressNotificationParam, ProtocolVersion, ReadResourceRequestParams, RequestMetaObject,
    ResourceContents, SubscribeRequestParams, UnsubscribeRequestParams,
};
// Logging is deprecated by SEP-2577 (rmcp 1.8.0+) — kept for now, see the
// `enable_roots` comment on `BrokerClientHandler::new` below.
#[allow(deprecated)]
use rmcp::model::{LoggingLevel, LoggingMessageNotificationParam};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientHandler, RoleClient};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use std::collections::HashMap;

use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{
    Health, InstanceId, KernelCallParams, KernelReadResource, KernelResource,
    KernelResourceContents, KernelResourceList, KernelTool, KernelToolResult, LogLevel,
    ToolContent,
};

/// `_meta` namespace per §5.4.
const META_NAMESPACE: &str = "io.kaijutsu.v1";

/// Transport kind for external MCP connections. Replaces the type that used
/// to live in the removed `mcp_pool` module.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum McpTransport {
    #[default]
    Stdio,
    StreamableHttp,
}

/// Connection config for an external MCP server. Superset of what
/// `rmcp::serve_client` needs; broker config loading populates this.
///
/// There used to be a documented `fork` field here (mcp.toml's header
/// comment described "share"/"instance"/"exclude" fork behavior) — it was
/// never implemented. Rather than leave documented-but-nonexistent config,
/// the doc comment was deleted alongside rebuilding the loader
/// (`mcp/toml.rs`); see that module's doc comment for the reasoning.
#[derive(Clone, Debug, Default)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<String>,
    pub transport: McpTransport,
    pub url: Option<String>,
    /// Per-server `InstancePolicy::call_timeout` override, sourced from
    /// mcp.toml's `call_timeout_ms` (component 3, `docs/external-mcp.md`). `None`
    /// means "use the kernel-wide `TimeoutPolicy::mcp_call_timeout_default`"
    /// (`InstancePolicy::for_kernel`'s existing behavior). A long-running
    /// consult (kaibo routinely runs 5-15 minutes) needs this — the kernel
    /// default of 120s would otherwise cut every such call off.
    pub call_timeout: Option<std::time::Duration>,
}

/// Minimal `ClientHandler` that translates rmcp notifications onto a
/// broadcast channel of `ServerNotification`. Phase 1 subscribers: none
/// (D-32). Unlike the legacy `KaijutsuClientHandler`, this one carries no
/// FlowBus references.
#[derive(Clone)]
struct BrokerClientHandler {
    info: ClientInfo,
    tx: broadcast::Sender<ServerNotification>,
}

impl BrokerClientHandler {
    // Roots is deprecated by SEP-2577 (rmcp 1.8.0+) with no replacement
    // capability — the MCP spec is dropping client-side roots/sampling
    // wholesale, not superseding them. Kaibo/bevy_brp still negotiate against
    // it today, so keep advertising it rather than silently withdrawing a
    // capability older or unmigrated peers may still probe for; revisit when
    // rmcp actually removes the API (tracked in docs/issues.md).
    #[allow(deprecated)]
    fn new(tx: broadcast::Sender<ServerNotification>) -> Self {
        let mut info = ClientInfo::default();
        // Advertise the newest protocol this rmcp knows, not `ProtocolVersion::
        // default()`. `default()` is `LATEST`, which is *not* the newest known
        // version — rmcp pins `LATEST = V_2025_11_25` while `KNOWN_VERSIONS`
        // tops out at `V_2026_07_28` (true in both 3.0.1 and 3.1.2). Taking the
        // default silently negotiated every external server (kaibo, bevy_brp)
        // down to 2025-11-25 and dropped the version-gated fields with it.
        // Bump this deliberately when rmcp learns a newer version; see
        // docs/issues.md "rmcp protocol-version fallback".
        info.protocol_version = ProtocolVersion::V_2026_07_28;
        info.client_info.name = "kaijutsu".into();
        info.client_info.version = env!("CARGO_PKG_VERSION").into();
        info.capabilities = ClientCapabilities::builder()
            .enable_roots()
            .enable_roots_list_changed()
            .build();
        Self { info, tx }
    }
}

// Logging is deprecated by SEP-2577 (rmcp 1.8.0+); see the `enable_roots`
// comment above — same rationale, kept for now.
#[allow(deprecated)]
fn rmcp_level_to_log_level(level: LoggingLevel) -> LogLevel {
    // rmcp's LoggingLevel values: Debug, Info, Notice, Warning, Error,
    // Critical, Alert, Emergency. Collapse to our 5-level enum.
    match format!("{:?}", level).as_str() {
        "Debug" => LogLevel::Debug,
        "Info" | "Notice" => LogLevel::Info,
        "Warning" => LogLevel::Warn,
        _ => LogLevel::Error,
    }
}

impl ClientHandler for BrokerClientHandler {
    fn get_info(&self) -> ClientInfo {
        self.info.clone()
    }

    // Logging is deprecated by SEP-2577 (rmcp 1.8.0+); kept for now — see the
    // `enable_roots` comment on `BrokerClientHandler::new`.
    #[allow(deprecated)]
    fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let tx = self.tx.clone();
        async move {
            let level = rmcp_level_to_log_level(params.level);
            let message = match serde_json::to_string(&params.data) {
                Ok(s) => s,
                Err(_) => String::from("<unserializable log payload>"),
            };
            let _ = tx.send(ServerNotification::Log {
                level,
                message,
                tool: params.logger,
            });
        }
    }

    fn on_progress(
        &self,
        _params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        // Phase 1: progress → not surfaced yet (coalescer comes in Phase 2).
        async {}
    }

    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let tx = self.tx.clone();
        async move {
            let _ = tx.send(ServerNotification::ToolsChanged);
        }
    }

    fn on_resource_updated(
        &self,
        params: rmcp::model::ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let tx = self.tx.clone();
        async move {
            let _ = tx.send(ServerNotification::ResourceUpdated { uri: params.uri });
        }
    }

    fn on_prompt_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let tx = self.tx.clone();
        async move {
            let _ = tx.send(ServerNotification::PromptsChanged);
        }
    }
}

/// Propagate PATH and other essential host env vars onto the child command —
/// matches legacy `mcp_pool::propagate_host_env`.
fn propagate_host_env(cmd: &mut Command) {
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    }
}

/// Harden a to-be-spawned external MCP subprocess `Command` against becoming
/// an orphan if this kernel process dies — by any means, including a crash,
/// a panic, or `kill -9`, none of which run `ExternalMcpServer::shutdown` or
/// any other graceful-shutdown code path. Without this, the OS simply
/// reparents the child to init on kernel death and it leaks permanently
/// (every external MCP server — kaibo, bevy_brp, … — is exactly this
/// shape).
///
/// Mirrors `background_exec::spawn_background`'s `pre_exec` exactly (see
/// that module's "Ownership and cleanup" doc section for the full
/// rationale, including why this was chosen over relying solely on
/// `kill_on_drop` or `systemd`'s `KillMode=control-group`): its own process
/// group (`setpgid`) plus Linux's `PR_SET_PDEATHSIG(SIGKILL)` so the OS
/// kills the child the instant its parent dies, unconditionally.
/// `kill_on_drop` stays as a secondary, weaker backstop — it only fires on a
/// clean in-process `Drop`, which a crash or `kill -9` never triggers.
fn harden_child_command(cmd: &mut Command) {
    // Backstop only: kills the child if this process cleanly drops the
    // `Child` (e.g. a panic unwind reaches a live `RunningService`/
    // `TokioChildProcess`). The real orphan guard is PDEATHSIG below, which
    // also covers `kill -9` and process crashes where no `Drop` ever runs.
    cmd.kill_on_drop(true);

    #[cfg(unix)]
    {
        // SAFETY: `setpgid`/`set_parent_process_death_signal` are
        // async-signal-safe per POSIX; safe to call between fork and exec.
        // Own process group matches kaish's and `background_exec`'s own
        // external-command spawn convention.
        #[allow(unsafe_code)]
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::setpgid(None, None)
                    .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
                #[cfg(target_os = "linux")]
                rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::Kill,
                ))
                .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
                Ok(())
            });
        }
    }
}

pub struct ExternalMcpServer {
    instance_id: InstanceId,
    config: McpServerConfig,
    service: PlRwLock<Option<RunningService<RoleClient, BrokerClientHandler>>>,
    tools_cache: PlRwLock<Vec<KernelTool>>,
    notif_tx: broadcast::Sender<ServerNotification>,
    /// Set to `true` when a transport error occurs. Cleared on successful
    /// reconnect. Drives `health()`.
    down: AtomicBool,
    /// Populated alongside `down = true`.
    down_reason: PlRwLock<Option<String>>,
}

impl ExternalMcpServer {
    /// Connect to an external MCP server per the given config. `instance_id`
    /// is used as the broker registration key; for existing `mcp_config.rs`
    /// entries it's typically `config.name`.
    ///
    /// `connect_timeout` bounds the entire spawn + handshake + initial
    /// `list_tools` round-trip. A wedged child server returns
    /// `McpError::Protocol("connect timeout: …")` rather than hanging the
    /// broker init path. Callers should source this from
    /// `kernel.timeouts().mcp_connect_timeout`.
    pub async fn connect(
        config: McpServerConfig,
        instance_id: InstanceId,
        connect_timeout: std::time::Duration,
    ) -> McpResult<Self> {
        let (notif_tx, _) = broadcast::channel(256);
        let handler = BrokerClientHandler::new(notif_tx.clone());

        let init = async {
            let service = match config.transport {
                McpTransport::Stdio => {
                    let mut cmd = Command::new(&config.command);
                    cmd.args(&config.args);
                    propagate_host_env(&mut cmd);
                    for (key, value) in &config.env {
                        cmd.env(key, value);
                    }
                    if let Some(cwd) = &config.cwd {
                        cmd.current_dir(cwd);
                    }
                    harden_child_command(&mut cmd);
                    let transport = TokioChildProcess::new(cmd)
                        .map_err(|e| McpError::Protocol(format!("spawn: {e}")))?;
                    rmcp::serve_client(handler, transport)
                        .await
                        .map_err(|e| McpError::Protocol(format!("init: {e}")))?
                }
                McpTransport::StreamableHttp => {
                    let url = config.url.as_deref().ok_or_else(|| {
                        McpError::Protocol("StreamableHttp transport requires url".to_string())
                    })?;
                    let transport = StreamableHttpClientTransport::from_uri(url);
                    rmcp::serve_client(handler, transport)
                        .await
                        .map_err(|e| McpError::Protocol(format!("init: {e}")))?
                }
            };
            let tools = service
                .peer()
                .list_all_tools()
                .await
                .map_err(|e| McpError::Protocol(format!("list_tools: {e}")))?;
            McpResult::Ok((service, tools))
        };

        let (service, tools) = tokio::time::timeout(connect_timeout, init)
            .await
            .map_err(|_| {
                McpError::Protocol(format!(
                    "connect timeout: spawn+handshake+list_tools exceeded {:?}",
                    connect_timeout
                ))
            })??;

        let kernel_tools: Vec<KernelTool> = tools
            .into_iter()
            .map(|t| KernelTool {
                instance: instance_id.clone(),
                name: t.name.to_string(),
                description: t.description.map(|s| s.to_string()),
                input_schema: serde_json::Value::Object(t.input_schema.as_ref().clone()),
            })
            .collect();

        Ok(Self {
            instance_id,
            config,
            service: PlRwLock::new(Some(service)),
            tools_cache: PlRwLock::new(kernel_tools),
            notif_tx,
            down: AtomicBool::new(false),
            down_reason: PlRwLock::new(None),
        })
    }

    fn mark_down(&self, reason: impl Into<String>) {
        self.down.store(true, Ordering::Relaxed);
        *self.down_reason.write() = Some(reason.into());
    }

    /// Tear down the current service and spin up a fresh one. Intended for
    /// post-failure recovery; Phase 1 does not invoke this automatically.
    /// Bounded by `connect_timeout` like `connect()`.
    pub async fn reconnect(&self, connect_timeout: std::time::Duration) -> McpResult<()> {
        // Drop the old service first so the subprocess fully exits.
        let _ = self.service.write().take();

        let handler = BrokerClientHandler::new(self.notif_tx.clone());
        let init = async {
            let new_service = match self.config.transport {
                McpTransport::Stdio => {
                    let mut cmd = Command::new(&self.config.command);
                    cmd.args(&self.config.args);
                    propagate_host_env(&mut cmd);
                    for (k, v) in &self.config.env {
                        cmd.env(k, v);
                    }
                    if let Some(cwd) = &self.config.cwd {
                        cmd.current_dir(cwd);
                    }
                    harden_child_command(&mut cmd);
                    let transport = TokioChildProcess::new(cmd)
                        .map_err(|e| McpError::Protocol(format!("spawn: {e}")))?;
                    rmcp::serve_client(handler, transport)
                        .await
                        .map_err(|e| McpError::Protocol(format!("init: {e}")))?
                }
                McpTransport::StreamableHttp => {
                    let url = self.config.url.as_deref().ok_or_else(|| {
                        McpError::Protocol("StreamableHttp transport requires url".to_string())
                    })?;
                    let transport = StreamableHttpClientTransport::from_uri(url);
                    rmcp::serve_client(handler, transport)
                        .await
                        .map_err(|e| McpError::Protocol(format!("init: {e}")))?
                }
            };
            // Refresh tool cache — list_changed may have fired during the outage.
            let tools = new_service
                .peer()
                .list_all_tools()
                .await
                .map_err(|e| McpError::Protocol(format!("list_tools: {e}")))?;
            McpResult::Ok((new_service, tools))
        };

        let (new_service, tools) = tokio::time::timeout(connect_timeout, init)
            .await
            .map_err(|_| {
                McpError::Protocol(format!(
                    "reconnect timeout: spawn+handshake+list_tools exceeded {:?}",
                    connect_timeout
                ))
            })??;
        let kernel_tools: Vec<KernelTool> = tools
            .into_iter()
            .map(|t| KernelTool {
                instance: self.instance_id.clone(),
                name: t.name.to_string(),
                description: t.description.map(|s| s.to_string()),
                input_schema: serde_json::Value::Object(t.input_schema.as_ref().clone()),
            })
            .collect();

        *self.service.write() = Some(new_service);
        *self.tools_cache.write() = kernel_tools;
        self.down.store(false, Ordering::Relaxed);
        *self.down_reason.write() = None;
        Ok(())
    }

    fn instance_down_error(&self) -> McpError {
        let reason = self
            .down_reason
            .read()
            .clone()
            .unwrap_or_else(|| "service not initialized".to_string());
        McpError::InstanceDown {
            instance: self.instance_id.clone(),
            reason,
        }
    }

    fn build_meta(&self, ctx: &CallContext) -> RequestMetaObject {
        // RequestMetaObject wraps MetaObject wraps JsonObject (rmcp 3.x split
        // the old flat `Meta` newtype into request/notification/result
        // variants — this is the request one); populate the three kaijutsu
        // fields per §5.4.
        let mut obj = serde_json::Map::new();
        obj.insert(
            format!("{META_NAMESPACE}.principal_id"),
            serde_json::Value::String(ctx.principal_id.to_hex()),
        );
        obj.insert(
            format!("{META_NAMESPACE}.context_id"),
            serde_json::Value::String(ctx.context_id.to_hex()),
        );
        if !ctx.trace.is_empty() {
            obj.insert(
                format!("{META_NAMESPACE}.trace"),
                serde_json::json!({
                    "traceparent": ctx.trace.traceparent,
                    "tracestate": ctx.trace.tracestate,
                }),
            );
        }
        RequestMetaObject::from(obj)
    }
}

/// Map an `rmcp::ServiceError` into `McpError`. `METHOD_NOT_FOUND` on the wire
/// means the server does not implement that capability — surface as
/// `McpError::Unsupported` without marking the instance down (R8). Every
/// other transport / protocol error flips the instance to `Down` via
/// `mark_down` (passed as a closure so we don't need `&self` here).
fn map_rmcp_service_error(
    err: rmcp::service::ServiceError,
    instance: &InstanceId,
    mark_down: impl FnOnce(String),
) -> McpError {
    use rmcp::service::ServiceError;
    match err {
        ServiceError::McpError(e) if e.code.0 == -32601 => {
            // METHOD_NOT_FOUND — capability simply not advertised.
            McpError::Unsupported
        }
        ServiceError::McpError(e) => {
            // Protocol-level error from the server (e.g. invalid params).
            // Do NOT mark down — the transport is fine, the request was bad.
            let _ = instance;
            McpError::Protocol(e.to_string())
        }
        other => {
            let msg = other.to_string();
            mark_down(msg.clone());
            McpError::Protocol(msg)
        }
    }
}

fn translate_result(result: CallToolResult) -> KernelToolResult {
    let is_error = result.is_error.unwrap_or(false);
    let content = result
        .content
        .into_iter()
        .map(|c: ContentBlock| match c.as_text() {
            Some(text) => ToolContent::Text(text.text.clone()),
            None => match serde_json::to_value(&c) {
                Ok(v) => ToolContent::Json(v),
                Err(_) => ToolContent::Text(String::from("<unserializable content>")),
            },
        })
        .collect();
    KernelToolResult {
        is_error,
        content,
        structured: result.structured_content,
    }
}

#[async_trait]
impl McpServerLike for ExternalMcpServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        Ok(self.tools_cache.read().clone())
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        if self.down.load(Ordering::Relaxed) {
            let reason = self
                .down_reason
                .read()
                .clone()
                .unwrap_or_else(|| "instance is down".to_string());
            return Err(McpError::InstanceDown {
                instance: self.instance_id.clone(),
                reason,
            });
        }

        // Build CallToolRequestParams, attaching _meta and arguments.
        let mut req = CallToolRequestParams::new(params.tool.clone());
        if let serde_json::Value::Object(map) = params.arguments.clone() {
            req = req.with_arguments(map);
        }
        req.meta = Some(self.build_meta(ctx));

        // Snapshot the peer inside a short-lived scope so the parking_lot
        // guard (non-Send) doesn't cross the await.
        let peer = {
            let guard = self.service.read();
            match guard.as_ref() {
                Some(s) => s.peer().clone(),
                None => {
                    return Err(McpError::InstanceDown {
                        instance: self.instance_id.clone(),
                        reason: "service not initialized".to_string(),
                    });
                }
            }
        };

        let result = peer.call_tool(req).await.map_err(|e| {
            self.mark_down(format!("{e}"));
            McpError::Protocol(e.to_string())
        })?;

        Ok(translate_result(result))
    }

    async fn list_resources(&self, _ctx: &CallContext) -> McpResult<KernelResourceList> {
        if self.down.load(Ordering::Relaxed) {
            return Err(self.instance_down_error());
        }
        let peer = {
            let guard = self.service.read();
            match guard.as_ref() {
                Some(s) => s.peer().clone(),
                None => return Err(self.instance_down_error()),
            }
        };
        let resources = peer.list_all_resources().await.map_err(|e| {
            map_rmcp_service_error(e, &self.instance_id, |reason| {
                self.mark_down(reason);
            })
        })?;
        let mapped = resources
            .into_iter()
            .map(|r| KernelResource {
                instance: self.instance_id.clone(),
                uri: r.uri.clone(),
                name: r.name.clone(),
                description: r.description.clone(),
                mime_type: r.mime_type.clone(),
                size: r.size,
            })
            .collect();
        Ok(KernelResourceList { resources: mapped })
    }

    async fn read_resource(
        &self,
        uri: &str,
        ctx: &CallContext,
    ) -> McpResult<KernelReadResource> {
        if self.down.load(Ordering::Relaxed) {
            return Err(self.instance_down_error());
        }
        let peer = {
            let guard = self.service.read();
            match guard.as_ref() {
                Some(s) => s.peer().clone(),
                None => return Err(self.instance_down_error()),
            }
        };
        let mut params = ReadResourceRequestParams::new(uri);
        params.meta = Some(self.build_meta(ctx));
        let result = peer.read_resource(params).await.map_err(|e| {
            map_rmcp_service_error(e, &self.instance_id, |reason| {
                self.mark_down(reason);
            })
        })?;
        let contents = result
            .contents
            .into_iter()
            .map(|c| match c {
                ResourceContents::TextResourceContents {
                    uri, mime_type, text, ..
                } => Ok(KernelResourceContents::Text {
                    uri,
                    mime_type,
                    text,
                }),
                ResourceContents::BlobResourceContents {
                    uri, mime_type, blob, ..
                } => Ok(KernelResourceContents::Blob {
                    uri,
                    mime_type,
                    blob_base64: blob,
                }),
                // `ResourceContents` is `#[non_exhaustive]` upstream — the
                // untagged deserializer only ever produces the two variants
                // above from any wire payload today, so this is reachable
                // only after a *future* rmcp bump adds a third shape (and
                // `#[non_exhaustive]` means the compiler cannot flag it for
                // us at that point — hence a runtime guard at all).
                //
                // Dropping the unrecognized content would hand back an
                // incomplete read as though it were complete, which is the
                // data corruption this project ranks below crashing. But the
                // choice isn't drop-or-crash: this function already has an
                // error channel, so fail the CALL loudly and leave the kernel
                // — and every other context sharing it — standing. A panic
                // here would take down a turn (and under `panic = "abort"`
                // the whole kernel) over one unmappable resource read from
                // one external server.
                other => Err(McpError::Protocol(format!(
                    "external MCP server `{}` returned a ResourceContents shape this \
                     kernel cannot map ({other:?}) — an rmcp upgrade added a variant; \
                     add a KernelResourceContents mapping in \
                     mcp/servers/external.rs::read_resource",
                    self.instance_id
                ))),
            })
            .collect::<McpResult<Vec<_>>>()?;
        Ok(KernelReadResource { contents })
    }

    // resources/subscribe is legacy-only per SEP-2577 (superseded by
    // `Peer::listen` / `subscriptions/listen` for peers on protocol version
    // 2026-07-28) — but that's a real subscription-model migration, not a
    // drop-in swap, and out of scope for this dependency bump. Kept for now;
    // tracked in docs/issues.md.
    #[allow(deprecated)]
    async fn subscribe(&self, uri: &str, _ctx: &CallContext) -> McpResult<()> {
        if self.down.load(Ordering::Relaxed) {
            return Err(self.instance_down_error());
        }
        let peer = {
            let guard = self.service.read();
            match guard.as_ref() {
                Some(s) => s.peer().clone(),
                None => return Err(self.instance_down_error()),
            }
        };
        let params = SubscribeRequestParams::new(uri);
        peer.subscribe(params).await.map_err(|e| {
            map_rmcp_service_error(e, &self.instance_id, |reason| {
                self.mark_down(reason);
            })
        })?;
        Ok(())
    }

    // resources/unsubscribe is likewise legacy-only per SEP-2577 — see the
    // `subscribe` comment above.
    #[allow(deprecated)]
    async fn unsubscribe(&self, uri: &str, _ctx: &CallContext) -> McpResult<()> {
        if self.down.load(Ordering::Relaxed) {
            return Err(self.instance_down_error());
        }
        let peer = {
            let guard = self.service.read();
            match guard.as_ref() {
                Some(s) => s.peer().clone(),
                None => return Err(self.instance_down_error()),
            }
        };
        let params = UnsubscribeRequestParams::new(uri);
        peer.unsubscribe(params).await.map_err(|e| {
            map_rmcp_service_error(e, &self.instance_id, |reason| {
                self.mark_down(reason);
            })
        })?;
        Ok(())
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }

    async fn health(&self) -> Health {
        if self.down.load(Ordering::Relaxed) {
            let reason = self
                .down_reason
                .read()
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            Health::Down { reason }
        } else {
            Health::Ready
        }
    }

    async fn shutdown(&self) -> McpResult<()> {
        // Drop the guard before awaiting — parking_lot guards aren't Send.
        let service = { self.service.write().take() };
        if let Some(service) = service {
            service
                .cancel()
                .await
                .map_err(|e| McpError::Protocol(format!("shutdown: {e}")))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::super::context::TraceContext;
    use kaijutsu_types::{ContextId, KernelId, PrincipalId, SessionId};

    /// Build a fake ExternalMcpServer without spawning a real subprocess to
    /// test `_meta` construction. We construct the fields manually because
    /// `connect()` requires real IPC.
    fn fake_server(instance: &str) -> ExternalMcpServer {
        let (tx, _) = broadcast::channel(16);
        ExternalMcpServer {
            instance_id: InstanceId::new(instance),
            config: McpServerConfig {
                name: instance.to_string(),
                command: String::from("/bin/true"),
                ..Default::default()
            },
            service: PlRwLock::new(None),
            tools_cache: PlRwLock::new(Vec::new()),
            notif_tx: tx,
            down: AtomicBool::new(false),
            down_reason: PlRwLock::new(None),
        }
    }

    #[test]
    fn meta_carries_kaijutsu_v1_fields() {
        let server = fake_server("test.ext");
        let ctx = CallContext {
            principal_id: PrincipalId::new(),
            context_id: ContextId::new(),
            session_id: SessionId::new(),
            kernel_id: KernelId::new(),
            cwd: None,
            trace: TraceContext {
                traceparent: "00-abc-def-01".to_string(),
                tracestate: String::new(),
            },
        };
        let meta = server.build_meta(&ctx);
        assert_eq!(
            meta.0.get("io.kaijutsu.v1.principal_id"),
            Some(&serde_json::Value::String(ctx.principal_id.to_hex()))
        );
        assert_eq!(
            meta.0.get("io.kaijutsu.v1.context_id"),
            Some(&serde_json::Value::String(ctx.context_id.to_hex()))
        );
        let trace = meta.0.get("io.kaijutsu.v1.trace").expect("trace present");
        assert_eq!(trace["traceparent"], "00-abc-def-01");
    }

    #[test]
    fn meta_omits_empty_trace() {
        let server = fake_server("test.ext");
        let ctx = CallContext::test();
        let meta = server.build_meta(&ctx);
        assert!(
            !meta.0.contains_key("io.kaijutsu.v1.trace"),
            "empty trace context should not be emitted"
        );
    }

    #[tokio::test]
    async fn down_state_rejects_calls() {
        let server = fake_server("test.ext");
        server.mark_down("simulated outage");
        assert!(matches!(server.health().await, Health::Down { .. }));

        let err = server
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new("test.ext"),
                    tool: "anything".to_string(),
                    arguments: serde_json::json!({}),
                },
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::InstanceDown { .. }));
    }

    // ---- connect() against a real subprocess ---------------------------
    //
    // Neither `connect()` nor `reconnect()` had ANY test coverage before
    // this module gained a caller (`mcp::external_registry`) — the tests
    // above all construct `ExternalMcpServer` by hand, bypassing the spawn
    // + rmcp handshake entirely. The two below close the "fails loudly"
    // half of that gap (no real MCP-speaking process involved — just a
    // missing binary and a silent one). The "connects and actually works"
    // half needs `mcp_stub_server`'s `CARGO_BIN_EXE_mcp_stub_server`, which
    // Cargo only defines for *integration* tests, not `--lib` unit tests —
    // see `tests/external_mcp_stub.rs`.

    #[tokio::test]
    async fn connect_fails_loudly_on_a_missing_binary() {
        let config = McpServerConfig {
            name: "ghost".to_string(),
            command: "/definitely/does/not/exist/mcp-server".to_string(),
            ..Default::default()
        };
        match ExternalMcpServer::connect(
            config,
            InstanceId::new("external.ghost"),
            std::time::Duration::from_secs(5),
        )
        .await
        {
            // Configured-but-unstartable must be visibly a Protocol error
            // (the caller logs it), never silently swallowed into an Ok.
            Err(McpError::Protocol(_)) => {}
            Err(other) => panic!("expected McpError::Protocol, got {other}"),
            Ok(_) => panic!("a nonexistent binary must fail to connect, not hang or succeed"),
        }
    }

    #[tokio::test]
    async fn connect_times_out_on_a_process_that_never_speaks_mcp() {
        // `sleep` spawns fine but never writes a byte, so the handshake
        // hangs until the connect_timeout fires — exercises the *other*
        // "unstartable" shape (a process that starts but doesn't answer),
        // distinct from spawn failure above.
        let config = McpServerConfig {
            name: "silent".to_string(),
            command: "/bin/sleep".to_string(),
            args: vec!["5".to_string()],
            ..Default::default()
        };
        let start = std::time::Instant::now();
        match ExternalMcpServer::connect(
            config,
            InstanceId::new("external.silent"),
            std::time::Duration::from_millis(200),
        )
        .await
        {
            Err(McpError::Protocol(_)) => {}
            Err(other) => panic!("expected McpError::Protocol, got {other}"),
            Ok(_) => panic!("a silent process must time out, not hang forever or succeed"),
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "connect() must respect connect_timeout, not the full `sleep 5`"
        );
    }

    // ---- orphan protection (defect fix) --------------------------------
    //
    // `connect()`/`reconnect()` route every spawned Command through
    // `harden_child_command`, which sets `setpgid(None, None)` +
    // `PR_SET_PDEATHSIG(SIGKILL)` in `pre_exec` (mirrors
    // `background_exec::spawn_background` — see that module's "Ownership
    // and cleanup" doc section). The test below exercises the exact same
    // `harden_child_command` → `TokioChildProcess::new` seam `connect()`
    // uses, without needing a real MCP handshake, and checks the ONE half
    // of that closure's effect that's observable from outside the child:
    // `setpgid` — a still-live process's group id is readable via
    // `getpgid`. `PR_SET_PDEATHSIG` itself has NO such external observation
    // point: `prctl(PR_GET_PDEATHSIG)` only ever reports the CALLING
    // thread's own flag, so proving it fired would require either (a) the
    // child process itself reading back its own flag and reporting it (this
    // suite doesn't spawn a purpose-built reporter binary for that), or (b)
    // an actual "kill this process and observe the child dies" system test,
    // which means forking the test binary — a multithreaded tokio
    // process — a genuinely risky thing to do inside `cargo test`.
    // `background_exec.rs`'s own test suite makes the identical call: it
    // has no direct PDEATHSIG test either, despite pre_exec setting it the
    // exact same way. setpgid and PDEATHSIG are set back-to-back inside the
    // SAME `pre_exec` closure, so a successful setpgid is the strongest
    // indirect evidence available short of the two options above that the
    // closure ran to completion and reached the PDEATHSIG call too.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawned_child_command_gets_its_own_process_group() {
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("5");
        harden_child_command(&mut cmd);

        let mut transport = TokioChildProcess::new(cmd).expect("spawn sleep");
        let pid = transport.id().expect("pid should be observable right after spawn");

        let child_pgid = rustix::process::getpgid(rustix::process::Pid::from_raw(pid as i32))
            .expect("getpgid must succeed for a just-spawned, still-live child");
        let own_pgid = rustix::process::getpgid(None).expect("getpgid(None) is this process's own");

        assert_eq!(
            child_pgid.as_raw_nonzero().get() as u32,
            pid,
            "setpgid(None, None) in harden_child_command's pre_exec should make the \
             child the leader of its own new process group (pgid == pid)"
        );
        assert_ne!(
            child_pgid.as_raw_nonzero(),
            own_pgid.as_raw_nonzero(),
            "the child's process group must differ from this test process's — \
             proves pre_exec actually ran rather than inheriting our group"
        );

        // Clean up so we don't leak a live `sleep 5` past this test.
        let _ = transport.graceful_shutdown().await;
    }
}
