//! SFTP adapter over the kernel VFS.
//!
//! Bridges `russh_sftp::server::Handler` onto the kernel's [`VfsOps`] mount
//! tree, so any off-the-shelf SFTP client (sshfs, `sftp`, an editor's remote-FS
//! plugin) reads and writes the unified tree — host FS, CRDT-backed `/etc/rc`
//! and `/v/...`, and the memory scratch at `/tmp` — over the same SSH server
//! that carries the Cap'n Proto RPC channel. See `docs/sftp.md`.
//!
//! Unlike the RPC channel (capnp is `!Send`, so it needs a dedicated
//! current-thread runtime + `LocalSet`), an SFTP session's handler futures are
//! `Send` and run directly on the server's ambient tokio runtime —
//! `russh_sftp::server::run` spawns the per-connection processing loop itself.
//! That loop is *sequential* per session (one packet processed to completion
//! before the next), so the handle map needs no interior mutability.
//!
//! This module is built in slices (see `docs/sftp.md` → Implementation slices):
//! the read path lands first; write, OpenSSH extensions, the TOCTOU
//! generation-guard, and capability binding follow.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use kaijutsu_kernel::{
    DirEntry, FileAttr, FileType, MountTable, SetAttr, StatFs, VfsError, VfsOps, VfsResult,
};
use kaijutsu_types::Principal;
use kaijutsu_types::paths;

use russh_sftp::extensions::{
    self, FsyncExtension, HardlinkExtension, Statvfs, StatvfsExtension,
};
use russh_sftp::protocol::{
    Attrs, Data, ExtendedReply, File, FileAttributes, Handle, Name, OpenFlags, Packet, Status,
    StatusCode, Version,
};
use russh_sftp::server::{Handler, StatusReply};

/// `posix-rename@openssh.com` is the OpenSSH extension sshfs uses for an
/// overwrite-on-exists rename; russh-sftp has no constant for it.
const POSIX_RENAME: &str = "posix-rename@openssh.com";

/// Cap on a single `READ`'s byte count, so a client can't ask us to allocate
/// (and frame) an unbounded buffer. 256 KiB matches common SFTP read windows.
const MAX_READ_LEN: u32 = 256 * 1024;

/// Symlink-follow hop limit for `STAT` (loop protection).
const MAX_SYMLINK_HOPS: usize = 16;

/// Directory entries returned per `READDIR` reply. Bounds the `Name` packet size
/// (a single all-entries reply would exceed the SSH channel max packet for a
/// large directory and disconnect the client) and the per-call `getattr` cost.
const READDIR_CHUNK: usize = 64;

/// Cap on simultaneously-open handles per session, so a client that opens
/// without closing can't exhaust memory. (A coarse stopgap until the slice-4
/// limits work; per-session because the handle map is per-session.)
const MAX_OPEN_HANDLES: usize = 1024;

/// Wire payload of `posix-rename@openssh.com`: two SSH strings, same layout as
/// [`HardlinkExtension`]. Defined locally because russh-sftp ships no struct for
/// this extension.
#[derive(serde::Deserialize)]
struct PosixRenameExtension {
    oldpath: String,
    newpath: String,
}

/// An open file handle: the resolved path plus the access flags. The
/// `generation` captured at open is the TOCTOU re-verify anchor used by the
/// write path (later slice).
struct OpenFile {
    path: PathBuf,
    /// Whether this handle permits reads (gates `read`).
    read: bool,
    /// Whether this handle was opened for writing (gates `write`/`fsetstat`).
    write: bool,
    /// APPEND mode: every write is forced to the current end-of-file, ignoring
    /// the client-supplied offset (SFTPv3 `SSH_FXF_APPEND`).
    append: bool,
    /// Content `generation` last observed through this handle — the TOCTOU
    /// re-verify anchor, captured at open and refreshed after each write.
    generation: u64,
}

/// An open directory handle. `readdir` emits the listing in bounded chunks
/// (one `getattr` per entry, on demand), then signals `Eof` — never one giant
/// `Name` packet that would blow the SSH channel's max-packet limit and
/// disconnect the client. The lightweight `DirEntry` list is held; the heavy
/// per-entry `File`/attrs are built a chunk at a time.
struct OpenDir {
    path: PathBuf,
    entries: Vec<DirEntry>,
    cursor: usize,
    /// Synthetic `.`/`..` are emitted once, in the first chunk (sshfs expects
    /// them even when the backend's `readdir` omits them).
    dots_sent: bool,
}

enum HandleEntry {
    File(OpenFile),
    Dir(OpenDir),
}

/// Per-connection SFTP session bound to the authenticated principal.
pub struct SftpSession {
    /// The authenticated principal this session acts as. Carried for logging
    /// and the forthcoming capability binding (slice 3); reads/writes act as
    /// this `who` once the binding lands.
    principal: Principal,
    /// Opaque per-session id, stamped on every operation span so a whole sshfs
    /// session's traces correlate by `sftp.session` in telemetry.
    session_id: String,
    vfs: Arc<MountTable>,
    handles: HashMap<String, HandleEntry>,
    next_handle: u64,
}

impl SftpSession {
    pub fn new(principal: Principal, vfs: Arc<MountTable>) -> Self {
        Self {
            principal,
            session_id: uuid::Uuid::now_v7().to_string(),
            vfs,
            handles: HashMap::new(),
            next_handle: 0,
        }
    }

    /// Fail loud if this session is at its open-handle ceiling, so a client
    /// that opens without closing can't exhaust memory.
    fn handle_capacity_denied(&self) -> Option<StatusReply> {
        if self.handles.len() >= MAX_OPEN_HANDLES {
            Some(StatusReply::new(StatusCode::Failure).with_message("too many open handles"))
        } else {
            None
        }
    }

    /// Allocate a fresh opaque handle string for an open file/dir.
    fn alloc_handle(&mut self, entry: HandleEntry) -> String {
        let id = self.next_handle;
        self.next_handle += 1;
        let key = format!("h{id}");
        self.handles.insert(key.clone(), entry);
        key
    }

