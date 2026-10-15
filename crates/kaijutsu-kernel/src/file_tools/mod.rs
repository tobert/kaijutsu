//! File-level tools for kernel-owned file editing.
//!
//! Supports the file tools in `mcp/servers/file.rs` (read, edit, write, glob,
//! grep): the kernel-document cache, write guard, hashline anchors, path
//! resolution, and the VFS walker.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────┐
//! │         Model / Human / MCP             │
//! └────────────────────┬────────────────────┘
//!                      │ Tool calls (JSON params)
//!                      ▼
//! ┌─────────────────────────────────────────┐
//! │   MCP file tools (mcp/servers/file.rs)  │
//! └────────────────────┬────────────────────┘
//!                      │
//!            ┌─────────┴──────────┐
//!            ▼                    ▼
//! ┌──────────────────┐  ┌──────────────────┐
//! │ FileDocumentCache │  │ VfsWalkerAdapter │
//! │ (kernel-owned)     │  │ (kaish-glob)     │
//! └────────┬─────────┘  └────────┬─────────┘
//!          │                     │
//!          ▼                     ▼
//! ┌──────────────────────────────────────────┐
//! │         VFS (MountTable)                 │
//! └──────────────────────────────────────────┘
//! ```

pub mod cache;
pub mod guard;
pub mod hashline;
pub mod path;
pub mod vfs_walker;

pub use cache::{CacheReadError, FileDocumentCache};
pub use guard::WorkspaceGuard;
pub use vfs_walker::VfsWalkerAdapter;
