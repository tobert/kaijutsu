//! `McpServerLike` — the one trait every tool source implements (§4.1, D-01).
//!
//! Virtual in-process servers (`BlockToolsServer`, `FileToolsServer`, …) and
//! external rmcp subprocesses both present this surface. The broker treats
//! them interchangeably.

use async_trait::async_trait;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::context::CallContext;
use super::error::McpResult;
use super::types::{
    ElicitationRequest, Health, InstanceId, KernelCallParams, KernelTool, KernelToolResult,
    LogLevel,
};

/// Fan-out notification from a server instance.
///
/// `Elicitation` is reserved per D-25; no emitter in Phase 1. The coalescer
/// (§5.3) subscribes to these streams in Phase 2.
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
}

/// Uniform tool interface (§4.1).
///
/// Resource and prompt methods default to `McpError::Unsupported`; servers
/// override as needed (block tools don't expose resources yet, external
/// servers do).
#[async_trait]
pub trait McpServerLike: Send + Sync + 'static {
    fn instance_id(&self) -> &InstanceId;

    /// List tools visible to `ctx`. Builtins typically ignore the context;
    /// external servers may filter based on `_meta`.
    async fn list_tools(&self, ctx: &CallContext) -> McpResult<Vec<KernelTool>>;

    /// Execute a single tool call. `cancel` is currently plumbed but unused
    /// by most Phase 1 servers — cancellation wiring is a follow-up (§9).
    async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        cancel: CancellationToken,
    ) -> McpResult<KernelToolResult>;

    /// Subscribe to notifications this server emits. Returning a receiver
    /// does NOT guarantee anything subscribes; in Phase 1 the broker creates
    /// these receivers but nothing reads them (D-32).
    fn notifications(&self) -> broadcast::Receiver<ServerNotification>;

    async fn health(&self) -> Health {
        Health::Ready
    }

    async fn shutdown(&self) -> McpResult<()> {
        Ok(())
    }
}