    /// Resolve a path to its final target, following symlinks — SFTP `STAT`
    /// semantics, bounded by [`MAX_SYMLINK_HOPS`]. `VfsOps::getattr` is
    /// lstat-shaped (it does not follow the final link), so `STAT` must walk the
    /// chain explicitly; `LSTAT` uses `getattr` directly.
    async fn getattr_following(&self, path: &Path) -> VfsResult<FileAttr> {
        let mut cur = path.to_path_buf();
        for _ in 0..MAX_SYMLINK_HOPS {
            let attr = self.vfs.getattr(&cur).await?;
            if !attr.is_symlink() {
                return Ok(attr);
            }
            let target = self.vfs.readlink(&cur).await?;
            cur = if target.is_absolute() {
                canonicalize(&target.to_string_lossy())
            } else {
                // A relative target resolves against the link's parent.
                let parent = cur.parent().unwrap_or(Path::new("/"));
                canonicalize(&parent.join(&target).to_string_lossy())
            };
        }
        Err(VfsError::TooManySymlinks)
    }
}

/// Lexically canonicalize a client-supplied path into an absolute path rooted
/// at `/`, the root of the global VFS tree.
///
/// This is **lexical** normalization — defense-in-depth, not the real escape
/// guard. It resolves `.` and `..` *without* string surgery, clamping `..` at
/// the root (a pop on an empty stack is a no-op) and treating a relative path
/// as relative to `/`, so `realpath(".")` resolves to `/`. Its job is to hand
/// `MountTable` a clean absolute path that routes to the right mount; the
/// authoritative escape check (symlinks resolved, `starts_with(root)`) lives in
/// the backends (`LocalBackend::resolve`, `ConfigCrdtFs::resolve`), which a
/// lexical normalizer structurally cannot replace.
fn canonicalize(raw: &str) -> PathBuf {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    for comp in Path::new(raw).components() {
        match comp {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(c) => out.push(c.to_os_string()),
        }
    }
    let mut p = PathBuf::from("/");
    for c in out {
        p.push(c);
    }
    p
}

