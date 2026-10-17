//! `BuiltinTurnServer` — the `done` tool a context calls to end its task.
//!
//! The tool records the model's verdict; the turn loop gives it its effect
//! (`runtime/llm_stream.rs`, `DONE_TOOL`). A context that is offered `done`
//! ends its turn by calling it: a successful call ends the turn after its
//! batch. In an autonomous turn, or in any turn on a headless kernel
//! (`KernelMode::Headless`), a reply with no tool call is answered with a
//! notice instead of ending the turn; a turn a person prompted on an
//! interactive kernel ends on that reply. The call and its result are ordinary durable blocks, so a
//! driver reading the context (`kj wait`, an ACP client) reads the verdict
//! there.
//!
//! In the `SystemMessagesTool` form (`llm_stream::INSTRUCTION_FORM`) the
//! instance also offers `get_system_messages`. The kernel's notice after a
//! text-only reply is recorded as a call to it and its result, so the model
//! reads the notice as runtime output rather than as a user message. Every
//! context offered `done` is offered it on every turn, so its tools stay the
//! same from turn to turn. A call the model makes itself returns no messages.
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
    /// The task cannot be completed; the feedback says why.
    GaveUp,
    /// The task as given is unsafe, or too ambiguous to do safely without
    /// guessing; the feedback says what is missing.
    Refused,
}

impl DoneStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::GaveUp => "gave_up",
            Self::Refused => "refused",
        }
    }
}

/// End the task and report how it ended.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DoneParams {
    /// `done` when the task is complete and checked, `blocked` when it needs
    /// something only someone else can give, `gave_up` when it cannot be
    /// completed, `refused` when the task as given is unsafe or too
    /// ambiguous to do safely without guessing.
    pub status: DoneStatus,
    /// "And remember, this is for posterity, so be honest. How do you
    /// feel?" Free-form, about 50 to 400 words.
    pub feedback: String,
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
        let mut tools = vec![KernelTool {
            instance: self.instance_id.clone(),
            name: crate::runtime::llm_stream::DONE_TOOL.to_string(),
            description: Some(
                "End the task and report how it ended. Call it when the task is complete and \
                 checked, when you are blocked on something only someone else can give, when \
                 you are giving up, or when you refuse a task that is unsafe or too ambiguous to \
                 do safely. The turn ends after this call. When you work without a person \
                 answering, a reply without a tool call does not end it."
                    .to_string(),
            ),
            input_schema: tool_input_schema::<DoneParams>(),
        }];
        if crate::runtime::llm_stream::INSTRUCTION_FORM == crate::runtime::llm_stream::InstructionForm::SystemMessagesTool {
            tools.push(KernelTool {
                instance: self.instance_id.clone(),
                name: crate::runtime::llm_stream::SYSTEM_MESSAGES_TOOL.to_string(),
                description: Some("Returns messages from the kaijutsu runtime about this task.".to_string()),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            });
        }
        Ok(tools)
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        _ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        if params.tool == crate::runtime::llm_stream::SYSTEM_MESSAGES_TOOL {
            return Ok(KernelToolResult::text(crate::runtime::llm_stream::system_messages_result(&[])));
        }
        if params.tool != crate::runtime::llm_stream::DONE_TOOL {
            return Err(McpError::ToolNotFound { instance: self.instance_id.clone(), tool: params.tool });
        }
        let done: DoneParams = serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
        if done.feedback.trim().is_empty() {
            return Ok(KernelToolResult::error_text("done: the feedback is empty; say what happened"));
        }
        Ok(KernelToolResult::text(format!("{}: {}", done.status.as_str(), done.feedback.trim())))
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
    async fn done_reports_its_status_and_feedback() {
        let server = BuiltinTurnServer::new();
        let ctx = CallContext::test();
        let result = server.call_tool(call(serde_json::json!({"status": "gave_up", "feedback": "no network"})),
            &ctx, CancellationToken::new()).await.unwrap();
        assert!(!result.is_error);
        assert!(matches!(result.content.as_slice(), [crate::mcp::ToolContent::Text(t)] if t == "gave_up: no network"),
            "{:?}", result.content);
    }

    /// A seat refuses a task that is unsafe, or too ambiguous to do safely
    /// without guessing, and says why in its feedback.
    #[tokio::test]
    async fn done_reports_a_refusal() {
        let server = BuiltinTurnServer::new();
        let ctx = CallContext::test();
        let result = server.call_tool(call(serde_json::json!({"status": "refused", "feedback": "the units are never stated"})),
            &ctx, CancellationToken::new()).await.unwrap();
        assert!(matches!(result.content.as_slice(), [crate::mcp::ToolContent::Text(t)] if t == "refused: the units are never stated"),
            "{:?}", result.content);
    }

    #[tokio::test]
    async fn done_refuses_an_unknown_status_an_old_summary_and_empty_feedback() {
        let server = BuiltinTurnServer::new();
        let ctx = CallContext::test();
        assert!(server.call_tool(call(serde_json::json!({"status": "finished", "feedback": "x"})),
            &ctx, CancellationToken::new()).await.is_err());
        assert!(server.call_tool(call(serde_json::json!({"status": "done", "summary": "x"})),
            &ctx, CancellationToken::new()).await.is_err(), "the field is feedback now");
        let empty = server.call_tool(call(serde_json::json!({"status": "done", "feedback": "  "})),
            &ctx, CancellationToken::new()).await.unwrap();
        assert!(empty.is_error);
    }

    /// The schema a model reads says the feedback is for posterity,
    /// free-form, and gives its size.
    #[tokio::test]
    async fn the_schema_describes_feedback_and_refused() {
        let server = BuiltinTurnServer::new();
        let tools = server.list_tools(&CallContext::test()).await.unwrap();
        let schema = serde_json::to_string(&tools[0].input_schema).unwrap();
        assert!(schema.contains("\"feedback\"") && !schema.contains("\"summary\""), "{schema}");
        assert!(schema.contains("posterity") && schema.contains("words"), "{schema}");
        assert!(schema.contains("refused"), "{schema}");
    }

    /// In the `SystemMessagesTool` form every context offered `done` is also
    /// offered `get_system_messages`, and a call the model makes itself
    /// returns no messages.
    #[tokio::test]
    async fn get_system_messages_is_offered_beside_done_and_returns_no_messages() {
        use crate::runtime::llm_stream::{INSTRUCTION_FORM, InstructionForm, SYSTEM_MESSAGES_TOOL};
        let server = BuiltinTurnServer::new();
        let ctx = CallContext::test();
        let names: Vec<String> = server.list_tools(&ctx).await.unwrap().into_iter().map(|t| t.name).collect();
        assert_eq!(names.iter().any(|n| n == SYSTEM_MESSAGES_TOOL), INSTRUCTION_FORM == InstructionForm::SystemMessagesTool, "{names:?}");
        let params = KernelCallParams { instance: InstanceId::new(BuiltinTurnServer::INSTANCE),
            tool: SYSTEM_MESSAGES_TOOL.into(), arguments: serde_json::json!({}) };
        let result = server.call_tool(params, &ctx, CancellationToken::new()).await.unwrap();
        assert!(matches!(result.content.as_slice(), [crate::mcp::ToolContent::Text(t)] if t == r#"{"messages":[]}"#),
            "{:?}", result.content);
    }
}
