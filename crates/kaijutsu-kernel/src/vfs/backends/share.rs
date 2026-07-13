//! `ShareFs` — the kernel-side half of `/r` client shares (reverse SFTP,
//! `docs/slash-r.md`, slice 1).
//!
//! Mounted once at `/r` (`crates/kaijutsu-server/src/rpc.rs`, before
//! `freeze_mounts()` — per-share mounts are impossible once the table
//! freezes, so this is the ONE backend that routes internally to N live
//! client sessions).
//!
//! [`ShareRegistry`] is the session-scoped, in-memory bookkeeping (rows
//! appear on channel-up, vanish on channel-down — nothing durable, a
//! client's CLI arg is the durable intent); [`ShareFs`] is the thin
//! `VfsOps` façade translating `/r/<client>/<share>/...` into
//! `(client, remote "/<share>/..." path)` pairs and delegating to the
//! registry. Registration itself (reading a client's `/index`, validating
//! shares, the live-duplicate-client-id rejection) is
//! `kaijutsu-server/src/share.rs`'s job — it owns the physical SSH channel
//! this registry's sessions ride.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use kaijutsu_types::Principal;
use kaijutsu_types::share::{GENERATION_EXTENSION, GenerationReply, GenerationRequest};

use russh_sftp::client::rawsession::RawSftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};

use crate::vfs::{
    DirEntry, FileAttr, FileType, STREAM_CHUNK_SIZE, SetAttr, StatFs, VfsError, VfsOps, VfsResult,
};

/// Ceiling on a single wire op against a client's share session
/// (`docs/slash-r.md` "Every remote op gets a timeout") — a hung laptop must
/// not park a kernel task forever.
pub const SHARE_OP_TIMEOUT: Duration = Duration::from_secs(10);

/// `READ` chunk size for [`ShareFs`]'s `read_all` override — matches the
/// forward adapter's `MAX_READ_LEN` and the SFTP `READ` window both
/// directions already use. (`STREAM_CHUNK_SIZE` — the streaming override's
/// cadence — is deliberately the same value, defined beside the trait.)
const READ_CHUNK: u32 = 256 * 1024;

/// One share a registered client is offering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareRow {
    pub name: String,
    pub rw: bool,
}

/// A live client's registration.
struct ClientEntry {
    principal: Principal,
    nick: String,
    shares: Vec<ShareRow>,
    /// `RawSftpSession` is not internally serialized against OUR usage
    /// discipline (`docs/slash-r.md`: "The session is a serialization
    /// point" — deepseek review): every op against one client's session
    /// takes this lock first, which doubles as the natural per-client
    /// throttle.
    session: Arc<Mutex<RawSftpSession>>,
    /// Compared on unregister so a fast reconnect's NEW registration can't be
    /// clobbered by the OLD connection's delayed cleanup task.
    token: Uuid,
}

/// A registration attempt refused before it reaches the registry map.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ShareRegisterError {
    /// `docs/slash-r.md` "Session shape": "a claim of a client-id already
    /// live from a DIFFERENT connection is rejected loudly."
    #[error("client id {0:?} is already live from a different connection")]
    AlreadyLive(String),
}

/// Session-scoped, in-memory registry of live client shares.
pub struct ShareRegistry {
    clients: RwLock<HashMap<String, ClientEntry>>,
    /// Bumped on every register/unregister — the coherence stamp for
    /// `/r`, `/r/<id>`, and `/r/index` listings, which have no host mtime of
    /// their own to derive a generation from.
    index_generation: AtomicU64,
}

impl Default for ShareRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ShareRegistry {
    pub fn new() -> Self {
        Self {
            clients: RwLock::new(HashMap::new()),
            index_generation: AtomicU64::new(1),
        }
    }

    fn bump_index(&self) {
        self.index_generation.fetch_add(1, Ordering::Release);
    }

    /// Current index generation — `Acquire`, paired with the `Release` in
    /// [`Self::bump_index`] (same ordering discipline as
    /// `MountTable::global_activity`/`bump_activity`).
    pub fn index_generation(&self) -> u64 {
        self.index_generation.load(Ordering::Acquire)
    }

    /// Register a validated session. Refuses loudly if `client_id` is
    /// already live from a different connection — never a silent rebind.
    /// Returns an opaque token the caller must hold and pass to
    /// [`Self::unregister`] (the compare-and-remove that keeps a slow-dying
    /// old connection's cleanup from clobbering a fast reconnect's new one).
    pub async fn register(
        &self,
        client_id: String,
        principal: Principal,
        nick: String,
        shares: Vec<ShareRow>,
        session: RawSftpSession,
    ) -> Result<Uuid, ShareRegisterError> {
        let mut clients = self.clients.write().await;
        if clients.contains_key(&client_id) {
            return Err(ShareRegisterError::AlreadyLive(client_id));
        }
        let token = Uuid::new_v4();
        clients.insert(
            client_id,
            ClientEntry {
                principal,
                nick,
                shares,
                session: Arc::new(Mutex::new(session)),
                token,
            },
        );
        drop(clients);
        self.bump_index();
        Ok(token)
    }

    /// Unregister a client, but only if `token` still matches the live
    /// entry's — a no-op if a newer registration already replaced it (the
    /// old connection's cleanup racing a reconnect's fresh register).
    pub async fn unregister(&self, client_id: &str, token: Uuid) {
        let mut clients = self.clients.write().await;
        let matches = clients.get(client_id).is_some_and(|e| e.token == token);
        if matches {
            clients.remove(client_id);
            drop(clients);
            self.bump_index();
        }
    }

