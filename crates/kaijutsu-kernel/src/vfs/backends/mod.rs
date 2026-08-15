//! VFS backends.
//!
//! Backends implement [`VfsOps`] for different storage types.

mod cas;
mod local;
mod memory;
mod roster;
mod share;

pub use cas::CasFs;
pub use local::LocalBackend;
pub use memory::MemoryBackend;
pub use roster::RosterFs;
pub use share::{ShareFs, ShareRegisterError, ShareRegistry, ShareRow, SHARE_OP_TIMEOUT};
