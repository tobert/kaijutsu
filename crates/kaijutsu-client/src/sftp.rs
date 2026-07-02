//! SFTP transport + content-addressed blob resolution over the shared SSH
//! connection.
//!
//! The kernel already speaks SFTP (`kaijutsu-server/src/sftp.rs`) as a sibling
//! subsystem of the Cap'n Proto RPC channel — this is the client half. A
//! [`SftpClient`] opens a *second* session channel bound to the `sftp`
//! subsystem and reads VFS paths; a [`BlobResolver`] layers a local XDG CAS
//! cache on top so a clip's `media` hash resolves from disk on a hit and pulls
//! the miss over the wire from `/v/blobs/<hash>` (docs/clips.md, docs/slash-v.md).
//!
//! Unlike the capnp RPC path (which is `!Send` and pinned to a dedicated
//! thread), SFTP futures are `Send`, so this rides the ambient async runtime /
//! Bevy task pool — it never touches the RPC actor's `spawn_local` world.

use std::path::PathBuf;

use async_trait::async_trait;
use russh_sftp::client::SftpSession;

use kaijutsu_cas::{ContentHash, ContentStore, FileStore, StoreError};
use kaijutsu_types::SSH_SFTP_SUBSYSTEM;

use crate::ssh::{SshClient, SshConfig, SshError};

/// Failures along the fetch path: the SSH transport, the SFTP protocol, a blob
/// that failed its self-verification, or the local cache store.
#[derive(Debug, thiserror::Error)]
pub enum SftpError {
    #[error("SSH transport: {0}")]
    Ssh(#[from] SshError),

    #[error("SFTP protocol: {0}")]
    Protocol(String),

    /// Fetched bytes did not hash back to the requested address. The CAS is
    /// self-verifying: a corrupt or substituted object crashes the resolve
    /// rather than caching a lie.
    #[error(
        "blob {expected} failed verification: fetched bytes hash to {got} \
         (corrupt or wrong object)"
    )]
    HashMismatch {
        expected: ContentHash,
        got: ContentHash,
    },

    #[error("local CAS cache: {0}")]
    Cache(#[from] StoreError),
}

/// A live SFTP session over the kaijutsu SSH transport.
///
/// Holds its own [`SshClient`] because the underlying SSH session must outlive
/// the channel the SFTP session wraps — dropping the client closes the
/// connection out from under the stream.
pub struct SftpClient {
    // Kept alive for the connection's lifetime; not used directly after connect.
    _ssh: SshClient,
    session: SftpSession,
}

impl SftpClient {
    /// The canonical VFS path for a content-addressed blob.
    pub fn blob_path(hash: &ContentHash) -> String {
        format!("/v/blobs/{hash}")
    }

    /// Connect, authenticate, and bind a second session channel to the `sftp`
    /// subsystem, returning a ready session.
    pub async fn connect(config: SshConfig) -> Result<Self, SftpError> {
        let mut ssh = SshClient::new(config);
        let channel = ssh.connect_subsystem(SSH_SFTP_SUBSYSTEM).await?;
        let session = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| SftpError::Protocol(e.to_string()))?;
        Ok(Self { _ssh: ssh, session })
    }

    /// Read an entire VFS path over SFTP. A missing path is an error (fail
    /// loud), never empty bytes.
    pub async fn read(&self, path: &str) -> Result<Vec<u8>, SftpError> {
        self.session
            .read(path)
            .await
            .map_err(|e| SftpError::Protocol(e.to_string()))
    }

    /// Read a blob from `/v/blobs/<hash>` (unverified — [`BlobResolver`] does
    /// the verification before it trusts the bytes).
    pub async fn read_blob(&self, hash: &ContentHash) -> Result<Vec<u8>, SftpError> {
        self.read(&Self::blob_path(hash)).await
    }
}

/// Something that can fetch blob bytes by hash. [`SftpClient`] is the production
/// implementor; tests supply a stub so the [`BlobResolver`] cache/verify logic
/// is exercised without a live server.
#[async_trait]
pub trait BlobFetch: Send + Sync {
    async fn fetch(&self, hash: &ContentHash) -> Result<Vec<u8>, SftpError>;
}

#[async_trait]
impl BlobFetch for SftpClient {
    async fn fetch(&self, hash: &ContentHash) -> Result<Vec<u8>, SftpError> {
        self.read_blob(hash).await
    }
}

/// Resolves content-addressed blobs through a local XDG CAS cache, fetching
/// misses over the wire and re-verifying before it trusts (and caches) them.
///
/// - **hit** → local bytes, no wire traffic.
/// - **miss** → fetch → verify the fetched bytes hash back to the requested
///   address → store → return. A mismatch is [`SftpError::HashMismatch`]; the
///   bad bytes are never cached.
pub struct BlobResolver<F: BlobFetch> {
    cache: FileStore,
    fetch: F,
}

