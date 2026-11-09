//! The client for a council v2 server (gogi's `docs/council-api.md`).
//!
//! - [`client`]: [`CouncilClient`], the HTTP transport every call shares, and
//!   [`CouncilError`], which types every failure and never turns one into an
//!   answer.
//! - [`calls`]: the council v2 calls, as [`CouncilClient`] methods. Their
//!   types, rules, spec ids, and pooling math are `gogi_proto`'s.
//! - [`sse`]: the server-sent event decoder a streamed reply is read
//!   through.
//! - [`json`]: a JSON value that keeps object member order.

pub mod client;
pub mod calls;
pub mod json;
pub mod sse;
#[cfg(any(test, feature = "test-util"))]
pub mod test_server;

pub use calls::{GenerationEvent, GenerationStream};
pub use client::{CouncilClient, CouncilError};
pub use json::Json;
