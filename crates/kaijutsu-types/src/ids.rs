//! Typed identifiers for principals, kernels, contexts, and sessions.
//!
//! All ID types wrap UUIDv7 (time-ordered, globally unique). They're opaque on
//! the wire (16 bytes / `Data` in Cap'n Proto) and display as standard UUID text
//! for logging. The `short()` form (first 8 hex chars) is for human-facing UI —
//! never used as a lookup key.
//!
//! `PrincipalId` also has a deterministic sentinel via `PrincipalId::system()`,
//! derived from UUIDv5 for kernel-generated blocks.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A principal identifier (UUIDv7, or UUIDv5 for sentinels).
#[derive(Clone, Copy, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrincipalId(uuid::Uuid);

/// A kernel identifier (UUIDv7).
#[derive(Clone, Copy, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KernelId(uuid::Uuid);

/// A context identifier (UUIDv7).
#[derive(Clone, Copy, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContextId(uuid::Uuid);

/// A session identifier (UUIDv7).
#[derive(Clone, Copy, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(uuid::Uuid);

// ── Shared behavior ─────────────────────────────────────────────────────────

macro_rules! impl_typed_id {
    ($T:ident, $name:literal) => {
        impl $T {
            /// Create a new time-ordered ID (UUIDv7).
            pub fn new() -> Self {
                Self(uuid::Uuid::now_v7())
            }

            /// First 8 hex characters — for human display only, not lookup.
            pub fn short(&self) -> String {
                self.0.as_simple().to_string()[..8].to_string()
            }

            /// Full 32-character hex string (no hyphens).
            pub fn to_hex(&self) -> String {
                self.0.as_simple().to_string()
            }

            /// The raw 16 bytes.
            pub fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }

            /// Reconstruct from 16 bytes.
            pub fn from_bytes(b: [u8; 16]) -> Self {
                Self(uuid::Uuid::from_bytes(b))
            }

            /// Try to reconstruct from a byte slice (must be exactly 16 bytes).
            pub fn try_from_slice(b: &[u8]) -> Option<Self> {
                if b.len() == 16 {
                    let mut arr = [0u8; 16];
                    arr.copy_from_slice(b);
                    Some(Self::from_bytes(arr))
                } else {
                    None
                }
            }

            /// Parse from a hex string (32 chars, no hyphens) or standard UUID format.
            pub fn parse(s: &str) -> Result<Self, uuid::Error> {
                uuid::Uuid::parse_str(s).map(Self)
            }

            /// Prefer a label for display; fall back to short hex.
            pub fn display_or(&self, label: Option<&str>) -> String {
                match label {
                    Some(l) if !l.is_empty() => l.to_string(),
                    _ => self.short(),
                }
            }

            /// Check if a query string matches this ID by hex prefix.
            pub fn matches_hex_prefix(&self, prefix: &str) -> bool {
                self.to_hex().starts_with(prefix)
            }

            /// A nil / zero ID — for sentinel values only.
            pub fn nil() -> Self {
                Self(uuid::Uuid::nil())
            }

            /// Check if this is the nil ID.
            pub fn is_nil(&self) -> bool {
                self.0.is_nil()
            }
        }

        impl Default for $T {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<uuid::Uuid> for $T {
            fn from(u: uuid::Uuid) -> Self {
                Self(u)
            }
        }

        impl From<$T> for uuid::Uuid {
            fn from(id: $T) -> uuid::Uuid {
                id.0
            }
        }

        impl From<[u8; 16]> for $T {
            fn from(b: [u8; 16]) -> Self {
                Self::from_bytes(b)
            }
        }

        impl From<$T> for [u8; 16] {
            fn from(id: $T) -> [u8; 16] {
                *id.as_bytes()
            }
        }

        impl fmt::Display for $T {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // Full UUID with hyphens for log readability
                write!(f, "{}", self.0)
            }
        }

        impl fmt::Debug for $T {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", $name, self.short())
            }
        }
    };
}

