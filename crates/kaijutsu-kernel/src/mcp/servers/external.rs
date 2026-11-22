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
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, ContentBlock,
    ElicitRequestParams, ElicitResult, ElicitationAction, ProgressNotificationParam,
    ProtocolVersion, ReadResourceRequestParams, RequestMetaObject, ResourceContents,
    SubscribeRequestParams, SubscriptionFilter, UnsubscribeRequestParams,
};
// Logging is deprecated by SEP-2577 (rmcp 1.8.0+) — kept for now, see the
// `enable_roots` comment on `BrokerClientHandler::new` below.
#[allow(deprecated)]
use rmcp::model::{LoggingLevel, LoggingMessageNotificationParam};
use rmcp::service::{NotificationContext, Peer, RequestContext, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientHandler, ClientLifecycleMode, ClientServiceExt, RoleClient};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use std::collections::HashMap;

use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{
    ElicitationRequest, Health, InstanceId, KernelCallParams, KernelReadResource, KernelResource,
    KernelResourceContents, KernelResourceList, KernelTool, KernelToolResult, LogLevel,
    ToolContent,
};

/// `_meta` namespace per §5.4.
const META_NAMESPACE: &str = "io.kaijutsu.v1";

/// Default request URI used for `UnixSocket` transport when `url` is omitted.
pub const DEFAULT_UNIX_SOCKET_URL: &str = "http://localhost/mcp";

/// Transport kind for external MCP connections.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum McpTransport {
    #[default]
    Stdio,
    StreamableHttp,
    UnixSocket,
}

/// Connection config for an external MCP server. Superset of what
/// `rmcp::serve_client` needs; broker config loading populates this.
#[derive(Clone, Debug, Default)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<String>,
    pub transport: McpTransport,
    pub url: Option<String>,
    /// Unix domain socket path for `UnixSocket` transport.
    pub socket: Option<String>,
    /// Extra HTTP headers sent with every request, for `StreamableHttp` and
    /// `UnixSocket` transports. Values are resolved at config load time.
    pub headers: HashMap<String, String>,
    /// Per-server `InstancePolicy::call_timeout` override, sourced from
    /// mcp.toml's `call_timeout_ms`. `None` falls back to
    /// `TimeoutPolicy::mcp_call_timeout_default`.
    pub call_timeout: Option<std::time::Duration>,
}

/// Minimal `ClientHandler` that translates rmcp notifications onto a
/// broadcast channel of `ServerNotification`. Phase 1 subscribers: none
/// (D-32). Unlike the legacy `KaijutsuClientHandler`, this one carries no
/// FlowBus references.
#[derive(Clone)]
struct BrokerClientHandler {
    info: ClientConfig,
    tx: broadcast::Sender<ServerNotification>,
}