    /// Live client ids, sorted (stable `readdir /r`).
    pub async fn live_clients(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.clients.read().await.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Whether `client_id` is currently registered.
    pub async fn is_live(&self, client_id: &str) -> bool {
        self.clients.read().await.contains_key(client_id)
    }

    /// The shares a live client offers, sorted by name.
    pub async fn shares_of(&self, client_id: &str) -> Option<Vec<ShareRow>> {
        self.clients.read().await.get(client_id).map(|e| {
            let mut s = e.shares.clone();
            s.sort_by(|a, b| a.name.cmp(&b.name));
            s
        })
    }

    /// The `/r/index` registry rows: `(client, nick, share, rw)`, sorted by
    /// client then share — `kj share ls` renders the same rows
    /// (`docs/slash-r.md`).
    pub async fn index_rows(&self) -> Vec<(String, String, String, bool)> {
        let clients = self.clients.read().await;
        let mut rows: Vec<(String, String, String, bool)> = Vec::new();
        for (client_id, entry) in clients.iter() {
            for share in &entry.shares {
                rows.push((client_id.clone(), entry.nick.clone(), share.name.clone(), share.rw));
            }
        }
        rows.sort();
        rows
    }

    /// The authenticated principal behind a live client-id, for display
    /// (`kj share ls`, future `/v/session` rows).
    pub async fn principal_of(&self, client_id: &str) -> Option<Principal> {
        self.clients.read().await.get(client_id).map(|e| e.principal.clone())
    }

    async fn share_row(&self, client_id: &str, share: &str) -> VfsResult<ShareRow> {
        let clients = self.clients.read().await;
        let entry = clients
            .get(client_id)
            .ok_or_else(|| VfsError::ShareDisconnected(client_id.to_string()))?;
        entry
            .shares
            .iter()
            .find(|s| s.name == share)
            .cloned()
            .ok_or_else(|| VfsError::not_found(format!("{client_id}/{share}")))
    }

    async fn session_of(&self, client_id: &str) -> VfsResult<Arc<Mutex<RawSftpSession>>> {
        self.clients
            .read()
            .await
            .get(client_id)
            .map(|e| e.session.clone())
            .ok_or_else(|| VfsError::ShareDisconnected(client_id.to_string()))
    }

    /// `getattr` on a path inside a live client's share: `LSTAT` (does not
    /// follow the final symlink — `VfsOps::getattr` is lstat-shaped
    /// throughout this crate, same convention as `LocalBackend`/the forward
    /// SFTP adapter) plus the required generation extension for that one
    /// path.
    pub async fn getattr(&self, client_id: &str, share: &str, remote_path: &str) -> VfsResult<FileAttr> {
        let row = self.share_row(client_id, share).await?;
        let session = self.session_of(client_id).await?;
        let guard = session.lock().await;
        let attrs = timeout_op(client_id, guard.lstat(remote_path.to_string())).await?.attrs;
        let generation = self
            .generations_for(client_id, &guard, std::slice::from_ref(&remote_path.to_string()))
            .await?[0];
        Ok(from_wire_attrs(&attrs, generation, row.rw))
    }

    /// `readdir` on a path inside a live client's share: pages the remote
    /// `OPENDIR`/`READDIR` to completion (this trait method has no
    /// pagination contract of its own — the SFTP adapter serving `/r` to ITS
    /// OWN clients pages independently, matching the forward adapter's own
    /// `READDIR_CHUNK` behavior one layer up).
    ///
    /// Deliberately does NOT fetch generations here: [`DirEntry`] carries
    /// only `name`/`kind` (matching every other backend's `readdir` — CasFs,
    /// LocalBackend, MemoryBackend), so a per-entry or batched generation
    /// fetch here would be wire work with nowhere to put the result. A
    /// caller that wants attrs per entry (the forward SFTP adapter's own
    /// `readdir` handler already does this generically for every backend)
    /// calls `getattr` afterward, which is where [`Self::generations_for`]'s
    /// batching would actually pay off for `/r` browsed over forward SFTP —
    /// tracked as follow-up, not solved by this trait method
    /// (`docs/slash-r.md` "Open questions": "Large-directory readdir over
    /// the wire").
    pub async fn readdir(&self, client_id: &str, share: &str, remote_dir: &str) -> VfsResult<Vec<DirEntry>> {
        self.share_row(client_id, share).await?;
        let session = self.session_of(client_id).await?;
        let guard = session.lock().await;
        let listing = timeout_op(client_id, list_dir(&guard, remote_dir)).await?;
        Ok(listing
            .into_iter()
            .map(|(name, attrs)| DirEntry { name, kind: wire_kind(&attrs) })
            .collect())
    }

    /// Cheap liveness probe against a registered session: `LSTAT /index`
    /// (the manifest every share server synthesizes — always present, no
    /// disk I/O client-side) under the usual [`SHARE_OP_TIMEOUT`]. The
    /// server's keepalive loop (`kaijutsu-server/src/share.rs`) calls this on
    /// an interval so an idle-but-dead session is evicted without waiting
    /// for the next VFS op to trip over it ("shares vanish from readdir the
    /// moment the channel drops", `docs/slash-r.md`). Takes the session lock
    /// only for the one wire op — never across the caller's sleep.
    pub async fn ping(&self, client_id: &str) -> VfsResult<()> {
        let session = self.session_of(client_id).await?;
        let guard = session.lock().await;
        timeout_op(client_id, guard.lstat("/index".to_string())).await?;
        Ok(())
    }

    /// `readlink` on a path inside a live client's share — same shape as
    /// [`Self::getattr`] (session lock, per-op timeout, mapped errors). The
    /// target comes back verbatim from the client's share server (which
    /// reports the link's own stored target, resolved-or-not is the
    /// caller's concern) — `getattr`/`readdir` already report these paths as
    /// `FileType::Symlink` via [`wire_kind`], so this is the read that makes
    /// that report honest instead of a blanket `NotASymlink`.
    pub async fn readlink(&self, client_id: &str, share: &str, remote_path: &str) -> VfsResult<PathBuf> {
        self.share_row(client_id, share).await?;
        let session = self.session_of(client_id).await?;
        let guard = session.lock().await;
        let name = timeout_op(client_id, guard.readlink(remote_path.to_string())).await?;
        let file = name
            .files
            .into_iter()
            .next()
            .ok_or_else(|| VfsError::other("readlink: empty Name reply"))?;
        Ok(PathBuf::from(file.filename))
    }

    /// A single wire `READ` — the plain (non-streaming) read this slice uses
    /// throughout (`docs/slash-r.md` explicitly defers the held-handle
    /// streaming override to a sibling lane).
    pub async fn read(
        &self,
        client_id: &str,
        share: &str,
        remote_path: &str,
        offset: u64,
        size: u32,
    ) -> VfsResult<Vec<u8>> {
        self.share_row(client_id, share).await?;
        let session = self.session_of(client_id).await?;
        let guard = session.lock().await;
        let handle = timeout_op(
            client_id,
            guard.open(remote_path.to_string(), OpenFlags::READ, FileAttributes::empty()),
        )
        .await?
        .handle;

        let result = timed_read(client_id, &guard, handle.as_str(), offset, size).await;
        let _ = timeout_op(client_id, guard.close(handle)).await;
        result
    }

    /// `read_all` override — loops `READ` to EOF rather than sizing from
    /// `getattr` (the standing gotcha: the default impl truncates a followed
    /// symlink, and here a remote's reported size could also be stale by the
    /// time the read completes). One held handle, closed on every exit path.
    pub async fn read_all(&self, client_id: &str, share: &str, remote_path: &str) -> VfsResult<Vec<u8>> {
        self.share_row(client_id, share).await?;
        let session = self.session_of(client_id).await?;
        let guard = session.lock().await;
        let handle = timeout_op(
            client_id,
            guard.open(remote_path.to_string(), OpenFlags::READ, FileAttributes::empty()),
        )
        .await?
        .handle;

        let mut out = Vec::new();
        let mut offset = 0u64;
        let result = loop {
            match timed_read(client_id, &guard, handle.as_str(), offset, READ_CHUNK).await {
                Ok(chunk) if chunk.is_empty() => break Ok(()),
                Ok(chunk) => {
                    offset += chunk.len() as u64;
                    out.extend_from_slice(&chunk);
                }
                Err(e) => break Err(e),
            }
        };
        let _ = timeout_op(client_id, guard.close(handle)).await;
        result.map(|()| out)
    }

    /// Held-handle streaming read — the RTT-amplification fix the whole
    /// design turns on (`docs/slash-r.md` "The pump rides a streaming read
    /// primitive"): ONE remote `OPEN`, then sequential `READ`s at
    /// [`STREAM_CHUNK_SIZE`], one `CLOSE` — versus the trait default's
    /// OPEN/READ/CLOSE *per chunk* when it loops [`ShareFs::read`].
    ///
    /// **Locking:** the per-client session mutex is taken PER WIRE OP (the
    /// open, each read, the close) and dropped between chunks — never held
    /// across the whole stream. A multi-GB transfer holding it for minutes
    /// would starve every other op on that client (getattr, readdir, the
    /// keepalive ping — which would then evict the session mid-transfer as
    /// "dead"). SFTP handles are session-scoped, so other ops interleaving
    /// on the same session between our READs is protocol-legal.
    ///
    /// Takes `Arc<Self>` so the returned stream is `'static`-captured
    /// (owns everything it needs); `ShareFs` clones its registry `Arc` per
    /// call. EOF/short-read contract matches the trait doc: zero-length
    /// read = clean end, a short read advances by the actual length, a wire
    /// error is the final item.
    pub fn open_read_stream(
        self: Arc<Self>,
        client_id: String,
        share: String,
        remote_path: String,
    ) -> BoxStream<'static, VfsResult<Bytes>> {
        Box::pin(async_stream::stream! {
            if let Err(e) = self.share_row(&client_id, &share).await {
                yield Err(e);
                return;
            }
            let session = match self.session_of(&client_id).await {
                Ok(s) => s,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };

            // OPEN — lock scoped to this one wire op.
            let handle = {
                let guard = session.lock().await;
                match timeout_op(
                    &client_id,
                    guard.open(remote_path.clone(), OpenFlags::READ, FileAttributes::empty()),
                )
                .await
                {
                    Ok(h) => h.handle,
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                }
            };

            // Armed from OPEN onward: if the consumer drops this stream
            // mid-transfer (an aborted pump), the guard's Drop spawns a
            // best-effort CLOSE — see [`StreamCloseGuard`] for why Drop
            // can't just close inline.
            let mut close_guard =
                StreamCloseGuard::new(session.clone(), handle.clone(), client_id.clone());

            let mut offset = 0u64;
            loop {
                // Each READ takes the lock for exactly one wire op.
                let chunk = {
                    let guard = session.lock().await;
                    timed_read(&client_id, &guard, &handle, offset, STREAM_CHUNK_SIZE).await
                };
                match chunk {
                    // Zero-length read == clean EOF.
                    Ok(c) if c.is_empty() => break,
                    // Short reads are legal: advance by the ACTUAL length.
                    Ok(c) => {
                        offset += c.len() as u64;
                        yield Ok(Bytes::from(c));
                    }
                    // A wire error is the stream's final item.
                    Err(e) => {
                        yield Err(e);
                        break;
                    }
                }
            }

            // Natural termination (EOF or error): close inline, best-effort
            // (same discipline as `read`/`read_all`/`list_dir`), and disarm
            // the drop guard so it doesn't double-close.
            close_guard.disarm();
            let guard = session.lock().await;
            let _ = timeout_op(&client_id, guard.close(handle)).await;
        })
    }

