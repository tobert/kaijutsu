//! VFS backends.
//!
//! Backends implement [`VfsOps`] for different storage types.

mod local;
mod memory;

pub use local::LocalBackend;
pub use memory::MemoryBackend;
