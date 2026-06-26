//! File-level tools for CRDT-backed file editing.
//!
//! Provides read, edit, write, glob, and grep tools that operate through
//! the VFS and cache files as CRDT documents. This enables concurrent
//! editing of source files with the same operational semantics as block editing.
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
//! │         File Tool Engines               │
//! │   (read, edit, write, glob, grep)       │
//! └────────────────────┬────────────────────┘
//!                      │
//!            ┌─────────┴──────────┐
//!            ▼                    ▼
//! ┌──────────────────┐  ┌──────────────────┐
//! │ FileDocumentCache │  │ VfsWalkerAdapter │
//! │ (CRDT-backed)     │  │ (kaish-glob)     │
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
