//! Kaijutsu server library
//!
//! SSH + Cap'n Proto server for kaijutsu.

pub mod auth_db;
pub mod composite_backend;
pub mod constants;
pub mod context_engine;
pub mod embedded_kaish;
pub mod git_backend;
pub mod kaish;
pub mod kaish_backend;
pub mod rpc;
pub mod ssh;

// Generated Cap'n Proto code
pub mod kaijutsu_capnp {
    include!(concat!(env!("OUT_DIR"), "/kaijutsu_capnp.rs"));
}

pub use auth_db::{AuthDb, User, SshKey};
pub use kaijutsu_kernel::{DriftRouter, ContextHandle, StagedDrift, DriftError};
pub use composite_backend::CompositeBackend;
pub use context_engine::{ContextEngine, ContextManager};
pub use embedded_kaish::EmbeddedKaish;
pub use git_backend::{
    GitCrdtBackend, RepoConfig, ChangeAttribution,
    FileChangeEvent, FileChangeKind, WatcherHandle,
};
pub use kaish::KaishProcess;
pub use kaish_backend::KaijutsuBackend;
pub use rpc::WorldImpl;
pub use ssh::{KeySource, SshServer, SshServerConfig};
