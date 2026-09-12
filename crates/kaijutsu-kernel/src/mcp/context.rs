//! `CallContext` — explicit execution context for every tool call (§4.1).
//!
//! No thread-locals (D-12). For external MCPs, a documented subset flows via
//! MCP `_meta` under the `io.kaijutsu.v1.*` namespace (§5.4, D-11).

use std::path::PathBuf;

use kaijutsu_types::{ContextId, KernelId, PrincipalId, SessionId};

/// W3C trace context (§5.2, D-23). Thin wrapper around the traceparent +
/// tracestate pair produced by `kaijutsu_telemetry::inject_trace_context()`.
///
/// `traceparent` is required when populated; `tracestate` may be empty.
/// `empty()` yields a detached context (no remote parent), equivalent to a
/// freshly-rooted span.
#[derive(Clone, Debug, Default)]
pub struct TraceContext {
    pub traceparent: String,
    pub tracestate: String,
}

impl TraceContext {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Capture the W3C context from the currently-active tracing span.
    pub fn from_current_span() -> Self {
        let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
        Self {
            traceparent,
            tracestate,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.traceparent.is_empty()
    }
}

/// Explicit per-call context. Passed by reference to every
/// `McpServerLike::call_tool`.
#[derive(Clone, Debug)]
pub struct CallContext {
    /// Attribution only, never authorization (D-22).
    pub principal_id: PrincipalId,
    /// Character performing this invocation; separate from its requester.
    pub actor_id: PrincipalId,
    /// Character delegated to review this actor's work.
    pub reviewer_id: Option<PrincipalId>,
    pub context_id: ContextId,
    pub session_id: SessionId,
    pub kernel_id: KernelId,
    /// Working directory for file-touching tools. `None` means filesystem
    /// tools must reject (§4.1).
    pub cwd: Option<PathBuf>,
    pub trace: TraceContext,
}

impl CallContext {
    pub fn new(
        principal_id: PrincipalId,
        context_id: ContextId,
        session_id: SessionId,
        kernel_id: KernelId,
    ) -> Self {
        Self {
            principal_id,
            actor_id: principal_id,
            reviewer_id: None,
            context_id,
            session_id,
            kernel_id,
            cwd: None,
            trace: TraceContext::empty(),
        }
    }

    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }

    pub fn with_actor(mut self, actor_id: PrincipalId, reviewer_id: Option<PrincipalId>) -> Self {
        self.actor_id = actor_id;
        self.reviewer_id = reviewer_id;
        self
    }

    pub fn with_trace(mut self, trace: TraceContext) -> Self {
        self.trace = trace;
        self
    }

    /// Minimal context for tests.
    pub fn test() -> Self {
        let principal_id = PrincipalId::new();
        Self::new(
            principal_id,
            ContextId::new(),
            SessionId::new(),
            KernelId::new(),
        )
        .with_actor(principal_id, Some(PrincipalId::new()))
    }

    /// System-principal context used for broker-internal calls (e.g., the
    /// pump's `list_tools()` on ToolsChanged diff). No cwd; filesystem tools
    /// must reject.
    pub fn system() -> Self {
        Self::new(
            PrincipalId::system(),
            ContextId::new(),
            SessionId::new(),
            KernelId::new(),
        )
    }

    /// System-principal context for a specific `ContextId` (Phase 3 M3).
    /// Used by `Broker::clear_binding` / `unregister` / resource-flush re-reads
    /// where the teardown path needs to address a specific context but there
    /// is no live user principal to attribute to.
    pub fn system_for_context(context_id: ContextId) -> Self {
        Self::new(
            PrincipalId::system(),
            context_id,
            SessionId::new(),
            KernelId::new(),
        )
    }
}
