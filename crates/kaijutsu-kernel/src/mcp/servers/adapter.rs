//! Adapters between MCP broker types and the internal call shape.
//!
//! Virtual MCP servers inline their execution logic directly.
//! This module bridges `CallContext` → `ExecContext` (adding the `cwd`
//! default) and `ExecResult` → `KernelToolResult` (collapsing onto the D-28
//! `is_error` channel).

use crate::execution::{ExecContext, ExecResult};

use super::super::context::CallContext;
use super::super::types::{KernelToolResult, ToolContent};

/// Build an `ExecContext` from a `CallContext`. `cwd` passes straight
/// through — a `None` here means the context genuinely has no working
/// directory, and it is up to each engine to reject rather than fabricate
/// one (fabricating `/` is exactly the bug this function used to have; see
/// `mcp::servers::file`'s up-front `None` rejection).
pub fn to_exec_context(ctx: &CallContext) -> ExecContext {
    ExecContext {
        principal_id: ctx.principal_id,
        context_id: ctx.context_id,
        cwd: ctx.cwd.clone(),
        session_id: ctx.session_id,
        kernel_id: ctx.kernel_id,
    }
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
