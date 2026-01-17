//! # kaijutsu-kernel
//!
//! Core kernel crate with VFS abstraction for kaijutsu.
//!
//! The kernel is the fundamental primitive. Everything is a kernel.
//! A kernel:
//! - Owns `/` in its VFS (virtual filesystem)
//! - Can mount worktrees, repos, other kernels at paths like `/mnt/project`
//! - Has a lease (who holds "the pen" for mutations)
//! - Has a consent mode (collaborative vs autonomous)
//! - Can checkpoint (distill history into summaries)
//! - Can be forked (heavy copy, isolated) or threaded (light, shared VFS)

pub mod cell_tools;
pub mod control;
pub mod crdt;
pub mod db;
pub mod kernel;
pub mod llm;
pub mod script;
pub mod state;
pub mod tools;
pub mod vfs;

pub use cell_tools::{CellEditEngine, CellListEngine, CellReadEngine};
pub use control::{ConsentMode, ControlPlane, Lease, LeaseHolder};
pub use crdt::{CellDoc, CellId, CellStore, SharedCellStore, shared_cell_store, shared_cell_store_with_db};
pub use db::{CellDb, CellKind, CellMeta, OpRecord, Snapshot};
pub use kernel::Kernel;
pub use script::{HookEvent, HookRegistry, ScriptEngine, ScriptResult};
pub use state::KernelState;
pub use tools::{ExecResult, ExecutionEngine, ToolInfo, ToolRegistry};
pub use llm::{
    AnthropicProvider, CompletionRequest, CompletionResponse, LlmError, LlmProvider, LlmRegistry,
    LlmResult, Message as LlmMessage, ResponseBlock, Role as LlmRole, Usage as LlmUsage,
};
pub use vfs::{
    backends::{LocalBackend, MemoryBackend},
    DirEntry, FileAttr, FileType, MountTable, SetAttr, StatFs, VfsError, VfsOps, VfsResult,
};
