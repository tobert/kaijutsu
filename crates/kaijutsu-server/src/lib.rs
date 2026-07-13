//! Kaijutsu server library
//!
//! SSH + Cap'n Proto server for kaijutsu.

pub mod auth_db;
pub mod beat;
pub mod clock;
pub mod constants;
pub mod interrupt;
pub mod llm_stream;
pub mod rpc;
pub mod sftp;
pub mod share;
pub mod ssh;

// Generated Cap'n Proto code
pub mod kaijutsu_capnp {
    include!(concat!(env!("OUT_DIR"), "/kaijutsu_capnp.rs"));
}

pub use auth_db::{AuthDb, SshKeyRecord};
pub use kaijutsu_kernel::runtime::docs_filesystem::KaijutsuFilesystem;
pub use kaijutsu_kernel::runtime::embedded_kaish::EmbeddedKaish;
pub use kaijutsu_kernel::runtime::input_filesystem::InputFilesystem;
pub use kaijutsu_kernel::runtime::kaish_backend::KaijutsuBackend;
pub use kaijutsu_kernel::runtime::mount_backend::MountBackend;
pub use kaijutsu_kernel::{ContextHandle, DriftError, DriftRouter, StagedDrift};
pub use rpc::{ConnectionState, ServerRegistry, SharedKernel, SharedKernelState, WorldImpl};
pub use ssh::{KeySource, SshServer, SshServerConfig};