impl BrokerClientHandler {
    // Roots is deprecated by SEP-2577 (rmcp 1.8.0+) with no replacement
    // capability — the MCP spec is dropping client-side roots/sampling
    // wholesale, not superseding them. Kaibo/bevy_brp still negotiate against
    // it today, so keep advertising it rather than silently withdrawing a
    // capability older or unmigrated peers may still probe for; revisit when
    // rmcp actually removes the API.
    #[allow(deprecated)]
    fn new(tx: broadcast::Sender<ServerNotification>) -> Self {
        let mut info = ClientConfig::default();
        // Advertise the newest protocol this rmcp knows, not `ProtocolVersion::
        // default()`. `default()` is `LATEST`, which is *not* the newest known
        // version — rmcp pins `LATEST = V_2025_11_25` while `KNOWN_VERSIONS`
        // tops out at `V_2026_07_28` (rmcp 3.4). Taking the
        // default silently negotiated every external server (kaibo, bevy_brp)
        // down to 2025-11-25 and dropped the version-gated fields with it.
        // Bump this deliberately when rmcp learns a newer version.
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
    fn get_info(&self) -> ClientConfig {
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
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let tx = self.tx.clone();
        async move {
            let token = match &params.progress_token.0 {
                rmcp::model::NumberOrString::String(s) => s.to_string(),
                rmcp::model::NumberOrString::Number(n) => n.to_string(),
            };
            let _ = tx.send(ServerNotification::Progress {
                token,
                progress: params.progress,
                total: params.total,
                message: params.message,
            });
        }
    }

    /// Answer an `elicitation/create` from an external server.
    ///
    /// We still **decline** — nothing in the kernel can collect an answer yet
    /// (that is slice 1b in `docs/issues.md`, a design pass: where a pending
    /// elicitation lives, how an answer routes back, timeout behaviour). What
    /// this override changes is that the decline is no longer *invisible*.
    ///
    /// rmcp's trait default declines silently, so an external server asking a
    /// question got a "no" that nobody — human, sibling context, or log —
    /// ever saw. CLAUDE.md: silent fallbacks are often a mistake. We now emit
    /// the reserved `ServerNotification::Elicitation` (D-25) and warn, so the
    /// request is at least observable and we can find out whether kaibo or
    /// bevy_brp ever actually send one.
    fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<ElicitResult, rmcp::ErrorData>> + Send + '_ {
        let tx = self.tx.clone();
        async move {
            let (message, schema) = match &request {
                ElicitRequestParams::FormElicitationParams {
                    message,
                    requested_schema,
                    ..
                } => (
                    message.clone(),
                    serde_json::to_value(requested_schema).ok(),
                ),
                ElicitRequestParams::UrlElicitationParams {
                    message,
                    url,
                    elicitation_id,
                    ..
                } => (
                    format!("{message} (url mode: {url}, id {elicitation_id})"),
                    None,
                ),
                // `ElicitRequestParams` is #[non_exhaustive]: a mode added by a
                // future spec must still be seen rather than silently declined.
                other => (format!("unrecognized elicitation mode: {other:?}"), None),
            };

            tracing::warn!(
                "MCP elicitation declined (no collector wired yet): {message}"
            );
            let _ = tx.send(ServerNotification::Elicitation(ElicitationRequest {
                message: std::borrow::Cow::Owned(message),
                schema,
            }));

            Ok(ElicitResult::new(ElicitationAction::Decline))
        }
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

/// The request headers for one streamable-HTTP server: the configured
/// headers plus a `User-Agent` of `kaijutsu/<version>` when none is
/// configured. Every outbound HTTP client identifies itself as kaijutsu.
///
/// An unusable header name or value fails the connection rather than being
/// dropped — a request that silently goes out without its credential comes
/// back as an authentication error far from the mistake.
fn http_headers(
    config: &McpServerConfig,
) -> McpResult<HashMap<http::HeaderName, http::HeaderValue>> {
    let mut headers = HashMap::with_capacity(config.headers.len() + 1);
    for (name, value) in &config.headers {
        let header = http::HeaderName::try_from(name.as_str()).map_err(|e| {
            McpError::Protocol(format!("header '{name}' is not a valid header name: {e}"))
        })?;
        // The error never quotes the value — it may be a credential.
        let header_value = http::HeaderValue::try_from(value.as_str()).map_err(|_| {
            McpError::Protocol(format!(
                "the value configured for header '{name}' cannot be sent in an HTTP header"
            ))
        })?;
        headers.insert(header, header_value);
    }
    headers
        .entry(http::header::USER_AGENT)
        .or_insert_with(|| http::HeaderValue::from_static(crate::llm::http_user_agent()));
    Ok(headers)
}

/// Build the streamable-HTTP transport for one server: its URL plus
/// [`http_headers`].
fn http_transport(
    config: &McpServerConfig,
) -> McpResult<rmcp::transport::StreamableHttpClientTransport<reqwest::Client>> {
    let url = config
        .url
        .as_deref()
        .ok_or_else(|| McpError::Protocol("StreamableHttp transport requires url".to_string()))?;
    let headers = http_headers(config)?;
    let transport_config =
        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url)
            .custom_headers(headers);
    Ok(rmcp::transport::StreamableHttpClientTransport::from_config(transport_config))
}

/// Build the streamable-HTTP-over-Unix-socket transport for one server: its
/// socket path, request URI (defaulting to [`DEFAULT_UNIX_SOCKET_URL`]), and
/// [`http_headers`].
#[cfg(unix)]
fn unix_socket_transport(
    config: &McpServerConfig,
) -> McpResult<
    rmcp::transport::StreamableHttpClientTransport<rmcp::transport::UnixSocketHttpClient>,
> {
    let socket = config
        .socket
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "@")
        .ok_or_else(|| {
            McpError::Protocol("UnixSocket transport requires a non-empty socket path".to_string())
        })?;
    let url = config
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .unwrap_or(DEFAULT_UNIX_SOCKET_URL);
    let headers = http_headers(config)?;
    let transport_config =
        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url)
            .custom_headers(headers);
    Ok(
        rmcp::transport::StreamableHttpClientTransport::from_unix_socket_with_config(
            socket,
            transport_config,
        ),
    )
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
/// The child has its own process group (`setpgid`) and Linux parent-death
/// signal (`PR_SET_PDEATHSIG(SIGKILL)`).
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
        // Own process group matches kaish's
        // external-command spawn convention.
        #[allow(unsafe_code)]
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::setpgid(None, None)
                    .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
                #[cfg(target_os = "linux")]
                rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::KILL,
                ))
                .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
                Ok(())
            });
        }
    }
}

/// Lifecycle mode for external MCP client connections: probe `server/discover`
/// for `2026-07-28` first, falling back to legacy `initialize` on `2025-11-25`.
fn client_lifecycle_mode() -> ClientLifecycleMode {
    ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        legacy_version: Some(ProtocolVersion::V_2025_11_25),
    }
}

