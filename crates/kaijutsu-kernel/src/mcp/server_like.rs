//! `McpServerLike` is the common interface for tool sources.
//!
//! Virtual in-process servers (`BlockToolsServer`, `FileToolsServer`, …) and
//! external rmcp subprocesses both present this surface. The broker treats
//! them interchangeably.

use async_trait::async_trait;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::context::CallContext;
use super::error::{McpError, McpResult};
use super::types::{
    ElicitationRequest, Health, InstanceId, KernelCallParams, KernelReadResource, KernelResourceList,
    KernelTool, KernelToolResult, LogLevel,
};

/// A server notification consumed by the broker's per-instance subscriber.
#[derive(Clone, Debug)]
pub enum ServerNotification {
    ToolsChanged,
    ResourceUpdated { uri: String },
    PromptsChanged,
    Log {
        level: LogLevel,
        message: String,
        tool: Option<String>,
    },
    Elicitation(ElicitationRequest),
    /// Progress against a long-running call we tagged with a `progressToken`.
    /// `token` is the self-describing string we minted in `build_meta`
    /// (`kaijutsu/<instance>/<nonce>`), so a log line identifies its own call
    /// without a correlation table.
    Progress {
        token: String,
        progress: f64,
        total: Option<f64>,
        message: Option<String>,
    },
}

/// Where result hooks run after tool admission. Execution-owned servers retain
/// commands beyond their initial reply and apply hooks to their actual outcome.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ResultHookOwner {
    Broker,
    Execution,
}

/// Uniform tool interface.
///
/// Resource and prompt methods default to `McpError::Unsupported`; servers
/// override as needed (block tools don't expose resources yet, external
/// servers do).
#[async_trait]
pub trait McpServerLike: Send + Sync + 'static {
    fn instance_id(&self) -> &InstanceId;

    fn result_hook_owner(&self) -> ResultHookOwner { ResultHookOwner::Broker }

    /// List tools visible to `ctx`. Builtins typically ignore the context;
    /// external servers may filter based on `_meta`.
    async fn list_tools(&self, ctx: &CallContext) -> McpResult<Vec<KernelTool>>;

    /// Execute a single tool call. `cancel` carries the caller's cancellation;
    /// retained operations transfer control to their operation receipt.
    async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        cancel: CancellationToken,
    ) -> McpResult<KernelToolResult>;

    /// Subscribe to server notifications. Registration starts the broker's
    /// subscriber; unregistering the instance stops it.
    fn notifications(&self) -> broadcast::Receiver<ServerNotification>;

    /// List resources this server advertises. Default is
    /// `Unsupported`; servers that expose resources override.
    async fn list_resources(&self, _ctx: &CallContext) -> McpResult<KernelResourceList> {
        Err(McpError::Unsupported)
    }

    /// Read a single resource by URI.
    async fn read_resource(
        &self,
        _uri: &str,
        _ctx: &CallContext,
    ) -> McpResult<KernelReadResource> {
        Err(McpError::Unsupported)
    }

    /// Subscribe to update notifications for a resource URI.
    /// Idempotent at the caller's layer; the broker tracks per-context
    /// subscription state and calls `unsubscribe` on binding drop (D-44).
    async fn subscribe(&self, _uri: &str, _ctx: &CallContext) -> McpResult<()> {
        Err(McpError::Unsupported)
    }

    /// Tear down a subscription previously created via `subscribe`.
    async fn unsubscribe(&self, _uri: &str, _ctx: &CallContext) -> McpResult<()> {
        Err(McpError::Unsupported)
    }

    async fn health(&self) -> Health {
        Health::Ready
    }

    async fn shutdown(&self) -> McpResult<()> {
        Ok(())
    }
}
