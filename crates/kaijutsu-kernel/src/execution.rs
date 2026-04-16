//! Minimal execution types for builtin virtual MCP servers.
//!
//! `ExecContext` and `ExecResult` are the inherent call shape used by engine
//! structs in `block_tools/` and `file_tools/` after Phase 1 M5 removed the
//! legacy `ExecutionEngine` trait and `ToolContext`/`ExecResult` from
//! `tools.rs` (which is gone).
//!
//! These types are internal to the kernel — they live only so the virtual
//! MCP servers can adapt `mcp::CallContext` to an engine call without
//! threading every field individually. The MCP-facing types
//! (`KernelCallParams`, `KernelToolResult`, `CallContext`) remain the only
//! public broker surface.

use std::path::PathBuf;

use kaijutsu_types::{ContextId, KernelId, PrincipalId, SessionId};

/// The subset of `mcp::CallContext` that existing engine bodies read.
///
/// `cwd` is a required path (defaults to `/` in the adapter when
/// `CallContext::cwd` is `None`). Engines that care about virtual-filesystem
/// paths or workspace guard enforcement pull from here.
#[derive(Debug, Clone)]
pub struct ExecContext {
    pub principal_id: PrincipalId,
    pub context_id: ContextId,
    pub cwd: PathBuf,
    pub session_id: SessionId,
    pub kernel_id: KernelId,
}

impl ExecContext {
    pub fn new(
        principal_id: PrincipalId,
        context_id: ContextId,
        cwd: impl Into<PathBuf>,
        session_id: SessionId,
        kernel_id: KernelId,
    ) -> Self {
        Self {
            principal_id,
            context_id,
            cwd: cwd.into(),
            session_id,
            kernel_id,
        }
    }

    pub fn test() -> Self {
        Self {
            principal_id: PrincipalId::new(),
            context_id: ContextId::new(),
            cwd: PathBuf::from("/"),
            session_id: SessionId::new(),
            kernel_id: KernelId::new(),
        }
    }
}

/// Result of an engine call. Mirrors the shape the virtual-server adapter
/// needs to translate into a `KernelToolResult`.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub success: bool,
    pub output: Option<kaijutsu_types::OutputData>,
}

impl ExecResult {
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
            output: None,
        }
    }

    pub fn failure(exit_code: i32, stderr: impl Into<String>) -> Self {
        Self {
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code,
            success: false,
            output: None,
        }
    }

    pub fn with_output_data(mut self, data: kaijutsu_types::OutputData) -> Self {
        self.output = Some(data);
        self
    }
}
