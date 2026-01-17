//! Kaish execution engine placeholder.
//!
//! NOTE: Full ExecutionEngine integration is deferred.
//! KaishProcess uses Cap'n Proto RPC which isn't Send/Sync,
//! so it can't implement the ExecutionEngine trait directly.
//!
//! For now, kaish execution is managed directly in rpc.rs.
//! The kernel is used for VFS operations.
//!
//! Future options:
//! - Make kaish-client use a thread-safe RPC approach
//! - Run kaish on a dedicated thread with channel bridge
//! - Use a different IPC mechanism (JSON-RPC, gRPC, etc.)
