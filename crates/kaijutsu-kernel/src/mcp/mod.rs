//! MCP-centric tool system.
//!
//! See `docs/tool-system-redesign.md` for the source of truth. As of Phase 1
//! M5 this module is the sole kernel tool-dispatch path — every tool, builtin
//! or external, speaks `McpServerLike` and every call goes through the
//! `Broker`. The legacy `tools.rs` / `mcp_pool.rs` / MCP FlowBuses are gone.

pub mod binding;
pub mod broker;
pub mod coalescer;
pub mod context;
pub mod error;
pub mod hook_table;
pub mod policy;
pub mod server_like;
pub mod servers;
pub mod types;

pub use binding::{ContextToolBinding, ResolvedName};
pub use broker::Broker;
pub use coalescer::{CoalescePolicy, NotificationCoalescer};
pub use context::{CallContext, TraceContext};
pub use error::{CoalescerError, HookId, McpError, McpResult, PolicyError};
pub use hook_table::{
    GlobPattern, Hook, HookAction, HookBody, HookEntry, HookPhase, HookTable, HookTables, LogSpec,
    ScriptRef,
};
pub use policy::InstancePolicy;
pub use server_like::{McpServerLike, ServerNotification};
pub use types::{
    ElicitationRequest, Health, InstanceId, KernelCallParams, KernelNotification, KernelTool,
    KernelToolResult, LogLevel, NotifKind, ToolContent,
};
