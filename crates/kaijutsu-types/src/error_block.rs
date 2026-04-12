//! Conversion trait for turning crate-specific errors into [`ErrorPayload`].
//!
//! Each crate implements `IntoErrorPayload` on its own error type. Producers
//! use the payload to mint Error blocks via `BlockSnapshot::error_for()`.

use crate::block::ErrorPayload;

/// Convert a typed error into a structured [`ErrorPayload`].
///
/// Returns the payload only — not a full `BlockSnapshot`. Building a snapshot
/// requires a `BlockId` (and therefore a `ContextId`, `PrincipalId`, and
/// sequence number), which only the producer with a `BlockStore` can mint.
pub trait IntoErrorPayload {
    fn into_error_payload(self) -> ErrorPayload;
}

impl IntoErrorPayload for ErrorPayload {
    fn into_error_payload(self) -> ErrorPayload {
        self
    }
}
