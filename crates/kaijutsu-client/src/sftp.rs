//! SFTP transport + content-addressed blob resolution over the shared SSH
//! connection.
//!
//! The kernel already speaks SFTP (`kaijutsu-server/src/sftp.rs`) as a sibling
//! subsystem of the Cap'n Proto RPC channel — this is the client half. A
//! [`SftpClient`] opens its **own** SSH connection and binds a channel to the
//! `sftp` subsystem to read VFS paths; a [`BlobResolver`] layers a local XDG CAS
//! cache on top so a clip's `media` hash resolves from disk on a hit and pulls
//! the miss over the wire from `/v/blobs/<hash>` (docs/clips.md, docs/slash-v.md).
//!
//! (Multiplexing the SFTP channel onto the *existing* RPC connection instead of
//! dialing a second one is a later optimization — it needs `SshClient` to split
//! the connection `Handle` from its per-subsystem channel. First cut: own conn.)
//!
//! Unlike the capnp RPC path (which is `!Send` and pinned to a dedicated
//! thread), SFTP futures are `Send`, so this rides the ambient async runtime /
//! Bevy task pool — it never touches the RPC actor's `spawn_local` world.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use russh_sftp::client::SftpSession;
use tokio::sync::Mutex as AsyncMutex;

use kaijutsu_cas::{CasConfig, ContentHash, ContentStore, FileStore, StoreError};
use kaijutsu_types::SSH_SFTP_SUBSYSTEM;

use crate::ssh::{SshClient, SshConfig, SshError};

/// Map a `russh_sftp` client error into our typed error, preserving the
/// no-such-file distinction (a genuinely-absent path) apart from opaque
/// transport/protocol failures.
fn map_sftp_err(e: russh_sftp::client::error::Error) -> SftpError {
    use russh_sftp::client::error::Error as E;
    use russh_sftp::protocol::StatusCode;
    match e {
        E::Status(s) if s.status_code == StatusCode::NoSuchFile => {
            SftpError::NotFound(s.error_message)
        }
        other => SftpError::Protocol(other.to_string()),
    }
}

/// Failures along the fetch path: the SSH transport, the SFTP protocol, a blob
/// that failed its self-verification, or the local cache store.
#[derive(Debug, thiserror::Error)]
pub enum SftpError {
    #[error("SSH transport: {0}")]
    Ssh(#[from] SshError),

    #[error("SFTP protocol: {0}")]
    Protocol(String),

    /// The path does not exist on the server (SFTP `SSH_FX_NO_SUCH_FILE`). A
    /// distinct variant so a caller can tell a genuinely-absent object from a
    /// transport failure: `/v/blobs/<hash>` not yet replicated is a normal,
    /// non-retryable "404", whereas a dropped connection mid-fetch is worth a
    /// retry. Flattening both into an opaque string would erase that.
    #[error("no such path: {0}")]
    NotFound(String),

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
    // Field order is load-bearing. Rust drops fields in *declaration* order, so
    // `session` (declared first) drops — and cleanly closes its channel —
    // BEFORE `_ssh` tears down the SSH connection underneath it. Reversed, the
    // connection would die first and the session's close would race a dead
    // transport. `_ssh` is otherwise unused after connect: it just keeps the
    // connection alive for the session's lifetime.
    session: SftpSession,
    _ssh: SshClient,
}

impl SftpClient {
    /// The canonical VFS path for a content-addressed blob.
    ///
    /// Sharded on the hash's **leading** two hex chars, matching the server's
    /// `/v/blobs/<ab>/<full-hash>` layout (BLAKE3 is uniform in every byte, so
    /// the UUIDv7 trailing-byte sharding rule deliberately does NOT apply to
    /// hashes). The blob pool is the one `/v` pool that grows without bound, so
    /// it is sharded 256× to keep any single `readdir` bounded.
    pub fn blob_path(hash: &ContentHash) -> String {
        format!("/v/blobs/{}/{}", hash.prefix(), hash)
    }

    /// Open an SSH connection, authenticate, and bind a channel to the `sftp`
    /// subsystem, returning a ready session.
    ///
    /// Opens its **own** connection (full TCP + auth), not a channel multiplexed
    /// onto an existing one — see the module docs for why, and the future
    /// optimization.
    pub async fn connect(config: SshConfig) -> Result<Self, SftpError> {
        let mut ssh = SshClient::new(config);
        let channel = ssh.connect_subsystem(SSH_SFTP_SUBSYSTEM).await?;
        let session = SftpSession::new(channel.into_stream())
            .await
            .map_err(map_sftp_err)?;
        Ok(Self { session, _ssh: ssh })
    }

