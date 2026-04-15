//! Adapters between MCP broker types and the internal engine call shape.
//!
//! Virtual MCP servers hold engine structs (e.g. `BlockCreateEngine`) and
//! delegate to their inherent `execute(params_json, &ExecContext)` methods.
//! This module bridges `CallContext` → `ExecContext` (adding the `cwd`
//! default) and `ExecResult` → `KernelToolResult` (collapsing onto the D-28
//! `is_error` channel).

use std::path::PathBuf;

use crate::execution::{ExecContext, ExecResult};

use super::super::context::CallContext;
use super::super::types::{KernelToolResult, ToolContent};

/// Build an `ExecContext` from a `CallContext`. `cwd` falls back to `/` when
/// `CallContext::cwd` is `None` — engines expect a concrete path.
pub fn to_exec_context(ctx: &CallContext) -> ExecContext {
    let cwd = ctx.cwd.clone().unwrap_or_else(|| PathBuf::from("/"));
    ExecContext::new(
        ctx.principal_id,
        ctx.context_id,
        cwd,
        ctx.session_id,
        ctx.kernel_id,
    )
}

/// Translate an engine `ExecResult` to a `KernelToolResult`. `success = false`
/// with `stderr` populated becomes `is_error = true` per D-28.
pub fn from_exec_result(result: ExecResult) -> KernelToolResult {
    if result.success {
        KernelToolResult {
            is_error: false,
            content: vec![ToolContent::Text(result.stdout)],
            structured: None,
        }
    } else {
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
