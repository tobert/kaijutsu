use std::fs;
use std::path::PathBuf;

use thiserror::Error;

use crate::config::CasConfig;
use crate::hash::ContentHash;
use crate::metadata::{CasMetadata, CasReference};
use crate::staging::{CasAddress, SealResult, StagingChunk, StagingError, StagingId};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("CAS is in read-only mode")]
    ReadOnly,

    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to write object {path}: {source}")]
    WriteObject {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to read object {path}: {source}")]
    ReadObject {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to read staging file {path}: {source}")]
    ReadStaging {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to rename staging file: {0}")]
    Rename(std::io::Error),

    #[error("failed to copy staging file: {0}")]
    Copy(std::io::Error),

    #[error("failed to remove file {path}: {source}")]
    Remove {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("metadata serialization failed: {0}")]
    MetadataSerde(serde_json::Error),

    #[error("metadata write failed for {path}: {source}")]
    MetadataWrite {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("metadata read failed for {path}: {source}")]
    MetadataRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("staging error: {0}")]
    Staging(#[from] StagingError),
}

/// Trait for content storage backends.
pub trait ContentStore: Send + Sync {
    fn store(&self, data: &[u8], mime_type: &str) -> Result<ContentHash, StoreError>;
    fn retrieve(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>, StoreError>;
    fn exists(&self, hash: &ContentHash) -> bool;
    fn path(&self, hash: &ContentHash) -> Option<PathBuf>;
    fn inspect(&self, hash: &ContentHash) -> Result<Option<CasReference>, StoreError>;
    fn remove(&self, hash: &ContentHash) -> Result<bool, StoreError>;
}

/// Filesystem-based content store with directory sharding.
#[derive(Debug, Clone)]
pub struct FileStore {
    config: CasConfig,
}

impl FileStore {
    /// Create a new FileStore. Directories are created lazily on first write.
    pub fn new(config: CasConfig) -> Self {
        Self { config }
    }

    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self::new(CasConfig::with_base_path(path))
    }

    pub fn read_only_at(path: impl Into<PathBuf>) -> Self {
        Self::new(CasConfig::read_only(path))
    }

    pub fn config(&self) -> &CasConfig {
        &self.config
    }

    fn object_path(&self, hash: &ContentHash) -> PathBuf {
        self.config
            .objects_dir()
            .join(hash.prefix())
            .join(hash.remainder())
    }

    fn metadata_path(&self, hash: &ContentHash) -> PathBuf {
        self.config
            .metadata_dir()
            .join(hash.prefix())
            .join(format!("{}.json", hash.remainder()))
    }

    fn staging_path(&self, id: &StagingId) -> PathBuf {
        self.config
            .staging_dir()
            .join(id.prefix())
            .join(id.remainder())
    }

    fn ensure_parent(&self, path: &PathBuf) -> Result<(), StoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| StoreError::CreateDir {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        Ok(())
    }

    fn write_metadata(
        &self,
        hash: &ContentHash,
        mime_type: &str,
        size: u64,
    ) -> Result<(), StoreError> {
        if !self.config.store_metadata {
            return Ok(());
        }
        let meta_path = self.metadata_path(hash);
        if meta_path.exists() {
            return Ok(());
        }
        self.ensure_parent(&meta_path)?;
        let metadata = CasMetadata {
            mime_type: mime_type.to_string(),
            size,
        };
        let json = serde_json::to_string(&metadata).map_err(StoreError::MetadataSerde)?;
        fs::write(&meta_path, json).map_err(|e| StoreError::MetadataWrite {
            path: meta_path,
            source: e,
        })
    }

    pub fn create_staging(&self) -> Result<StagingChunk, StoreError> {
        if self.config.read_only {
            return Err(StoreError::ReadOnly);
        }
        let id = StagingId::new();
        let path = self.staging_path(&id);
        Ok(StagingChunk::create(id, path)?)
    }

    pub fn create_staging_with_id(&self, id: StagingId) -> Result<StagingChunk, StoreError> {
        if self.config.read_only {
            return Err(StoreError::ReadOnly);
        }
        let path = self.staging_path(&id);
        Ok(StagingChunk::create(id, path)?)
    }

    pub fn staging_path_for(&self, id: &StagingId) -> PathBuf {
        self.staging_path(id)
    }

    pub fn seal(&self, chunk: &StagingChunk, mime_type: &str) -> Result<SealResult, StoreError> {
        self.seal_path(&chunk.path, mime_type)
    }

    pub fn seal_path(
        &self,
        staging_path: &PathBuf,
        mime_type: &str,
    ) -> Result<SealResult, StoreError> {
        if self.config.read_only {
            return Err(StoreError::ReadOnly);
        }

        let data = fs::read(staging_path).map_err(|e| StoreError::ReadStaging {
            path: staging_path.clone(),
            source: e,
        })?;
        let content_hash = ContentHash::from_data(&data);
        let size_bytes = data.len() as u64;
        let obj_path = self.object_path(&content_hash);

        self.ensure_parent(&obj_path)?;

        if !obj_path.exists() {
            match fs::rename(staging_path, &obj_path) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                    fs::copy(staging_path, &obj_path).map_err(StoreError::Copy)?;
                    let _ = fs::remove_file(staging_path);
                }
                Err(e) => return Err(StoreError::Rename(e)),
            }
        } else {
            let _ = fs::remove_file(staging_path);
        }

        self.write_metadata(&content_hash, mime_type, size_bytes)?;

        Ok(SealResult {
            content_hash,
            content_path: obj_path,
            size_bytes,
        })
    }

    pub fn staging_exists(&self, id: &StagingId) -> bool {
        self.staging_path(id).exists()
    }

    pub fn address_path(&self, address: &CasAddress) -> Option<PathBuf> {
        match address {
            CasAddress::Content(hash) => self.path(hash),
            CasAddress::Staging(id) => {
                let path = self.staging_path(id);
                path.exists().then_some(path)
            }
        }
    }

    pub fn remove_staging(&self, id: &StagingId) -> Result<(), StoreError> {
        let path = self.staging_path(id);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| StoreError::Remove {
                path,
                source: e,
            })?;
        }
        Ok(())
    }
}

