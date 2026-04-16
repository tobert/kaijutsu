//! Builtin hook registry — named bodies that the admin wire addresses by
//! string, not by `Arc<dyn Hook>` (D-50).
//!
//! `BuiltinHookRegistry` is frozen after construction; the admin server
//! looks up a name and builds a fresh `Arc<dyn Hook>` per `hook_add` call.
//! The registry never returns the same Arc twice — makes each hook entry
//! independently droppable.
//!
//! Phase 4 seeds:
//! - `tracing_audit` — emits one `tracing::trace!` event per invocation.
//!   The positive control for exit criterion #1.
//! - `no_op` — returns `Ok(())` unconditionally. Useful as a negative
//!   control and for tests that want to exercise the Invoke path without
//!   observing side effects.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use super::context::CallContext;
use super::error::McpResult;
use super::hook_table::Hook;
use super::types::KernelCallParams;

type HookFactory = fn() -> Arc<dyn Hook>;

pub struct BuiltinHookRegistry {
    factories: HashMap<&'static str, HookFactory>,
}

impl BuiltinHookRegistry {
    pub fn new() -> Self {
        let mut factories: HashMap<&'static str, HookFactory> = HashMap::new();
        factories.insert("tracing_audit", tracing_audit_factory);
        factories.insert("no_op", no_op_factory);
        Self { factories }
    }

    /// Sorted list of registered builtin names. `hook_list` uses this for
    /// admin introspection.
    pub fn known_names(&self) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = self.factories.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// Build a fresh `Arc<dyn Hook>` by name. Returns `None` when the name
    /// is not registered — admin server converts to `McpError::ToolNotFound`.
    pub fn build(&self, name: &str) -> Option<Arc<dyn Hook>> {
        self.factories.get(name).map(|f| f())
    }
}

impl Default for BuiltinHookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for BuiltinHookRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltinHookRegistry")
            .field("known_names", &self.known_names())
            .finish()
    }
}

// ─ seed hooks ──────────────────────────────────────────────────────────

pub struct TracingAuditHook;

fn tracing_audit_factory() -> Arc<dyn Hook> {
    Arc::new(TracingAuditHook)
}

#[async_trait]
impl Hook for TracingAuditHook {
    async fn invoke(
        &self,
        params: &KernelCallParams,
        ctx: &CallContext,
    ) -> McpResult<()> {
        tracing::trace!(
            target: "kaijutsu::hooks::audit",
            instance = %params.instance,
            tool = %params.tool,
            context_id = %ctx.context_id,
            principal_id = %ctx.principal_id,
            "hook.audit",
        );
        Ok(())
    }
}

pub struct NoOpHook;

fn no_op_factory() -> Arc<dyn Hook> {
    Arc::new(NoOpHook)
}

#[async_trait]
impl Hook for NoOpHook {
    async fn invoke(
        &self,
        _params: &KernelCallParams,
        _ctx: &CallContext,
    ) -> McpResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{ContextId, KernelId, PrincipalId, SessionId};
    use super::super::types::InstanceId;

    fn test_ctx() -> CallContext {
        CallContext::new(
            PrincipalId::new(),
            ContextId::new(),
            SessionId::new(),
            KernelId::new(),
        )
    }

    fn test_params() -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new("svc"),
            tool: "t".into(),
            arguments: serde_json::json!({}),
        }
    }

    #[test]
    fn registry_lists_known_names() {
        let r = BuiltinHookRegistry::new();
        let names = r.known_names();
        assert!(names.contains(&"tracing_audit"));
        assert!(names.contains(&"no_op"));
        // Sorted — useful invariant for admin output.
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[tokio::test]
    async fn registry_builds_tracing_audit() {
        let r = BuiltinHookRegistry::new();
        let h = r.build("tracing_audit").expect("tracing_audit must exist");
        h.invoke(&test_params(), &test_ctx()).await.unwrap();
    }

    #[tokio::test]
    async fn registry_builds_no_op() {
        let r = BuiltinHookRegistry::new();
        let h = r.build("no_op").expect("no_op must exist");
        h.invoke(&test_params(), &test_ctx()).await.unwrap();
    }

    #[test]
    fn registry_unknown_name_returns_none() {
        let r = BuiltinHookRegistry::new();
        assert!(r.build("no_such_hook").is_none());
    }

    /// `TracingAuditHook::invoke` emits a TRACE event with the expected
    /// fields. Exit criterion #1 unit-test anchor.
    #[tokio::test]
    async fn tracing_audit_emits_trace_event() {
        use tracing_subscriber::layer::SubscriberExt;

        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        struct Capture(Arc<std::sync::Mutex<Vec<String>>>);
        impl<S> tracing_subscriber::Layer<S> for Capture
        where
            S: tracing::Subscriber,
        {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut s = String::new();
                let mut vis = V(&mut s);
                event.record(&mut vis);
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("{}: {}", event.metadata().name(), s));
            }
        }
        struct V<'a>(&'a mut String);
        impl tracing::field::Visit for V<'_> {
            fn record_debug(
                &mut self,
                field: &tracing::field::Field,
                value: &dyn std::fmt::Debug,
            ) {
                use std::fmt::Write;
                let _ = write!(self.0, "{}={:?} ", field.name(), value);
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                use std::fmt::Write;
                let _ = write!(self.0, "{}={} ", field.name(), value);
            }
        }

        let subscriber = tracing_subscriber::registry().with(Capture(events.clone()));
        let _guard = tracing::subscriber::set_default(subscriber);

        TracingAuditHook
            .invoke(&test_params(), &test_ctx())
            .await
            .unwrap();

        let recorded = events.lock().unwrap().clone();
        assert!(
            recorded.iter().any(|s| s.contains("hook.audit") && s.contains("tool=t")),
            "expected hook.audit event with tool=t; got {recorded:?}",
        );
    }
}