/// Active background task driving one `subscriptions/listen` stream.
struct SubscriptionHandle {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

/// Returns true when the connected peer negotiated a protocol version that
/// uses `subscriptions/listen` instead of legacy `resources/subscribe`.
fn uses_subscriptions_listen(peer: &Peer<RoleClient>) -> bool {
    peer.peer_info()
        .is_some_and(|info| info.protocol_version >= ProtocolVersion::V_2026_07_28)
}

/// Forward a notification delivered on a `subscriptions/listen` stream onto
/// the server's broker-visible broadcast channel.
fn forward_subscription_notification(
    tx: &broadcast::Sender<ServerNotification>,
    notif: rmcp::model::ServerNotification,
) {
    match notif {
        rmcp::model::ServerNotification::ResourceUpdatedNotification(n) => {
            let _ = tx.send(ServerNotification::ResourceUpdated { uri: n.params.uri });
        }
        rmcp::model::ServerNotification::ToolListChangedNotification(_) => {
            let _ = tx.send(ServerNotification::ToolsChanged);
        }
        rmcp::model::ServerNotification::PromptListChangedNotification(_) => {
            let _ = tx.send(ServerNotification::PromptsChanged);
        }
        rmcp::model::ServerNotification::ResourceListChangedNotification(_) => {
            tracing::debug!("MCP resource list changed notification received");
        }
        _ => {}
    }
}

pub struct ExternalMcpServer {
    instance_id: InstanceId,
    config: McpServerConfig,
    service: PlRwLock<Option<RunningService<RoleClient, BrokerClientHandler>>>,
    tools_cache: PlRwLock<Vec<KernelTool>>,
    notif_tx: broadcast::Sender<ServerNotification>,
    /// Active `subscriptions/listen` tasks keyed by resource URI.
    subscriptions: PlRwLock<HashMap<String, SubscriptionHandle>>,
    /// Set to `true` when a transport error occurs. Cleared on successful
    /// reconnect. Drives `health()`.
    down: AtomicBool,
    /// Populated alongside `down = true`.
    down_reason: PlRwLock<Option<String>>,
    /// Where this server's images go ([`Self::with_images`]). Without one an
    /// image is refused, never passed on as base64 text.
    images: Option<ImageStore>,
}

use super::block::ImageStore;

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
                    handler
                        .serve_with_lifecycle(transport, client_lifecycle_mode())
                        .await
                        .map_err(|e| McpError::Protocol(format!("init: {e}")))?
                }
                McpTransport::StreamableHttp => {
                    let transport = http_transport(&config)?;
                    handler
                        .serve_with_lifecycle(transport, client_lifecycle_mode())
                        .await
                        .map_err(|e| McpError::Protocol(format!("init: {e}")))?
                }
                McpTransport::UnixSocket => {
                    #[cfg(unix)]
                    {
                        let transport = unix_socket_transport(&config)?;
                        handler
                            .serve_with_lifecycle(transport, client_lifecycle_mode())
                            .await
                            .map_err(|e| McpError::Protocol(format!("init: {e}")))?
                    }
                    #[cfg(not(unix))]
                    {
                        return Err(McpError::Protocol(
                            "UnixSocket transport is only supported on Unix platforms".to_string(),
                        ));
                    }
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
            subscriptions: PlRwLock::new(HashMap::new()),
            down: AtomicBool::new(false),
            down_reason: PlRwLock::new(None),
            images: None,
        })
    }

    /// Attach the images this server returns through `images`.
    pub fn with_images(mut self, images: ImageStore) -> Self {
        self.images = Some(images);
        self
    }

    fn mark_down(&self, reason: impl Into<String>) {
        self.down.store(true, Ordering::Relaxed);
        *self.down_reason.write() = Some(reason.into());
    }

    /// Cancel and await all active `subscriptions/listen` tasks.
    async fn cancel_all_subscriptions(&self) {
        let handles: Vec<SubscriptionHandle> = {
            let mut guard = self.subscriptions.write();
            guard.drain().map(|(_, h)| h).collect()
        };
        for handle in handles {
            handle.cancel.cancel();
            let _ = handle.task.await;
        }
    }

    /// Tear down the current service and spin up a fresh one.
    /// Bounded by `connect_timeout` like `connect()`.
    pub async fn reconnect(&self, connect_timeout: std::time::Duration) -> McpResult<()> {
        self.cancel_all_subscriptions().await;
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
                    handler
                        .serve_with_lifecycle(transport, client_lifecycle_mode())
                        .await
                        .map_err(|e| McpError::Protocol(format!("init: {e}")))?
                }
                McpTransport::StreamableHttp => {
                    let transport = http_transport(&self.config)?;
                    handler
                        .serve_with_lifecycle(transport, client_lifecycle_mode())
                        .await
                        .map_err(|e| McpError::Protocol(format!("init: {e}")))?
                }
                McpTransport::UnixSocket => {
                    #[cfg(unix)]
                    {
                        let transport = unix_socket_transport(&self.config)?;
                        handler
                            .serve_with_lifecycle(transport, client_lifecycle_mode())
                            .await
                            .map_err(|e| McpError::Protocol(format!("init: {e}")))?
                    }
                    #[cfg(not(unix))]
                    {
                        return Err(McpError::Protocol(
                            "UnixSocket transport is only supported on Unix platforms".to_string(),
                        ));
                    }
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
        let mut meta = RequestMetaObject::from(obj);
        // A server only reports progress against a token the *client* supplied,
        // so without this a 15-minute consult is silent by construction —
        // nothing in `on_progress` could have fired no matter how it was
        // written. The token is self-describing (`kaijutsu/<instance>/<nonce>`)
        // so a progress line identifies its own call without a correlation
        // table on our side.
        meta.set_progress_token(rmcp::model::ProgressToken(
            rmcp::model::NumberOrString::String(
                format!(
                    "kaijutsu/{}/{}",
                    self.instance_id,
                    uuid::Uuid::now_v7().simple()
                )
                .into(),
            ),
        ));
        meta
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
        // Per-call outcomes that say nothing about the instance's health.
        // Marking the whole server Down here would take out every other
        // in-flight and future call to it (e.g. all concurrent kaibo
        // consultations) because one call used a shape we don't drive.
        //
        // `UnexpectedResponse` is the live one: now that we negotiate
        // 2026-07-28, a server is permitted to answer `tools/call` with an
        // MRTR `InputRequiredResult` (SEP-2322) or a `CreateTaskResult`
        // (SEP-2663) — `rmcp` gates both on the peer's negotiated version
        // (`service/server.rs:68`). The raw `Peer` methods we call only match
        // the plain result, so those arrive here. That is "the server tried a
        // protocol feature we don't support", not "the transport is broken".
        ServiceError::UnexpectedResponse => {
            let _ = instance;
            McpError::Unsupported
        }
        ServiceError::InputRequiredRoundsExceeded { max_rounds } => {
            let _ = instance;
            McpError::Protocol(format!(
                "server kept requesting input beyond {max_rounds} MRTR rounds"
            ))
        }
        ServiceError::Timeout { timeout } => {
            // A slow call is not a sick server. This matters concretely: a
            // 5–15 minute kaibo consultation that overruns its `call_timeout`
            // used to take the whole instance Down, and `mark_down` has no
            // automatic recovery — `reconnect()` is never called on its own
            // ("Phase 1 does not invoke this automatically"), so a single slow
            // consult disabled kaibo until someone ran `kj mcp reload`.
            // Genuine unreachability still surfaces as a transport error below.
            let _ = instance;
            McpError::Protocol(format!("call timed out after {timeout:?}"))
        }
        ServiceError::Cancelled { reason } => {
            // We cancelled, or the call was cancelled for us. Not the
            // server's fault and not a health signal.
            let _ = instance;
            McpError::Protocol(format!(
                "call cancelled: {}",
                reason.as_deref().unwrap_or("<unknown>")
            ))
        }
        other => {
            let msg = other.to_string();
            mark_down(msg.clone());
            McpError::Protocol(msg)
        }
    }
}

