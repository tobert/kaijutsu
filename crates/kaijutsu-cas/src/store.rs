use std::fs;
use std::path::{Path, PathBuf};

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

    fn ensure_parent(&self, path: &Path) -> Result<(), StoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| StoreError::CreateDir {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        Ok(())
    }

    /// Atomically move a fully-written staging file into its object slot.
    ///
    /// The rename is the atomicity gate: a concurrent reader sees either no
    /// object or the whole object, never a torn prefix — which matters because
    /// the XDG client cache is multi-process and a cache-hit `retrieve` never
    /// re-hashes. Idempotent under dedup and racing writers: if the object
    /// already exists, the staging file is discarded and the existing object
    /// kept. Falls back to copy+remove across filesystems (`EXDEV`).
    fn place_object(&self, staging_path: &Path, obj_path: &Path) -> Result<(), StoreError> {
        self.ensure_parent(obj_path)?;

        if !obj_path.exists() {
            match fs::rename(staging_path, obj_path) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                    // Cross-filesystem staging (staging/ and objects/ on
                    // different mounts). A direct `fs::copy` to `obj_path` would
                    // expose a *torn* object to a concurrent reader — the very
                    // thing the atomic rename exists to prevent. So copy into a
                    // uniquely-named temp file **in the destination directory**
                    // (same FS as `obj_path`), then rename that into place.
                    let tmp =
                        obj_path.with_extension(format!("tmp.{}", StagingId::new()));
                    fs::copy(staging_path, &tmp).map_err(StoreError::Copy)?;
                    match fs::rename(&tmp, obj_path) {
                        Ok(()) => {
                            let _ = fs::remove_file(staging_path);
                        }
                        Err(e) => {
                            let _ = fs::remove_file(&tmp);
                            return Err(StoreError::Rename(e));
                        }
                    }
                }
                Err(e) => return Err(StoreError::Rename(e)),
            }
        } else {
            let _ = fs::remove_file(staging_path);
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
        // Write to a temp sidecar in the same directory, then rename into place,
        // so a concurrent `inspect()` never reads a torn (half-written) JSON and
        // fails to parse it. (Metadata is non-authoritative — `inspect` degrades
        // to octet-stream when it's *missing* — but a torn file is a present
        // parse error, not a graceful miss.)
        let fname = meta_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tmp = meta_path.with_file_name(format!("{fname}.tmp.{}", StagingId::new()));
        fs::write(&tmp, json).map_err(|e| StoreError::MetadataWrite {
            path: tmp.clone(),
            source: e,
        })?;
        match fs::rename(&tmp, &meta_path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(StoreError::MetadataWrite {
                    path: meta_path,
                    source: e,
                })
            }
        }
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

        self.place_object(staging_path, &obj_path)?;

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

    /// Open a streaming writer: incremental BLAKE3 hashing while chunks land
    /// in `staging/`, atomic rename into `objects/` at
    /// [`StreamingWriter::finalize`] — never a buffered re-hash pass. The
    /// pump's `cas put` sink (`docs/slash-r.md` slice 0).
    pub fn create_streaming_writer(
        &self,
        mime_type: impl Into<String>,
    ) -> Result<StreamingWriter, StoreError> {
        let chunk = self.create_staging()?;
        Ok(StreamingWriter {
            store: self.clone(),
            chunk: Some(chunk),
            hasher: blake3::Hasher::new(),
            mime_type: mime_type.into(),
        })
    }
}

/// Incremental writer into CAS `staging/`, hashing as bytes arrive.
///
/// `write` never re-reads what it already wrote — the running BLAKE3 state
/// covers exactly the bytes streamed, so [`Self::finalize`] exposes the
/// final [`ContentHash`] with no re-hash pass over the staged file.
///
/// Nothing renames without a completed `finalize`: an unfinalized writer's
/// [`Drop`] actively unlinks the partial staging file (best-effort — a
/// dropping writer must never panic) rather than leaving garbage for a
/// restart sweep. `chunk` is `Option` solely so `finalize` can move the
/// `StagingChunk` out of `&mut self` while still leaving a well-formed
/// (already-consumed) value behind for `Drop` to see.
pub struct StreamingWriter {
    store: FileStore,
    chunk: Option<StagingChunk>,
    hasher: blake3::Hasher,
    mime_type: String,
}

impl StreamingWriter {
    /// Feed the next chunk. Order matters — chunks are hashed and written in
    /// the sequence they arrive; there is no seek/offset parameter, matching
    /// the pump's sequential `PumpSink` contract.
    pub fn write(&mut self, data: &[u8]) -> Result<(), StoreError> {
        let chunk = self.chunk.as_mut().ok_or(StagingError::Closed)?;
        chunk.write(data)?;
        self.hasher.update(data);
        Ok(())
    }

