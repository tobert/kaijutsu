//! Typed identifiers for kernels, contexts, principals, and sessions.
//!
//! Re-exported from kaijutsu-types. This module adds shim functions for
//! legacy document-ID-based ContextId recovery (orphan rule prevents
//! adding inherent impls on the foreign type).

// Re-export all ID types and prefix resolution from kaijutsu-types.
pub use kaijutsu_types::{
    ContextId, KernelId, PrefixError, PrefixResolvable, PrincipalId, SessionId,
    resolve_context_prefix, resolve_prefix,
};

/// Fixed namespace for deriving stable ContextIds from document IDs via UUIDv5.
const KAIJUTSU_CONTEXT_NS: uuid::Uuid = uuid::uuid!("b47d0a58-11b0-4694-9fbc-803f9f78c17d");

/// Derive a stable ContextId from a document ID.
/// Same input always produces the same output (UUIDv5 deterministic hash).
///
/// Free function because ContextId is defined in kaijutsu-types (orphan rule).
pub fn context_id_from_document_id(document_id: &str) -> ContextId {
    let uuid = uuid::Uuid::new_v5(&KAIJUTSU_CONTEXT_NS, document_id.as_bytes());
    ContextId::from_bytes(*uuid.as_bytes())
}

/// Recover a ContextId from a context name (the part after `@` in a document ID).
///
/// If the name is a valid UUID, parse it — preserving the original identity
/// (e.g. a CC session UUID used as a context name).
/// Otherwise, derive a stable ID from the full document ID via UUIDv5.
///
/// Free function because ContextId is defined in kaijutsu-types (orphan rule).
pub fn context_id_recover(context_name: &str, document_id: &str) -> ContextId {
    match uuid::Uuid::parse_str(context_name) {
        Ok(uuid) => ContextId::from_bytes(*uuid.as_bytes()),
        Err(_) => context_id_from_document_id(document_id),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_document_id_is_deterministic() {
        let a = context_id_from_document_id("kernel-1@main");
        let b = context_id_from_document_id("kernel-1@main");
        assert_eq!(a, b);
    }

    #[test]
    fn test_from_document_id_different_inputs() {
        let a = context_id_from_document_id("kernel-1@main");
        let b = context_id_from_document_id("kernel-1@debug");
        assert_ne!(a, b);
    }

    #[test]
    fn test_recover_parses_uuid_context_name() {
        let original = uuid::Uuid::parse_str("a1b2c3d4-e5f6-4789-abcd-ef0123456789").unwrap();
        let context_name = original.to_string();
        let doc_id = format!("kernel-1@{}", context_name);

        let recovered = context_id_recover(&context_name, &doc_id);
        assert_eq!(recovered, ContextId::from_bytes(*original.as_bytes()));
    }

    #[test]
    fn test_recover_derives_for_non_uuid_name() {
        let recovered = context_id_recover("main", "kernel-1@main");
        let derived = context_id_from_document_id("kernel-1@main");
        assert_eq!(recovered, derived);
    }

    #[test]
    fn test_recover_roundtrips_context_id() {
        let original = ContextId::new();
        let context_name = original.to_string();
        let doc_id = format!("kernel-1@{}", context_name);

        let recovered = context_id_recover(&context_name, &doc_id);
        assert_eq!(recovered, original);
    }
}
