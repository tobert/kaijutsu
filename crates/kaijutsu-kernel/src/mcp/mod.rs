//! MCP-centric tool system (Phase 1 of the redesign).
//!
//! See `docs/tool-system-redesign.md` for the source of truth. This module is
//! the replacement for `tools::ExecutionEngine`, `tools::ToolRegistry`,
//! `block_tools::engines::*`, `file_tools::*`, and `mcp_pool::McpToolEngine`.
//! During Phase 1 the old modules still live alongside; call sites switch in
//! M4, deletions happen in M5.

pub mod binding;
pub mod broker;
pub mod coalescer;
pub mod context;
pub mod error;
pub mod hook_table;
pub mod policy;
pub mod server_like;
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
