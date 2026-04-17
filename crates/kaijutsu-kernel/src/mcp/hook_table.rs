//! Hook tables — match-action hook engine (§4.3, D-07).
//!
//! Phase 4 wires evaluation at the four pinch points:
//! - `PreCall` / `PostCall` / `OnError` — evaluated around
//!   `Broker::call_tool` (`broker.rs`); see `evaluate_phase`.
//! - `OnNotification` — evaluated **post-coalesce**, once per emitted block
//!   in `emit_for_bindings` and `handle_resource_flush`. Raw
//!   `ServerNotification` events from `pump_loop` are NOT hook-evaluated;
//!   the hook sees what the LLM/UI sees. Notifications carry a synthetic
//!   `tool = "__notification.<kind>"` so users can filter via
//!   `match_tool: Some("__notification.log")` or similar. `__`-prefix is
//!   the convention for synthetic tool names; no real instance should
//!   advertise tools in that namespace.
//!
//! `HookAction::Deny(String)` carries a reason message; the broker
//! discards the message content in the LLM-visible path and returns
//! `McpError::Denied { by_hook: <id> }` (D-28 channel discipline). The
//! reason lands only in tracing events.
//!
//! `HookAction::Log(LogSpec)` emits a `tracing::event!`, NOT a
//! Notification block (D-48). LLM-visible audit is achieved by an
//! `Invoke` body that calls the block tools server explicitly.
//!
//! Hook bodies MUST NOT `tokio::spawn` child tasks that re-enter the
//! broker. The reentrancy counter (D-29) lives in `tokio::task_local!`
//! and a spawned task starts fresh — losing the depth guard opens a
//! reentrancy path around the cap.
//!
//! `HookBody::Kaish` is reserved per D-07 / D-08; admin server rejects
//! it at `hook_add` time with `McpError::Unsupported`. Implementation
//! is a §9 follow-up.

use std::sync::Arc;

use async_trait::async_trait;
use kaijutsu_types::{ContextId, PrincipalId};

use super::context::CallContext;
use super::error::{HookId, McpResult};
use super::types::{KernelCallParams, KernelToolResult};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HookPhase {
    PreCall,
    PostCall,
    OnError,
    OnNotification,
    /// Filter the per-context tool list returned by
    /// `Broker::list_visible_tools`. `Deny` strips matching tools; `Log`
    /// observes and continues. `ShortCircuit` / `Invoke` have no coherent
    /// list-filter semantics and are rejected at `hook_add` time (D-56).
    ListTools,
}

/// Opaque glob pattern. Phase 1 keeps it a plain string; actual matching is
/// wired in Phase 4.
#[derive(Clone, Debug)]
pub struct GlobPattern(pub String);

#[derive(Clone, Debug)]
pub struct LogSpec {
    pub target: String,
    pub level: tracing::Level,
}

/// Reference to a kaish script. Body implementation deferred (§9).
#[derive(Clone, Debug)]
pub struct ScriptRef {
    pub id: String,
}

/// Hook body: either a builtin function or a kaish script (deferred).
///
/// `Builtin.name` is the registry key the body was built from (or any other
/// opaque tag for ad-hoc bodies). It travels with the body so the admin
/// surface and tracing events can report which builtin is firing without
/// reflecting on `Arc<dyn Hook>`.
#[derive(Clone)]
pub enum HookBody {
    Builtin { name: String, hook: Arc<dyn Hook> },
    Kaish(ScriptRef),
}

impl std::fmt::Debug for HookBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HookBody::Builtin { name, .. } => {
                f.debug_tuple("Builtin").field(name).finish()
            }
            HookBody::Kaish(s) => f.debug_tuple("Kaish").field(&s.id).finish(),
        }
    }
}

/// Hook action: continue the chain, terminate with a result, terminate with
/// an error, or observe and continue (§4.3).
///
/// `Deny` carries a `String` reason rather than `McpError`. The broker
/// converts denials uniformly to `McpError::Denied { by_hook }` at the
/// LLM boundary (D-28); the reason string is tracing-only.
#[derive(Clone, Debug)]
pub enum HookAction {
    Invoke(HookBody),
    ShortCircuit(KernelToolResult),
    Deny(String),
    Log(LogSpec),
}

#[derive(Clone, Debug)]
pub struct HookEntry {
    pub id: HookId,
    pub match_instance: Option<GlobPattern>,
    pub match_tool: Option<GlobPattern>,
    pub match_context: Option<ContextId>,
    pub match_principal: Option<PrincipalId>,
    pub action: HookAction,
    pub priority: i32,
}

#[derive(Default)]
pub struct HookTable {
    pub phase: Option<HookPhase>,
    pub entries: Vec<HookEntry>,
}

#[derive(Default)]
pub struct HookTables {
    pub pre_call: HookTable,
    pub post_call: HookTable,
    pub on_error: HookTable,
    pub on_notification: HookTable,
    /// Phase 5 (D-56): list-time filter on `Broker::list_visible_tools`.
    pub list_tools: HookTable,
}

/// Builtin hook body trait. Phase 4 wires evaluation.
#[async_trait]
pub trait Hook: Send + Sync + 'static {
    async fn invoke(
        &self,
        params: &KernelCallParams,
        ctx: &CallContext,
    ) -> McpResult<()>;
}