impl_typed_id!(PrincipalId, "PrincipalId");
impl_typed_id!(KernelId, "KernelId");
impl_typed_id!(ContextId, "ContextId");
impl_typed_id!(SessionId, "SessionId");

// ── PrincipalId sentinels ───────────────────────────────────────────────────

/// Fixed namespace for deriving deterministic PrincipalIds via UUIDv5.
const KAIJUTSU_PRINCIPAL_NS: uuid::Uuid =
    uuid::uuid!("e8a3c6f1-7b2d-4e90-a5f8-1c9d0e3b4a67");

impl PrincipalId {
    /// The well-known "system" principal.
    ///
    /// Used for kernel-generated blocks (shell output, system messages, etc.).
    /// Deterministic: same value every time (UUIDv5 derived from `b"system"`).
    pub fn system() -> Self {
        Self(uuid::Uuid::new_v5(&KAIJUTSU_PRINCIPAL_NS, b"system"))
    }
}

// ── Prefix resolution ───────────────────────────────────────────────────────

/// Error from ambiguous prefix resolution.
#[derive(Debug, thiserror::Error)]
pub enum PrefixError {
    #[error("no match for prefix '{0}'")]
    NoMatch(String),
    #[error("ambiguous prefix '{prefix}': matches {candidates:?}")]
    Ambiguous {
        prefix: String,
        candidates: Vec<String>,
    },
}

