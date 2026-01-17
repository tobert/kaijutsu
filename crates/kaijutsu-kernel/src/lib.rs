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

pub mod control;
pub mod kernel;
pub mod state;
pub mod tools;
pub mod vfs;

pub use control::{ConsentMode, ControlPlane, Lease, LeaseHolder};
pub use kernel::Kernel;
pub use state::KernelState;
pub use tools::{ExecutionEngine, ToolInfo, ToolRegistry};
pub use vfs::{
    backends::{LocalBackend, MemoryBackend},
    DirEntry, FileAttr, FileType, MountTable, SetAttr, StatFs, VfsError, VfsOps, VfsResult,
};
