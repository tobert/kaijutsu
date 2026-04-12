use crate::hash::ContentHash;
use serde::{Deserialize, Serialize};

/// Metadata stored alongside CAS objects as a JSON sidecar file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CasMetadata {
    pub mime_type: String,
    pub size: u64,
}

/// Reference to content in the CAS, combining hash with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasReference {
    pub hash: ContentHash,
    pub mime_type: String,
    pub size_bytes: u64,
    pub local_path: Option<String>,
}

impl CasReference {
    pub fn new(hash: ContentHash, mime_type: impl Into<String>, size_bytes: u64) -> Self {
        Self {
            hash,
            mime_type: mime_type.into(),
            size_bytes,
            local_path: None,
        }
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.local_path = Some(path.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cas_metadata_serde() {
        let meta = CasMetadata {
            mime_type: "image/png".to_string(),
            size: 48000,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let restored: CasMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, restored);
    }

    #[test]
    fn test_cas_reference_roundtrip() {
        let hash = ContentHash::from_data(b"test");
        let reference =
            CasReference::new(hash.clone(), "text/plain", 4).with_path("/tmp/cas/ab/cdef");

        let json = serde_json::to_string(&reference).unwrap();
        let restored: CasReference = serde_json::from_str(&json).unwrap();
        assert_eq!(reference.hash, restored.hash);
        assert_eq!(reference.mime_type, restored.mime_type);
        assert_eq!(reference.size_bytes, restored.size_bytes);
        assert_eq!(reference.local_path, restored.local_path);
    }
}