impl ContentStore for FileStore {
    fn store(&self, data: &[u8], mime_type: &str) -> Result<ContentHash, StoreError> {
        if self.config.read_only {
            return Err(StoreError::ReadOnly);
        }

        let hash = ContentHash::from_data(data);
        let obj_path = self.object_path(&hash);

        self.ensure_parent(&obj_path)?;

        if !obj_path.exists() {
            fs::write(&obj_path, data).map_err(|e| StoreError::WriteObject {
                path: obj_path,
                source: e,
            })?;
        }

        self.write_metadata(&hash, mime_type, data.len() as u64)?;

        Ok(hash)
    }

    fn retrieve(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.object_path(hash);
        if path.exists() {
            let data = fs::read(&path).map_err(|e| StoreError::ReadObject {
                path,
                source: e,
            })?;
            Ok(Some(data))
        } else {
            Ok(None)
        }
    }

    fn exists(&self, hash: &ContentHash) -> bool {
        self.object_path(hash).exists()
    }

    fn path(&self, hash: &ContentHash) -> Option<PathBuf> {
        let path = self.object_path(hash);
        path.exists().then_some(path)
    }

    fn inspect(&self, hash: &ContentHash) -> Result<Option<CasReference>, StoreError> {
        let obj_path = self.object_path(hash);
        if !obj_path.exists() {
            return Ok(None);
        }

        let meta_path = self.metadata_path(hash);
        if meta_path.exists() {
            let json = fs::read_to_string(&meta_path).map_err(|e| StoreError::MetadataRead {
                path: meta_path,
                source: e,
            })?;
            let metadata: CasMetadata =
                serde_json::from_str(&json).map_err(StoreError::MetadataSerde)?;
            Ok(Some(
                CasReference::new(hash.clone(), metadata.mime_type, metadata.size)
                    .with_path(obj_path.to_string_lossy()),
            ))
        } else {
            let file_size = fs::metadata(&obj_path)
                .map_err(|e| StoreError::ReadObject {
                    path: obj_path.clone(),
                    source: e,
                })?
                .len();
            Ok(Some(
                CasReference::new(hash.clone(), "application/octet-stream", file_size)
                    .with_path(obj_path.to_string_lossy()),
            ))
        }
    }

    fn remove(&self, hash: &ContentHash) -> Result<bool, StoreError> {
        if self.config.read_only {
            return Err(StoreError::ReadOnly);
        }

        let obj_path = self.object_path(hash);
        if !obj_path.exists() {
            return Ok(false);
        }

        fs::remove_file(&obj_path).map_err(|e| StoreError::Remove {
            path: obj_path,
            source: e,
        })?;

        let meta_path = self.metadata_path(hash);
        if meta_path.exists() {
            fs::remove_file(&meta_path).map_err(|e| StoreError::Remove {
                path: meta_path,
                source: e,
            })?;
        }

        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use tempfile::TempDir;

    #[test]
    fn test_store_and_retrieve() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        let data = b"Hello, World!";
        let hash = store.store(data, "text/plain").unwrap();
        assert_eq!(hash.as_str().len(), 32);

        let retrieved = store.retrieve(&hash).unwrap().expect("should exist");
        assert_eq!(retrieved, data);
    }

    #[test]
    fn test_deduplication() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        let data = b"Duplicate Me";
        let hash1 = store.store(data, "text/plain").unwrap();
        let hash2 = store.store(data, "text/plain").unwrap();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_exists() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        let hash = store.store(b"existence test", "text/plain").unwrap();
        assert!(store.exists(&hash));

