use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::hash::ContentHash;

#[derive(Debug, Error)]
pub enum StagingError {
    #[error("staging file already closed")]
    Closed,

    #[error("failed to create staging directory: {0}")]
    CreateDir(io::Error),

    #[error("failed to create staging file: {0}")]
    CreateFile(io::Error),

    #[error("write failed: {0}")]
    Write(io::Error),

    #[error("flush failed: {0}")]
    Flush(io::Error),

    #[error("sync failed: {0}")]
    Sync(io::Error),
}

/// A staging ID — same format as ContentHash but generated from random data.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StagingId(String);

impl StagingId {
    pub fn new() -> Self {
        let uuid = Uuid::new_v4();
        let hash_bytes = blake3::hash(uuid.as_bytes());
        let hash_hex = hex::encode(&hash_bytes.as_bytes()[..16]);
        Self(hash_hex)
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
}

impl Default for StagingId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for StagingId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A handle to a staging file for incremental writes.
#[derive(Debug)]
pub struct StagingChunk {
    pub id: StagingId,
    pub path: PathBuf,
    file: Option<File>,
    bytes_written: u64,
}

impl StagingChunk {
    pub(crate) fn create(id: StagingId, path: PathBuf) -> Result<Self, StagingError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(StagingError::CreateDir)?;
        }
        let file = File::create(&path).map_err(StagingError::CreateFile)?;
        Ok(Self {
            id,
            path,
            file: Some(file),
            bytes_written: 0,
        })
    }

    pub fn id(&self) -> &StagingId {
        &self.id
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    pub fn write(&mut self, data: &[u8]) -> Result<(), StagingError> {
        let file = self.file.as_mut().ok_or(StagingError::Closed)?;
        file.write_all(data).map_err(StagingError::Write)?;
        self.bytes_written += data.len() as u64;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), StagingError> {
        if let Some(ref mut file) = self.file {
            file.flush().map_err(StagingError::Flush)?;
        }
        Ok(())
    }

    pub fn sync(&mut self) -> Result<(), StagingError> {
        if let Some(ref mut file) = self.file {
            file.sync_all().map_err(StagingError::Sync)?;
        }
        Ok(())
    }

    pub fn close(&mut self) {
        self.file = None;
    }

    pub fn is_open(&self) -> bool {
        self.file.is_some()
    }
}

/// Result of sealing a staging chunk.
#[derive(Debug, Clone)]
pub struct SealResult {
    pub content_hash: ContentHash,
    pub content_path: PathBuf,
    pub size_bytes: u64,
}

/// Address that can refer to either sealed content or staging.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "id")]
pub enum CasAddress {
    Content(ContentHash),
    Staging(StagingId),
}

impl CasAddress {
    pub fn prefix(&self) -> &str {
        match self {
            CasAddress::Content(hash) => hash.prefix(),
            CasAddress::Staging(id) => id.prefix(),
        }
    }

    pub fn remainder(&self) -> &str {
        match self {
            CasAddress::Content(hash) => hash.remainder(),
            CasAddress::Staging(id) => id.remainder(),
        }
    }

    pub fn is_content(&self) -> bool {
        matches!(self, CasAddress::Content(_))
    }

    pub fn is_staging(&self) -> bool {
        matches!(self, CasAddress::Staging(_))
    }
}

impl std::fmt::Display for CasAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CasAddress::Content(hash) => write!(f, "content:{}", hash),
            CasAddress::Staging(id) => write!(f, "staging:{}", id),
        }
    }
}

impl From<ContentHash> for CasAddress {
    fn from(hash: ContentHash) -> Self {
        CasAddress::Content(hash)
    }
}

impl From<StagingId> for CasAddress {
    fn from(id: StagingId) -> Self {
        CasAddress::Staging(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_staging_id_format() {
        let id = StagingId::new();
        assert_eq!(id.as_str().len(), 32);
        assert!(id.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_staging_id_uniqueness() {
        let id1 = StagingId::new();
        let id2 = StagingId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_staging_id_prefix_remainder() {
        let id = StagingId::new();
        assert_eq!(id.prefix().len(), 2);
        assert_eq!(id.remainder().len(), 30);
        assert_eq!(
            format!("{}{}", id.prefix(), id.remainder()),
            id.as_str()
        );
    }

    #[test]
    fn test_cas_address_display() {
        let content = CasAddress::Content(ContentHash::from_data(b"test"));
        let staging = CasAddress::Staging(StagingId::new());
        assert!(content.to_string().starts_with("content:"));
        assert!(staging.to_string().starts_with("staging:"));
    }
}