/// Resolve a query string against a set of context IDs and optional labels.
///
/// Resolution order:
/// 1. Exact label match
/// 2. Unique label prefix match
/// 3. Unique hex prefix match
/// 4. Error (no match or ambiguous)
pub fn resolve_context_prefix<'a>(
    contexts: impl Iterator<Item = (ContextId, Option<&'a str>)>,
    query: &str,
) -> Result<ContextId, PrefixError> {
    let entries: Vec<(ContextId, Option<&str>)> = contexts.collect();

    // 1. Exact label match
    for &(id, label) in &entries {
        if let Some(l) = label
            && l == query
        {
            return Ok(id);
        }
    }

    // 2. Unique label prefix match
    let label_matches: Vec<(ContextId, &str)> = entries
        .iter()
        .filter_map(|&(id, label)| {
            label.and_then(|l| {
                if l.starts_with(query) {
                    Some((id, l))
                } else {
                    None
                }
            })
        })
        .collect();

    if label_matches.len() == 1 {
        return Ok(label_matches[0].0);
    }
    if label_matches.len() > 1 {
        return Err(PrefixError::Ambiguous {
            prefix: query.to_string(),
            candidates: label_matches.iter().map(|(_, l)| l.to_string()).collect(),
        });
    }

    // 3. Unique hex prefix match
    let hex_matches: Vec<ContextId> = entries
        .iter()
        .filter(|(id, _)| id.matches_hex_prefix(query))
        .map(|(id, _)| *id)
        .collect();

    match hex_matches.len() {
        0 => Err(PrefixError::NoMatch(query.to_string())),
        1 => Ok(hex_matches[0]),
        _ => Err(PrefixError::Ambiguous {
            prefix: query.to_string(),
            candidates: hex_matches.iter().map(|id| id.short()).collect(),
        }),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── Basic ID operations ─────────────────────────────────────────────

    #[test]
    fn test_new_is_unique() {
        let a = ContextId::new();
        let b = ContextId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn test_short_is_8_chars() {
        let id = KernelId::new();
        assert_eq!(id.short().len(), 8);
    }

    #[test]
    fn test_hex_is_32_chars() {
        let id = ContextId::new();
        assert_eq!(id.to_hex().len(), 32);
    }

    #[test]
    fn test_roundtrip_bytes() {
        let id = ContextId::new();
        let bytes = *id.as_bytes();
        let id2 = ContextId::from_bytes(bytes);
        assert_eq!(id, id2);
    }

    #[test]
    fn test_try_from_slice() {
        let id = KernelId::new();
        let bytes = id.as_bytes().as_slice();
        assert_eq!(KernelId::try_from_slice(bytes), Some(id));
        assert_eq!(KernelId::try_from_slice(&[0u8; 15]), None);
        assert_eq!(KernelId::try_from_slice(&[0u8; 17]), None);
    }

    #[test]
    fn test_parse_hex() {
        let id = ContextId::new();
        let hex = id.to_hex();
        let parsed = ContextId::parse(&hex).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_parse_uuid_format() {
        let id = ContextId::new();
        let uuid_str = id.to_string(); // has hyphens
        let parsed = ContextId::parse(&uuid_str).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_display_or() {
        let id = ContextId::new();
        assert_eq!(id.display_or(Some("main")), "main");
        assert_eq!(id.display_or(Some("")), id.short());
        assert_eq!(id.display_or(None), id.short());
    }

    #[test]
    fn test_nil() {
        let id = ContextId::nil();
        assert!(id.is_nil());
        assert!(!ContextId::new().is_nil());
    }

    #[test]
    fn test_ordering_is_time_ordered() {
        let ids: Vec<ContextId> = (0..10).map(|_| ContextId::new()).collect();
        for i in 1..ids.len() {
            assert!(ids[i] >= ids[i - 1]);
        }
    }

    // ── Serde roundtrips ────────────────────────────────────────────────

    #[test]
    fn test_serde_roundtrip_context_id() {
        let id = ContextId::new();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: ContextId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_serde_roundtrip_kernel_id() {
        let id = KernelId::new();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: KernelId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_serde_roundtrip_principal_id() {
        let id = PrincipalId::new();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: PrincipalId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_serde_roundtrip_session_id() {
        let id = SessionId::new();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    // ── Postcard roundtrips ─────────────────────────────────────────────

    #[test]
    fn test_postcard_roundtrip_context_id() {
        let id = ContextId::new();
        let bytes = postcard::to_stdvec(&id).unwrap();
        let parsed: ContextId = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_postcard_roundtrip_kernel_id() {
        let id = KernelId::new();
        let bytes = postcard::to_stdvec(&id).unwrap();
        let parsed: KernelId = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_postcard_roundtrip_principal_id() {
        let id = PrincipalId::new();
        let bytes = postcard::to_stdvec(&id).unwrap();
        let parsed: PrincipalId = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_postcard_roundtrip_session_id() {
        let id = SessionId::new();
        let bytes = postcard::to_stdvec(&id).unwrap();
        let parsed: SessionId = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(id, parsed);
    }

    // ── PrincipalId::system() ───────────────────────────────────────────

    #[test]
    fn test_system_principal_is_deterministic() {
        let a = PrincipalId::system();
        let b = PrincipalId::system();
        assert_eq!(a, b);
    }

    #[test]
    fn test_system_principal_differs_from_new() {
        let system = PrincipalId::system();
        let fresh = PrincipalId::new();
        assert_ne!(system, fresh);
    }

    #[test]
    fn test_system_principal_is_not_nil() {
        assert!(!PrincipalId::system().is_nil());
    }

    // ── Type safety (distinct newtypes) ─────────────────────────────────

    #[test]
    fn test_type_safety_distinct_newtypes() {
        // Same underlying bytes, but different types must not be interchangeable.
        // We can't test compile-time errors, but we verify they don't accidentally
        // share identity through serialization.
        let bytes = *ContextId::new().as_bytes();
        let ctx = ContextId::from_bytes(bytes);
        let kern = KernelId::from_bytes(bytes);
        let princ = PrincipalId::from_bytes(bytes);
        let sess = SessionId::from_bytes(bytes);

        // All have the same underlying bytes...
        assert_eq!(ctx.as_bytes(), kern.as_bytes());
        assert_eq!(kern.as_bytes(), princ.as_bytes());
        assert_eq!(princ.as_bytes(), sess.as_bytes());

        // ...but Debug format shows the type name
        assert!(format!("{:?}", ctx).starts_with("ContextId("));
        assert!(format!("{:?}", kern).starts_with("KernelId("));
        assert!(format!("{:?}", princ).starts_with("PrincipalId("));
        assert!(format!("{:?}", sess).starts_with("SessionId("));
    }

    // ── Display / Debug formatting ──────────────────────────────────────

    #[test]
    fn test_display_is_full_uuid_with_hyphens() {
        let id = ContextId::new();
        let displayed = id.to_string();
        // Standard UUID format: 8-4-4-4-12
        assert_eq!(displayed.len(), 36);
        assert_eq!(displayed.chars().filter(|c| *c == '-').count(), 4);
    }

    #[test]
    fn test_debug_shows_type_and_short() {
        let id = KernelId::new();
        let debug = format!("{:?}", id);
        assert!(debug.starts_with("KernelId("));
        assert!(debug.ends_with(')'));
        // The inner part is the 8-char short form
        let inner = &debug["KernelId(".len()..debug.len() - 1];
        assert_eq!(inner.len(), 8);
    }

    // ── Prefix resolution ───────────────────────────────────────────────

    #[test]
    fn test_resolve_exact_label() {
        let a = ContextId::new();
        let b = ContextId::new();
        let entries = vec![(a, Some("main")), (b, Some("debug"))];
        let result = resolve_context_prefix(entries.into_iter(), "main").unwrap();
        assert_eq!(result, a);
    }

    #[test]
    fn test_resolve_label_prefix() {
        let a = ContextId::new();
        let b = ContextId::new();
        let entries = vec![(a, Some("main-session")), (b, Some("debug"))];
        let result = resolve_context_prefix(entries.into_iter(), "main").unwrap();
        assert_eq!(result, a);
    }

    #[test]
    fn test_resolve_hex_prefix() {
        let a = ContextId::new();
        let b = ContextId::new();
        let prefix = &a.to_hex()[..6];
        let entries = vec![(a, None), (b, None)];
        let result = resolve_context_prefix(entries.into_iter(), prefix);
        match result {
            Ok(id) => assert_eq!(id, a),
            Err(PrefixError::Ambiguous { .. }) => {} // possible with UUIDv7
            Err(e) => panic!("unexpected error: {}", e),
        }
    }

    #[test]
    fn test_resolve_no_match() {
        let a = ContextId::new();
        let entries = vec![(a, Some("main"))];
        let result = resolve_context_prefix(entries.into_iter(), "nonexistent");
        assert!(matches!(result, Err(PrefixError::NoMatch(_))));
    }

    #[test]
    fn test_resolve_ambiguous_label() {
        let a = ContextId::new();
        let b = ContextId::new();
        let entries = vec![(a, Some("main-1")), (b, Some("main-2"))];
        let result = resolve_context_prefix(entries.into_iter(), "main");
        assert!(matches!(result, Err(PrefixError::Ambiguous { .. })));
    }

    // ── From conversions ────────────────────────────────────────────────

    #[test]
    fn test_from_uuid() {
        let u = uuid::Uuid::now_v7();
        let id = ContextId::from(u);
        let back: uuid::Uuid = id.into();
        assert_eq!(u, back);
    }

    #[test]
    fn test_from_bytes_array() {
        let bytes: [u8; 16] = *ContextId::new().as_bytes();
        let id = PrincipalId::from(bytes);
        let back: [u8; 16] = id.into();
        assert_eq!(bytes, back);
    }

    #[test]
    fn test_from_conversions_preserve_identity() {
        let original = KernelId::new();
        let uuid: uuid::Uuid = original.into();
        let roundtripped = KernelId::from(uuid);
        assert_eq!(original, roundtripped);
    }
}