    /// Batch-fetch generations for a set of remote paths in ONE extended
    /// request — see `docs/slash-r.md`'s RTT-amplification note and
    /// `kaijutsu_types::share`'s module doc for why this rides
    /// `SSH_FXP_EXTENDED` rather than an ATTRS field.
    async fn generations_for(
        &self,
        client_id: &str,
        guard: &RawSftpSession,
        paths: &[String],
    ) -> VfsResult<Vec<u64>> {
        let req = GenerationRequest { paths: paths.to_vec() };
        let payload = russh_sftp::ser::to_bytes(&req)
            .map_err(|e| VfsError::other(format!("generation request encode: {e}")))?
            .to_vec();
        let packet = timeout_op(client_id, guard.extended(GENERATION_EXTENSION, payload)).await?;
        let russh_sftp::protocol::Packet::ExtendedReply(reply) = packet else {
            return Err(VfsError::other("generation extension: unexpected reply packet"));
        };
        let decoded: GenerationReply = russh_sftp::de::from_bytes(&mut reply.data.into())
            .map_err(|e| VfsError::other(format!("generation reply decode: {e}")))?;
        if decoded.generations.len() != paths.len() {
            return Err(VfsError::other(format!(
                "generation reply had {} entries for {} requested paths",
                decoded.generations.len(),
                paths.len()
            )));
        }
        Ok(decoded.generations)
    }
}

