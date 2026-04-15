//! Adapters between the new MCP types and the legacy `tools::{ExecResult,
//! ToolContext}` shape. Lives only while M2 delegation wrappers exist; deleted
//! with the old engines at M5.
//!
//! Naming: `to_*` is new → old; `from_*` is old → new.

use std::path::PathBuf;

use crate::tools::{ExecResult, ToolContext};

use super::super::context::CallContext;
use super::super::types::{KernelToolResult, ToolContent};

/// Build a legacy `ToolContext` from a `CallContext`. `cwd` falls back to `/`
/// when `CallContext::cwd` is `None` — the existing engines require a path.
pub fn to_tool_context(ctx: &CallContext) -> ToolContext {
    let cwd = ctx.cwd.clone().unwrap_or_else(|| PathBuf::from("/"));
    ToolContext::new(
        ctx.principal_id,
        ctx.context_id,
        cwd,
        ctx.session_id,
        ctx.kernel_id,
    )
}

/// Translate a legacy `ExecResult` to a `KernelToolResult`. The old engines
/// signal failure via `success = false` with `stderr` populated; we surface
/// that on the MCP surface via `is_error` per D-28.
pub fn from_exec_result(result: ExecResult) -> KernelToolResult {
    if result.success {
        KernelToolResult {
            is_error: false,
            content: vec![ToolContent::Text(result.stdout)],
            structured: None,
        }
    } else {
        // Prefer stderr; fall back to stdout if stderr is empty (some engines
        // populate stdout on failure with a structured error body).
        let body = if !result.stderr.is_empty() {
            result.stderr
        } else {
            result.stdout
        };
        KernelToolResult {
            is_error: true,
            content: vec![ToolContent::Text(body)],
            structured: None,
        }
    }
}