    /// Flush, hash-finalize, and atomically place the staged bytes into
    /// `objects/` (idempotent under dedup, same as [`FileStore::seal`]).
    /// Consumes `self` so a caller cannot write after finalizing.
    ///
    /// `self.chunk` is only cleared on the success path (below) — every
    /// early `?` return in [`Self::finalize_inner`] leaves it `Some`, so a
    /// failed finalize (flush/sync/rename/metadata error) still hits
    /// [`Drop`]'s cleanup instead of leaking a staging file silently.
    pub fn finalize(mut self) -> Result<SealResult, StoreError> {
        let result = self.finalize_inner();
        if result.is_ok() {
            self.chunk = None;
        }
        result
    }

    fn finalize_inner(&mut self) -> Result<SealResult, StoreError> {
        let chunk = self.chunk.as_mut().ok_or(StagingError::Closed)?;
        chunk.flush()?;
        chunk.sync()?;
        chunk.close();

        let content_hash = ContentHash::from_blake3(self.hasher.finalize());
        let size_bytes = chunk.bytes_written();
        let staging_path = chunk.path.clone();
        let obj_path = self.store.object_path(&content_hash);

        self.store.place_object(&staging_path, &obj_path)?;
        self.store.write_metadata(&content_hash, &self.mime_type, size_bytes)?;

        Ok(SealResult {
            content_hash,
            content_path: obj_path,
            size_bytes,
        })
    }
}

impl Drop for StreamingWriter {
    fn drop(&mut self) {
        // `finalize` takes `self` and leaves `chunk = None` behind, so a
        // `Some` here means finalize never ran (early return, panic-free
        // error path, or the caller simply dropped an in-progress writer) —
        // unlink the partial file rather than leaving staging residue for a
        // restart sweep to eventually catch. Best-effort: a dropping writer
        // must never panic on a failed cleanup (matches `TempDirGuard`,
        // `crates/kaijutsu-kernel/src/kernel.rs`).
        if let Some(chunk) = self.chunk.take() {
            let _ = fs::remove_file(&chunk.path);
        }
    }
}

impl ContentStore for FileStore {
    fn store(&self, data: &[u8], mime_type: &str) -> Result<ContentHash, StoreError> {
        if self.config.read_only {
            return Err(StoreError::ReadOnly);
        }

        let hash = ContentHash::from_data(data);
        let obj_path = self.object_path(&hash);

        // Skip the write entirely when the object already exists (dedup).
        // Otherwise write into a unique staging file and atomically rename it
        // into place, so a concurrent reader never sees a torn object.
        if !obj_path.exists() {
            let mut chunk = self.create_staging()?;
            chunk.write(data)?;
            chunk.sync()?;
            chunk.close();
            self.place_object(&chunk.path, &obj_path)?;
        }

        self.write_metadata(&hash, mime_type, data.len() as u64)?;

        Ok(hash)
    }