/// Closes a remote SFTP handle when a [`ShareRegistry::open_read_stream`]
/// consumer DROPS the stream mid-transfer (a `pump` aborted between chunks).
///
/// The constraint: `Drop::drop` is synchronous and the SFTP `CLOSE` is an
/// async wire op behind an async mutex — it cannot run inline in `Drop`. So
/// the guard `tokio::spawn`s a best-effort close with its own clones of the
/// session `Arc` + handle string; the spawned task outlives the dropped
/// stream and the remote handle is reaped promptly instead of leaking until
/// session close. A stream driven to natural termination (EOF or error)
/// closes inline in the stream body and [`Self::disarm`]s this guard first.
///
/// `Handle::try_current` rather than `tokio::spawn` directly: a panic in
/// `Drop` aborts the process, and while every real consumer is async (the
/// stream itself can only be polled on a runtime), a dropped-without-polling
/// stream on a non-runtime thread must degrade to the documented leak, not
/// an abort.
struct StreamCloseGuard {
    session: Option<Arc<Mutex<RawSftpSession>>>,
    handle: String,
    client_id: String,
}

impl StreamCloseGuard {
    fn new(session: Arc<Mutex<RawSftpSession>>, handle: String, client_id: String) -> Self {
        Self {
            session: Some(session),
            handle,
            client_id,
        }
    }

    /// Take over closing responsibility (the stream body closes inline on
    /// natural termination) — the guard's `Drop` becomes a no-op.
    fn disarm(&mut self) {
        self.session = None;
    }
}

impl Drop for StreamCloseGuard {
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return; // disarmed — the stream body closed inline
        };
        let handle = std::mem::take(&mut self.handle);
        let client_id = std::mem::take(&mut self.client_id);
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(async move {
                    let guard = session.lock().await;
                    if let Err(e) = timeout_op(&client_id, guard.close(handle)).await {
                        // Best-effort: the session may already be gone (a
                        // dropped stream often accompanies a disconnect).
                        tracing::debug!("share stream drop-close for {client_id}: {e}");
                    }
                });
            }
            Err(_) => {
                tracing::warn!(
                    "share stream for {client_id} dropped outside a tokio runtime; \
                     remote handle leaks until session close"
                );
            }
        }
    }
}

