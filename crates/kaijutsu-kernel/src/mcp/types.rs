//! Kernel newtype wrappers at the broker API boundary (D-10).
//!
//! `rmcp` types are used at the wire — external transport and virtual-server
//! return values. The broker API exposes kaijutsu newtype wrappers so a future
//! rmcp version bump is a single choke point. Conversions live alongside the
//! wrappers.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

/// Stable identifier for an MCP server instance known to the broker.
///
/// Convention: `builtin.<name>` for in-process virtual servers, `<config-key>`
/// for external servers (matches the configured instance name).
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InstanceId(pub String);

impl InstanceId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for InstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for InstanceId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}
impl From<String> for InstanceId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Broker-level view of a single tool advertised by a server.
#[derive(Clone, Debug)]
pub struct KernelTool {
    pub instance: InstanceId,
    pub name: String,
    pub description: Option<String>,
    /// JSON Schema for input params (derived from `schemars` for builtins).
    pub input_schema: serde_json::Value,
}

/// Params for a single tool call through the broker.
#[derive(Clone, Debug)]
pub struct KernelCallParams {
    pub instance: InstanceId,
    pub tool: String,
    pub arguments: serde_json::Value,
}

/// Uniform tool result. `is_error` is the channel by which all LLM-visible
/// failures surface (D-28); non-error completions carry `content` entries.
#[derive(Clone, Debug, Default)]
pub struct KernelToolResult {
    pub is_error: bool,
    pub content: Vec<ToolContent>,
    /// Optional structured payload alongside textual content.
    pub structured: Option<serde_json::Value>,
}

impl KernelToolResult {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            is_error: false,
            content: vec![ToolContent::Text(s.into())],
            structured: None,
        }
    }
    pub fn error_text(s: impl Into<String>) -> Self {
        Self {
            is_error: true,
            content: vec![ToolContent::Text(s.into())],
            structured: None,
        }
    }
}

/// Minimal content shape — expand as needed when servers start returning
/// images/resources. Keeps the kernel API stable across rmcp revs.
#[derive(Clone, Debug)]
pub enum ToolContent {
    Text(String),
    Json(serde_json::Value),
}

/// Health reported by an `McpServerLike` implementation.
#[derive(Clone, Debug)]
pub enum Health {
    Ready,
    Degraded { reason: String },
    Down { reason: String },
}

/// Logging severity on notifications (§4.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Coalescer key axis (§5.3).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NotifKind {
    ToolsChanged,
    PromptsChanged,
    ResourceUpdated,
    Log,
    Elicitation,
}

/// Elicitation request reserved as a notification variant (D-25). Nothing
/// wires this up in Phase 1; the seat exists so external servers can emit
/// without a type-shape break when live handling arrives.
#[derive(Clone, Debug)]
pub struct ElicitationRequest {
    pub message: Cow<'static, str>,
    pub schema: Option<serde_json::Value>,
}

/// Broker-published notification envelope surfaced to downstream consumers.
/// In Phase 1 nothing subscribes; the channel exists so the broker can be
/// wired before Phase 2 adds emission.
#[derive(Clone, Debug)]
pub enum KernelNotification {
    ToolsChanged { instance: InstanceId },
    ResourceUpdated { instance: InstanceId, uri: String },
    PromptsChanged { instance: InstanceId },
    Log {
        instance: InstanceId,
        level: LogLevel,
        message: String,
        tool: Option<String>,
    },
    Coalesced {
        instance: InstanceId,
        kind: NotifKind,
        summary: String,
        count: usize,
    },
}