/// Map a [`VfsError`] to the closest SFTP [`StatusCode`].
///
/// SFTPv3 has a thin error vocabulary; we lean on `NoSuchFile`,
/// `PermissionDenied`, and the catch-all `Failure`. Getting `Eof` right matters
/// for reads (a wrong code hangs clients), so `Eof` is produced explicitly at
/// the call sites, not derived from a `VfsError`.
fn status_for(err: &VfsError) -> StatusCode {
    match err {
        VfsError::NotFound(_) | VfsError::NoMountPoint(_) => StatusCode::NoSuchFile,
        VfsError::PermissionDenied(_)
        | VfsError::ReadOnly
        | VfsError::PathEscapesRoot(_) => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

/// Build a fail-loud [`StatusReply`] carrying the VFS error text, so a client
/// (and our logs) see *why* an op failed rather than a bare code.
fn reply(err: VfsError) -> StatusReply {
    status_for(&err).with_message(err.to_string())
}

/// Refuse SFTP writes to the capability-gated trees until the SFTP session
/// carries a real loadout binding (slice 3 in `docs/sftp.md`).
///
/// `/etc/rc` and `/etc/config` are the two privileged write surfaces the file
/// tools gate with `RcWrite`/`ConfigWrite` (`mcp/binding.rs`). Those gates live
/// above `VfsOps`, so a raw SFTP write would *bypass* them. Rather than silently
/// becoming a capability-bypass, an SFTP write here fails loud; the gate is
/// wired through the shared guard in a later slice. Everything else is governed
/// by the mount's own `read_only()` flag, which `VfsOps` already enforces.
fn privileged_write_denied(path: &Path) -> Option<StatusReply> {
    let s = path.to_string_lossy();
    // Component-boundary match (so `/etc/rcfoo` is NOT mistaken for `/etc/rc`)
    // via the shared predicate in `kaijutsu_types::paths` — the single source
    // of truth for this boundary check, also used by `editor::config_owned`
    // and the file-tools `is_rc_path` gate.
    if paths::is_rc_path(&s) || paths::is_config_path(&s) {
        Some(StatusCode::PermissionDenied.with_message(
            "SFTP writes to /etc/rc and /etc/config are not yet capability-gated; refused",
        ))
    } else {
        None
    }
}

/// Extract the permission bits (low 12) an attr packet carries, or `default`.
/// The wire `permissions` field also encodes the file-type mode bits, so it is
/// masked before use as a create/mkdir mode.
fn perm_from_attrs(attrs: &FileAttributes, default: u32) -> u32 {
    attrs.permissions.map(|p| p & 0o7777).unwrap_or(default)
}

/// Translate a wire [`FileAttributes`] into a kernel [`SetAttr`]. Times arrive
/// as `u32` Unix seconds; `permissions` is masked to the permission bits.
fn set_attr_from(attrs: &FileAttributes) -> SetAttr {
    let mut set = SetAttr::new();
    set.size = attrs.size;
    set.perm = attrs.permissions.map(|p| p & 0o7777);
    set.uid = attrs.uid;
    set.gid = attrs.gid;
    set.atime = attrs
        .atime
        .map(|t| UNIX_EPOCH + Duration::from_secs(t as u64));
    set.mtime = attrs
        .mtime
        .map(|t| UNIX_EPOCH + Duration::from_secs(t as u64));
    set
}

/// Seconds since the Unix epoch, saturating — SFTPv3 attrs carry a `u32` time.
fn unix_secs(t: SystemTime) -> u32 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

/// Convert kernel [`FileAttr`] into the wire [`FileAttributes`], encoding the
/// file *type* into the Unix mode bits alongside the permission bits (clients
/// read `S_IFDIR`/`S_IFLNK`/`S_IFREG` to render `ls -l` type columns).
fn to_attributes(attr: &FileAttr) -> FileAttributes {
    let mut fa = FileAttributes::empty();
    fa.size = Some(attr.size);
    fa.uid = attr.uid;
    fa.gid = attr.gid;
    fa.permissions = Some(attr.perm);
    match attr.kind {
        FileType::File => fa.set_regular(true),
        FileType::Directory => fa.set_dir(true),
        FileType::Symlink => fa.set_symlink(true),
    }
    fa.atime = attr.atime.map(unix_secs);
    fa.mtime = Some(unix_secs(attr.mtime));
    fa
}

/// Build a directory listing `File` (name + `ls -l` longname) from a kernel
/// [`DirEntry`] and its [`FileAttr`].
fn dir_file(entry: &DirEntry, attr: &FileAttr) -> File {
    File::new(entry.name.clone(), to_attributes(attr))
}

impl Handler for SftpSession {
    type Error = StatusReply;

    fn unimplemented(&self) -> Self::Error {
        StatusReply::new(StatusCode::OpUnsupported)
    }

    #[tracing::instrument(
        name = "sftp.init",
        level = "info",
        skip_all,
        fields(sftp.session = %self.session_id, sftp.user = %self.principal.username)
    )]
    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        // Advertise the OpenSSH extensions stock clients depend on. sshfs in
        // particular *refuses to write* without `statvfs@openssh.com` and uses
        // `posix-rename@openssh.com` for atomic overwrite-renames.
        let mut ext = HashMap::new();
        ext.insert(extensions::STATVFS.to_string(), "2".to_string());
        ext.insert(POSIX_RENAME.to_string(), "1".to_string());
        ext.insert(extensions::HARDLINK.to_string(), "1".to_string());
        ext.insert(extensions::FSYNC.to_string(), "1".to_string());
        Ok(Version {
            version: russh_sftp::protocol::VERSION,
            extensions: ext,
        })
    }

    #[tracing::instrument(name = "sftp.realpath", level = "debug", skip_all, fields(sftp.session = %self.session_id, sftp.user = %self.principal.username, sftp.path = %path))]
    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let resolved = canonicalize(&path);
        log::debug!(
            "sftp realpath {:?} -> {} ({})",
            path,
            resolved.display(),
            self.principal.username
        );
        Ok(Name {
            id,
            files: vec![File::dummy(resolved.to_string_lossy().into_owned())],
        })
    }

    #[tracing::instrument(name = "sftp.stat", level = "debug", skip_all, fields(sftp.session = %self.session_id, sftp.user = %self.principal.username, sftp.path = %path))]
    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        // STAT follows symlinks to the final target.
        let path = canonicalize(&path);
        let attr = self.getattr_following(&path).await.map_err(reply)?;
        Ok(Attrs {
            id,
            attrs: to_attributes(&attr),
        })
    }

    #[tracing::instrument(name = "sftp.lstat", level = "debug", skip_all, fields(sftp.session = %self.session_id, sftp.user = %self.principal.username, sftp.path = %path))]
    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        // LSTAT does not follow the final symlink; getattr is lstat-shaped.
        let path = canonicalize(&path);
        let attr = self.vfs.getattr(&path).await.map_err(reply)?;
        Ok(Attrs {
            id,
            attrs: to_attributes(&attr),
        })
    }

    #[tracing::instrument(name = "sftp.fstat", level = "debug", skip_all, fields(sftp.session = %self.session_id, sftp.user = %self.principal.username, sftp.handle = %handle))]
    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let path = match self.handles.get(&handle) {
            Some(HandleEntry::File(f)) => f.path.clone(),
            Some(HandleEntry::Dir(d)) => d.path.clone(),
            None => return Err(StatusReply::new(StatusCode::Failure).with_message("bad handle")),
        };
        let attr = self.vfs.getattr(&path).await.map_err(reply)?;
        Ok(Attrs {
            id,
            attrs: to_attributes(&attr),
        })
    }

    #[tracing::instrument(
        name = "sftp.opendir",
        level = "debug",
        skip_all,
        fields(sftp.session = %self.session_id, sftp.user = %self.principal.username, sftp.path = %path)
    )]
    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        if let Some(denied) = self.handle_capacity_denied() {
            return Err(denied);
        }
        let path = canonicalize(&path);
        // Hold only the lightweight DirEntry list; per-entry attrs are resolved
        // chunk-by-chunk in readdir, not all up front.
        let entries = self.vfs.readdir(&path).await.map_err(reply)?;
        let handle = self.alloc_handle(HandleEntry::Dir(OpenDir {
            path,
            entries,
            cursor: 0,
            dots_sent: false,
        }));
        Ok(Handle { id, handle })
    }

    #[tracing::instrument(
        name = "sftp.readdir",
        level = "debug",
        skip_all,
        fields(
            sftp.session = %self.session_id,
            sftp.user = %self.principal.username,
            sftp.handle = %handle,
            sftp.entries = tracing::field::Empty,
        )
    )]
    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        // Phase 1: under the handle borrow, carve out this chunk and advance the
        // cursor. We can't hold the &mut borrow across the getattr awaits below.
        let (dir_path, batch, include_dots) = match self.handles.get_mut(&handle) {
            Some(HandleEntry::Dir(dir)) => {
                let include_dots = !dir.dots_sent;
                dir.dots_sent = true;
                let start = dir.cursor;
                let end = (start + READDIR_CHUNK).min(dir.entries.len());
                let batch = dir.entries[start..end].to_vec();
                dir.cursor = end;
                // Nothing left to emit (and dots already sent on a prior call).
                if batch.is_empty() && !include_dots {
                    return Err(StatusReply::new(StatusCode::Eof));
                }
                (dir.path.clone(), batch, include_dots)
            }
            _ => {
                return Err(StatusReply::new(StatusCode::Failure).with_message("bad dir handle"));
            }
        };

        // Phase 2: build File entries, one getattr per entry (on demand).
        let mut files = Vec::with_capacity(batch.len() + 2);
        if include_dots {
            let dir_attrs = to_attributes(&FileAttr::directory(0o755));
            files.push(File::new(".".to_string(), dir_attrs.clone()));
            files.push(File::new("..".to_string(), dir_attrs));
        }
        for entry in &batch {
            let child = dir_path.join(&entry.name);
            let attr = match self.vfs.getattr(&child).await {
                Ok(a) => a,
                Err(e) => {
                    log::debug!("sftp readdir getattr {} failed: {e}", child.display());
                    stub_attr(entry.kind)
                }
            };
            files.push(dir_file(entry, &attr));
        }
        tracing::Span::current().record("sftp.entries", files.len());
        Ok(Name { id, files })
    }

    #[tracing::instrument(
        name = "sftp.open",
        level = "info",
        skip_all,
        fields(
            sftp.session = %self.session_id,
            sftp.user = %self.principal.username,
            sftp.path = %filename,
            sftp.write = tracing::field::Empty,
            sftp.created = tracing::field::Empty,
        )
    )]
    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        if let Some(denied) = self.handle_capacity_denied() {
            return Err(denied);
        }
        let path = canonicalize(&filename);

        // APPEND implies write access (a client may WRITE on a READ|APPEND
        // handle), so it counts as a write open alongside WRITE/CREATE/TRUNCATE.
        let wants_write =
            pflags.intersects(OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::CREATE | OpenFlags::TRUNCATE);
        tracing::Span::current().record("sftp.write", wants_write);

        if wants_write
            && let Some(denied) = privileged_write_denied(&path)
        {
            return Err(denied);
        }

        // Probe current state once. Rejecting a directory open happens *before*
        // any mutation — truncating or creating over a directory is nonsense
        // and could error or corrupt the backend mid-op.
        let existing = self.vfs.getattr(&path).await.ok();
        if let Some(attr) = &existing
            && attr.is_dir()
        {
            return Err(StatusReply::new(StatusCode::Failure).with_message("is a directory"));
        }

        let generation = if wants_write {
            match &existing {
                Some(_) => {
                    // O_EXCL: CREATE|EXCLUDE must fail if the file already exists.
                    if pflags.contains(OpenFlags::CREATE) && pflags.contains(OpenFlags::EXCLUDE) {
                        return Err(StatusReply::new(StatusCode::Failure)
                            .with_message("file already exists"));
                    }
                    if pflags.contains(OpenFlags::TRUNCATE) {
                        self.vfs.truncate(&path, 0).await.map_err(reply)?;
                    }
                    // Re-read: truncate advanced the generation.
                    self.vfs.getattr(&path).await.map_err(reply)?.generation
                }
                None => {
                    if !pflags.contains(OpenFlags::CREATE) {
                        return Err(StatusReply::new(StatusCode::NoSuchFile)
                            .with_message("no such file"));
                    }
                    let mode = perm_from_attrs(&attrs, 0o644);
                    tracing::Span::current().record("sftp.created", true);
                    self.vfs.create(&path, mode).await.map_err(reply)?.generation
                }
            }
        } else {
            // Read-only open: the file must already exist.
            match existing {
                Some(attr) => attr.generation,
                None => {
                    return Err(StatusReply::new(StatusCode::NoSuchFile)
                        .with_message("no such file"));
                }
            }
        };

        let handle = self.alloc_handle(HandleEntry::File(OpenFile {
            // A pure-read open still records read access; a write open may also
            // be readable (the client set READ alongside WRITE).
            read: pflags.contains(OpenFlags::READ) || !wants_write,
            write: wants_write,
            append: pflags.contains(OpenFlags::APPEND),
            path,
            generation,
        }));
        Ok(Handle { id, handle })
    }

    #[tracing::instrument(
        name = "sftp.write",
        level = "debug",
        skip_all,
        fields(
            sftp.session = %self.session_id,
            sftp.user = %self.principal.username,
            sftp.handle = %handle,
            sftp.offset = offset,
            sftp.bytes = data.len(),
            sftp.append = tracing::field::Empty,
        )
    )]
    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        // Pull the path + the generation we last observed through this handle,
        // without holding the handle-map borrow across the VFS awaits.
        let (path, expected, append) = match self.handles.get(&handle) {
            Some(HandleEntry::File(f)) if f.write => (f.path.clone(), f.generation, f.append),
            Some(HandleEntry::File(_)) => {
                return Err(StatusReply::new(StatusCode::PermissionDenied)
                    .with_message("handle not open for writing"));
            }
            _ => return Err(StatusReply::new(StatusCode::Failure).with_message("bad file handle")),
        };
        tracing::Span::current().record("sftp.append", append);

        // TOCTOU guard: SFTP clients expect a handle to pin the *file object*,
        // not the path string. `VfsOps` cannot pin an inode, so we re-verify the
        // content `generation` captured at open (and refreshed after each of our
        // own writes). A mismatch means the file was replaced underneath us
        // (rename-replace) — refuse rather than silently writing the wrong file.
        // One getattr serves both the guard and the append offset.
        let attr = self.vfs.getattr(&path).await.map_err(reply)?;
        if attr.generation != expected {
            return Err(StatusReply::new(StatusCode::Failure).with_message(
                "file changed underneath the open handle (possible rename-replace); write refused",
            ));
        }

        // APPEND forces the write to the current end-of-file, ignoring the
        // client-supplied offset (SSH_FXF_APPEND).
        let write_offset = if append { attr.size } else { offset };
        self.vfs
            .write(&path, write_offset, &data)
            .await
            .map_err(reply)?;

        // Our own write advanced the generation; refresh the anchor so the next
        // write on this handle compares against the right value.
        //
        // There is a narrow window between this write and the getattr below: a
        // concurrent unlink+create at the same path by another principal would
        // refresh the anchor to the replacement's generation and mask it. The
        // per-session sequential packet loop makes this exceedingly unlikely,
        // and the consequence is writing to the path the client explicitly
        // opened — acceptable in a shared-trust kernel.
        let new_gen = self.vfs.getattr(&path).await.map_err(reply)?.generation;
        if let Some(HandleEntry::File(f)) = self.handles.get_mut(&handle) {
            f.generation = new_gen;
        }
        Ok(ok_status(id))
    }

    #[tracing::instrument(name = "sftp.mkdir", level = "info", skip_all, fields(sftp.session = %self.session_id, sftp.user = %self.principal.username, sftp.path = %path))]
    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let path = canonicalize(&path);
        if let Some(denied) = privileged_write_denied(&path) {
            return Err(denied);
        }
        let mode = perm_from_attrs(&attrs, 0o755);
        self.vfs.mkdir(&path, mode).await.map_err(reply)?;
        Ok(ok_status(id))
    }

    #[tracing::instrument(name = "sftp.rmdir", level = "info", skip_all, fields(sftp.session = %self.session_id, sftp.user = %self.principal.username, sftp.path = %path))]
    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let path = canonicalize(&path);
        if let Some(denied) = privileged_write_denied(&path) {
            return Err(denied);
        }
        self.vfs.rmdir(&path).await.map_err(reply)?;
        Ok(ok_status(id))
    }

    #[tracing::instrument(name = "sftp.remove", level = "info", skip_all, fields(sftp.session = %self.session_id, sftp.user = %self.principal.username, sftp.path = %filename))]
    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let path = canonicalize(&filename);
        if let Some(denied) = privileged_write_denied(&path) {
            return Err(denied);
        }
        self.vfs.unlink(&path).await.map_err(reply)?;
        Ok(ok_status(id))
    }

    #[tracing::instrument(name = "sftp.rename", level = "info", skip_all, fields(sftp.session = %self.session_id, sftp.user = %self.principal.username, sftp.from = %oldpath, sftp.to = %newpath))]
    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let from = canonicalize(&oldpath);
        let to = canonicalize(&newpath);
        // Either endpoint touching a privileged tree is a privileged write.
        if let Some(denied) = privileged_write_denied(&from).or_else(|| privileged_write_denied(&to))
        {
            return Err(denied);
        }
        // Base SFTPv3 RENAME must FAIL if the destination exists — overwrite is
        // the posix-rename@openssh.com extension's job (handled in `extended`).
        // Don't rely on backend rename semantics for this.
        if self.vfs.exists(&to).await {
            return Err(StatusReply::new(StatusCode::Failure).with_message("destination exists"));
        }
        self.vfs.rename(&from, &to).await.map_err(reply)?;
        Ok(ok_status(id))
    }

    #[tracing::instrument(name = "sftp.setstat", level = "info", skip_all, fields(sftp.session = %self.session_id, sftp.user = %self.principal.username, sftp.path = %path))]
    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let path = canonicalize(&path);
        if let Some(denied) = privileged_write_denied(&path) {
            return Err(denied);
        }
        self.vfs
            .setattr(&path, set_attr_from(&attrs))
            .await
            .map_err(reply)?;
        Ok(ok_status(id))
    }

    #[tracing::instrument(name = "sftp.fsetstat", level = "info", skip_all, fields(sftp.session = %self.session_id, sftp.user = %self.principal.username, sftp.handle = %handle))]
    async fn fsetstat(
        &mut self,
        id: u32,
        handle: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        // fsetstat mutates; it must honor the same write-open gate and TOCTOU
        // guard as `write` (a setattr(size=…) truncates). A read-only handle
        // must not be a back door to truncation. Directory handles carry no
        // generation, so they skip the guard (a dir setstat is metadata-only).
        let (path, expected) = match self.handles.get(&handle) {
            Some(HandleEntry::File(f)) if f.write => (f.path.clone(), Some(f.generation)),
            Some(HandleEntry::File(_)) => {
                return Err(StatusReply::new(StatusCode::PermissionDenied)
                    .with_message("handle not open for writing"));
            }
            Some(HandleEntry::Dir(d)) => (d.path.clone(), None),
            None => return Err(StatusReply::new(StatusCode::Failure).with_message("bad handle")),
        };
        if let Some(denied) = privileged_write_denied(&path) {
            return Err(denied);
        }

        if let Some(expected) = expected {
            let current = self.vfs.getattr(&path).await.map_err(reply)?.generation;
            if current != expected {
                return Err(StatusReply::new(StatusCode::Failure).with_message(
                    "file changed underneath the open handle (possible rename-replace); setstat refused",
                ));
            }
        }

        let size_changed = attrs.size.is_some();
        self.vfs
            .setattr(&path, set_attr_from(&attrs))
            .await
            .map_err(reply)?;

        // A size change is a content mutation that advances generation; refresh
        // the anchor. A pure mtime/perm setstat does not bump it, so leave the
        // anchor untouched there (re-reading would be a needless getattr).
        if size_changed && expected.is_some() {
            let new_gen = self.vfs.getattr(&path).await.map_err(reply)?.generation;
            if let Some(HandleEntry::File(f)) = self.handles.get_mut(&handle) {
                f.generation = new_gen;
            }
        }
        Ok(ok_status(id))
    }

    #[tracing::instrument(name = "sftp.symlink", level = "info", skip_all, fields(sftp.session = %self.session_id, sftp.user = %self.principal.username, sftp.link = %linkpath, sftp.target = %targetpath))]
    async fn symlink(
        &mut self,
        id: u32,
        linkpath: String,
        targetpath: String,
    ) -> Result<Status, Self::Error> {
        let link = canonicalize(&linkpath);
        if let Some(denied) = privileged_write_denied(&link) {
            return Err(denied);
        }
        // The target is stored verbatim (it may be relative); only the link
        // location is canonicalized.
        self.vfs
            .symlink(&link, Path::new(&targetpath))
            .await
            .map_err(reply)?;
        Ok(ok_status(id))
    }

    #[tracing::instrument(name = "sftp.readlink", level = "debug", skip_all, fields(sftp.session = %self.session_id, sftp.user = %self.principal.username, sftp.path = %path))]
    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let path = canonicalize(&path);
        let target = self.vfs.readlink(&path).await.map_err(reply)?;
        Ok(Name {
            id,
            files: vec![File::dummy(target.to_string_lossy().into_owned())],
        })
    }

    #[tracing::instrument(
        name = "sftp.read",
        level = "debug",
        skip_all,
        fields(
            sftp.session = %self.session_id,
            sftp.user = %self.principal.username,
            sftp.handle = %handle,
            sftp.offset = offset,
            sftp.req_len = len,
            sftp.bytes = tracing::field::Empty,
            sftp.eof = tracing::field::Empty,
        )
    )]
    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let path = match self.handles.get(&handle) {
            Some(HandleEntry::File(f)) if f.read => f.path.clone(),
            Some(HandleEntry::File(_)) => {
                return Err(StatusReply::new(StatusCode::PermissionDenied)
                    .with_message("handle not open for reading"));
            }
            _ => return Err(StatusReply::new(StatusCode::Failure).with_message("bad file handle")),
        };
        // Cap the request so a client can't make us allocate an unbounded buffer.
        let len = len.min(MAX_READ_LEN);
        let data = self.vfs.read(&path, offset, len).await.map_err(reply)?;
        if data.is_empty() {
            // No bytes at this offset == end of file. The explicit Eof is what
            // tells the client to stop reading; omitting it hangs the transfer.
            tracing::Span::current().record("sftp.eof", true);
            return Err(StatusReply::new(StatusCode::Eof));
        }
        tracing::Span::current().record("sftp.bytes", data.len());
        Ok(Data { id, data })
    }

    #[tracing::instrument(name = "sftp.close", level = "debug", skip_all, fields(sftp.session = %self.session_id, sftp.user = %self.principal.username, sftp.handle = %handle))]
    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        if self.handles.remove(&handle).is_none() {
            return Err(StatusReply::new(StatusCode::Failure).with_message("bad handle"));
        }
        Ok(ok_status(id))
    }

    #[tracing::instrument(name = "sftp.extended", level = "info", skip_all, fields(sftp.session = %self.session_id, sftp.user = %self.principal.username, sftp.request = %request))]
    async fn extended(
        &mut self,
        id: u32,
        request: String,
        data: Vec<u8>,
    ) -> Result<Packet, Self::Error> {
        match request.as_str() {
            // Overwrite-on-exists rename. Plain SFTPv3 RENAME fails if the
            // target exists; sshfs needs this for atomic replace.
            POSIX_RENAME => {
                let ext: PosixRenameExtension = decode_ext(data)?;
                let from = canonicalize(&ext.oldpath);
                let to = canonicalize(&ext.newpath);
                if let Some(denied) =
                    privileged_write_denied(&from).or_else(|| privileged_write_denied(&to))
                {
                    return Err(denied);
                }
                // Best-effort overwrite: remove an existing destination first,
                // then rename. (A future slice can make this atomic in the VFS.)
                if self.vfs.exists(&to).await {
                    self.vfs.unlink(&to).await.map_err(reply)?;
                }
                self.vfs.rename(&from, &to).await.map_err(reply)?;
                Ok(Packet::Status(ok_status(id)))
            }
            extensions::HARDLINK => {
                let ext: HardlinkExtension = decode_ext(data)?;
                let old = canonicalize(&ext.oldpath);
                let new = canonicalize(&ext.newpath);
                if let Some(denied) = privileged_write_denied(&new) {
                    return Err(denied);
                }
                self.vfs.link(&old, &new).await.map_err(reply)?;
                Ok(Packet::Status(ok_status(id)))
            }
            // Every vfs.write is synchronous write-through, so there is nothing
            // buffered to flush — fsync is a success no-op. We still validate the
            // handle so a bad handle fails loud.
            extensions::FSYNC => {
                let ext: FsyncExtension = decode_ext(data)?;
                if !self.handles.contains_key(&ext.handle) {
                    return Err(StatusReply::new(StatusCode::Failure).with_message("bad handle"));
                }
                Ok(Packet::Status(ok_status(id)))
            }
            extensions::STATVFS => {
                let ext: StatvfsExtension = decode_ext(data)?;
                let path = canonicalize(&ext.path);
                // Path must resolve, but statfs itself is filesystem-wide.
                self.vfs.getattr(&path).await.map_err(reply)?;
                let stat = self.vfs.statfs().await.map_err(reply)?;
                let bytes = russh_sftp::ser::to_bytes(&to_statvfs(&stat, self.vfs.read_only()))
                    .map_err(|e| {
                        StatusReply::new(StatusCode::Failure)
                            .with_message(format!("statvfs encode: {e}"))
                    })?;
                Ok(Packet::ExtendedReply(ExtendedReply {
                    id,
                    data: bytes.to_vec(),
                }))
            }
            other => Err(StatusReply::new(StatusCode::OpUnsupported)
                .with_message(format!("unsupported extension: {other}"))),
        }
    }
}

