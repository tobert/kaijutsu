//! Client side of the council API (`docs/council-api.md`).
//!
//! - [`wire`]: the request and response types, as the OpenAPI file defines them.
//! - [`canon`]: RFC 8785 canonical JSON and spec ids.
//! - [`math`]: per-read and pooled answer math, and [`math::verify`], which
//!   recomputes every derived number of a response.
//! - `client`: the `/council/v1` calls on [`crate::MkClient`].

pub mod canon;
mod client;
pub mod math;
pub mod wire;
