use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

/// A content hash — 128 bits (16 bytes, 32 hex chars) of BLAKE3.
///
/// Deserialization is validated: `try_from = "String"` routes every decode
/// through [`ContentHash::from_str_checked`], so a malformed hash fails loud at
/// the boundary rather than panicking later in `prefix()`/`remainder()`. It
/// still serializes as the bare hex string (`into = "String"`), so the wire and
/// at-rest formats are unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContentHash(String);

#[derive(Debug, Error)]
pub enum HashError {
    #[error("invalid hash length: expected 32 hex chars, got {0}")]
    InvalidLength(usize),

    #[error("invalid hex character in hash")]
    InvalidHex,
}

impl ContentHash {
    pub fn from_data(data: &[u8]) -> Self {
        let hash_bytes = blake3::hash(data);
        let hash_hex = hex::encode(&hash_bytes.as_bytes()[..16]);
        Self(hash_hex)
    }

    pub fn from_str_checked(s: &str) -> Result<Self, HashError> {
        if s.len() != 32 {
            return Err(HashError::InvalidLength(s.len()));
        }
        if !s.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(HashError::InvalidHex);
        }
        Ok(Self(s.to_lowercase()))
    }

    pub fn prefix(&self) -> &str {
        &self.0[0..2]
    }

    pub fn remainder(&self) -> &str {
        &self.0[2..]
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for ContentHash {
    type Err = HashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_str_checked(s)
    }
}

impl TryFrom<String> for ContentHash {
    type Error = HashError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::from_str_checked(&s)
    }
}

impl From<ContentHash> for String {
    fn from(hash: ContentHash) -> Self {
        hash.0
    }
}

impl AsRef<str> for ContentHash {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_data_produces_32_hex_chars() {
        let hash = ContentHash::from_data(b"Hello, World!");
        assert_eq!(hash.as_str().len(), 32);
        assert!(hash.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_from_data_is_deterministic() {
        let hash1 = ContentHash::from_data(b"test data");
        let hash2 = ContentHash::from_data(b"test data");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_from_data_different_input_different_hash() {
        let hash1 = ContentHash::from_data(b"data a");
        let hash2 = ContentHash::from_data(b"data b");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_prefix_and_remainder() {
        let hash = ContentHash::from_data(b"test");
        assert_eq!(hash.prefix().len(), 2);
        assert_eq!(hash.remainder().len(), 30);
        assert_eq!(
            format!("{}{}", hash.prefix(), hash.remainder()),
            hash.as_str()
        );
    }

    #[test]
    fn test_from_str_valid() {
        let hash_str = "abcdef01234567890123456789abcdef";
        let hash: ContentHash = hash_str.parse().unwrap();
        assert_eq!(hash.as_str(), hash_str);
    }

    #[test]
    fn test_from_str_invalid_length() {
        let result: Result<ContentHash, _> = "short".parse();
        assert!(matches!(result, Err(HashError::InvalidLength(5))));
    }

    #[test]
    fn test_from_str_invalid_hex() {
        let result: Result<ContentHash, _> = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".parse();
        assert!(matches!(result, Err(HashError::InvalidHex)));
    }

    #[test]
    fn test_serde_roundtrip() {
        let hash = ContentHash::from_data(b"serde test");
        let json = serde_json::to_string(&hash).unwrap();
        assert_eq!(json, format!("\"{}\"", hash.as_str()));
        let restored: ContentHash = serde_json::from_str(&json).unwrap();
        assert_eq!(hash, restored);
    }

    #[test]
    fn test_deserialize_rejects_short_string() {
        // A malformed hash must not deserialize — otherwise prefix()/remainder()
        // panic on the too-short string at some later call site.
        let result: Result<ContentHash, _> = serde_json::from_str("\"abc\"");
        assert!(result.is_err(), "short hash should fail to deserialize");
    }

    #[test]
    fn test_deserialize_rejects_non_hex() {
        let result: Result<ContentHash, _> =
            serde_json::from_str("\"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz\"");
        assert!(result.is_err(), "non-hex hash should fail to deserialize");
    }

    #[test]
    fn test_deserialize_accepts_valid() {
        let restored: ContentHash =
            serde_json::from_str("\"abcdef01234567890123456789abcdef\"").unwrap();
        assert_eq!(restored.as_str(), "abcdef01234567890123456789abcdef");
    }

    #[test]
    fn test_compatible_with_hootenanny() {
        let hash = ContentHash::from_data(b"Concurrent Data");
        assert_eq!(hash.as_str(), "5c735d76fe3537a0f35cf4a4eb14a532");
    }
}