/// `OPENDIR`/`READDIR`-to-`Eof`/`CLOSE`, mirroring the high-level
/// `SftpSession::read_dir` helper's loop (we're on `RawSftpSession` instead,
/// for the custom generation extension it doesn't expose). `.`/`..` entries
/// are dropped — this is the mount-relative listing `ShareFs::readdir`
/// returns, which never carries synthetic dot-entries (matches every other
/// backend in this crate).
async fn list_dir(
    guard: &RawSftpSession,
    remote_dir: &str,
) -> Result<Vec<(String, FileAttributes)>, russh_sftp::client::error::Error> {
    let handle = guard.opendir(remote_dir.to_string()).await?.handle;
    let mut out = Vec::new();
    loop {
        match guard.readdir(handle.as_str()).await {
            Ok(name) => {
                for f in name.files {
                    if f.filename == "." || f.filename == ".." {
                        continue;
                    }
                    out.push((f.filename, f.attrs));
                }
            }
            Err(russh_sftp::client::error::Error::Status(s))
                if s.status_code == StatusCode::Eof =>
            {
                break;
            }
            Err(e) => {
                let _ = guard.close(handle).await;
                return Err(e);
            }
        }
    }
    // Best-effort close, matching this function's own error path and
    // `read`/`read_all`'s discipline: the listing is already complete and
    // correct — propagating a failed CLOSE here would discard good data over
    // a handle the remote will reap at session end anyway.
    let _ = guard.close(handle).await;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn wire_kind(attrs: &FileAttributes) -> FileType {
    if attrs.is_dir() {
        FileType::Directory
    } else if attrs.is_symlink() {
        FileType::Symlink
    } else {
        FileType::File
    }
}

/// Translate a remote `FileAttributes` into a kernel `FileAttr`. Attrs are
/// TRANSLATED, not passed through (`docs/slash-r.md`): remote uid/gid are
/// squashed away (a laptop's numeric ids are meaningless kernel-side), and
/// mode bits are synthesized from the share's ro/rw state rather than
/// trusted from the remote stat. Remote mtime stays display-only; coherence
/// is `generation`, supplied by the caller from the required extension.
fn from_wire_attrs(attrs: &FileAttributes, generation: u64, rw: bool) -> FileAttr {
    let kind = wire_kind(attrs);
    let perm = match (kind, rw) {
        (FileType::Directory, true) => 0o755,
        (FileType::Directory, false) => 0o555,
        (_, true) => 0o644,
        (_, false) => 0o444,
    };
    let mtime = attrs
        .mtime
        .map(|s| UNIX_EPOCH + Duration::from_secs(s as u64))
        .unwrap_or(UNIX_EPOCH);
    let atime = attrs.atime.map(|s| UNIX_EPOCH + Duration::from_secs(s as u64));
    FileAttr {
        size: attrs.size.unwrap_or(0),
        kind,
        perm,
        mtime,
        generation,
        atime,
        ctime: None,
        nlink: 1,
        uid: None,
        gid: None,
    }
}

/// Map a `russh_sftp` client-side error onto the closest [`VfsError`],
/// distinguishing a genuine "no such path" (`SSH_FX_NO_SUCH_FILE`) from a
/// transport/protocol failure — anything past status-code granularity
/// indicates the wire itself broke, which is a disconnect from `ShareFs`'s
/// point of view (the caller sees a fresh error on the next op regardless,
/// since the underlying `RawSftpSession` drops its pending requests once its
/// channel closes).
fn map_client_err(client_id: &str, e: russh_sftp::client::error::Error) -> VfsError {
    use russh_sftp::client::error::Error as E;
    match e {
        E::Status(s) if s.status_code == StatusCode::NoSuchFile => VfsError::not_found(s.error_message),
        E::Status(s) if s.status_code == StatusCode::PermissionDenied => {
            VfsError::permission_denied(s.error_message)
        }
        // EOF is a distinct, non-error outcome for the read path — callers
        // that care (`timed_read`) intercept the raw `Eof` status BEFORE it
        // reaches this generic mapper; a bare `Eof` reaching here means some
        // OTHER op hit end-of-file unexpectedly (e.g. `lstat`/`extended`
        // never should), which is honestly a protocol-shape surprise.
        other => VfsError::ShareDisconnected(format!("{client_id}: {other}")),
    }
}

/// Run one wire op under [`SHARE_OP_TIMEOUT`], mapping a timeout or a
/// protocol error to the dedicated `/r` errors.
async fn timeout_op<T, F>(client_id: &str, fut: F) -> VfsResult<T>
where
    F: std::future::Future<Output = Result<T, russh_sftp::client::error::Error>>,
{
    match tokio::time::timeout(SHARE_OP_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(map_client_err(client_id, e)),
        Err(_) => Err(VfsError::ShareTimeout(client_id.to_string())),
    }
}

/// A single `READ`, treating end-of-file as `Ok(vec![])` rather than an
/// error — the `SSH_FX_EOF` status is how the wire signals "no more bytes at
/// this offset," which is exactly [`VfsOps::read`]'s own contract for a read
/// past EOF. Intercepted here, before [`map_client_err`]'s generic mapping,
/// so EOF is never confused with a real protocol failure.
async fn timed_read(
    client_id: &str,
    guard: &RawSftpSession,
    handle: &str,
    offset: u64,
    len: u32,
) -> VfsResult<Vec<u8>> {
    match tokio::time::timeout(SHARE_OP_TIMEOUT, guard.read(handle, offset, len)).await {
        Ok(Ok(data)) => Ok(data.data),
        Ok(Err(russh_sftp::client::error::Error::Status(s))) if s.status_code == StatusCode::Eof => {
            Ok(Vec::new())
        }
        Ok(Err(e)) => Err(map_client_err(client_id, e)),
        Err(_) => Err(VfsError::ShareTimeout(client_id.to_string())),
    }
}

/// What a `/r`-mount-relative path names.
enum Route {
    /// `/r` itself.
    Root,
    /// `/r/index`.
    Index,
    /// `/r/<client_id>`.
    Client(String),
    /// `/r/<client_id>/<share>[/<rest>]`. `remote_path` is already the
    /// `/<share>/<rest>`-shaped string the client's own SFTP session expects.
    InShare {
        client_id: String,
        share: String,
        remote_path: String,
    },
}

/// `/r/index`'s literal name, distinct from a client id — a client id could
/// theoretically collide with this string (client ids are caller-chosen), in
/// which case the client is simply unreachable under `/r` while its id
/// remains "index"; not worth guarding further; a real client-id UUID never
/// collides in practice.
const INDEX_NAME: &str = "index";

/// Lexically clean a mount-relative path into `Normal` components — same
/// discipline as every other backend's path resolution in this crate
/// (`CasFs::segments`, the forward SFTP adapter's `canonicalize`).
fn clean_components(path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s.to_string_lossy().into_owned()),
        }
    }
    out
}

