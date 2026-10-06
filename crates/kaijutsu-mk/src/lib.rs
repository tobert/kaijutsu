//! The client for the megakernel service (`docs/mk.md`).
//!
//! - [`client`]: [`MkClient`], the HTTP transport every call shares, and
//!   [`MkError`], which types every failure and never turns one into an
//!   answer.
//! - [`council`]: the council API's types, math, and calls.
//! - [`json`]: a JSON value that keeps object member order.

pub mod client;
pub mod council;
pub mod json;

pub use client::{MkClient, MkError};
pub use json::Json;