impl<F: BlobFetch> BlobResolver<F> {
    /// Build a resolver over an explicit cache directory (tests, custom roots).
    pub fn new(cache: FileStore, fetch: F) -> Self {
        Self { cache, fetch }
    }

    /// Build a resolver whose cache is the per-user XDG blob cache
    /// (`$XDG_CACHE_HOME/kaijutsu/cas`).
    pub fn with_xdg_cache(fetch: F) -> Self {
        Self::new(FileStore::at_path(default_cache_dir()), fetch)
    }

    /// Resolve a blob to its bytes, fetching + caching on a miss.
    pub async fn resolve(&self, hash: &ContentHash) -> Result<Vec<u8>, SftpError> {
        if let Some(bytes) = self.cache.retrieve(hash)? {
            return Ok(bytes);
        }

        let bytes = self.fetch.fetch(hash).await?;

        let got = ContentHash::from_data(&bytes);
        if got != *hash {
            return Err(SftpError::HashMismatch {
                expected: hash.clone(),
                got,
            });
        }

        // The mime is unknown at the transport layer; the clip carries the real
        // one. The cache keys on the content hash, so the placeholder is inert.
        self.cache.store(&bytes, "application/octet-stream")?;
        Ok(bytes)
    }
}

/// `$XDG_CACHE_HOME/kaijutsu/cas`, falling back to `./.cache/kaijutsu/cas` only
/// when the platform exposes no cache dir (rare; a benign relative cache, never
/// a data path).
pub fn default_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("kaijutsu")
        .join("cas")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// A fetch stub that returns fixed bytes and counts calls, so the resolver's
    /// cache/verify logic is testable without a server.
    struct StubFetch {
        body: Vec<u8>,
        calls: Mutex<usize>,
    }

    impl StubFetch {
        fn new(body: impl Into<Vec<u8>>) -> Self {
            Self {
                body: body.into(),
                calls: Mutex::new(0),
            }
        }
        fn call_count(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl BlobFetch for StubFetch {
        async fn fetch(&self, _hash: &ContentHash) -> Result<Vec<u8>, SftpError> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.body.clone())
        }
    }

    #[tokio::test]
    async fn a_miss_fetches_verifies_and_caches() {
        let dir = TempDir::new().unwrap();
        let body = b"clip bytes".to_vec();
        let hash = ContentHash::from_data(&body);
        let resolver = BlobResolver::new(FileStore::at_path(dir.path()), StubFetch::new(body.clone()));

        let got = resolver.resolve(&hash).await.unwrap();
        assert_eq!(got, body, "returns the fetched bytes");
        assert_eq!(resolver.fetch.call_count(), 1, "fetched once");
        assert!(resolver.cache.exists(&hash), "cached the verified blob");
    }

    #[tokio::test]
    async fn a_second_resolve_is_a_cache_hit() {
        let dir = TempDir::new().unwrap();
        let body = b"cache me once".to_vec();
        let hash = ContentHash::from_data(&body);
        let resolver = BlobResolver::new(FileStore::at_path(dir.path()), StubFetch::new(body.clone()));

        resolver.resolve(&hash).await.unwrap(); // miss → fetch + cache
        let got = resolver.resolve(&hash).await.unwrap(); // hit
        assert_eq!(got, body);
        assert_eq!(resolver.fetch.call_count(), 1, "the second resolve did not fetch");
    }

    #[tokio::test]
    async fn a_prewarmed_cache_never_fetches() {
        let dir = TempDir::new().unwrap();
        let body = b"already local".to_vec();
        let cache = FileStore::at_path(dir.path());
        let hash = cache.store(&body, "application/octet-stream").unwrap();

        // Stub would return junk — proving the resolver serves the cache, not it.
        let resolver = BlobResolver::new(cache, StubFetch::new(vec![0xde, 0xad]));
        let got = resolver.resolve(&hash).await.unwrap();
        assert_eq!(got, body);
        assert_eq!(resolver.fetch.call_count(), 0, "cache hit skips the wire");
    }

    #[tokio::test]
    async fn a_hash_mismatch_fails_loud_and_caches_nothing() {
        let dir = TempDir::new().unwrap();
        // Ask for the hash of the real object, but the fetch returns an impostor.
        let wanted = ContentHash::from_data(b"the real object");
        let resolver =
            BlobResolver::new(FileStore::at_path(dir.path()), StubFetch::new(b"an impostor".to_vec()));

        let err = resolver.resolve(&wanted).await.unwrap_err();
        assert!(matches!(err, SftpError::HashMismatch { .. }), "got {err:?}");
        assert!(
            resolver.cache.retrieve(&wanted).unwrap().is_none(),
            "corrupt bytes must never enter the cache"
        );
    }
}
