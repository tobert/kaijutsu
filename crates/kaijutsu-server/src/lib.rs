//! Kaijutsu server library
//!
//! SSH + Cap'n Proto server for kaijutsu.

pub mod auth_db;
pub mod constants;
pub mod context_engine;
pub mod docs_filesystem;
pub mod embedded_kaish;
pub mod input_filesystem;
pub mod interrupt;
pub mod kaish_backend;
pub mod kj_builtin;
pub mod mount_backend;
pub mod rpc;
pub mod ssh;
pub mod synthesis_rhai;

// Generated Cap'n Proto code
pub mod kaijutsu_capnp {
    include!(concat!(env!("OUT_DIR"), "/kaijutsu_capnp.rs"));
}

pub use auth_db::{AuthDb, SshKeyRecord};
pub use context_engine::ContextEngine;
pub use docs_filesystem::KaijutsuFilesystem;
pub use embedded_kaish::EmbeddedKaish;
pub use input_filesystem::InputFilesystem;
pub use kaijutsu_kernel::{ContextHandle, DriftError, DriftRouter, StagedDrift};
pub use kaish_backend::KaijutsuBackend;
pub use mount_backend::MountBackend;
pub use rpc::{ConnectionState, ServerRegistry, SharedKernel, SharedKernelState, WorldImpl};
pub use ssh::{KeySource, SshServer, SshServerConfig};
