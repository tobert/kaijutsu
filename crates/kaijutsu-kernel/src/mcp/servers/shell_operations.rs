//! Inspect and cancel shell operations in the calling context.

use std::sync::Weak;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::super::broker::Broker;
use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use super::super::schema::tool_input_schema;
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{InstanceId, KernelCallParams, KernelTool, KernelToolResult, ToolContent};

// Two JSON strings can expand sixfold; keep the response below the broker budget.
const MAX_READ_BYTES: usize = 4 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListOperationsParams {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadOperationParams {
    /// The operation_id returned by a shell submission.
    pub id: String,
    /// Byte offset in stdout; use the previous next_offset to continue reading.
    #[serde(default)]
    pub offset: usize,
    /// Byte offset in stderr; use the previous next_stderr_offset to continue reading.
    #[serde(default)]
    pub stderr_offset: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelOperationParams {
    /// The operation_id returned by a shell submission.
    pub id: String,
}

fn tool_def<P: JsonSchema + 'static>(instance: &InstanceId, name: &str, description: &str) -> McpResult<KernelTool> {
    Ok(KernelTool {
        instance: instance.clone(), name: name.to_owned(), description: Some(description.to_owned()),
        input_schema: tool_input_schema::<P>(),
    })
}

pub struct ShellOperationsServer {
    instance_id: InstanceId,
    broker: Weak<Broker>,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl ShellOperationsServer {
    pub const INSTANCE: &'static str = "builtin.shell_operations";
    pub const TOOL_LIST: &'static str = "list_shell_operations";
    pub const TOOL_READ: &'static str = "read_shell_operation";
    pub const TOOL_KILL: &'static str = "cancel_shell_operation";

    pub fn new(broker: Weak<Broker>) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self { instance_id: InstanceId::new(Self::INSTANCE), broker, notif_tx }
    }
}

fn json_result(value: serde_json::Value) -> KernelToolResult {
    KernelToolResult { is_error: false, content: vec![ToolContent::Json(value.clone())], structured: Some(value) }
}

/// `registry.get(id, context)` found nothing — but that collapses two very
/// different situations onto one message: an id nobody ever created (a
/// hallucinated or mistyped one), and an id that IS a real operation, just
/// not in the caller's context. `shell_operations` rows are never deleted
/// (no `DELETE FROM shell_operations` exists anywhere in this crate — a
/// receipt is durable for as long as its kernel.db is), so a once-valid id
/// cannot silently decay into "not found"; the only way this branch is
/// reached for a real id is that it belongs to a different context. A
/// second, context-agnostic lookup (`operation_for_retention`, already used
/// by the retention path) tells the two apart for free.
fn no_such_operation_message(registry: &crate::shell_operations::ShellOperationRegistry, id: &str) -> String {
    match registry.operation_for_retention(id) {
        Ok(_) => format!(
            "shell operation {id} exists, but not in this context — an id from \
             list_shell_operations only stays valid in the context it came from"
        ),
        Err(_) => format!(
            "no shell operation {id} was ever created — check it came from a real \
             list_shell_operations or shell submission in this context"
        ),
    }
}

fn page(text: &str, offset: usize) -> (&str, usize) {
    let mut start = offset.min(text.len());
    while !text.is_char_boundary(start) { start -= 1; }
    let mut end = start.saturating_add(MAX_READ_BYTES).min(text.len());
    while !text.is_char_boundary(end) { end -= 1; }
    (&text[start..end], end)
}

#[async_trait]
impl McpServerLike for ShellOperationsServer {
    fn instance_id(&self) -> &InstanceId { &self.instance_id }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        Ok(vec![
            tool_def::<ListOperationsParams>(&self.instance_id, Self::TOOL_LIST,
                "List shell operations in this context, including work waiting for approval and completed work.")?,
            tool_def::<ReadOperationParams>(&self.instance_id, Self::TOOL_READ,
                "Read a shell operation's stdout and stderr in bounded pages. Use returned offsets to continue reading.")?,
            tool_def::<CancelOperationParams>(&self.instance_id, Self::TOOL_KILL,
                "Cancel a running kaish job owned by a shell operation. An approval ask is answered through kj ledger.")?,
        ])
    }

    async fn call_tool(&self, params: KernelCallParams, ctx: &CallContext, _cancel: CancellationToken) -> McpResult<KernelToolResult> {
        let broker = self.broker.upgrade().ok_or_else(|| McpError::Protocol("shell operation broker unavailable".into()))?;
        let dispatcher = broker.kj_dispatcher().await
            .ok_or_else(|| McpError::Protocol("shell operation dispatcher unavailable".into()))?;
        let registry = dispatcher.kernel().shell_operations();
        match params.tool.as_str() {
            Self::TOOL_LIST => {
                let _: ListOperationsParams = serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                let entries = registry.list_for_context(ctx.context_id).map_err(McpError::Protocol)?;
                Ok(json_result(serde_json::json!(entries.into_iter().map(|entry| {
                    serde_json::json!({
                        "receipt": entry.receipt, "source": entry.source,
                        "created_at": entry.created_at, "completed_at": entry.completed_at,
                        "status": entry.envelope.as_ref().map(|e| e.status.as_str())
                            .unwrap_or(if entry.receipt.ask_id.is_some() && entry.receipt.job_id.is_none() { "waiting" } else { "running" }),
                        "exit_code": entry.envelope.as_ref().and_then(|e| e.exit_code),
                    })
                }).collect::<Vec<_>>())))
            }
            Self::TOOL_READ => {
                let p: ReadOperationParams = serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                let entry = registry.get(&p.id, ctx.context_id).map_err(McpError::Protocol)?
                    .ok_or_else(|| McpError::Protocol(no_such_operation_message(registry, &p.id)))?;
                let block = dispatcher.block_store().get_block_snapshot(ctx.context_id, &entry.receipt.output_block_id)
                    .map_err(|e| McpError::Protocol(e.to_string()))?
                    .ok_or_else(|| McpError::Protocol("shell operation output block is missing".into()))?;
                let stdout = entry.envelope.as_ref().map_or(block.content.as_str(), |e| e.stdout.as_str());
                let stderr = entry.envelope.as_ref().map_or(block.stderr.as_deref().unwrap_or(""), |e| e.stderr.as_str());
                let (out, next) = page(stdout, p.offset);
                let (err, next_err) = page(stderr, p.stderr_offset);
                Ok(json_result(serde_json::json!({
                    "receipt": entry.receipt, "completed_at": entry.completed_at,
                    "status": entry.envelope.as_ref().map(|e| e.status.as_str())
                        .unwrap_or(if entry.receipt.ask_id.is_some() && entry.receipt.job_id.is_none() { "waiting" } else { "running" }),
                    "exit_code": entry.envelope.as_ref().and_then(|e| e.exit_code),
                    "error": entry.envelope.as_ref().and_then(|e| e.error.as_deref()),
                    "stdout": out, "stderr": err, "next_offset": next,
                    "next_stderr_offset": next_err, "total_bytes": stdout.len(),
                    "total_stderr_bytes": stderr.len(), "has_more": next < stdout.len() || next_err < stderr.len(),
                })))
            }
            Self::TOOL_KILL => {
                let p: CancelOperationParams = serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                if !broker.binding(&ctx.context_id).await.is_some_and(|b| b.allows(&crate::mcp::Capability::Facade("shell_write".into()))) {
                    return Err(McpError::Protocol("cancel_shell_operation requires facade:shell_write".into()));
                }
                let cancelled = registry.cancel(&p.id, ctx.context_id).await.map_err(McpError::Protocol)?;
                Ok(json_result(serde_json::json!({"operation_id": p.id, "cancellation_requested": cancelled})))
            }
            _ => Err(McpError::ToolNotFound { instance: self.instance_id.clone(), tool: params.tool }),
        }
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> { self.notif_tx.subscribe() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_pages_preserve_utf8_and_have_truthful_offsets() {
        let input = "界".repeat(MAX_READ_BYTES);
        let mut offset = 0;
        let mut restored = String::new();
        while offset < input.len() {
            let (part, next) = page(&input, offset);
            assert!(next > offset);
            assert!(part.len() <= MAX_READ_BYTES);
            restored.push_str(part);
            offset = next;
        }
        assert_eq!(restored, input);
        assert_eq!(page("界x", 1), ("界x", 4));
        assert_eq!(page("界x", usize::MAX), ("", 4));
    }

    use crate::kj::test_helpers::{register_context, test_dispatcher_persistent};
    use crate::mcp::CallContext;
    use crate::mcp::InstancePolicy;
    use kaijutsu_types::{PrincipalId, SessionId};
    use std::sync::Arc;

    /// A fresh broker with `ShellOperationsServer` registered, backed by a
    /// real (temporary-db) dispatcher — the same shape `shell.rs`'s tests use
    /// for its `ShellServer`. `read_shell_operation` and `list_shell_operations`
    /// carry no facade requirement, so — unlike `shell.rs`'s tests — no
    /// `set_binding` call is needed: an unbound context is permissive on a
    /// bare broker that never touches `engage_unbound_deny` (see
    /// `Broker::call_tool_inner`'s comment on that gate).
    async fn wired() -> (Arc<Broker>, Arc<crate::kj::KjDispatcher>) {
        let d = Arc::new(test_dispatcher_persistent().await);
        d.set_self_arc();
        let broker = Arc::new(Broker::new());
        broker.set_kj_dispatcher(&d).await;
        broker
            .register(
                Arc::new(ShellOperationsServer::new(Arc::downgrade(&broker))),
                InstancePolicy::default(),
            )
            .await
            .unwrap();
        (broker, d)
    }

    fn read_call(id: &str) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new(ShellOperationsServer::INSTANCE),
            tool: ShellOperationsServer::TOOL_READ.to_string(),
            arguments: serde_json::json!({"id": id}),
        }
    }

    /// `read_shell_operation` on an id nobody ever created, versus a real id
    /// read from the wrong context, must produce DIFFERENT error text. Before
    /// this fix both collapsed onto the identical "no shell operation <id> in
    /// this context" message (2026-09-18 benchmark notes: two such failures
    /// in one ACP run — the model recovered, but blind to which kind of
    /// failure it had hit).
    ///
    /// This pins cross-context reuse, not "reaped after being valid": reading
    /// `shell_operations.rs` (the registry) shows no `DELETE FROM
    /// shell_operations` anywhere in the crate, so a once-valid id cannot
    /// decay into "not found" the way the benchmark note guessed — the row is
    /// durable for as long as its kernel.db. The one real, present-day cause
    /// of the ambiguous message besides a truly invented id is a real id
    /// from a *different* context, which is what this test drives.
    #[tokio::test]
    async fn unknown_id_and_cross_context_id_produce_different_errors() {
        let (broker, d) = wired().await;
        let kernel = d.kernel();
        let principal = PrincipalId::new();
        let context_a = register_context(&d, Some("shell-ops-a"), None, principal);
        let context_b = register_context(&d, Some("shell-ops-b"), None, principal);
        kernel.kernel_db().lock().update_context_review(context_a, Some(principal), Some(PrincipalId::new())).unwrap();
        kernel.blocks().create_document(context_a, crate::DocumentKind::Conversation, None).unwrap();

        let call_a = CallContext::new(principal, context_a, SessionId::new(), d.kernel_id());
        let receipt = crate::runtime::tool_command::create_operation(kernel, &call_a, "echo real", None)
            .expect("a real operation must be creatable in context A");

        let call_b = CallContext::new(principal, context_b, SessionId::new(), d.kernel_id());

        let never_created = uuid::Uuid::now_v7().to_string();
        let never_created_err = broker
            .call_tool(read_call(&never_created), &call_b, CancellationToken::new())
            .await
            .expect_err("an id nobody ever created must fail");
        let cross_context_err = broker
            .call_tool(read_call(&receipt.operation_id), &call_b, CancellationToken::new())
            .await
            .expect_err("reading context A's operation from context B must fail");

        let never_created_msg = never_created_err.to_string();
        let cross_context_msg = cross_context_err.to_string();
        assert_ne!(
            never_created_msg, cross_context_msg,
            "an invented id and a real-id-wrong-context must not collapse onto \
             the same message"
        );
        assert!(
            !never_created_msg.contains("exists"),
            "an id nobody created must not be told it exists somewhere: {never_created_msg}"
        );
        assert!(
            cross_context_msg.contains("exists"),
            "a real id read from the wrong context should say it exists elsewhere: {cross_context_msg}"
        );

        // The happy path is untouched: the SAME id, read from ITS OWN
        // context, still succeeds.
        broker
            .call_tool(read_call(&receipt.operation_id), &call_a, CancellationToken::new())
            .await
            .expect("reading the operation from its own context must still succeed");
    }
}
