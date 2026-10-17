//! Client side of council v2 (`docs/council-api-v2.md`, a draft mkviolet
//! serves).
//!
//! - [`wire`]: the request and response types v2 adds or changes, with the
//!   [`wire::Context`] every read sends whole. Specs, decision answers,
//!   identity, and errors are v1's ([`crate::council::wire`]).
//! - `client`: the `/council/v2` calls, through [`crate::MkClient::v2`].
//!
//! A caller selects v2 only when `GET /council/v2/identity` answers and lists
//! the capabilities it needs; otherwise it keeps to `/council/v1` and
//! `/mk/v1`.

mod client;
pub mod wire;

pub use client::{CouncilV2, GenerationStream};
