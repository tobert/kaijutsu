//! Typed identifiers for kernels, contexts, principals, and sessions.
//!
//! Re-exported from kaijutsu-types.

pub use kaijutsu_types::{
    ContextId, KernelId, PrefixError, PrefixResolvable, PrincipalId, SessionId,
    resolve_context_prefix, resolve_prefix,
};
