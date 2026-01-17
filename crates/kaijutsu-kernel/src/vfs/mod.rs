//! Virtual Filesystem abstraction.
//!
//! This module provides a path-based VFS designed for RPC exposure.
//! Key components:
//!
//! - [`VfsOps`] - Core trait for filesystem operations
//! - [`MountTable`] - Routes operations to backends based on path
//! - [`MemoryBackend`] - In-memory filesystem (for /scratch, testing)
//! - [`LocalBackend`] - Local filesystem access (with path security)
//!
//! ## Design Decisions
//!
//! - **Path-based, no inodes**: Operations use paths, not inode numbers.
//!   FUSE clients handle inode ↔ path mapping locally.
//! - **Explicit offset/size**: Read/write take offset and size for
//!   efficient RPC without handle state.
//! - **Longest-prefix routing**: MountTable routes to the most specific
//!   mount point that matches.

pub mod backends;
mod error;
mod mount;
mod ops;
mod types;

pub use backends::{LocalBackend, MemoryBackend};
pub use error::{VfsError, VfsResult};
pub use mount::{MountInfo, MountTable};
pub use ops::VfsOps;
pub use types::{DirEntry, FileAttr, FileType, OpenFlags, SetAttr, StatFs};
