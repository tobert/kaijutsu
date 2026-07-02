//! VFS backends.
//!
//! Backends implement [`VfsOps`] for different storage types.

mod cas;
mod local;
mod memory;

pub use cas::CasFs;
pub use local::LocalBackend;
pub use memory::MemoryBackend;
