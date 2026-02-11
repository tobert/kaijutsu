//! Kaijutsu server library
//!
//! SSH + Cap'n Proto server for kaijutsu.

pub mod auth_db;
pub mod constants;
pub mod context_engine;
pub mod docs_filesystem;
pub mod embedded_kaish;
pub mod git_backend;
pub mod git_filesystem;
pub mod kaish_backend;
pub mod mount_backend;
pub mod rpc;
pub mod ssh;

// Generated Cap'n Proto code
pub mod kaijutsu_capnp {
    include!(concat!(env!("OUT_DIR"), "/kaijutsu_capnp.rs"));
}

pub use auth_db::{AuthDb, User, SshKey};
pub use kaijutsu_kernel::{DriftRouter, ContextHandle, StagedDrift, DriftError};
pub use context_engine::{ContextEngine, ContextManager};
pub use docs_filesystem::KaijutsuFilesystem;
pub use embedded_kaish::EmbeddedKaish;
pub use git_backend::{
    GitCrdtBackend, RepoConfig, ChangeAttribution,
    FileChangeEvent, FileChangeKind, WatcherHandle,
};
pub use git_filesystem::GitFilesystem;
pub use kaish_backend::KaijutsuBackend;
pub use mount_backend::MountBackend;
pub use rpc::WorldImpl;
pub use ssh::{KeySource, SshServer, SshServerConfig};
