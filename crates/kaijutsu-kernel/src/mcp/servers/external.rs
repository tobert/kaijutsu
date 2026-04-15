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
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo, Content, LoggingLevel,
    LoggingMessageNotificationParam, Meta, ProgressNotificationParam,
};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientHandler, RoleClient};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use std::collections::HashMap;

use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{
    Health, InstanceId, KernelCallParams, KernelTool, KernelToolResult, LogLevel, ToolContent,
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
#[derive(Clone, Debug, Default)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<String>,
    pub transport: McpTransport,
    pub url: Option<String>,
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
    fn new(tx: broadcast::Sender<ServerNotification>) -> Self {
        let mut info = ClientInfo::default();
        info.client_info.name = "kaijutsu".into();
        info.client_info.version = env!("CARGO_PKG_VERSION").into();
        info.capabilities = ClientCapabilities::builder()
            .enable_roots()
            .enable_roots_list_changed()
            .build();
        Self { info, tx }
    }
}

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
    pub async fn connect(config: McpServerConfig, instance_id: InstanceId) -> McpResult<Self> {
        let (notif_tx, _) = broadcast::channel(256);
        let handler = BrokerClientHandler::new(notif_tx.clone());

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
                let transport = TokioChildProcess::new(cmd.configure(|_| {}))
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

        // Discover tools.
        let tools = service
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| McpError::Protocol(format!("list_tools: {e}")))?;

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
    pub async fn reconnect(&self) -> McpResult<()> {
        // Drop the old service first so the subprocess fully exits.
        let _ = self.service.write().take();

        let handler = BrokerClientHandler::new(self.notif_tx.clone());
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
                let transport = TokioChildProcess::new(cmd.configure(|_| {}))
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

        // Refresh tool cache — list_changed may have fired during the
        // outage.
        let tools = new_service
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| McpError::Protocol(format!("list_tools: {e}")))?;
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

    fn build_meta(&self, ctx: &CallContext) -> Meta {
        // Meta is a newtype over JsonObject; populate the three kaijutsu
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
        Meta(obj)
    }
}

fn translate_result(result: CallToolResult) -> KernelToolResult {
    let is_error = result.is_error.unwrap_or(false);
    let content = result
        .content
        .into_iter()
        .map(|c: Content| match c.as_text() {
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
}