fn route(path: &Path) -> Route {
    let comps = clean_components(path);
    match comps.as_slice() {
        [] => Route::Root,
        [name] if name == INDEX_NAME => Route::Index,
        [client_id] => Route::Client(client_id.clone()),
        [client_id, share] => Route::InShare {
            client_id: client_id.clone(),
            share: share.clone(),
            remote_path: format!("/{share}"),
        },
        [client_id, share, rest @ ..] => Route::InShare {
            client_id: client_id.clone(),
            share: share.clone(),
            remote_path: format!("/{share}/{}", rest.join("/")),
        },
    }
}

fn dir_attr() -> FileAttr {
    FileAttr::directory(0o555)
}

/// The `VfsOps` façade over [`ShareRegistry`], mounted at `/r`.
pub struct ShareFs {
    registry: Arc<ShareRegistry>,
}

impl ShareFs {
    pub fn new(registry: Arc<ShareRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl VfsOps for ShareFs {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        match route(path) {
            Route::Root => Ok(dir_attr()),
            Route::Index => {
                let rows = self.registry.index_rows().await;
                let bytes = index_bytes(&rows);
                let mut attr = FileAttr::file(bytes.len() as u64, 0o444);
                attr.generation = self.registry.index_generation();
                Ok(attr)
            }
            Route::Client(id) => {
                if self.registry.is_live(&id).await {
                    Ok(dir_attr())
                } else {
                    Err(VfsError::not_found(id))
                }
            }
            Route::InShare { client_id, share, remote_path } => {
                self.registry.getattr(&client_id, &share, &remote_path).await
            }
        }
    }

    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        match route(path) {
            Route::Root => {
                let mut entries: Vec<DirEntry> = self
                    .registry
                    .live_clients()
                    .await
                    .into_iter()
                    .map(DirEntry::directory)
                    .collect();
                entries.push(DirEntry::file(INDEX_NAME));
                Ok(entries)
            }
            Route::Index => Err(VfsError::not_a_directory(INDEX_NAME)),
            Route::Client(id) => {
                let shares = self
                    .registry
                    .shares_of(&id)
                    .await
                    .ok_or_else(|| VfsError::not_found(id))?;
                Ok(shares.into_iter().map(|s| DirEntry::directory(s.name)).collect())
            }
            Route::InShare { client_id, share, remote_path } => {
                self.registry.readdir(&client_id, &share, &remote_path).await
            }
        }
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        match route(path) {
            Route::Index => {
                let rows = self.registry.index_rows().await;
                let bytes = index_bytes(&rows);
                let start = (offset as usize).min(bytes.len());
                let end = (start + size as usize).min(bytes.len());
                Ok(bytes[start..end].to_vec())
            }
            Route::Root | Route::Client(_) => Err(VfsError::is_a_directory(path.display().to_string())),
            Route::InShare { client_id, share, remote_path } => {
                self.registry.read(&client_id, &share, &remote_path, offset, size).await
            }
        }
    }

    async fn read_all(&self, path: &Path) -> VfsResult<Vec<u8>> {
        match route(path) {
            Route::Index => Ok(index_bytes(&self.registry.index_rows().await)),
            Route::Root | Route::Client(_) => Err(VfsError::is_a_directory(path.display().to_string())),
            Route::InShare { client_id, share, remote_path } => {
                self.registry.read_all(&client_id, &share, &remote_path).await
            }
        }
    }

