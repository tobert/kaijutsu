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
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod vfs_walker;
pub mod whoami;
pub mod write;

pub use cache::FileDocumentCache;
pub use edit::EditEngine;
pub use glob::GlobEngine;
pub use grep::GrepEngine;
pub use read::ReadEngine;
pub use vfs_walker::VfsWalkerAdapter;
pub use whoami::WhoamiEngine;
pub use write::WriteEngine;
