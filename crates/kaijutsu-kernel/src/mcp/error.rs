//! Broker-internal error type (§4.5, D-26).
//!
//! Named variants only — no `Other(anyhow::Error)` catch-all. Adding a new
//! category requires a new variant AND a decision entry in
//! `docs/tool-system-redesign.md` §6.
//!
//! LLM-visible failures (D-28) are *not* routed through this type; they
//! arrive at the model as `KernelToolResult { is_error: true, … }`. `McpError`
//! is for broker-internal control flow only; conversion happens at the LLM
//! boundary.

use thiserror::Error;

use super::types::InstanceId;

/// Hook identifier (opaque, stable across restarts of the process).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HookId(pub String);

impl std::fmt::Display for HookId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reasons a policy rejection can fire (§5.5).
#[derive(Clone, Debug, Error)]
pub enum PolicyError {
    #[error("tool call exceeded timeout ({timeout_ms} ms) on {instance}")]
    Timeout { instance: InstanceId, timeout_ms: u64 },
    #[error("concurrency cap reached on {instance} (max {max})")]
    ConcurrencyCap { instance: InstanceId, max: usize },
    #[error("result size {size} bytes exceeded max {max} bytes on {instance}")]
    ResultTooLarge {
        instance: InstanceId,
        size: usize,
        max: usize,
    },
}

/// Coalescer-side errors (§5.3). Seat only in Phase 1 — no emitters yet.
#[derive(Clone, Debug, Error)]
pub enum CoalescerError {
    #[error("coalescer window closed while flushing")]
    WindowClosed,
}

/// Broker-internal errors (§4.5, D-26).
#[derive(Debug, Error)]
pub enum McpError {
    #[error("server does not support this operation")]
    Unsupported,

    #[error("tool `{tool}` not found on instance {instance}")]
    ToolNotFound { instance: InstanceId, tool: String },

    #[error("instance {0} not registered with broker")]
    InstanceNotFound(InstanceId),

    #[error("instance {instance} is down: {reason}")]
    InstanceDown {
        instance: InstanceId,
        reason: String,
    },

    #[error("invalid params: {0}")]
    InvalidParams(#[from] serde_json::Error),

    #[error("mcp protocol error: {0}")]
    Protocol(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("call cancelled")]
    Cancelled,

    #[error("denied by hook {by_hook}")]
    Denied { by_hook: HookId },

    #[error("hook recursion depth exceeded ({depth})")]
    HookRecursionLimit { depth: u32 },

    #[error("coalescer error: {reason}")]
    Coalescer { reason: CoalescerError },

    #[error("policy violation: {0}")]
    Policy(#[from] PolicyError),
}

pub type McpResult<T> = Result<T, McpError>;
