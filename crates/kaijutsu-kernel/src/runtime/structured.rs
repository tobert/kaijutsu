//! Addressed `kj` commands with structured arguments and optional transcript output.

use std::sync::Arc;
use kaijutsu_types::{BlockId, PrincipalId, Refusal, Role, Status, ToolKind};
use crate::Kernel;
use super::command::{self, CommandContextSwitch, CommandRunOptions};
use super::command_outcome::{CommandExecution, CommandHookEffect, CommandOutcome};
use super::context_shell::{ShellIdentity, ShellPolicy};
use super::embedded_kaish::EmbeddedKaish;

pub struct ExecutedKj {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub command_block_id: Option<BlockId>,
    pub latch: Option<super::kj_builtin::KjLatchInfo>,
    pub data: Option<serde_json::Value>,
}

// Source execution supplies per-call cancellation; kaish's execute_argv has
// no ExecuteOptions argument. Backticks are literal inside kaish double quotes.
fn kaish_quote(word: &str) -> String {
    let escaped = word.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$");
    format!("\"{}\"", escaped)
}

/// Execute one `kj` invocation. Structured argv cannot supply shell syntax.
/// The addressed context stays pinned even when the command switches its shell.
/// Quiet calls execute without authoring a command/output pair or receipt.
pub async fn execute_kj(
    kernel: &Arc<Kernel>,
    identity: ShellIdentity,
    argv: &[String],
    quiet: bool,
    review_notices: Option<tokio::sync::mpsc::UnboundedSender<Refusal>>,
) -> Result<Result<ExecutedKj, Refusal>, String> {
    let context = identity.context;
    if kernel.kernel_db().lock().get_context(context).map_err(|e| e.to_string())?.is_none() {
        return Err("context not found".into());
    }
    let dispatcher = kernel.broker().kj_dispatcher().await.ok_or("kj dispatcher is not registered")?;
    let kaish = EmbeddedKaish::for_context(&dispatcher, "structured-kj", identity, ShellPolicy::Agent,
        dispatcher.semantic_index(), dispatcher.block_source()).await.map_err(|e| e.to_string())?;
    let documents = kernel.blocks();
    if documents.get(context).is_none() { return Err(format!("context {context} is not materialized")); }
    let mut code = String::from("kj");
    for arg in argv { code.push(' '); code.push_str(&kaish_quote(arg)); }
    let pair = if quiet { None } else {
        let last = documents.last_block_id(context);
        let command = documents.insert_tool_call_as(context, None, last.as_ref(), "kj",
            serde_json::json!({"argv": argv}), Some(ToolKind::Builtin), Some(identity.performer), None, Some(Role::User))
            .map_err(|e| e.to_string())?;
        let output = documents.insert_tool_result_as(context, &command, Some(&command), "", false, None,
            Some(ToolKind::Builtin), Some(PrincipalId::system()), None).map_err(|e| e.to_string())?;
        let epoch = kernel.kernel_db().lock().continuation_epoch(context).map_err(|e| e.to_string())?;
        kernel.shell_operations().register(context, identity.requester, identity.performer, command, output, &code, epoch)?;
        documents.set_status(context, &output, Status::Running).map_err(|e| e.to_string())?;
        Some((command, output))
    };
    let call_ctx = crate::mcp::CallContext::new(identity.requester, context, identity.session, kernel.id())
        .with_actor(identity.performer, identity.reviewer);
    let outcome = match kernel.broker().shell_pre_call_hooks(&code, &call_ctx).await {
        crate::mcp::ShellHookVerdict::Proceed => match pair {
            Some((command, output)) => command::run_into_blocks(&kaish, &code, context, &command, &output,
                kernel, &call_ctx, CommandRunOptions { stdin: None,
                    context_switch: CommandContextSwitch::Pinned, review_notices, ..Default::default() }).await?,
            None => command::run_without_blocks(&kaish, &code, kernel, &call_ctx,
                kaish_kernel::ExecuteOptions::default(), CommandRunOptions { stdin: None,
                    context_switch: CommandContextSwitch::Pinned, review_notices, ..Default::default() }).await?,
        },
        verdict => {
            let mut outcome = CommandOutcome::new(CommandExecution::NotRun, 0);
            outcome.apply_hook(verdict);
            if let Some((command, output)) = pair {
                command::settle_outcome(kernel, context, &command, &output, &outcome)?;
                if let Some(ask) = outcome.refusal().and_then(|refusal| refusal.ask_id()) {
                    kernel.kernel_db().lock().link_ask_blocks(ask, &command, &output, crate::PairOwner::Session)
                        .map_err(|e| e.to_string())?;
                }
            }
            outcome
        }
    };
    if let Some(error) = &outcome.settlement_error { return Err(error.clone()); }
    if let Some(refusal) = outcome.refusal() { return Ok(Err(refusal.clone())); }
    if let Some(CommandHookEffect::Refused { reason, .. }) = &outcome.hook { return Err(reason.clone()); }
    let result = outcome.exec_result();
    let raw = crate::ansi_ingest::raw_stdout(&result);
    let stdout = crate::ansi_ingest::project(&raw).map_or_else(|| result.text_out().into_owned(), |p| p.text);
    Ok(Ok(ExecutedKj {
        exit_code: result.code.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        stdout, stderr: result.err.clone(), command_block_id: pair.map(|(command, _)| command),
        latch: super::kj_builtin::latch_from_result(&result),
        data: outcome.output_data().and_then(|output| output.rich_json),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn structured_execution_preserves_literal_arguments_and_distinct_identities() {
        let dispatcher = Arc::new(crate::kj::test_helpers::test_dispatcher().await);
        dispatcher.set_self_arc();
        let kernel = dispatcher.kernel();
        kernel.broker().set_kj_dispatcher(&dispatcher).await;
        let requester = PrincipalId::new();
        let performer = PrincipalId::new();
        let context = crate::kj::test_helpers::register_context(&dispatcher, Some("structured"), None, requester);
        kernel.blocks().create_document(context, crate::block_store::DocumentKind::Conversation, None).unwrap();
        let content = "literal $(echo unexpected); $HOME `echo unexpected` \"quotes\" \\ end";
        let argv: Vec<String> = ["block", "create", "--role", "user", "--kind", "text", "--content", content]
            .into_iter().map(str::to_owned).collect();
        let reply = execute_kj(kernel, ShellIdentity { requester, performer, reviewer: None,
            context, session: kaijutsu_types::SessionId::new() }, &argv, false, None).await.unwrap().unwrap();
        assert_eq!(reply.exit_code, 0, "{}", reply.stderr);
        let command = reply.command_block_id.unwrap();
        assert_eq!(command.principal_id, performer);
        let authored = kernel.blocks().block_snapshots(context).unwrap().into_iter()
            .find(|block| block.kind == kaijutsu_types::BlockKind::Text).unwrap();
        assert_eq!(authored.content, content, "arguments must remain literal");
        assert_eq!(authored.id.principal_id, performer);
        let (stored_requester, stored_performer): (Vec<u8>, Vec<u8>) = kernel.kernel_db().lock().conn_for_ledger()
            .query_row("SELECT principal_id,actor_id FROM shell_operations WHERE command_block_id=?1",
                [command.to_key()], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(stored_requester, requester.as_bytes());
        assert_eq!(stored_performer, performer.as_bytes());
    }

}
