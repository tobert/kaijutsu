//! Kaijutsu server library
//!
//! SSH + Cap'n Proto server for kaijutsu.

pub mod rpc;
pub mod ssh;

// Generated Cap'n Proto code
pub mod kaijutsu_capnp {
    include!(concat!(env!("OUT_DIR"), "/kaijutsu_capnp.rs"));
}

pub use rpc::WorldImpl;
pub use ssh::{SshServer, SshServerConfig};