        let missing: ContentHash = "00000000000000000000000000000000".parse().unwrap();
        assert!(!store.exists(&missing));
    }

    #[test]
    fn test_inspect_with_metadata() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        let data = b"inspectable";
        let hash = store.store(data, "image/png").unwrap();

        let reference = store.inspect(&hash).unwrap().expect("should exist");
        assert_eq!(reference.hash, hash);
        assert_eq!(reference.mime_type, "image/png");
        assert_eq!(reference.size_bytes, data.len() as u64);
        assert!(reference.local_path.is_some());
    }

    #[test]
    fn test_sharded_path_layout() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        let hash = store.store(b"layout test", "text/plain").unwrap();
        let obj_path = store.path(&hash).expect("should have path");

        let path_str = obj_path.to_string_lossy();
        assert!(path_str.contains(&format!("objects/{}/", hash.prefix())));
        assert!(path_str.ends_with(hash.remainder()));
    }

    #[test]
    fn test_remove() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        let hash = store.store(b"remove me", "text/plain").unwrap();
        assert!(store.exists(&hash));

        let removed = store.remove(&hash).unwrap();
        assert!(removed);
        assert!(!store.exists(&hash));

        // Metadata sidecar should also be gone
        let meta_path = store.metadata_path(&hash);
        assert!(!meta_path.exists());
    }

    #[test]
    fn test_remove_absent_returns_false() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        let missing: ContentHash = "00000000000000000000000000000000".parse().unwrap();
        let removed = store.remove(&missing).unwrap();
        assert!(!removed);
    }

    #[test]
    fn test_read_only_prevents_writes() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::read_only_at(temp_dir.path());
        assert!(matches!(
            store.store(b"fail", "text/plain"),
            Err(StoreError::ReadOnly)
        ));
    }

    #[test]
    fn test_read_only_allows_reads() {
        let temp_dir = TempDir::new().unwrap();
        let writable = FileStore::at_path(temp_dir.path());
        let hash = writable.store(b"readable", "text/plain").unwrap();

        let readonly = FileStore::read_only_at(temp_dir.path());
        let data = readonly.retrieve(&hash).unwrap().expect("should read");
        assert_eq!(data, b"readable");
    }

    #[test]
    fn test_staging_create_write_seal() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        let mut chunk = store.create_staging().unwrap();
        chunk.write(b"Hello, ").unwrap();
        chunk.write(b"World!").unwrap();
        chunk.flush().unwrap();
        assert_eq!(chunk.bytes_written(), 13);

        let result = store.seal(&chunk, "text/plain").unwrap();

        assert!(!chunk.path().exists());
        assert!(store.exists(&result.content_hash));
        let data = store.retrieve(&result.content_hash).unwrap().unwrap();
        assert_eq!(data, b"Hello, World!");
    }

    #[test]
    fn test_staging_seal_matches_oneshot() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        let data = b"staging vs oneshot";
        let oneshot_hash = store.store(data, "text/plain").unwrap();

        let mut chunk = store.create_staging().unwrap();
        chunk.write(data).unwrap();
        chunk.flush().unwrap();
        let result = store.seal(&chunk, "text/plain").unwrap();

        assert_eq!(result.content_hash, oneshot_hash);
    }

    #[test]
    fn test_staging_seal_dedup() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        let data = b"duplicate staging";
        let hash1 = store.store(data, "text/plain").unwrap();

        let mut chunk = store.create_staging().unwrap();
        chunk.write(data).unwrap();
        chunk.flush().unwrap();
        let result = store.seal(&chunk, "text/plain").unwrap();

        assert_eq!(result.content_hash, hash1);
        assert!(!chunk.path().exists());
    }

    #[test]
    fn test_staging_remove() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        let mut chunk = store.create_staging().unwrap();
        chunk.write(b"to be removed").unwrap();
        chunk.flush().unwrap();

        let id = chunk.id().clone();
        assert!(store.staging_exists(&id));

        store.remove_staging(&id).unwrap();
        assert!(!store.staging_exists(&id));
    }

    #[test]
    fn test_concurrent_writes() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(FileStore::at_path(temp_dir.path()));

        let data = b"Concurrent Data";
        let expected: ContentHash = "5c735d76fe3537a0f35cf4a4eb14a532".parse().unwrap();

        let handles: Vec<_> = (0..10)
            .map(|_| {
                let s = store.clone();
                thread::spawn(move || s.store(data, "application/octet-stream").unwrap())
            })
            .collect();

        for handle in handles {
            assert_eq!(handle.join().unwrap(), expected);
        }

        let retrieved = store.retrieve(&expected).unwrap().unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn test_inspect_without_metadata() {
        let temp_dir = TempDir::new().unwrap();
        let config = CasConfig {
            base_path: temp_dir.path().to_path_buf(),
            store_metadata: false,
            read_only: false,
        };
        let store = FileStore::new(config);

        let hash = store.store(b"no metadata", "text/plain").unwrap();

        let reference = store.inspect(&hash).unwrap().expect("should exist");
        assert_eq!(reference.hash, hash);
        assert_eq!(reference.mime_type, "application/octet-stream");
        assert_eq!(reference.size_bytes, 11);
    }
}
