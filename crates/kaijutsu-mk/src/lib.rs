//! The client for the megakernel service (`docs/mk.md`).
//!
//! - [`client`]: [`MkClient`], the HTTP transport every call shares, and
//!   [`MkError`], which types every failure and never turns one into an
//!   answer.
//! - [`council`]: the council API's types, math, and calls.
//! - [`generate`]: `/mk/v1/generate` types, the SSE decoder, and the calls.
//! - [`model`]: the service's model identity and the render call.
//! - [`json`]: a JSON value that keeps object member order.

pub mod client;
#[cfg(test)]
mod conformance;
pub mod council;
pub mod generate;
pub mod json;
pub mod model;
#[cfg(any(test, feature = "test-util"))]
pub mod test_server;

pub use client::{MkClient, MkError, ServiceError};
pub use json::Json;