/// Deserialize an extension's SSH-wire payload, failing loud as a `BadMessage`
/// rather than a generic failure (a malformed extension is a protocol error).
fn decode_ext<T: serde::de::DeserializeOwned>(data: Vec<u8>) -> Result<T, StatusReply> {
    russh_sftp::de::from_bytes(&mut Bytes::from(data)).map_err(|e| {
        StatusReply::new(StatusCode::BadMessage).with_message(format!("bad extension payload: {e}"))
    })
}

/// Map kernel [`StatFs`] onto the `statvfs@openssh.com` reply struct.
fn to_statvfs(stat: &StatFs, read_only: bool) -> Statvfs {
    const ST_RDONLY: u64 = 0x1;
    Statvfs {
        block_size: stat.bsize as u64,
        fragment_size: stat.frsize as u64,
        blocks: stat.blocks,
        blocks_free: stat.bfree,
        blocks_avail: stat.bavail,
        inodes: stat.files,
        inodes_free: stat.ffree,
        inodes_avail: stat.ffree,
        fs_id: 0,
        flags: if read_only { ST_RDONLY } else { 0 },
        name_max: stat.namelen as u64,
    }
}

/// A typed-but-empty attribute stub for a directory entry whose `getattr`
/// failed — keeps the entry visible with the right type column.
fn stub_attr(kind: FileType) -> FileAttr {
    match kind {
        FileType::Directory => FileAttr::directory(0o755),
        FileType::Symlink => FileAttr::symlink(0),
        FileType::File => FileAttr::file(0, 0o644),
    }
}

