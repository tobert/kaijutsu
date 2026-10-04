//! Client side of the council API (`docs/council-api.md`).
//!
//! - [`wire`]: the request and response types, as the OpenAPI file defines them.
//! - [`canon`]: RFC 8785 canonical JSON and spec ids.
//! - [`math`]: per-read and pooled answer math, and [`math::verify`], which
//!   recomputes every derived number of a response.
//! - [`client`]: an async HTTP client that maps every failure to a typed error
//!   and never turns one into an answer.

pub mod canon;
pub mod client;
pub mod json;
pub mod math;
pub mod wire;

pub use client::{CouncilClient, CouncilError};
pub use json::Json;