    /// Held-handle streaming override — the stitch both `/r` lanes were
    /// built toward (`docs/slash-r.md` slice 0's RTT-amplification catch):
    /// a pump from `/r/<id>/<share>/file` holds ONE remote SFTP handle for
    /// the whole transfer via [`ShareRegistry::open_read_stream`], where the
    /// trait default looping [`Self::read`] would OPEN/READ/CLOSE per
    /// 256 KiB chunk — three round trips each at network latency.
    ///
    /// The synthetic levels (`/r`, `/r/index`, `/r/<id>`, and the share dir
    /// itself) fall back to the loop-`read` shape of the trait default:
    /// `/r/index` is tiny (one `read` covers it), and a directory errors
    /// naturally — and loudly — on the first read.
    fn open_read_stream<'a>(&'a self, path: &'a Path) -> BoxStream<'a, VfsResult<Bytes>> {
        if let Route::InShare { client_id, share, remote_path } = route(path)
            && remote_path != format!("/{share}")
        {
            return self.registry.clone().open_read_stream(client_id, share, remote_path);
        }
        // Trait-default shape, restated (an overriding impl can't call the
        // default it replaced): loop this backend's own `read` at the shared
        // chunk size.
        Box::pin(futures::stream::unfold(
            (self, path, 0u64, false),
            |(this, path, offset, done)| async move {
                if done {
                    return None;
                }
                match this.read(path, offset, STREAM_CHUNK_SIZE).await {
                    Ok(chunk) if chunk.is_empty() => None,
                    Ok(chunk) => {
                        let advanced = offset + chunk.len() as u64;
                        Some((Ok(Bytes::from(chunk)), (this, path, advanced, false)))
                    }
                    Err(e) => Some((Err(e), (this, path, offset, true))),
                }
            },
        ))
    }

    async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
        match route(path) {
            // The synthetic levels (/r, /r/index, /r/<id>, and /r/<id>/<share>
            // itself — a share root is always a real directory, never a link)
            // genuinely hold no symlinks.
            Route::Root | Route::Index | Route::Client(_) => {
                Err(VfsError::NotASymlink(path.display().to_string()))
            }
            // A share root (`/r/<id>/<share>`, remote "/<share>") is always a
            // real directory — answered locally so the client's generic
            // Failure reply can't be mislabeled a disconnect by
            // map_client_err.
            Route::InShare { ref share, ref remote_path, .. }
                if *remote_path == format!("/{share}") =>
            {
                Err(VfsError::NotASymlink(path.display().to_string()))
            }
            // Inside a share the entry may really be a symlink — getattr and
            // readdir already report FileType::Symlink for these via
            // wire_kind, so the readlink that backs that report goes over the
            // wire. (A non-link path answers with the remote's own error.)
            Route::InShare { client_id, share, remote_path } => {
                self.registry.readlink(&client_id, &share, &remote_path).await
            }
        }
    }

    // ── writes: no write support this slice regardless of a share's
    // advertised `rw` (`docs/slash-r.md` slice 1 scope — slice 3 is
    // writable shares, with both-ends enforcement). ──

    async fn write(&self, _path: &Path, _offset: u64, _data: &[u8]) -> VfsResult<u32> {
        Err(VfsError::ReadOnly)
    }
    async fn create(&self, _path: &Path, _mode: u32) -> VfsResult<FileAttr> {
        Err(VfsError::ReadOnly)
    }
    async fn mkdir(&self, _path: &Path, _mode: u32) -> VfsResult<FileAttr> {
        Err(VfsError::ReadOnly)
    }
    async fn unlink(&self, _path: &Path) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }
    async fn rmdir(&self, _path: &Path) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }
    async fn rename(&self, _from: &Path, _to: &Path) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }
    async fn truncate(&self, _path: &Path, _size: u64) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }
    async fn setattr(&self, _path: &Path, _attr: SetAttr) -> VfsResult<FileAttr> {
        Err(VfsError::ReadOnly)
    }
    async fn symlink(&self, _path: &Path, _target: &Path) -> VfsResult<FileAttr> {
        Err(VfsError::ReadOnly)
    }
    async fn link(&self, _oldpath: &Path, _newpath: &Path) -> VfsResult<FileAttr> {
        Err(VfsError::ReadOnly)
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn statfs(&self) -> VfsResult<StatFs> {
        Ok(StatFs::default())
    }

    async fn real_path(&self, _path: &Path) -> VfsResult<Option<PathBuf>> {
        Ok(None)
    }

    /// Every `readdir` under `/r` is a network round trip to somebody's
    /// laptop (`docs/slash-r.md` "The crawl boundary") — the kernel's own
    /// ambient machinery must never walk it unprompted.
    fn opaque_to_sweeps(&self) -> bool {
        true
    }
}

