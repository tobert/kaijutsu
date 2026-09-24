//! Match-action hooks for tool calls, visible tool lists, and notifications.
//!
//! `PreCall`, `PostCall`, and `OnError` surround broker calls and contextual
//! shell commands. `OnNotification` runs after coalescing, once per emitted
//! block; synthetic tool names such as `__notification.log` select its kind.
//! `ListTools` filters discovery and cannot execute bodies or wait for approval.
//!
//! Denial reasons reach the caller. `Log` writes tracing events; an `Invoke`
//! body can author blocks explicitly. Kaish bodies require contextual kernel
//! wiring. Stored script bodies are snapshots; path bodies read at invocation.
//!
//! Executable hooks inherit their owner's cancellation and finish cleanup
//! before returning. Hook bodies must not spawn tasks that re-enter the broker
//! without inheriting its task-local hook depth; doing so bypasses recursion
//! limits. See `docs/gate-resume.md` for verdicts and approval waits.

use std::sync::Arc;

use async_trait::async_trait;
use kaijutsu_types::{ContextId, PrincipalId};

use super::context::CallContext;
use super::error::{HookId, McpResult};
use super::types::{KernelCallParams, KernelToolResult};

/// Broker and contextual shell hook phases. Persisted
/// as stable lower-snake strings (`pre_call`, …) via `hook_persist`, decoupled
/// from these Rust names — renaming a variant needs a `phase_to_str` update,
/// not a DB migration.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum McpHookPhase {
    PreCall,
    PostCall,
    OnError,
    OnNotification,
    /// Filter the per-context tool list returned by
    /// `Broker::list_visible_tools`. `Deny` strips matching tools; `Log`
    /// observes and continues. `ShortCircuit` / `Invoke` have no coherent
    /// list-filter semantics and are rejected at `hook_add` time.
    ListTools,
}

/// Glob pattern matched by the broker against hook invocation metadata.
#[derive(Clone, Debug)]
pub struct GlobPattern(pub String);

#[derive(Clone, Debug)]
pub struct LogSpec {
    pub target: String,
    pub level: tracing::Level,
}

/// Hook body: either a builtin function or an inline kaish script.
///
/// `Builtin.name` is the registry key the body was built from (or any other
/// opaque tag for ad-hoc bodies). It travels with the body so the admin
/// surface and tracing events can report which builtin is firing without
/// reflecting on `Arc<dyn Hook>`.
///
/// `Kaish(body)` carries the script source directly. A separate script-
/// storage table for shared/reusable bodies is a future follow-up; today
/// each hook owns its own copy.
///
/// `KaishPath(path)` carries a VFS path instead of a body — the body is
/// read fresh at every fire (`Broker::run_kaish_hook` via
/// `Broker::read_kaish_hook_body`), never snapshotted. An edit to the
/// file at `path` reaches the running hook with no reinstall
/// (`docs/rc-on-disk.md`, "slice 5"). A path that cannot be read at fire
/// time is a `Deny` — see `unreadable_hook_body_outcome` in `broker.rs`.
#[derive(Clone)]
pub enum HookBody {
    Builtin { name: String, hook: Arc<dyn Hook> },
    Kaish(String),
    KaishPath(String),
}

impl std::fmt::Debug for HookBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HookBody::Builtin { name, .. } => {
                f.debug_tuple("Builtin").field(name).finish()
            }
            HookBody::Kaish(body) => {
                let preview: String = body.chars().take(32).collect();
                f.debug_tuple("Kaish").field(&preview).finish()
            }
            HookBody::KaishPath(path) => f.debug_tuple("KaishPath").field(path).finish(),
        }
    }
}

/// Configuration for `HookAction::Ask`. A struct (not inline enum fields)
/// so the ask surface can grow — e.g. a configurable options list — without
/// another `HookAction` variant shape change.
#[derive(Clone, Debug, Default)]
pub struct AskSpec {
    /// Human-readable description shown to the answering client. `None`
    /// falls back to an auto-generated `"{instance}.{tool}"` at fire time
    /// (`Broker::evaluate_phase`), so a hook config doesn't have to spell
    /// out the obvious case.
    pub description: Option<String>,
}

/// Hook action: continue the chain, terminate with a result, terminate with
/// an error, block on a permission ask, or observe and continue (§4.3).
///
/// `Deny` carries a `String` reason rather than `McpError`. The broker
/// converts denials uniformly to one refusal shape at the LLM boundary
/// (D-28), carrying the reason with them.
///
/// `Ask` terminates as `McpError::denied_by_hook` when a subscriber actually
/// answers "no" — a real verdict, same D-28 channel as `Deny`. When the
/// fail-closed default fires instead (no subscriber attached, or nobody
/// answered in time), it terminates as `McpError::gate_unavailable` —
/// distinct on purpose: both refuse the call, but only one of them is a
/// decision. See `super::permission` (D-57).
#[derive(Clone, Debug)]
pub enum HookAction {
    Invoke(HookBody),
    ShortCircuit(KernelToolResult),
    Deny(String),
    Log(LogSpec),
    Ask(AskSpec),
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
    /// If `action` is a `HookBody::Kaish` body sourced from the
    /// `hook_scripts` table at install time, the originating
    /// `script_id` — provenance metadata. The body in `HookBody::Kaish`
    /// is the snapshot taken at install; subsequent edits to the
    /// source script do NOT propagate per
    /// [[feedback_script_snapshot_on_instantiation]]. Persistence
    /// writes both `hooks.action_kaish_body` (snapshot) and
    /// `hooks.action_kaish_script_id` (provenance); they are no
    /// longer mutually exclusive. `None` means "inline body, no
    /// script provenance" — the original case.
    pub kaish_script_id: Option<String>,
}

#[derive(Default)]
pub struct HookTable {
    pub phase: Option<McpHookPhase>,
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

/// Builtin hook execution. The evaluator awaits cleanup after cancellation;
/// implementations must observe the owner token while waiting, finish owned
/// effects, and return `McpError::Cancelled` before starting further work.
#[async_trait]
pub trait Hook: Send + Sync + 'static {
    async fn invoke(
        &self,
        params: &KernelCallParams,
        ctx: &CallContext,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> McpResult<()>;
}