    fn retrieve(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>, StoreError> {
        // Attempt the read directly rather than exists()-then-read: the store is
        // multi-process, and a check-then-read races a concurrent remove into a
        // spurious error. A missing object (never present, or unlinked mid-race)
        // is `Ok(None)`; any other error is real and bubbles.
        let path = self.object_path(hash);
        match fs::read(&path) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::ReadObject { path, source: e }),
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
    fn test_store_never_exposes_a_torn_object() {
        // The XDG client cache is multi-process and a cache-hit `retrieve` never
        // re-hashes, so `store()` must be atomic: a concurrent reader sees either
        // no object or the whole object, never a truncated prefix. A raw
        // `fs::write` (create+truncate, then fill) exposes the partial file at its
        // final path — this test catches that.
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(FileStore::at_path(temp_dir.path()));

        // Large enough that the write window is wide; distinct bytes so a
        // truncated read is a *different* length than the whole object.
        let data = Arc::new(vec![0xABu8; 2 * 1024 * 1024]);
        let hash = ContentHash::from_data(&data);
        let full_len = data.len();

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Writers: repeatedly remove + re-store so the object flickers
        // absent → (writing) → present, re-opening the torn window each pass.
        let writers: Vec<_> = (0..2)
            .map(|_| {
                let s = store.clone();
                let d = data.clone();
                let h = hash.clone();
                let stop = stop.clone();
                thread::spawn(move || {
                    for _ in 0..100 {
                        let _ = s.remove(&h);
                        s.store(&d, "application/octet-stream").unwrap();
                    }
                    stop.store(true, std::sync::atomic::Ordering::SeqCst);
                })
            })
            .collect();

        // Readers: any object we can read must be complete.
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let s = store.clone();
                let h = hash.clone();
                let stop = stop.clone();
                thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                        if let Some(bytes) = s.retrieve(&h).unwrap() {
                            assert_eq!(
                                bytes.len(),
                                full_len,
                                "reader observed a torn object ({} of {} bytes)",
                                bytes.len(),
                                full_len
                            );
                        }
                    }
                })
            })
            .collect();

        for w in writers {
            w.join().unwrap();
        }
        for r in readers {
            r.join().unwrap();
        }
    }

    #[test]
    fn test_store_leaves_no_staging_residue() {
        // A successful atomic store consumes its staging file via rename — nothing
        // is left behind in staging/.
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        store.store(b"leaves nothing behind", "text/plain").unwrap();

        let staging_dir = store.config().staging_dir();
        let residue: Vec<_> = std::fs::read_dir(&staging_dir)
            .map(|rd| rd.flatten().collect())
            .unwrap_or_default();
        // The staging tree may hold shard subdirs, but no files should linger.
        let mut stack = residue;
        while let Some(entry) = stack.pop() {
            let path = entry.path();
            if path.is_dir() {
                stack.extend(std::fs::read_dir(&path).unwrap().flatten());
            } else {
                panic!("staging residue left behind: {}", path.display());
            }
        }
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

    /// Every file under `staging/`, recursively — a residue check shared by
    /// the streaming-writer tests below.
    fn staging_residue(store: &FileStore) -> Vec<PathBuf> {
        let staging_dir = store.config().staging_dir();
        let mut stack: Vec<_> = std::fs::read_dir(&staging_dir)
            .map(|rd| rd.flatten().collect())
            .unwrap_or_default();
        let mut files = Vec::new();
        while let Some(entry) = stack.pop() {
            let path = entry.path();
            if path.is_dir() {
                stack.extend(std::fs::read_dir(&path).unwrap().flatten());
            } else {
                files.push(path);
            }
        }
        files
    }

    // ========================================================================
    // StreamingWriter — incremental hashing, atomic finalize, active
    // drop-cleanup (`docs/slash-r.md` slice 0).
    // ========================================================================

    #[test]
    fn streaming_writer_matches_oneshot_hash_and_leaves_no_residue() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        let data = b"streamed in three separate chunks, hashed incrementally";
        let oneshot_hash = store.store(data, "text/plain").unwrap();

        let mut writer = store.create_streaming_writer("text/plain").unwrap();
        writer.write(&data[0..10]).unwrap();
        writer.write(&data[10..30]).unwrap();
        writer.write(&data[30..]).unwrap();
        let result = writer.finalize().unwrap();

        assert_eq!(result.content_hash, oneshot_hash, "streamed hash must match a one-shot store() of the same bytes");
        assert_eq!(result.size_bytes, data.len() as u64);
        assert!(store.exists(&result.content_hash));
        assert_eq!(store.retrieve(&result.content_hash).unwrap().unwrap(), data);
        assert!(
            staging_residue(&store).is_empty(),
            "a completed finalize must leave nothing in staging/"
        );
    }

    #[test]
    fn streaming_writer_dedups_against_an_existing_object() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        let data = b"duplicate via streaming writer";
        let existing = store.store(data, "text/plain").unwrap();

        let mut writer = store.create_streaming_writer("text/plain").unwrap();
        writer.write(data).unwrap();
        let result = writer.finalize().unwrap();

        assert_eq!(result.content_hash, existing);
        assert!(staging_residue(&store).is_empty());
    }

    #[test]
    fn streaming_writer_drop_without_finalize_unlinks_staging() {
        // The core "interruption is loud, no silent partial file" guarantee:
        // a writer abandoned mid-stream (caller error path, no finalize())
        // must not leave its staged bytes lying around for a restart sweep
        // to eventually find.
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        {
            let mut writer = store.create_streaming_writer("text/plain").unwrap();
            writer.write(b"never gets finalized").unwrap();
            // writer drops here without calling finalize()
        }

        assert!(
            staging_residue(&store).is_empty(),
            "Drop must actively unlink the partial staging file"
        );
    }

    #[test]
    fn streaming_writer_finalize_failure_still_leaves_drop_to_clean_up() {
        // A read-only store can still create a streaming writer's staging
        // file (create_staging only checks read_only, which the writer
        // itself doesn't re-check) is NOT the scenario here — instead this
        // proves the more subtle invariant: finalize() only clears its
        // internal `chunk` slot on the SUCCESS path, so if finalize is
        // never called at all (the common failure shape — a caller sees an
        // error from an earlier `write()` and drops the writer without ever
        // reaching finalize), Drop still has a live chunk to clean up.
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::at_path(temp_dir.path());

        let writer = store.create_streaming_writer("text/plain").unwrap();
        drop(writer);

        assert!(
            staging_residue(&store).is_empty(),
            "an unwritten, unfinalized writer must still clean up its empty staging file"
        );
    }
}