/// Translate an MCP tool result for the kernel. An image is handed to
/// `attach`, which stores it for the model and answers with its CAS hash or
/// the reason it was refused; the result keeps one short line in its place,
/// never the base64. An image is an `image` content item, or an embedded
/// resource whose blob is an image: by its MIME type, or by its bytes when
/// it has none.
fn translate_result(
    result: CallToolResult,
    mut attach: impl FnMut(&str, &[u8]) -> Result<String, String>,
) -> KernelToolResult {
    let is_error = result.is_error.unwrap_or(false);
    let content = result
        .content
        .into_iter()
        .map(|c: ContentBlock| {
            if let Some(text) = c.as_text() {
                return ToolContent::Text(text.text.clone());
            }
            if let Some(image) = image_in(&c) {
                return ToolContent::Text(attach_line(image, &mut attach));
            }
            match serde_json::to_value(&c) {
                Ok(v) => ToolContent::Json(v),
                Err(_) => ToolContent::Text(String::from("<unserializable content>")),
            }
        })
        .collect();
    KernelToolResult {
        is_error,
        content,
        structured: result.structured_content,
    }
}

/// An image a content item carries: its declared MIME type, its base64
/// payload, and the resource URI it came under.
struct ImagePayload<'a> {
    mime: Option<&'a str>,
    data: &'a str,
    uri: Option<&'a str>,
}

/// The image in `content`, if it carries one. A blob with no MIME type counts
/// when its bytes are an attachable image; any other blob stays as it is.
fn image_in(content: &ContentBlock) -> Option<ImagePayload<'_>> {
    if let Some(image) = content.as_image() {
        return Some(ImagePayload { mime: Some(&image.mime_type), data: &image.data, uri: None });
    }
    let ResourceContents::BlobResourceContents { uri, mime_type, blob, .. } = &content.as_resource()?.resource else {
        return None;
    };
    let is_image = match mime_type.as_deref() {
        Some(mime) => mime.starts_with("image/"),
        None => decode_head(blob).is_some_and(|head| sniffed_attachable(&head).is_some()),
    };
    is_image.then_some(ImagePayload { mime: mime_type.as_deref(), data: blob, uri: Some(uri) })
}

/// The MIME type of `bytes` when they are an image a model can be shown.
fn sniffed_attachable(bytes: &[u8]) -> Option<&'static str> {
    let mime = crate::kj::cas::sniff_image_format(bytes)?.mime();
    super::block::ATTACHABLE_IMAGE_TYPES.contains(&mime).then_some(mime)
}

/// The first bytes of base64 `data`, enough to sniff an image format.
fn decode_head(data: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    // 88 characters are 66 bytes, past `kj::cas::SNIFF_LEN`.
    let head: String = data.chars().filter(|c| !c.is_ascii_whitespace()).take(88).collect();
    let usable = head.len() / 4 * 4;
    base64::engine::general_purpose::STANDARD.decode(&head[..usable]).ok()
}

/// The decoded size of base64 `data`, an upper bound read without decoding.
fn decoded_size_bound(data: &str) -> usize {
    data.bytes().filter(|b| !b.is_ascii_whitespace()).count().div_ceil(4) * 3
}

/// Base64 as MCP carries it; line breaks and other whitespace are ignored.
fn decode_base64(data: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::Engine as _;
    let compact: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    base64::engine::general_purpose::STANDARD.decode(compact)
}

