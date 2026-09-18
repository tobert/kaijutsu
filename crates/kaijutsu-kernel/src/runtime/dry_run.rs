//! Advisory shell hooks admitted to the kernel's joined runtime.

use std::sync::Arc;
use tracing::Instrument;
use crate::{Kernel, mcp::{CallContext, DryRunReport}};

/// Evaluate shell hooks without executing the proposed command. Hook bodies
/// can have effects, so accepted evaluation survives its requesting connection
/// and observes kernel shutdown. Archive prevents new admission.
pub async fn inspect_shell(
    kernel: &Arc<Kernel>, ctx: CallContext, command: String,
) -> Result<DryRunReport, String> {
    let (reply, completed) = tokio::sync::oneshot::channel();
    let broker = kernel.broker().clone();
    let depth = crate::mcp::broker::current_hook_depth();
    let span = tracing::Span::current();
    kernel.spawn_context_task(ctx.context_id, move |admission, stop| crate::mcp::broker::inherit_hook_depth(depth, async move {
        debug_assert_eq!(admission.context(), ctx.context_id);
        let result = broker.shell_pre_call_hooks_dry_run(&command, &ctx, &stop)
            .await.map_err(|error| error.to_string());
        if let Err(Err(error)) = reply.send(result) {
            tracing::warn!(context = %ctx.context_id,
                "advisory hook evaluation failed after its caller departed: {error}");
        }
    }.instrument(span)))?;
    completed.await.map_err(|_| "advisory hook task stopped before replying".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{PrincipalId, SessionId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountCalls(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl crate::mcp::Hook for CountCalls {
        async fn invoke(&self, _: &crate::mcp::KernelCallParams, _: &CallContext,
            _: &tokio_util::sync::CancellationToken) -> crate::mcp::McpResult<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn archive_refuses_advisory_work_before_hook_effects() {
        use crate::mcp::{HookAction, HookBody, HookEntry, HookId};
        let dispatcher = Arc::new(crate::kj::test_helpers::test_dispatcher_persistent().await);
        dispatcher.set_self_arc();
        let kernel = dispatcher.kernel();
        kernel.broker().set_kj_dispatcher(&dispatcher).await;
        let principal = PrincipalId::new();
        let context = crate::kj::test_helpers::register_context(
            &dispatcher, Some("advisory-archive"), None, principal);
        let call = CallContext::new(principal, context, SessionId::new(), kernel.id());
        let calls = Arc::new(AtomicUsize::new(0));
        kernel.broker().hooks().write().await.pre_call.entries.push(HookEntry {
            id: HookId("count-advisory".into()), match_instance: None, match_tool: None,
            match_context: Some(context), match_principal: None, priority: 0, kaish_script_id: None,
            action: HookAction::Invoke(HookBody::Builtin { name: "count-advisory".into(),
                hook: Arc::new(CountCalls(calls.clone())) }),
        });
        inspect_shell(kernel, call.clone(), "echo advisory".into()).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the live context must run its hook");
        kernel.kernel_db().lock().archive_context(context).unwrap();
        let error = inspect_shell(kernel, call, "echo advisory".into()).await.unwrap_err();
        assert!(error.contains("archived"), "{error}");
        kernel.shutdown_runtime_worker().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "archive must prevent new hook effects");
    }
}
