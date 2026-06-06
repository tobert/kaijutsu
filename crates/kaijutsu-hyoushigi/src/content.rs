//! Content references and the basis (equivalence-class) digest.

use kaijutsu_cas::{CasReference, ContentHash};
use serde::{Deserialize, Serialize};

/// A reference to a cell's content: a CAS hash plus an open-string MIME.
///
/// This is the *cell-body contract* — hash + mime, nothing else. The hash is a
/// real [`ContentHash`], so a malformed hash crashes at construction, not deep
/// in a lookup. The MIME is an **open** label, opaque to the substrate (which
/// never switches on it); the closed `ContentType` render hint is *derived* from
/// it at materialization (`ContentType::from_mime`), unknown → `Plain`. The
/// content store and the memoization key are the same thing: identical inputs →
/// identical hash → no recompute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRef {
    pub hash: ContentHash,
    pub mime: String,
}

impl ContentRef {
    pub fn new(hash: ContentHash, mime: impl Into<String>) -> Self {
        Self {
            hash,
            mime: mime.into(),
        }
    }

    /// Hash literal bytes into a `ContentRef` — the common "produce content" path.
    pub fn of(bytes: &[u8], mime: impl Into<String>) -> Self {
        Self {
            hash: ContentHash::from_data(bytes),
            mime: mime.into(),
        }
    }
}

/// `kaijutsu-cas`'s `CasReference` is *one way* to satisfy the cell-body
/// contract; it carries `size_bytes`/`local_path` a cell doesn't, which we drop.
impl From<CasReference> for ContentRef {
    fn from(r: CasReference) -> Self {
        Self {
            hash: r.hash,
            mime: r.mime_type,
        }
    }
}

/// A digest of the *equivalence class* a speculation was computed against.
///
/// `compute_basis` snapshots this at `speculate_at`; at `commit_deadline` the
/// engine recomputes it against current context and commits iff it matches, else
/// **squashes**. Reuses BLAKE3 via [`ContentHash`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHash(ContentHash);

impl ContextHash {
    /// Digest the serialized context slice that defines the equivalence class.
    pub fn of(bytes: &[u8]) -> Self {
        Self(ContentHash::from_data(bytes))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_ref_of_is_deterministic() {
        let a = ContentRef::of(b"beat-1", "audio/midi");
        let b = ContentRef::of(b"beat-1", "audio/midi");
        assert_eq!(a, b);
        assert_eq!(a.mime, "audio/midi");
    }

    #[test]
    fn content_ref_from_cas_reference_drops_path() {
        let hash = ContentHash::from_data(b"x");
        let cas = CasReference::new(hash.clone(), "image/png", 3).with_path("/tmp/cas/ab/cd");
        let cref: ContentRef = cas.into();
        assert_eq!(cref.hash, hash);
        assert_eq!(cref.mime, "image/png");
    }

    #[test]
    fn context_hash_classes_match_on_equal_input() {
        assert_eq!(ContextHash::of(b"ctx"), ContextHash::of(b"ctx"));
        assert_ne!(ContextHash::of(b"ctx"), ContextHash::of(b"ctx'"));
    }
}