/// The line a tool result keeps for `image`: where it was attached, or why
/// it was not.
fn attach_line(image: ImagePayload<'_>, attach: &mut impl FnMut(&str, &[u8]) -> Result<String, String>) -> String {
    // The URI is the server's choice: a `data:` URI or a long one would
    // carry the payload or flood the line, so only a short one is named.
    let from = image
        .uri
        .filter(|uri| uri.len() <= 120 && !uri.starts_with("data:"))
        .map(|uri| format!(" from {uri}"))
        .unwrap_or_default();
    let max = super::block::MAX_ATTACHED_IMAGE_BYTES;
    let bound = decoded_size_bound(image.data);
    if bound > max {
        let mime = image.mime.unwrap_or("image");
        return format!("[image {mime}{from} not attached: about {bound} bytes is over the {max} byte image limit]");
    }
    let bytes = match decode_base64(image.data) {
        Ok(bytes) => bytes,
        Err(e) => {
            let mime = image.mime.unwrap_or("image");
            return format!("[image {mime}{from} not attached: its data is not valid base64 ({e})]");
        }
    };
    let Some(mime) = image.mime.or_else(|| sniffed_attachable(&bytes)) else {
        return format!("[image{from} not attached: its bytes are not a recognized image]");
    };
    match attach(mime, &bytes) {
        Ok(hash) => format!("[image {mime}{from}, {} bytes, attached as {hash}; it follows this result]", bytes.len()),
        Err(reason) => format!("[image {mime}{from} not attached: {reason}]"),
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

        // Route through the shared classifier rather than marking down on any
        // error: `call_tool` used to treat every failure as an instance-health
        // signal, which is wrong for per-call outcomes (see
        // `map_rmcp_service_error`).
        let result = peer
            .call_tool(req)
            .await
            .map_err(|e| map_rmcp_service_error(e, &self.instance_id, |m| self.mark_down(m)))?;

        Ok(translate_result(result, |mime, bytes| match &self.images {
            Some(images) => images.attach(ctx.context_id, ctx.actor_id, mime, bytes),
            None => Err("this server has no image store".to_string()),
        }))
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

        if uses_subscriptions_listen(&peer) {
            {
                let guard = self.subscriptions.read();
                if let Some(existing) = guard.get(uri)
                    && !existing.task.is_finished()
                {
                    return Ok(());
                }
            }

            let filter = SubscriptionFilter::builder()
                .resource_subscription(uri)
                .build();
            let mut sub = peer.listen(filter).await.map_err(|e| {
                map_rmcp_service_error(e, &self.instance_id, |reason| {
                    self.mark_down(reason);
                })
            })?;

            let cancel = CancellationToken::new();
            let task_cancel = cancel.clone();
            let tx = self.notif_tx.clone();
            let uri_for_log = uri.to_string();
            let task = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = task_cancel.cancelled() => {
                            let _ = sub.cancel().await;
                            break;
                        }
                        item = sub.next() => {
                            match item {
                                Ok(Some(notif)) => forward_subscription_notification(&tx, notif),
                                Ok(None) => break,
                                Err(err) => {
                                    tracing::warn!(
                                        uri = %uri_for_log,
                                        "subscriptions/listen stream error: {err}"
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }
            });

            let displaced = self
                .subscriptions
                .write()
                .insert(uri.to_string(), SubscriptionHandle { cancel, task });
            if let Some(old) = displaced {
                old.cancel.cancel();
                let _ = old.task.await;
            }
            return Ok(());
        }

        // Legacy protocol versions (< 2026-07-28) use resources/subscribe.
        #[allow(deprecated)]
        {
            let params = SubscribeRequestParams::new(uri);
            peer.subscribe(params).await.map_err(|e| {
                map_rmcp_service_error(e, &self.instance_id, |reason| {
                    self.mark_down(reason);
                })
            })?;
        }
        Ok(())
    }

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

        if uses_subscriptions_listen(&peer) {
            let removed = self.subscriptions.write().remove(uri);
            if let Some(handle) = removed {
                handle.cancel.cancel();
                let _ = handle.task.await;
            }
            return Ok(());
        }

        // Legacy protocol versions (< 2026-07-28) use resources/unsubscribe.
        #[allow(deprecated)]
        {
            let params = UnsubscribeRequestParams::new(uri);
            peer.unsubscribe(params).await.map_err(|e| {
                map_rmcp_service_error(e, &self.instance_id, |reason| {
                    self.mark_down(reason);
                })
            })?;
        }
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
        self.cancel_all_subscriptions().await;
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
            subscriptions: PlRwLock::new(HashMap::new()),
            down: AtomicBool::new(false),
            down_reason: PlRwLock::new(None),
            images: None,
        }
    }

    const PNG_HEAD: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D];

    fn b64(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn texts(result: &KernelToolResult) -> Vec<String> {
        result
            .content
            .iter()
            .map(|c| match c {
                ToolContent::Text(s) => s.clone(),
                ToolContent::Json(v) => v.to_string(),
            })
            .collect()
    }

    /// Base64 wrapped at 76 columns, as some encoders emit it, decodes.
    #[test]
    fn wrapped_base64_attaches_the_same_bytes() {
        let encoded = b64(PNG_HEAD);
        let wrapped = format!("{}\n{}", &encoded[..8], &encoded[8..]);
        let mut got = Vec::new();
        let result = translate_result(
            CallToolResult::success(vec![ContentBlock::image(wrapped, "image/png")]),
            |mime, bytes| {
                got.push((mime.to_string(), bytes.to_vec()));
                Ok("abc".into())
            },
        );
        assert_eq!(got, vec![("image/png".to_string(), PNG_HEAD.to_vec())]);
        assert!(texts(&result)[0].contains("attached as abc"), "{:?}", texts(&result));
    }

    /// The resource URI is the server's to choose; a `data:` URI or a long
    /// one would put the payload back in the line, so the line names it only
    /// when it is short and not inline data.
    #[test]
    fn the_line_never_repeats_an_inline_or_long_uri() {
        let encoded = b64(PNG_HEAD);
        for uri in [format!("data:image/png;base64,{encoded}"), format!("file:///{}", "x".repeat(400))] {
            let blob = ContentBlock::resource(ResourceContents::blob(encoded.clone(), uri.clone()).with_mime_type("image/png"));
            let result = translate_result(CallToolResult::success(vec![blob]), |_, _| Ok("abc".into()));
            let line = texts(&result).join("\n");
            assert!(!line.contains(&uri) && line.len() < 300, "{line}");
            assert!(line.contains("attached as abc"), "{line}");
        }
    }

    /// An image whose base64 is longer than the largest attachable image is
    /// refused before it is decoded.
    #[test]
    fn an_oversized_image_is_refused_before_decoding() {
        let max = super::super::block::MAX_ATTACHED_IMAGE_BYTES;
        let huge = b64(PNG_HEAD) + &"A".repeat(max / 3 * 4);
        for content in [
            ContentBlock::image(huge.clone(), "image/png"),
            // Untyped: only its leading bytes say it is an image.
            ContentBlock::resource(ResourceContents::blob(huge.clone(), "file:///big")),
        ] {
            let result = translate_result(CallToolResult::success(vec![content]), |_, _| {
                panic!("an oversized image is never attached")
            });
            let line = texts(&result).join("\n");
            assert!(line.contains("not attached") && line.contains("image limit") && line.len() < 300, "{line}");
        }
    }

    /// A blob that is not an image keeps its old shape and reaches no attach.
    #[test]
    fn a_blob_that_is_not_an_image_is_left_alone() {
        let pdf = ContentBlock::resource(
            ResourceContents::blob(b64(b"%PDF-1.7"), "file:///a.pdf").with_mime_type("application/pdf"),
        );
        let untyped_text = ContentBlock::resource(ResourceContents::blob(b64(b"plain words"), "file:///notes"));
        let result = translate_result(CallToolResult::success(vec![pdf, untyped_text]), |_, _| {
            panic!("only an image is attached")
        });
        assert!(result.content.iter().all(|c| matches!(c, ToolContent::Json(_))), "{:?}", texts(&result));
    }

    /// Pins the client half of the 2026-07-28 fix against the newest version in
    /// `ProtocolVersion::KNOWN_VERSIONS`, so any future rmcp version bump
    /// catches an outdated pinned literal.
    #[test]
    fn client_advertises_the_newest_protocol_rmcp_knows() {
        let (tx, _) = broadcast::channel(4);
        let handler = BrokerClientHandler::new(tx);
        let advertised = handler.get_info().protocol_version;
        let newest = ProtocolVersion::KNOWN_VERSIONS
            .last()
            .expect("rmcp KNOWN_VERSIONS must be non-empty");

        assert_eq!(
            advertised,
            ProtocolVersion::V_2026_07_28,
            "client must advertise 2026-07-28"
        );
        assert_eq!(
            &advertised,
            newest,
            "client protocol_version must match the newest version in rmcp's KNOWN_VERSIONS"
        );
    }

    /// A per-call protocol outcome must not condemn the whole instance.
    /// Now that we negotiate 2026-07-28 a server may answer `tools/call` with
    /// an MRTR `InputRequiredResult` or a task result; the raw `Peer` methods
    /// surface those as `UnexpectedResponse`. Marking down on that would take
    /// out every other call to the server, and `mark_down` has no automatic
    /// recovery — it stays Down until `kj mcp reload`.
    #[test]
    fn per_call_outcomes_do_not_mark_the_instance_down() {
        use rmcp::service::ServiceError;

        let instance = InstanceId::new("test.ext");
        for err in [
            ServiceError::UnexpectedResponse,
            ServiceError::InputRequiredRoundsExceeded { max_rounds: 10 },
            ServiceError::Cancelled { reason: None },
            ServiceError::Timeout {
                timeout: std::time::Duration::from_secs(900),
            },
        ] {
            let label = err.to_string();
            let mut marked = false;
            let _ = map_rmcp_service_error(err, &instance, |_| marked = true);
            assert!(
                !marked,
                "{label} must not mark the instance Down — it is a per-call \
                 outcome, not a health signal"
            );
        }
    }

    /// The counterweight to the test above: a genuinely broken transport
    /// still has to mark the instance Down, or the health model is decorative.
    #[test]
    fn transport_failure_still_marks_the_instance_down() {
        use rmcp::service::ServiceError;

        let instance = InstanceId::new("test.ext");
        let mut marked = false;
        let _ = map_rmcp_service_error(ServiceError::TransportClosed, &instance, |_| {
            marked = true
        });
        assert!(marked, "a closed transport is a real health signal");
    }

    /// The load-bearing half of progress support. A server only reports
    /// against a `progressToken` the client supplied, so if this stops being
    /// set, `on_progress` goes silent and no test of the handler would notice
    /// — the notification simply never arrives. Assert the token exists and is
    /// self-describing, since a bare uuid in a log is useless without a
    /// correlation table we do not keep.
    #[test]
    fn requests_carry_a_self_describing_progress_token() {
        let server = fake_server("test.ext");
        let ctx = CallContext {
            publishes_pair: false,
            tool_call: None,
            principal_id: PrincipalId::new(),
            actor_id: PrincipalId::new(),
            reviewer_id: None,
            context_id: ContextId::new(),
            session_id: SessionId::new(),
            kernel_id: KernelId::new(),
            cwd: None,
            trace: TraceContext {
                traceparent: String::new(),
                tracestate: String::new(),
            },
        };

        let meta = server.build_meta(&ctx);
        let token = meta
            .get_progress_token()
            .expect("requests must carry a progressToken or progress is impossible");
        let token = match token.0 {
            rmcp::model::NumberOrString::String(s) => s.to_string(),
            rmcp::model::NumberOrString::Number(n) => n.to_string(),
        };
        assert!(
            token.starts_with("kaijutsu/test.ext/"),
            "token should name its instance so a progress log is self-locating, got {token}"
        );

        // Two calls must not collide, or progress lines from concurrent
        // consults interleave under one identity.
        let token2 = match server.build_meta(&ctx).get_progress_token().expect("token").0 {
            rmcp::model::NumberOrString::String(s) => s.to_string(),
            rmcp::model::NumberOrString::Number(n) => n.to_string(),
        };
        assert_ne!(token, token2, "progress tokens must be per-call");
    }

    #[test]
    fn meta_carries_kaijutsu_v1_fields() {
        let server = fake_server("test.ext");
        let ctx = CallContext {
            publishes_pair: false,
            tool_call: None,
            principal_id: PrincipalId::new(),
            actor_id: PrincipalId::new(),
            reviewer_id: None,
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
    // `PR_SET_PDEATHSIG(SIGKILL)` in `pre_exec`.
    #[test]
    fn http_transport_rejects_an_unusable_header_name() {
        let config = McpServerConfig {
            name: "exa".into(),
            transport: McpTransport::StreamableHttp,
            url: Some("https://mcp.exa.ai/mcp".into()),
            headers: HashMap::from([("x api key".to_string(), "secret".to_string())]),
            ..Default::default()
        };
        let err = http_transport(&config).err().expect("a space is not a header name");
        assert!(format!("{err:?}").contains("x api key"), "{err:?}");
    }

    /// The name is safe to print; the value is a credential and is not.
    #[test]
    fn an_unusable_header_value_is_reported_without_quoting_it() {
        let config = McpServerConfig {
            name: "exa".into(),
            transport: McpTransport::StreamableHttp,
            url: Some("https://mcp.exa.ai/mcp".into()),
            // A newline cannot be sent in a header value.
            headers: HashMap::from([("x-api-key".to_string(), "sk-leak\nX: y".to_string())]),
            ..Default::default()
        };
        let err = http_transport(&config).err().expect("a newline is not a header value");
        let rendered = format!("{err:?}");
        assert!(rendered.contains("x-api-key"), "{rendered}");
        assert!(!rendered.contains("sk-leak"), "the error leaked the credential: {rendered}");
    }

    #[test]
    fn http_headers_default_the_user_agent_to_kaijutsu() {
        let config = McpServerConfig {
            name: "exa".into(),
            transport: McpTransport::StreamableHttp,
            url: Some("https://mcp.exa.ai/mcp".into()),
            headers: HashMap::from([("x-api-key".to_string(), "secret".to_string())]),
            ..Default::default()
        };
        let headers = http_headers(&config).unwrap();
        assert_eq!(
            headers.get(&http::header::USER_AGENT).and_then(|v| v.to_str().ok()),
            Some(crate::llm::http_user_agent()),
        );
        assert_eq!(headers.len(), 2, "the configured header is kept: {headers:?}");
    }

    /// A configured `User-Agent` is the operator's statement; keep it.
    #[test]
    fn a_configured_user_agent_wins_over_the_default() {
        let config = McpServerConfig {
            name: "exa".into(),
            transport: McpTransport::StreamableHttp,
            url: Some("https://mcp.exa.ai/mcp".into()),
            headers: HashMap::from([("User-Agent".to_string(), "custom/1".to_string())]),
            ..Default::default()
        };
        let headers = http_headers(&config).unwrap();
        assert_eq!(
            headers.get(&http::header::USER_AGENT).and_then(|v| v.to_str().ok()),
            Some("custom/1"),
        );
    }

    #[test]
    fn http_transport_requires_a_url() {
        let config = McpServerConfig {
            name: "exa".into(),
            transport: McpTransport::StreamableHttp,
            ..Default::default()
        };
        assert!(http_transport(&config).is_err());
    }

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

    // ---- 2026-07-28 subscriptions/listen --------------------------------

    #[derive(Clone)]
    struct SubscriptionTestServer {
        update_tx: tokio::sync::broadcast::Sender<String>,
        cancelled_tx: std::sync::Arc<tokio::sync::Notify>,
    }

    impl rmcp::ServerHandler for SubscriptionTestServer {
        fn get_info(&self) -> rmcp::model::ServerConfig {
            let mut info = rmcp::model::ServerConfig::default();
            info.protocol_version = ProtocolVersion::V_2026_07_28;
            info.capabilities = rmcp::model::ServerCapabilities::builder()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_resources_list_changed()
                .build();
            info
        }

        fn accepted_subscription_filter(
            &self,
            requested: &rmcp::model::SubscriptionFilter,
        ) -> Option<rmcp::model::SubscriptionFilter> {
            Some(requested.clone())
        }

        fn listen(
            &self,
            context: rmcp::service::SubscriptionContext,
        ) -> impl std::future::Future<Output = Result<(), rmcp::ErrorData>> + Send + '_ {
            let mut rx = self.update_tx.subscribe();
            let cancelled = self.cancelled_tx.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = context.cancelled() => {
                            cancelled.notify_one();
                            break;
                        }
                        Ok(uri) = rx.recv() => {
                            let _ = context.sink().notify_resource_updated(uri).await;
                        }
                    }
                }
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn subscribe_uses_subscriptions_listen_on_2026_07_28() {
        let (update_tx, _) = tokio::sync::broadcast::channel::<String>(16);
        let cancelled = std::sync::Arc::new(tokio::sync::Notify::new());
        let server_handler = SubscriptionTestServer {
            update_tx: update_tx.clone(),
            cancelled_tx: cancelled.clone(),
        };

        let (client_io, server_io) = tokio::io::duplex(65536);
        let server_task = tokio::spawn(async move {
            let running = rmcp::serve_server(server_handler, server_io)
                .await
                .expect("serve_server");
            let _ = running.waiting().await;
        });

        let (notif_tx, _) = broadcast::channel(64);
        let handler = BrokerClientHandler::new(notif_tx.clone());
        let running_client = handler
            .serve_with_lifecycle(client_io, client_lifecycle_mode())
            .await
            .expect("serve_with_lifecycle");

        let external = ExternalMcpServer {
            instance_id: InstanceId::new("test.sub"),
            config: McpServerConfig {
                name: "sub".into(),
                ..Default::default()
            },
            service: PlRwLock::new(Some(running_client)),
            tools_cache: PlRwLock::new(Vec::new()),
            notif_tx,
            subscriptions: PlRwLock::new(HashMap::new()),
            down: AtomicBool::new(false),
            down_reason: PlRwLock::new(None),
            images: None,
        };

        let mut rx = external.notifications();
        let ctx = CallContext::test();
        external
            .subscribe("kaijutsu://contexts/abc/drift", &ctx)
            .await
            .expect("subscribe must succeed over subscriptions/listen on 2026-07-28");

        update_tx
            .send("kaijutsu://contexts/abc/drift".to_string())
            .expect("send update");

        let notif = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for ResourceUpdated")
            .expect("broadcast recv");
        match notif {
            ServerNotification::ResourceUpdated { uri } => {
                assert_eq!(uri, "kaijutsu://contexts/abc/drift");
            }
            other => panic!("expected ResourceUpdated, got {other:?}"),
        }

        external
            .unsubscribe("kaijutsu://contexts/abc/drift", &ctx)
            .await
            .expect("unsubscribe must cancel listen stream");

        tokio::time::timeout(std::time::Duration::from_secs(2), cancelled.notified())
            .await
            .expect("server must observe cancellation after unsubscribe");

        external.shutdown().await.expect("shutdown");
        server_task.abort();
    }

    #[derive(Clone)]
    struct LegacySubscriptionServer {
        subscribed: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
        unsubscribed: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
    }

    impl rmcp::ServerHandler for LegacySubscriptionServer {
        fn get_info(&self) -> rmcp::model::ServerConfig {
            let mut info = rmcp::model::ServerConfig::default();
            info.protocol_version = ProtocolVersion::V_2025_11_25;
            info.capabilities = rmcp::model::ServerCapabilities::builder()
                .enable_resources()
                .enable_resources_subscribe()
                .build();
            info
        }

        #[allow(deprecated)]
        fn subscribe(
            &self,
            request: SubscribeRequestParams,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> impl std::future::Future<Output = Result<(), rmcp::ErrorData>> + Send + '_ {
            self.subscribed.lock().push(request.uri);
            std::future::ready(Ok(()))
        }

        #[allow(deprecated)]
        fn unsubscribe(
            &self,
            request: UnsubscribeRequestParams,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> impl std::future::Future<Output = Result<(), rmcp::ErrorData>> + Send + '_ {
            self.unsubscribed.lock().push(request.uri);
            std::future::ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn subscribe_uses_legacy_resources_subscribe_before_2026_07_28() {
        let subscribed = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let unsubscribed = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let server_handler = LegacySubscriptionServer {
            subscribed: subscribed.clone(),
            unsubscribed: unsubscribed.clone(),
        };

        let (client_io, server_io) = tokio::io::duplex(65536);
        let server_task = tokio::spawn(async move {
            let running = rmcp::serve_server(server_handler, server_io)
                .await
                .expect("serve_server");
            let _ = running.waiting().await;
        });

        let (notif_tx, _) = broadcast::channel(64);
        let handler = BrokerClientHandler::new(notif_tx.clone());
        let running_client = handler
            .serve_with_lifecycle(client_io, ClientLifecycleMode::Initialize)
            .await
            .expect("legacy initialize");

        let external = ExternalMcpServer {
            instance_id: InstanceId::new("test.legacy_sub"),
            config: McpServerConfig {
                name: "legacy_sub".into(),
                ..Default::default()
            },
            service: PlRwLock::new(Some(running_client)),
            tools_cache: PlRwLock::new(Vec::new()),
            notif_tx,
            subscriptions: PlRwLock::new(HashMap::new()),
            down: AtomicBool::new(false),
            down_reason: PlRwLock::new(None),
            images: None,
        };

        let ctx = CallContext::test();
        external
            .subscribe("file:///legacy", &ctx)
            .await
            .expect("legacy subscribe must succeed");
        external
            .unsubscribe("file:///legacy", &ctx)
            .await
            .expect("legacy unsubscribe must succeed");

        assert_eq!(subscribed.lock().as_slice(), &["file:///legacy"]);
        assert_eq!(unsubscribed.lock().as_slice(), &["file:///legacy"]);

        external.shutdown().await.expect("shutdown");
        server_task.abort();
    }

    // ---- UnixSocket transport -------------------------------------------

    #[cfg(unix)]
    #[test]
    fn unix_socket_transport_requires_a_socket_path() {
        let missing = McpServerConfig {
            name: "uds".into(),
            transport: McpTransport::UnixSocket,
            ..Default::default()
        };
        assert!(unix_socket_transport(&missing).is_err());

        let bare_at = McpServerConfig {
            name: "uds".into(),
            transport: McpTransport::UnixSocket,
            socket: Some("@".into()),
            ..Default::default()
        };
        assert!(unix_socket_transport(&bare_at).is_err());
    }

    #[cfg(unix)]
    #[derive(Clone, Default)]
    struct UnixEchoServer;

    #[cfg(unix)]
    impl rmcp::ServerHandler for UnixEchoServer {
        fn get_info(&self) -> rmcp::model::ServerConfig {
            let mut info = rmcp::model::ServerConfig::default();
            info.protocol_version = ProtocolVersion::V_2026_07_28;
            info.capabilities = rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build();
            info
        }

        fn list_tools(
            &self,
            _request: Option<rmcp::model::PaginatedRequestParams>,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> impl std::future::Future<Output = Result<rmcp::model::ListToolsResult, rmcp::ErrorData>>
        + Send
        + '_ {
            let schema = serde_json::json!({
                "type": "object",
                "properties": {
                    "msg": { "type": "string" }
                },
                "required": ["msg"]
            });
            let schema_map = schema.as_object().cloned().unwrap();
            let tool = rmcp::model::Tool::new("echo", "Echo back msg", schema_map);
            std::future::ready(Ok(rmcp::model::ListToolsResult::with_all_items(vec![tool])))
        }

        fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> impl std::future::Future<Output = Result<rmcp::model::CallToolResponse, rmcp::ErrorData>>
        + Send
        + '_ {
            let msg = request
                .arguments
                .as_ref()
                .and_then(|a| a.get("msg"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            std::future::ready(Ok(CallToolResult::success(vec![ContentBlock::text(
                format!("echo:{msg}"),
            )])
            .into()))
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_connect_lists_and_calls_tools() {
        use rmcp::transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("mcp.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).expect("bind unix socket");

        let seen_header = std::sync::Arc::new(parking_lot::Mutex::new(None::<String>));
        let seen_header_srv = seen_header.clone();

        let mcp_service = std::sync::Arc::new(StreamableHttpService::new(
            || Ok(UnixEchoServer),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default(),
        ));

        let server_task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let svc = mcp_service.clone();
                let header_slot = seen_header_srv.clone();
                tokio::spawn(async move {
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let hyper_svc = hyper::service::service_fn(
                        move |req: hyper::Request<hyper::body::Incoming>| {
                            let svc = svc.clone();
                            let header_slot = header_slot.clone();
                            async move {
                                if let Some(v) = req.headers().get("x-test-client")
                                    && let Ok(s) = v.to_str()
                                {
                                    *header_slot.lock() = Some(s.to_string());
                                }
                                Ok::<_, std::convert::Infallible>(svc.handle(req).await)
                            }
                        },
                    );
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, hyper_svc)
                        .await;
                });
            }
        });

        let config = McpServerConfig {
            name: "unix-echo".into(),
            transport: McpTransport::UnixSocket,
            socket: Some(sock_path.to_str().unwrap().to_string()),
            headers: HashMap::from([("x-test-client".to_string(), "kaijutsu-test".to_string())]),
            ..Default::default()
        };

        let external = ExternalMcpServer::connect(
            config,
            InstanceId::new("external.unix_echo"),
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("connect over UnixSocket");

        let ctx = CallContext::test();
        let tools = external.list_tools(&ctx).await.expect("list_tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        let result = external
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new("external.unix_echo"),
                    tool: "echo".to_string(),
                    arguments: serde_json::json!({ "msg": "hello-uds" }),
                },
                &ctx,
                CancellationToken::new(),
            )
            .await
            .expect("call_tool over UnixSocket");
        assert!(!result.is_error);
        assert_eq!(texts(&result), vec!["echo:hello-uds"]);
        assert_eq!(seen_header.lock().as_deref(), Some("kaijutsu-test"));

        external.shutdown().await.expect("shutdown");
        server_task.abort();
    }
}
