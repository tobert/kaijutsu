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
use std::time::{SystemTime, UNIX_EPOCH};

use kaijutsu_kernel::{DirEntry, FileAttr, FileType, MountTable, VfsError, VfsOps};
use kaijutsu_types::Principal;

use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
};
use russh_sftp::server::{Handler, StatusReply};

/// An open file handle: the resolved path plus the access flags. The
/// `generation` captured at open is the TOCTOU re-verify anchor used by the
/// write path (later slice).
struct OpenFile {
    path: PathBuf,
    #[allow(dead_code)] // consumed by the write slice
    read: bool,
    #[allow(dead_code)] // consumed by the write slice
    write: bool,
    #[allow(dead_code)] // consumed by the write slice
    generation: u64,
}

/// An open directory handle. `readdir` drains `entries` in one shot, then
/// signals `Eof` on the next call (the SFTP `READDIR` loop convention).
struct OpenDir {
    #[allow(dead_code)] // retained for diagnostics / future paging
    path: PathBuf,
    files: Vec<File>,
    sent: bool,
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
    vfs: Arc<MountTable>,
    handles: HashMap<String, HandleEntry>,
    next_handle: u64,
}

impl SftpSession {
    pub fn new(principal: Principal, vfs: Arc<MountTable>) -> Self {
        Self {
            principal,
            vfs,
            handles: HashMap::new(),
            next_handle: 0,
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
}

/// Lexically canonicalize a client-supplied path into an absolute path rooted
/// at `/`, the root of the global VFS tree.
///
/// This is the directory-traversal guard: it resolves `.` and `..` *without*
/// string surgery that could escape the mount boundary. `..` past the root is
/// clamped (a pop on an empty stack is a no-op), and a relative path is treated
/// as relative to `/`. `realpath(".")` therefore resolves to `/`.
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

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let path = canonicalize(&path);
        let attr = self.vfs.getattr(&path).await.map_err(reply)?;
        Ok(Attrs {
            id,
            attrs: to_attributes(&attr),
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        // VfsOps::getattr is lstat-shaped (it does not follow the final
        // symlink for type reporting); reuse it for both.
        self.stat(id, path).await
    }

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

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let path = canonicalize(&path);
        let entries = self.vfs.readdir(&path).await.map_err(reply)?;

        // Resolve each entry's attributes for a useful longname. A getattr that
        // fails (race, dangling symlink) degrades to a typed stub rather than
        // dropping the entry.
        let mut files = Vec::with_capacity(entries.len());
        for entry in &entries {
            let child = path.join(&entry.name);
            let attr = match self.vfs.getattr(&child).await {
                Ok(a) => a,
                Err(_) => stub_attr(entry.kind),
            };
            files.push(dir_file(entry, &attr));
        }

        let handle = self.alloc_handle(HandleEntry::Dir(OpenDir {
            path,
            files,
            sent: false,
        }));
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        match self.handles.get_mut(&handle) {
            Some(HandleEntry::Dir(dir)) => {
                if dir.sent {
                    return Err(StatusReply::new(StatusCode::Eof));
                }
                dir.sent = true;
                Ok(Name {
                    id,
                    files: std::mem::take(&mut dir.files),
                })
            }
            _ => Err(StatusReply::new(StatusCode::Failure).with_message("bad dir handle")),
        }
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let path = canonicalize(&filename);

        // Write path lands in the next slice; for now a write/create/truncate
        // open fails loud rather than silently dropping data.
        if pflags.intersects(OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE) {
            return Err(StatusReply::new(StatusCode::OpUnsupported)
                .with_message("sftp write not yet implemented"));
        }

        let attr = self.vfs.getattr(&path).await.map_err(reply)?;
        if attr.is_dir() {
            return Err(StatusReply::new(StatusCode::Failure).with_message("is a directory"));
        }

        let handle = self.alloc_handle(HandleEntry::File(OpenFile {
            path,
            read: true,
            write: false,
            generation: attr.generation,
        }));
        Ok(Handle { id, handle })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let path = match self.handles.get(&handle) {
            Some(HandleEntry::File(f)) => f.path.clone(),
            _ => return Err(StatusReply::new(StatusCode::Failure).with_message("bad file handle")),
        };
        let data = self.vfs.read(&path, offset, len).await.map_err(reply)?;
        if data.is_empty() {
            // No bytes at this offset == end of file. The explicit Eof is what
            // tells the client to stop reading; omitting it hangs the transfer.
            return Err(StatusReply::new(StatusCode::Eof));
        }
        Ok(Data { id, data })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        if self.handles.remove(&handle).is_none() {
            return Err(StatusReply::new(StatusCode::Failure).with_message("bad handle"));
        }
        Ok(ok_status(id))
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
