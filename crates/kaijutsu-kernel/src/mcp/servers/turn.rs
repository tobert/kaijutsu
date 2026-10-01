//! `BuiltinTurnServer` — the `done` tool a context calls to end its task.
//!
//! The tool records the model's verdict; the turn loop gives it its effect
//! (`runtime/llm_stream.rs`, `DONE_TOOL`). A context that is offered `done`
//! ends its turn by calling it: a successful call ends the turn after its
//! batch, and a reply with no tool call is answered with a notice instead of
//! ending the turn. The call and its result are ordinary durable blocks, so a
//! driver reading the context (`kj wait`, an ACP client) reads the verdict
//! there.
//!
//! `*` does not cover this instance: a context type opts in with
//! `kj binding allow builtin.turn`, because offering `done` changes how every
//! turn in the context ends.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use super::super::schema::tool_input_schema;
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{InstanceId, KernelCallParams, KernelTool, KernelToolResult};

/// How the task ended.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DoneStatus {
    /// The task is complete and checked.
    Done,
    /// The task needs something only someone else can give: an answer, an
    /// approval, or access.
    Blocked,
    /// The task cannot be completed; the summary says why.
    GaveUp,
}

impl DoneStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::GaveUp => "gave_up",
        }
    }
}

/// End the task and report how it ended.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DoneParams {
    /// `done` when the task is complete and checked, `blocked` when it needs
    /// something only someone else can give, `gave_up` when it cannot be
    /// completed.
    pub status: DoneStatus,
    /// What happened, for whoever reads the result: what changed and how it
    /// was checked, what is needed, or why it stopped.
    pub summary: String,
}

pub struct BuiltinTurnServer {
    instance_id: InstanceId,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl BuiltinTurnServer {
    pub const INSTANCE: &'static str = "builtin.turn";

    pub fn new() -> Self {
        let (notif_tx, _) = broadcast::channel(1);
        Self { instance_id: InstanceId::new(Self::INSTANCE), notif_tx }
    }
}

impl Default for BuiltinTurnServer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl McpServerLike for BuiltinTurnServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        Ok(vec![KernelTool {
            instance: self.instance_id.clone(),
            name: crate::runtime::llm_stream::DONE_TOOL.to_string(),
            description: Some(
                "End the task and report how it ended. Call it when the task is complete and \
                 checked, when you are blocked on something only someone else can give, or when \
                 you are giving up. The turn ends after this call; a reply without a tool call \
                 does not end it."
                    .to_string(),
            ),
            input_schema: tool_input_schema::<DoneParams>(),
        }])
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        _ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        if params.tool != crate::runtime::llm_stream::DONE_TOOL {
            return Err(McpError::ToolNotFound { instance: self.instance_id.clone(), tool: params.tool });
        }
        let done: DoneParams = serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
        if done.summary.trim().is_empty() {
            return Ok(KernelToolResult::error_text("done: the summary is empty; say what happened"));
        }
        Ok(KernelToolResult::text(format!("{}: {}", done.status.as_str(), done.summary.trim())))
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(args: serde_json::Value) -> KernelCallParams {
        KernelCallParams { instance: InstanceId::new(BuiltinTurnServer::INSTANCE), tool: "done".into(), arguments: args }
    }

    #[tokio::test]
    async fn done_reports_its_status_and_summary() {
        let server = BuiltinTurnServer::new();
        let ctx = CallContext::test();
        let result = server.call_tool(call(serde_json::json!({"status": "gave_up", "summary": "no network"})),
            &ctx, CancellationToken::new()).await.unwrap();
        assert!(!result.is_error);
        assert!(matches!(result.content.as_slice(), [crate::mcp::ToolContent::Text(t)] if t == "gave_up: no network"),
            "{:?}", result.content);
    }

    #[tokio::test]
    async fn done_refuses_an_unknown_status_and_an_empty_summary() {
        let server = BuiltinTurnServer::new();
        let ctx = CallContext::test();
        assert!(server.call_tool(call(serde_json::json!({"status": "finished", "summary": "x"})),
            &ctx, CancellationToken::new()).await.is_err());
        let empty = server.call_tool(call(serde_json::json!({"status": "done", "summary": "  "})),
            &ctx, CancellationToken::new()).await.unwrap();
        assert!(empty.is_error);
    }
}
