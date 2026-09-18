//! Editor shell reads: caller cancellation, kernel ownership, complete UTF-8 text.
//!
//! Reads use the opener's contextual shell without command hooks or shell-state
//! write-back. They return text to the editor's splice transaction; they do not
//! author a command/output pair. See `docs/kaish-integration.md`.

use std::sync::Arc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use crate::kj::KjDispatcher;
use super::context_shell::{ShellCwd, ShellIdentity, ShellPolicy};
use super::embedded_kaish::EmbeddedKaish;

/// Keep execution alive through cooperative cancellation when the read's caller
/// disappears. The existing runtime joins this task on shutdown, including when
/// a `kj editor keys` call re-enters from another task on that same runtime.
pub(crate) async fn read_shell(
    dispatcher: Arc<KjDispatcher>, identity: ShellIdentity, code: String,
) -> Result<String, String> {
    let (reply, completed) = oneshot::channel();
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let kernel = dispatcher.kernel().clone();
    let depth = crate::mcp::broker::current_hook_depth();
    let span = tracing::Span::current();
    kernel.spawn_context_task(identity.context, move |admission, shutdown| crate::mcp::broker::inherit_hook_depth(depth, async move {
        let run = execute(&dispatcher, admission, identity, &code, cancel.clone());
        tokio::pin!(run);
        let result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => { cancel.cancel(); run.await }
            result = &mut run => result,
        };
        let _ = reply.send(result);
    }.instrument(span)))?;
    completed.await.map_err(|_| "editor shell task stopped before completing the read".to_string())?
}

async fn execute(
    dispatcher: &KjDispatcher, admission: super::admission::ContextAdmission, identity: ShellIdentity, code: &str, cancel: CancellationToken,
) -> Result<String, String> {
    debug_assert_eq!(admission.context(), identity.context);
    let kaish = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err("editor shell read cancelled before execution".into()),
        result = EmbeddedKaish::for_context(dispatcher, "editor-read", identity, ShellPolicy::Internal, ShellCwd::Context,
            dispatcher.semantic_index(), dispatcher.block_source()) => result.map_err(|e| e.to_string())?,
    };
    let options = kaish_kernel::ExecuteOptions { cancel_token: Some(cancel), ..Default::default() };
    let result = kaish.execute_with_options(code, options).await.map_err(|e| e.to_string())?;
    complete_text(&result)
}

fn complete_text(result: &kaish_kernel::interpreter::ExecResult) -> Result<String, String> {
    if result.did_spill {
        return Err("output was truncated; redirect it to a file and use ':r <file>' to read complete text".into());
    }
    if result.code != 0 {
        return Err(format!("exited {}: {}", result.code, result.err.trim()));
    }
    result.try_text_out().map(|text| text.into_owned())
        .map_err(|e| format!("output is not valid UTF-8: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PausedTool {
        id: crate::mcp::InstanceId,
        entered: Arc<tokio::sync::Notify>,
        dropped: CancellationToken,
    }

    #[async_trait::async_trait]
    impl crate::mcp::McpServerLike for PausedTool {
        fn instance_id(&self) -> &crate::mcp::InstanceId { &self.id }
        async fn list_tools(&self, _: &crate::mcp::CallContext) -> crate::mcp::McpResult<Vec<crate::mcp::KernelTool>> {
            Ok(vec![crate::mcp::KernelTool { instance: self.id.clone(), name: "pause-editor-read".into(),
                description: None, input_schema: serde_json::json!({"type":"object","properties":{}}) }])
        }
        async fn call_tool(&self, _: crate::mcp::KernelCallParams, _: &crate::mcp::CallContext,
            _: CancellationToken) -> crate::mcp::McpResult<crate::mcp::KernelToolResult> {
            let _on_drop = self.dropped.clone().drop_guard();
            self.entered.notify_one();
            std::future::pending().await
        }
        fn notifications(&self) -> tokio::sync::broadcast::Receiver<crate::mcp::ServerNotification> {
            tokio::sync::broadcast::channel(1).1
        }
    }

    async fn fixture() -> (Arc<KjDispatcher>, ShellIdentity) {
        use crate::kj::test_helpers::{test_dispatcher, register_context};
        use crate::vfs::VfsOps;
        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();
        d.kernel().broker().set_kj_dispatcher(&d).await;
        d.kernel().vfs().write_all(std::path::Path::new("/config/kernel/gate.toml"), b"[global]\n").await.unwrap();
        let principal = kaijutsu_types::PrincipalId::new();
        let context = register_context(&d, Some("editor-owner"), None, principal);
        (d, ShellIdentity { requester: principal, performer: principal, reviewer: None,
            context, session: kaijutsu_types::SessionId::new() })
    }

    #[tokio::test]
    async fn caller_drop_and_shutdown_cancel_editor_execution() {
        for shutdown in [false, true] {
            let (d, identity) = fixture().await;
            let entered = Arc::new(tokio::sync::Notify::new());
            let dropped = CancellationToken::new();
            let tool = Arc::new(PausedTool { id: crate::mcp::InstanceId("editor-probe".into()),
                entered: entered.clone(), dropped: dropped.clone() });
            d.kernel().broker().register(tool, crate::mcp::InstancePolicy::default()).await.unwrap();
            d.kernel().broker().set_binding(identity.context, crate::mcp::ContextToolBinding {
                all_instances: true, ..Default::default()
            }).await.unwrap();
            let mut read = Box::pin(read_shell(d.clone(), identity, "pause-editor-read".into()));
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                tokio::select! {
                    result = &mut read => panic!("read completed before the controlled tool: {result:?}"),
                    _ = entered.notified() => {}
                }
            }).await.expect("controlled tool must enter before cancellation");
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                if shutdown {
                    let (result, joined) = tokio::join!(&mut read, d.kernel().shutdown_runtime_worker());
                    joined.unwrap();
                    let error = result.unwrap_err();
                    assert!(error.to_lowercase().contains("cancel"), "{error}");
                } else {
                    drop(read);
                    dropped.cancelled().await;
                    d.kernel().shutdown_runtime_worker().await.unwrap();
                }
                assert!(dropped.is_cancelled(), "worker must release pending execution before shutdown returns");
            }).await.expect("editor cancellation must finish without releasing the tool");
        }
    }

    #[tokio::test]
    async fn editor_read_reenters_the_runtime_without_deadlock() {
        let (d, identity) = fixture().await;
        let (reply, completed) = oneshot::channel();
        let owner = d.clone();
        d.kernel().spawn_runtime_task(move |_| async move {
            let _ = reply.send(read_shell(owner, identity, "echo nested".into()).await);
        }).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), completed).await.unwrap().unwrap().unwrap();
        assert_eq!(result, "nested\n");
        d.kernel().shutdown_runtime_worker().await.unwrap();
    }

    #[test]
    fn incomplete_output_is_never_an_editor_splice() {
        let mut result = kaish_kernel::interpreter::ExecResult::success("preview");
        result.did_spill = true;
        result.original_code = Some(0);
        for code in [0, 3] {
            result.code = code;
            assert!(complete_text(&result).unwrap_err().contains("truncated"));
        }
    }
}