/// A success `SSH_FXP_STATUS` for ops that reply with a bare status.
fn ok_status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_string(),
        language_tag: "en-US".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_kernel::MemoryBackend;
    use russh_sftp::protocol::FileAttributes;

    #[tokio::test]
    async fn write_guard_detects_rename_replace() {
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/", MemoryBackend::new()).await;
        vfs.write_all(Path::new("/f.txt"), b"orig").await.unwrap();

        let mut s = SftpSession::new(Principal::system(), vfs.clone());
        // Open for write (no truncate) — captures the current generation.
        let h = Handler::open(&mut s, 1, "/f.txt".into(), OpenFlags::WRITE, FileAttributes::empty())
            .await
            .expect("open for write");

        // External replace behind the handle's back: remove + recreate, which
        // advances the per-file generation.
        vfs.unlink(Path::new("/f.txt")).await.unwrap();
        vfs.write_all(Path::new("/f.txt"), b"replacement")
            .await
            .unwrap();

        let err = Handler::write(&mut s, 2, h.handle.clone(), 0, b"X".to_vec())
            .await
            .expect_err("write must be refused after replace");
        assert_eq!(err.status_code, StatusCode::Failure);
        // The replacement file is untouched — no corruption.
        assert_eq!(
            vfs.read_all(Path::new("/f.txt")).await.unwrap(),
            b"replacement"
        );
    }

    #[tokio::test]
    async fn sequential_writes_through_one_handle_succeed() {
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/", MemoryBackend::new()).await;

        let mut s = SftpSession::new(Principal::system(), vfs.clone());
        let h = Handler::open(
            &mut s,
            1,
            "/g.txt".into(),
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
            FileAttributes::empty(),
        )
        .await
        .expect("open create");

        // Two writes in sequence: the second must not trip the TOCTOU guard on
        // our *own* first write — the anchor refreshes after each write.
        Handler::write(&mut s, 2, h.handle.clone(), 0, b"abc".to_vec())
            .await
            .expect("first write");
        Handler::write(&mut s, 3, h.handle.clone(), 3, b"def".to_vec())
            .await
            .expect("second write");
        Handler::close(&mut s, 4, h.handle).await.expect("close");

        assert_eq!(vfs.read_all(Path::new("/g.txt")).await.unwrap(), b"abcdef");
    }

    #[tokio::test]
    async fn append_writes_at_end_ignoring_offset() {
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/", MemoryBackend::new()).await;
        vfs.write_all(Path::new("/log.txt"), b"start").await.unwrap();

        let mut s = SftpSession::new(Principal::system(), vfs.clone());
        let h = Handler::open(
            &mut s,
            1,
            "/log.txt".into(),
            OpenFlags::WRITE | OpenFlags::APPEND,
            FileAttributes::empty(),
        )
        .await
        .expect("open append");

        // The client sends offset 0, but APPEND forces the write to EOF.
        Handler::write(&mut s, 2, h.handle, 0, b"-more".to_vec())
            .await
            .expect("append write");
        assert_eq!(
            vfs.read_all(Path::new("/log.txt")).await.unwrap(),
            b"start-more"
        );
    }

    #[tokio::test]
    async fn fsetstat_on_read_handle_is_refused() {
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/", MemoryBackend::new()).await;
        vfs.write_all(Path::new("/ro.txt"), b"keepme").await.unwrap();

        let mut s = SftpSession::new(Principal::system(), vfs.clone());
        let h = Handler::open(&mut s, 1, "/ro.txt".into(), OpenFlags::READ, FileAttributes::empty())
            .await
            .expect("open read-only");

        // A size=0 fsetstat would truncate — it must be refused on a read handle.
        let mut attrs = FileAttributes::empty();
        attrs.size = Some(0);
        let err = Handler::fsetstat(&mut s, 2, h.handle, attrs)
            .await
            .expect_err("read-only handle must not truncate");
        assert_eq!(err.status_code, StatusCode::PermissionDenied);
        assert_eq!(vfs.read_all(Path::new("/ro.txt")).await.unwrap(), b"keepme");
    }

    #[tokio::test]
    async fn posix_rename_overwrites_existing_target() {
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/", MemoryBackend::new()).await;
        vfs.write_all(Path::new("/a.txt"), b"aaa").await.unwrap();
        vfs.write_all(Path::new("/b.txt"), b"bbb").await.unwrap();

        let mut s = SftpSession::new(Principal::system(), vfs.clone());
        // posix-rename payload has the same two-SSH-string wire shape as
        // HardlinkExtension, so reuse its serializer to build the bytes.
        let payload: Vec<u8> = HardlinkExtension {
            oldpath: "/a.txt".into(),
            newpath: "/b.txt".into(),
        }
        .try_into()
        .unwrap();

        let packet = Handler::extended(&mut s, 1, POSIX_RENAME.into(), payload)
            .await
            .expect("posix-rename over existing target");
        assert!(matches!(packet, Packet::Status(_)));

        // /b.txt now holds /a.txt's content (plain RENAME would have failed on
        // the existing target); /a.txt is gone.
        assert_eq!(vfs.read_all(Path::new("/b.txt")).await.unwrap(), b"aaa");
        assert!(!vfs.exists(Path::new("/a.txt")).await);
    }

    #[tokio::test]
    async fn readdir_injects_dot_and_dotdot_then_eofs() {
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/", MemoryBackend::new()).await;
        vfs.write_all(Path::new("/only.txt"), b"x").await.unwrap();

        let mut s = SftpSession::new(Principal::system(), vfs);
        let h = Handler::opendir(&mut s, 1, "/".into()).await.expect("opendir");

        let first = Handler::readdir(&mut s, 2, h.handle.clone())
            .await
            .expect("first chunk");
        let names: Vec<&str> = first.files.iter().map(|f| f.filename.as_str()).collect();
        assert!(names.contains(&"."), "first chunk carries .");
        assert!(names.contains(&".."), "first chunk carries ..");
        assert!(names.contains(&"only.txt"));

        // Everything fit in one chunk; the next call signals Eof.
        let err = Handler::readdir(&mut s, 3, h.handle)
            .await
            .expect_err("second call is Eof");
        assert_eq!(err.status_code, StatusCode::Eof);
    }

    #[tokio::test]
    async fn open_is_refused_past_the_handle_cap() {
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/", MemoryBackend::new()).await;
        vfs.write_all(Path::new("/f.txt"), b"x").await.unwrap();

        let mut s = SftpSession::new(Principal::system(), vfs);
        for i in 0..MAX_OPEN_HANDLES {
            Handler::open(&mut s, i as u32, "/f.txt".into(), OpenFlags::READ, FileAttributes::empty())
                .await
                .expect("open under the cap");
        }
        let err = Handler::open(&mut s, 9999, "/f.txt".into(), OpenFlags::READ, FileAttributes::empty())
            .await
            .expect_err("open past the cap must be refused");
        assert_eq!(err.status_code, StatusCode::Failure);
    }

    #[tokio::test]
    async fn stat_follows_symlink_but_lstat_does_not() {
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/", MemoryBackend::new()).await;
        vfs.write_all(Path::new("/target.txt"), b"hi").await.unwrap();
        vfs.symlink(Path::new("/link"), Path::new("/target.txt"))
            .await
            .unwrap();

        let mut s = SftpSession::new(Principal::system(), vfs);
        // LSTAT reports the link itself.
        let l = Handler::lstat(&mut s, 1, "/link".into()).await.unwrap();
        assert!(l.attrs.is_symlink());
        // STAT follows to the regular-file target.
        let st = Handler::stat(&mut s, 2, "/link".into()).await.unwrap();
        assert!(st.attrs.is_regular());
        assert!(!st.attrs.is_symlink());
    }

    #[tokio::test]
    async fn base_rename_refuses_existing_destination() {
        // SFTPv3 RENAME must fail on an existing target; overwrite is
        // posix-rename's job.
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/", MemoryBackend::new()).await;
        vfs.write_all(Path::new("/a.txt"), b"a").await.unwrap();
        vfs.write_all(Path::new("/b.txt"), b"b").await.unwrap();

        let mut s = SftpSession::new(Principal::system(), vfs.clone());
        let err = Handler::rename(&mut s, 1, "/a.txt".into(), "/b.txt".into())
            .await
            .expect_err("base rename must refuse an existing destination");
        assert_eq!(err.status_code, StatusCode::Failure);
        assert!(vfs.exists(Path::new("/a.txt")).await);
        assert_eq!(vfs.read_all(Path::new("/b.txt")).await.unwrap(), b"b");
    }

    #[tokio::test]
    async fn read_on_write_only_handle_is_refused() {
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/", MemoryBackend::new()).await;
        let mut s = SftpSession::new(Principal::system(), vfs);
        // WRITE|CREATE without READ.
        let h = Handler::open(
            &mut s,
            1,
            "/w.txt".into(),
            OpenFlags::WRITE | OpenFlags::CREATE,
            FileAttributes::empty(),
        )
        .await
        .unwrap();
        let err = Handler::read(&mut s, 2, h.handle, 0, 16)
            .await
            .expect_err("write-only handle must refuse read");
        assert_eq!(err.status_code, StatusCode::PermissionDenied);
    }

    #[tokio::test]
    async fn unknown_extension_is_op_unsupported() {
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/", MemoryBackend::new()).await;
        let mut s = SftpSession::new(Principal::system(), vfs);
        let err = Handler::extended(&mut s, 1, "made-up@example.com".into(), vec![])
            .await
            .expect_err("unknown extension");
        assert_eq!(err.status_code, StatusCode::OpUnsupported);
    }

    #[tokio::test]
    async fn hardlink_surfaces_backend_unsupported_cleanly() {
        // MemoryBackend has no hard links; the extension must surface that as a
        // clean status, not a panic.
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/", MemoryBackend::new()).await;
        vfs.write_all(Path::new("/src.txt"), b"x").await.unwrap();
        let mut s = SftpSession::new(Principal::system(), vfs);
        let payload: Vec<u8> = HardlinkExtension {
            oldpath: "/src.txt".into(),
            newpath: "/link.txt".into(),
        }
        .try_into()
        .unwrap();
        let err = Handler::extended(&mut s, 1, extensions::HARDLINK.into(), payload)
            .await
            .expect_err("memory backend has no hard links");
        assert_eq!(err.status_code, StatusCode::Failure);
    }

    #[test]
    fn privileged_deny_matches_on_component_boundary() {
        assert!(privileged_write_denied(Path::new(paths::RC_ROOT)).is_some());
        assert!(privileged_write_denied(Path::new("/etc/rc/coder/S10.kai")).is_some());
        assert!(privileged_write_denied(Path::new(paths::CONFIG_ROOT)).is_some());
        assert!(privileged_write_denied(Path::new("/etc/config/models")).is_some());
        // Not gated: a sibling whose name merely starts with the gated prefix.
        assert!(privileged_write_denied(Path::new("/etc/rcfoo")).is_none());
        assert!(privileged_write_denied(Path::new("/etc/configuration")).is_none());
        assert!(privileged_write_denied(Path::new("/tmp/x")).is_none());
    }

    #[test]
    fn canonicalize_roots_relative_paths() {
        assert_eq!(canonicalize("."), PathBuf::from("/"));
        assert_eq!(canonicalize(""), PathBuf::from("/"));
        assert_eq!(canonicalize("/"), PathBuf::from("/"));
        assert_eq!(canonicalize("etc/rc"), PathBuf::from("/etc/rc"));
        assert_eq!(canonicalize("/etc/rc"), PathBuf::from("/etc/rc"));
    }

    #[test]
    fn canonicalize_resolves_dots() {
        assert_eq!(canonicalize("/etc/./rc"), PathBuf::from("/etc/rc"));
        assert_eq!(canonicalize("/etc/rc/../config"), PathBuf::from("/etc/config"));
        assert_eq!(canonicalize("/a/b/c/../../d"), PathBuf::from("/a/d"));
    }

    #[test]
    fn canonicalize_clamps_parent_at_root() {
        // The traversal guard: `..` can never climb above `/`.
        assert_eq!(canonicalize("/.."), PathBuf::from("/"));
        assert_eq!(canonicalize("../../etc/passwd"), PathBuf::from("/etc/passwd"));
        assert_eq!(canonicalize("/../../../etc"), PathBuf::from("/etc"));
    }

    #[test]
    fn status_mapping_covers_the_common_errors() {
        assert_eq!(
            status_for(&VfsError::NotFound("x".into())),
            StatusCode::NoSuchFile
        );
        assert_eq!(
            status_for(&VfsError::NoMountPoint("x".into())),
            StatusCode::NoSuchFile
        );
        assert_eq!(status_for(&VfsError::ReadOnly), StatusCode::PermissionDenied);
        assert_eq!(
            status_for(&VfsError::PermissionDenied("x".into())),
            StatusCode::PermissionDenied
        );
        assert_eq!(
            status_for(&VfsError::PathEscapesRoot("x".into())),
            StatusCode::PermissionDenied
        );
        assert_eq!(
            status_for(&VfsError::IsADirectory("x".into())),
            StatusCode::Failure
        );
    }

    #[test]
    fn attributes_encode_file_type_into_mode() {
        let dir = to_attributes(&FileAttr::directory(0o755));
        assert!(dir.is_dir());
        assert!(!dir.is_regular());

        let file = to_attributes(&FileAttr::file(123, 0o644));
        assert!(file.is_regular());
        assert!(!file.is_dir());
        assert_eq!(file.size, Some(123));

        let link = to_attributes(&FileAttr::symlink(7));
        assert!(link.is_symlink());
    }
}