    /// Read an entire VFS path over SFTP. A missing path is [`SftpError::NotFound`]
    /// (fail loud), never empty bytes.
    ///
    /// **Reads to EOF across the packet cap.** `SftpSession::read` opens the file
    /// and drives `read_to_end`, whose `poll_read` issues one SFTP `READ` per
    /// packet — each capped at the negotiated `max_read_len` (256 KiB
    /// server-side) — advancing the offset until EOF (verified against
    /// russh-sftp 2.3 `client/fs/file.rs`). So a blob larger than one packet is
    /// reassembled whole, and the resolver's re-hash verifies the full object,
    /// not a truncated prefix.
    ///
    /// Reads the whole object into memory. Fine for the symbolic scores and
    /// small clips of the first cut; a large-media streaming path (chunked read
    /// straight into CAS staging, incremental hash) is a deferred follow-up
    /// (`docs/issues.md` `/v` Track B — `/v/blobs` + client CAS sync).
    pub async fn read(&self, path: &str) -> Result<Vec<u8>, SftpError> {
        self.session.read(path).await.map_err(map_sftp_err)
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
    /// Per-hash fetch locks — the single-flight gate. Concurrent misses for one
    /// hash serialize on the hash's `AsyncMutex`, so only the first crosses the
    /// wire; the rest wake to a cache hit (the double-check inside the lock). A
    /// vamp repeating one clip must not open N transfers. The map is keyed by
    /// hash and self-prunes: the last holder out of a hash's lock drops its
    /// entry (all clones happen under this std `Mutex`, so the strong-count
    /// check is race-free).
    locks: Mutex<HashMap<ContentHash, Arc<AsyncMutex<()>>>>,
}

impl<F: BlobFetch> BlobResolver<F> {
    /// Build a resolver over an explicit cache directory (tests, custom roots).
    pub fn new(cache: FileStore, fetch: F) -> Self {
        Self {
            cache,
            fetch,
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Build a resolver whose cache is the per-user XDG blob cache
    /// (`$XDG_CACHE_HOME/kaijutsu/cas`).
    ///
    /// Metadata sidecars are **off**: the cache keys on the content hash and the
    /// mime is never read back here (the clip carries the real one), so a
    /// per-object `.json` would just double the inode count — and `FileStore`'s
    /// metadata is first-writer-wins, which would pin our placeholder mime.
    pub fn with_xdg_cache(fetch: F) -> Self {
        let config = CasConfig {
            base_path: default_cache_dir(),
            store_metadata: false,
            read_only: false,
        };
        Self::new(FileStore::new(config), fetch)
    }

    /// Resolve a blob to its bytes, fetching + caching on a miss. Concurrent
    /// resolves for the same hash coalesce onto a single wire transfer
    /// (single-flight).
    pub async fn resolve(&self, hash: &ContentHash) -> Result<Vec<u8>, SftpError> {
        // Fast path: a cache hit needs no lock and no wire.
        if let Some(bytes) = self.cache.retrieve(hash)? {
            return Ok(bytes);
        }

        // Single-flight: take (or create) this hash's fetch lock. All clones
        // happen while holding the std `Mutex`, so the strong-count prune below
        // is race-free.
        let lock = {
            let mut locks = self.locks.lock().unwrap();
            locks
                .entry(hash.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };

        let result = {
            let _guard = lock.lock().await;
            self.fetch_verify_store(hash).await
        };

        // Reclaim the slot once no concurrent resolve still holds this lock
        // (map + our local `lock` == 2). A waiter still queued keeps a clone, so
        // the count stays > 2 and the last one out prunes it.
        {
            let mut locks = self.locks.lock().unwrap();
            if Arc::strong_count(&lock) <= 2 {
                locks.remove(hash);
            }
        }

        result
    }

    /// The miss path, run under the per-hash lock: re-check the cache (a prior
    /// holder may have just filled it), else fetch, verify, and cache.
    async fn fetch_verify_store(&self, hash: &ContentHash) -> Result<Vec<u8>, SftpError> {
        // Double-check: whoever held the lock before us may have cached it.
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

    #[test]
    fn blob_path_is_sharded_on_leading_two_hex() {
        let h = ContentHash::from_data(b"whatever");
        assert_eq!(
            SftpClient::blob_path(&h),
            format!("/v/blobs/{}/{}", h.prefix(), h)
        );
        // The shard is the leaf hash's own prefix — the server maps it back.
        assert!(SftpClient::blob_path(&h).starts_with(&format!("/v/blobs/{}/", h.prefix())));
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

    /// A fetch stub that sleeps (so concurrent resolves genuinely overlap) and
    /// counts calls across threads.
    struct SlowStub {
        body: Vec<u8>,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl BlobFetch for SlowStub {
        async fn fetch(&self, _hash: &ContentHash) -> Result<Vec<u8>, SftpError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(self.body.clone())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_resolves_of_one_hash_coalesce_to_one_fetch() {
        let dir = TempDir::new().unwrap();
        let body = b"repeated clip in a vamp".to_vec();
        let hash = ContentHash::from_data(&body);
        let resolver = Arc::new(BlobResolver::new(
            FileStore::at_path(dir.path()),
            SlowStub {
                body: body.clone(),
                calls: std::sync::atomic::AtomicUsize::new(0),
            },
        ));

        // Fire eight concurrent resolves for the SAME hash; the slow fetch means
        // they all reach the per-hash lock before the first completes.
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let r = resolver.clone();
                let h = hash.clone();
                tokio::spawn(async move { r.resolve(&h).await })
            })
            .collect();

        for handle in handles {
            let got = handle.await.unwrap().unwrap();
            assert_eq!(got, body, "every resolver gets the bytes");
        }

        assert_eq!(
            resolver.fetch.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "single-flight: concurrent misses coalesce onto one wire transfer"
        );
        // The lock map self-pruned after the flight settled.
        assert!(
            resolver.locks.lock().unwrap().is_empty(),
            "per-hash lock entry must be reclaimed once the flight completes"
        );
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
