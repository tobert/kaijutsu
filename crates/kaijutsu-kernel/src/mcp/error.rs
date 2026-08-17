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
use kaijutsu_types::ContextId;

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

    /// A denial with a verdict behind it — a `Deny` hook matched, or an
    /// `Ask` hook's subscriber answered "no". Distinct from
    /// [`McpError::GateUnavailable`] below on purpose (Amy's ruling,
    /// 2026-08-17, `docs/gate-and-shell-split.md` "'Gate unavailable' and
    /// 'denied' must be distinguishable to a model"): this variant means a
    /// human or a rule actually decided.
    #[error("denied by hook {by_hook}")]
    Denied { by_hook: HookId },

    /// An `Ask` hook fired but never reached a verdict: no
    /// `PermissionAsker` subscriber was attached, or nobody answered inside
    /// the timeout. Still fails closed — the call is refused exactly like
    /// `Denied` — but the reason is honest: this is a broken control, not a
    /// "no" (CLAUDE.md: "silent fallbacks are often a mistake"; there is no
    /// adversary inside the trust boundary to hide gate state from, per
    /// `docs/instrument-design.md` "Many hands, one trust boundary", so
    /// nothing is lost by saying so). A model reading this can retry,
    /// escalate through a different channel, or ask a human directly,
    /// instead of learning the wrong lesson ("that action is refused")
    /// from a control that was simply absent.
    #[error("gate for {by_hook} had nothing to answer it: {reason}")]
    GateUnavailable { by_hook: HookId, reason: String },

    #[error("tool `{tool}` on instance {instance} is not in this context's capability allow-set")]
    CapabilityDenied { instance: InstanceId, tool: String },

    #[error("facade `{facade}` is not in this context's capability allow-set")]
    FacadeDenied { facade: String },

    /// A tool call named something that exists somewhere in the broker's
    /// registry, but this context's loadout/binding doesn't grant it — as
    /// distinct from [`McpError::ToolNotFound`], which means the name never
    /// resolved to anything at all (a typo or a hallucinated tool). Both look
    /// identical to `binding.resolve()` (deny-by-default hides the tool the
    /// same way either way), so the dispatch path checks the unfiltered
    /// registry to tell them apart before reporting which one happened.
    #[error(
        "tool `{tool}` denied to context {context}: not granted by its loadout/binding \
         (deny-by-default — see `kj binding show`/`kj binding allow`)"
    )]
    LoadoutDenied { context: ContextId, tool: String },

    /// The context's tool binding could not be read from the kernel DB (a
    /// real storage/IO failure, not "never bound" — that case returns an
    /// empty binding, not an error). Distinct from `CapabilityDenied` so a
    /// storage fault doesn't masquerade as a deliberate capability decision.
    #[error("could not load context {context}'s tool binding: {reason}")]
    BindingUnavailable { context: ContextId, reason: String },

    #[error("hook recursion depth exceeded ({depth})")]
    HookRecursionLimit { depth: u32 },

    #[error("coalescer error: {reason}")]
    Coalescer { reason: CoalescerError },

    #[error("policy violation: {0}")]
    Policy(#[from] PolicyError),
}

pub type McpResult<T> = Result<T, McpError>;