/// Render the `/r/index` registry TSV: `client  nick  share  rw  attached  path`
/// (slash-v index style; `kj share ls` renders the same rows,
/// `docs/slash-r.md`). `attached` is always `"yes"` here — a row only exists
/// while the client is live (there is no stale-row state to distinguish).
fn index_bytes(rows: &[(String, String, String, bool)]) -> Vec<u8> {
    let mut out = String::from("client\tnick\tshare\trw\tattached\tpath\n");
    for (client, nick, share, rw) in rows {
        out.push_str(client);
        out.push('\t');
        out.push_str(nick);
        out.push('\t');
        out.push_str(share);
        out.push('\t');
        out.push_str(if *rw { "rw" } else { "ro" });
        out.push('\t');
        out.push_str("yes");
        out.push('\t');
        out.push_str(&kaijutsu_types::paths::r_share_path(client, share));
        out.push('\n');
    }
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_splits_client_share_and_rest() {
        assert!(matches!(route(Path::new("")), Route::Root));
        assert!(matches!(route(Path::new("index")), Route::Index));
        assert!(matches!(route(Path::new("c1")), Route::Client(id) if id == "c1"));
        match route(Path::new("c1/downloads/sub/file.txt")) {
            Route::InShare { client_id, share, remote_path } => {
                assert_eq!(client_id, "c1");
                assert_eq!(share, "downloads");
                assert_eq!(remote_path, "/downloads/sub/file.txt");
            }
            _ => panic!("expected InShare"),
        }
    }

    #[tokio::test]
    async fn duplicate_client_id_registration_is_rejected() {
        let registry = ShareRegistry::new();
        // A registry with no live session can still be exercised for the
        // rejection path via a session that will never be dialed — we only
        // need the map contract, so construct a `RawSftpSession` over a
        // throwaway duplex pipe (never driven).
        let (a, _b) = tokio::io::duplex(64);
        let session = RawSftpSession::new(a);
        registry
            .register(
                "client-1".to_string(),
                Principal::system(),
                "nick".to_string(),
                vec![ShareRow { name: "share".to_string(), rw: false }],
                session,
            )
            .await
            .expect("first registration succeeds");

        let (c, _d) = tokio::io::duplex(64);
        let second = RawSftpSession::new(c);
        let err = registry
            .register(
                "client-1".to_string(),
                Principal::system(),
                "nick2".to_string(),
                vec![],
                second,
            )
            .await
            .expect_err("a live client id must reject a second registration");
        assert_eq!(err, ShareRegisterError::AlreadyLive("client-1".to_string()));
    }

    #[tokio::test]
    async fn unregister_with_a_stale_token_is_a_no_op() {
        let registry = ShareRegistry::new();
        let (a, _b) = tokio::io::duplex(64);
        let token = registry
            .register(
                "client-1".to_string(),
                Principal::system(),
                "nick".to_string(),
                vec![],
                RawSftpSession::new(a),
            )
            .await
            .unwrap();

        // Simulate a fast reconnect: unregister, then re-register under the
        // same id before the OLD connection's cleanup (using the OLD token)
        // runs.
        registry.unregister("client-1", token).await;
        let (c, _d) = tokio::io::duplex(64);
        let new_token = registry
            .register(
                "client-1".to_string(),
                Principal::system(),
                "nick-reconnected".to_string(),
                vec![],
                RawSftpSession::new(c),
            )
            .await
            .unwrap();

        // The stale token must NOT evict the fresh registration.
        registry.unregister("client-1", token).await;
        assert!(registry.is_live("client-1").await, "stale-token unregister must be a no-op");

        registry.unregister("client-1", new_token).await;
        assert!(!registry.is_live("client-1").await);
    }

    #[tokio::test]
    async fn index_rows_reflect_live_registrations() {
        let registry = ShareRegistry::new();
        let (a, _b) = tokio::io::duplex(64);
        registry
            .register(
                "client-1".to_string(),
                Principal::system(),
                "amy-laptop".to_string(),
                vec![
                    ShareRow { name: "downloads".to_string(), rw: false },
                    ShareRow { name: "src".to_string(), rw: true },
                ],
                RawSftpSession::new(a),
            )
            .await
            .unwrap();

        let rows = registry.index_rows().await;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.0 == "client-1" && r.2 == "downloads" && !r.3));
        assert!(rows.iter().any(|r| r.0 == "client-1" && r.2 == "src" && r.3));
    }

    #[test]
    fn index_bytes_render_the_registry_tsv() {
        let rows = vec![
            ("c1".to_string(), "amy-laptop".to_string(), "downloads".to_string(), false),
        ];
        let bytes = index_bytes(&rows);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("client\tnick\tshare\trw\tattached\tpath\n"));
        assert!(text.contains("c1\tamy-laptop\tdownloads\tro\tyes\t/r/c1/downloads\n"));
    }

    #[tokio::test]
    async fn share_fs_root_readdir_lists_live_clients_and_index() {
        let registry = Arc::new(ShareRegistry::new());
        let (a, _b) = tokio::io::duplex(64);
        registry
            .register(
                "c1".to_string(),
                Principal::system(),
                "nick".to_string(),
                vec![ShareRow { name: "downloads".to_string(), rw: false }],
                RawSftpSession::new(a),
            )
            .await
            .unwrap();

        let fs = ShareFs::new(registry);
        let entries = fs.readdir(Path::new("")).await.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"c1"));
        assert!(names.contains(&"index"));
    }

    #[tokio::test]
    async fn share_fs_getattr_on_unknown_client_is_not_found() {
        let registry = Arc::new(ShareRegistry::new());
        let fs = ShareFs::new(registry);
        let err = fs.getattr(Path::new("no-such-client")).await.unwrap_err();
        assert!(matches!(err, VfsError::NotFound(_)));
    }

    /// The synthetic namespace levels (`/r`, `/r/index`, `/r/<id>`, and the
    /// share root itself) answer readlink locally with `NotASymlink` — no
    /// wire op, no live session needed. Paths INSIDE a share go over the
    /// wire (covered by the integration suite, which has a live session).
    #[tokio::test]
    async fn readlink_on_synthetic_levels_is_not_a_symlink_locally() {
        let registry = Arc::new(ShareRegistry::new());
        let fs = ShareFs::new(registry);
        for path in ["", "index", "c1", "c1/share"] {
            let err = fs.readlink(Path::new(path)).await.unwrap_err();
            assert!(
                matches!(err, VfsError::NotASymlink(_)),
                "readlink({path:?}) must be NotASymlink, got {err:?}"
            );
        }
    }

    #[test]
    fn share_fs_is_opaque_to_sweeps() {
        let fs = ShareFs::new(Arc::new(ShareRegistry::new()));
        assert!(fs.opaque_to_sweeps());
        assert!(fs.read_only());
    }
}
