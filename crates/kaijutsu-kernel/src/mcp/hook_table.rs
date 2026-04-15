//! Hook tables — match-action hook engine seat (§4.3, D-07).
//!
//! **Not evaluated in Phase 1** — the broker owns `HookTables` but the tables
//! start empty and call_tool does not consult them. Phase 4 wires evaluation,
//! `builtin.hooks` admin server, and reentrancy-depth enforcement (D-29).
//!
//! `HookBody::Kaish` is reserved per D-07 / D-08; implementation is a §9
//! follow-up.

use std::sync::Arc;

use async_trait::async_trait;
use kaijutsu_types::{ContextId, PrincipalId};

use super::context::CallContext;
use super::error::{HookId, McpError, McpResult};
use super::types::{KernelCallParams, KernelToolResult};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HookPhase {
    PreCall,
    PostCall,
    OnError,
    OnNotification,
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
pub enum HookBody {
    Builtin(Arc<dyn Hook>),
    Kaish(ScriptRef),
}

impl std::fmt::Debug for HookBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HookBody::Builtin(_) => f.debug_tuple("Builtin").finish(),
            HookBody::Kaish(s) => f.debug_tuple("Kaish").field(&s.id).finish(),
        }
    }
}

/// Hook action: continue the chain, terminate with a result, terminate with an
/// error, or observe and continue (§4.3).
#[derive(Debug)]
pub enum HookAction {
    Invoke(HookBody),
    ShortCircuit(KernelToolResult),
    Deny(McpError),
    Log(LogSpec),
}

#[derive(Debug)]
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
