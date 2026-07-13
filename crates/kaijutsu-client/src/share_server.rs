//! Client-side share server — the reverse-SFTP half of `/r` client shares
//! (`docs/slash-r.md`, slice 1).
//!
//! A `russh_sftp::server::Handler` over real host files, serving N
//! caller-configured directories as top-level shares plus a synthesized
//! `/index` manifest. This is the easy half of the reverse-SFTP design: real
//! files, no VFS underneath — the one part that must be right is the jail,
//! because the machine running this is **outside** the kernel's shared-trust
//! unix boundary (`docs/slash-r.md` "Client side: a jailed, trivial file
//! server").
//!
//! Read-only in this slice: every mutating op refuses with `PermissionDenied`
//! regardless of a share's advertised `rw` flag (`docs/slash-r.md` slice 1
//! scope; slice 3 is writable shares).

use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncSeekExt};

use kaijutsu_types::share::{
    encode_manifest, GenerationReply, GenerationRequest, ShareManifestRow, GENERATION_EXTENSION,
    GENERATION_EXTENSION_VERSION,
};

use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Packet, Status, StatusCode,
    Version,
};
use russh_sftp::server::{Handler, StatusReply};

/// Cap on a single `READ`'s byte count — mirrors the forward adapter's
/// `MAX_READ_LEN` (`kaijutsu-server/src/sftp.rs`) and the SFTP `READ` window
/// both directions already use.
const MAX_READ_LEN: u32 = 256 * 1024;

/// Directory entries returned per `READDIR` reply — mirrors the forward
/// adapter's `READDIR_CHUNK`, bounding the `Name` packet size for a large
/// directory.
const READDIR_CHUNK: usize = 64;

/// Generation stamped on the synthesized `/index` manifest file. The bytes
/// are computed once at construction and never change during a session's
/// lifetime, so (like `CasFs::IMMUTABLE_GENERATION`) a constant is honest: a
/// caching reader never needs to invalidate it.
const INDEX_GENERATION: u64 = 1;

/// The virtual manifest file's name at the session root.
const INDEX_NAME: &str = "index";

/// One directory this client is offering, canonicalized once at construction.
#[derive(Debug, Clone)]
struct ShareRoot {
    name: String,
    /// Canonicalized at construction (symlinks resolved) — "the root is what
    /// it resolves to" (`docs/slash-r.md`).
    root: PathBuf,
    // `rw` is intentionally NOT carried here: the only place it matters this
    // slice is the manifest (`ShareServerConfig::manifest`, built once from
    // the same `ShareArg`s below) — every mutating Handler method refuses
    // unconditionally regardless of the flag, so this struct has no use for
    // it yet.
}

/// Immutable per-session configuration: the shares offered, the client's
/// claimed identity, and the precomputed manifest bytes. Cheap to clone
/// (`Arc`-wrapped internally by callers) — one `ShareServerConfig` seeds every
/// `ShareHandler` this process serves (one per SSH share subsystem dial).
#[derive(Debug, Clone)]
pub struct ShareServerConfig {
    shares: Vec<ShareRoot>,
    manifest: Arc<Vec<u8>>,
}

impl ShareServerConfig {
    /// Build the config from validated share args, canonicalizing each root
    /// once. Fails loud if a root doesn't exist or isn't a directory — a
    /// share that can't be jailed must never be offered.
    pub fn new(
        shares: &[ShareArg],
        client_id: impl Into<String>,
        nick: impl Into<String>,
    ) -> io::Result<Self> {
        let client_id = client_id.into();
        let nick = nick.into();
        let mut roots = Vec::with_capacity(shares.len());
        let mut rows = Vec::with_capacity(shares.len());
        for s in shares {
            let canonical = s.path.canonicalize().map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("--share {}: {} ({e})", s.name, s.path.display()),
                )
            })?;
            let meta = std::fs::metadata(&canonical)?;
            if !meta.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("--share {}: {} is not a directory", s.name, canonical.display()),
                ));
            }
            roots.push(ShareRoot {
                name: s.name.clone(),
                root: canonical,
            });
            rows.push(ShareManifestRow {
                name: s.name.clone(),
                rw: s.rw,
                client_id: client_id.clone(),
                nick: nick.clone(),
            });
        }
        let manifest = encode_manifest(&rows);
        Ok(Self {
            shares: roots,
            manifest: Arc::new(manifest),
        })
    }

    fn find(&self, name: &str) -> Option<&ShareRoot> {
        self.shares.iter().find(|s| s.name == name)
    }
}

/// One `--share [name=]path[:rw]` CLI occurrence, already split into its
/// parts. See [`parse_share_arg`] for the grammar and [`validate_unique_names`]
/// for the cross-arg check clap's per-value parser can't express.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareArg {
    pub name: String,
    pub path: PathBuf,
    pub rw: bool,
}

/// Parse one `--share` occurrence: `[name=]path[:rw]`.
///
/// - A trailing literal `:rw` (exact match) opts the share into read-write
///   in the manifest; anything else stays read-only. Chosen over a smarter
///   split because POSIX paths may contain `:`; a hand-authored share
///   argument choosing to end its path in literal `:rw` is not supportable
///   without an escape hatch this slice doesn't need.
/// - `name=` (split on the FIRST `=`) is an explicit share name; omitted, the
///   name defaults to the path's basename.
///
/// Pure and unit-testable: does not touch the filesystem (canonicalization
/// and existence are [`ShareServerConfig::new`]'s job, which needs an error
/// path anyway and shouldn't run twice).
pub fn parse_share_arg(raw: &str) -> Result<ShareArg, String> {
    let (body, rw) = match raw.strip_suffix(":rw") {
        Some(rest) => (rest, true),
        None => (raw, false),
    };
    let (name, path_str) = match body.split_once('=') {
        Some((name, path)) if !name.is_empty() => (Some(name.to_string()), path),
        _ => (None, body),
    };
    if path_str.is_empty() {
        return Err(format!("--share {raw:?}: empty path"));
    }
    let path = PathBuf::from(path_str);
    let name = name.unwrap_or_else(|| {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path_str.to_string())
    });
    if name.is_empty() {
        return Err(format!("--share {raw:?}: could not derive a share name from the path"));
    }
    Ok(ShareArg { name, path, rw })
}

/// Cross-arg validation `parse_share_arg` can't express on its own:
/// `--share a/x --share b/x` both default to name `x` — reject rather than
/// silently letting the second shadow the first (`docs/slash-r.md` "Open
/// questions" — the lean answer is to make the collision a parse error).
pub fn validate_unique_names(shares: &[ShareArg]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for s in shares {
        if !seen.insert(s.name.as_str()) {
            return Err(format!(
                "--share name {:?} is used more than once; give one of them an explicit name=",
                s.name
            ));
        }
    }
    Ok(())
}

/// Lexically clean a client-visible path into `/`-rooted `Normal` components,
/// exactly like the forward adapter's `canonicalize` (`kaijutsu-server/src/sftp.rs`)
/// — `.`/`..` resolved without string surgery, `..` clamped at the root.
fn clean_components(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in Path::new(raw).components() {
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

/// What a client-visible path names at the session root.
enum Route {
    /// `/` itself.
    Root,
    /// `/index` — the manifest.
    Index,
    /// `/<share>` — a share's own root directory.
    ShareRoot(ShareRoot),
    /// `/<share>/<rest>` — somewhere inside a share.
    InShare(ShareRoot, PathBuf),
}

fn route(config: &ShareServerConfig, raw: &str) -> Result<Route, StatusReply> {
    let comps = clean_components(raw);
    match comps.as_slice() {
        [] => Ok(Route::Root),
        [name] if name == INDEX_NAME => Ok(Route::Index),
        [name] => config
            .find(name)
            .cloned()
            .map(Route::ShareRoot)
            .ok_or_else(|| StatusReply::new(StatusCode::NoSuchFile)),
        [name, rest @ ..] => config
            .find(name)
            .cloned()
            .map(|s| Route::InShare(s, rest.iter().collect()))
            .ok_or_else(|| StatusReply::new(StatusCode::NoSuchFile)),
    }
}

/// Map an `io::Error` to the closest SFTP status, same discipline as the
/// forward adapter's `status_for` (`kaijutsu-server/src/sftp.rs`).
fn io_status(e: &io::Error) -> StatusReply {
    let code = match e.kind() {
        io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    };
    code.with_message(e.to_string())
}

/// Resolve `root/rel`, **following** the full chain (the "open"/"descend"
/// case, `docs/slash-r.md`): canonicalize the whole candidate and verify it
/// stays under `root`. Catches escapes at ANY component, not just the leaf —
/// an intermediate symlink pointing outside the jail is refused here just as
/// much as a leaf one. Used by `open` (content), `opendir`, and `stat`
/// (follow semantics). The canonicalize→open gap is the documented residual
/// TOCTOU (`docs/slash-r.md`: "a local process swapping a symlink
/// mid-serve... outside this design's threat model") for every caller except
/// [`open_file_beneath`], which closes it atomically via `openat2` on Linux.
fn resolve_follow(root: &Path, rel: &Path) -> io::Result<PathBuf> {
    let candidate = root.join(rel);
    let real = std::fs::canonicalize(&candidate)?;
    if !real.starts_with(root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} escapes the share root", rel.display()),
        ));
    }
    Ok(real)
}

/// Resolve `root/rel` for an **lstat** (the "report, don't descend" case):
/// canonicalize only the PARENT (validating every intermediate component),
/// then rejoin the leaf's own name unresolved — so a symlink at the leaf
/// itself is reported AS a symlink (`docs/slash-r.md`: "A link pointing out
/// of the jail lists as a symlink but refuses to open"), while a symlink
/// among the intermediate directories still can't smuggle a listing out.
fn resolve_lstat(root: &Path, rel: &Path) -> io::Result<PathBuf> {
    let leaf = rel
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "empty path"))?;

    let real_parent = match rel.parent() {
        Some(p) if !p.as_os_str().is_empty() => {
            let real_parent = std::fs::canonicalize(root.join(p))?;
            if !real_parent.starts_with(root) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{} escapes the share root", rel.display()),
                ));
            }
            real_parent
        }
        // No parent component (the leaf lives directly under the share
        // root) — the root itself is already canonicalized at construction,
        // nothing further to resolve.
        _ => root.to_path_buf(),
    };
    Ok(real_parent.join(leaf))
}

/// Refuse devices, FIFOs, and sockets — a FIFO served over the wire blocks a
/// kernel task indefinitely (`docs/slash-r.md`). Regular files and
/// directories only; symlinks never reach here (both resolvers above return
/// a real, already-followed-or-preserved path, never a dangling symlink
/// type).
#[cfg(unix)]
fn refuse_special(meta: &std::fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let ft = meta.file_type();
    if ft.is_fifo() || ft.is_socket() || ft.is_char_device() || ft.is_block_device() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to serve a special file (device/fifo/socket)",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn refuse_special(_meta: &std::fs::Metadata) -> io::Result<()> {
    Ok(())
}

/// Open a real file for reading, closing the lstat→open race where the OS
/// lets us: Linux opens through `openat2` with `RESOLVE_BENEATH`, so
/// resolution and containment happen atomically in the kernel — no window
/// between the check and the open. Elsewhere (or if the running kernel lacks
/// `openat2`, pre-5.6), falls back to the portable
/// [`resolve_follow`]-then-`open` discipline with the residual TOCTOU that
/// implies, documented and accepted (`docs/slash-r.md`).
fn open_file_beneath(root: &Path, rel: &Path) -> io::Result<std::fs::File> {
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{openat2, Mode, OFlags, ResolveFlags};
        let root_fd = std::fs::File::open(root)?;
        match openat2(
            &root_fd,
            rel,
            // NONBLOCK is load-bearing, not an optimization: opening a FIFO
            // for read WITHOUT it blocks the calling thread until a writer
            // opens the other end — exactly the "served FIFO blocks a kernel
            // task indefinitely" hazard this function exists to refuse. The
            // special-file check below runs on the fd AFTER open() returns,
            // which is too late if open() itself already hung. NONBLOCK has
            // no effect on a regular file open, so this is free for the
            // common case.
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
            ResolveFlags::BENEATH,
        ) {
            Ok(fd) => {
                let file = std::fs::File::from(fd);
                let meta = file.metadata()?;
                if meta.is_dir() {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "is a directory"));
                }
                refuse_special(&meta)?;
                return Ok(file);
            }
            // Kernel too old for openat2 (<5.6) — fall back below. Any OTHER
            // error (including a genuine jail escape, which openat2 surfaces
            // as ENOENT/ELOOP/EXDEV under RESOLVE_BENEATH) must NOT fall
            // through to the less-atomic path: that would let a real escape
            // attempt get a second, racier chance to succeed.
            Err(e) if e == rustix::io::Errno::NOSYS => {}
            Err(e) => return Err(e.into()),
        }
    }

    open_file_fallback(root, rel)
}

/// The portable fallback body of [`open_file_beneath`]: canonicalize-then-
/// open, with the documented residual TOCTOU. Split out so the fallback is
/// directly testable even on hosts whose `openat2` path handles the default
/// case — this code is live on non-Linux unix and pre-5.6 kernels.
fn open_file_fallback(root: &Path, rel: &Path) -> io::Result<std::fs::File> {
    let real = resolve_follow(root, rel)?;
    let file = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // O_NONBLOCK for the same reason the openat2 path carries it: a
            // FIFO swapped in between `resolve_follow` and this open would
            // otherwise block the thread until a writer appears — the
            // check-then-open gap is exactly the fallback's documented
            // residual race, so the type check must run on the OPENED fd
            // (fstat below), never a pre-open stat alone.
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&real)?
        }
        #[cfg(not(unix))]
        {
            std::fs::File::open(&real)?
        }
    };
    // fstat the opened object itself — no window between check and use.
    let meta = file.metadata()?;
    if meta.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "is a directory"));
    }
    refuse_special(&meta)?;
    Ok(file)
}

/// Host mtime-nanos — `LocalBackend`'s own generation rule
/// (`crates/kaijutsu-kernel/src/vfs/backends/local.rs`), now crossing the
/// wire as the `kaijutsu-generation@kaijutsu.dev` extension reply.
fn generation_of(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn dir_attrs(meta: Option<&std::fs::Metadata>) -> FileAttributes {
    let mut fa = FileAttributes::empty();
    // `permissions` MUST be assigned before `set_dir` — `set_dir`/`set_regular`/
    // `set_symlink` OR their type bit INTO `self.permissions` (russh_sftp's
    // `set_type`), so assigning `.permissions` afterward clobbers the type bit
    // it just set. Every attrs builder in this file follows this order.
    fa.permissions = Some(meta.map(perm_bits).unwrap_or(0o555));
    fa.set_dir(true);
    if let Some(m) = meta {
        fa.mtime = Some(unix_secs(m.modified()));
        fa.size = Some(0);
    }
    fa
}

#[cfg(unix)]
fn perm_bits(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn perm_bits(_meta: &std::fs::Metadata) -> u32 {
    0o755
}

fn unix_secs(t: io::Result<SystemTime>) -> u32 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

/// Attrs for a directory entry, WITHOUT following a symlink (lstat
/// semantics) — used by `readdir`/`lstat`.
fn attrs_from_lstat(meta: &std::fs::Metadata) -> FileAttributes {
    let mut fa = FileAttributes::empty();
    // See the ordering note on `dir_attrs`: permissions first, type bit OR'd
    // in after.
    fa.permissions = Some(perm_bits(meta));
    if meta.is_dir() {
        fa.set_dir(true);
    } else if meta.file_type().is_symlink() {
        fa.set_symlink(true);
    } else if meta.is_file() {
        fa.set_regular(true);
        fa.size = Some(meta.len());
    } else {
        // Special file (fifo/socket/device): visible in a listing, never
        // openable (`refuse_special`). No `FileType` variant fits — leave the
        // type bits unset; size/perm still report.
        fa.size = Some(meta.len());
    }
    fa.mtime = Some(unix_secs(meta.modified()));
    fa
}

enum HandleEntry {
    /// The in-memory manifest — served from `Arc<Vec<u8>>`, no real fd.
    Manifest,
    File(tokio::fs::File),
    Dir {
        /// `None` metadata marks THE synthesized `/index` manifest entry —
        /// only `opendir`'s `Route::Root` arm ever creates one, so the
        /// marker is structural: a real file that happens to be named
        /// `index` inside a share always carries `Some(meta)` and lists
        /// with its own attrs, never the manifest's (and the manifest entry
        /// itself needs no host `stat` to exist — no silent drop, no
        /// `temp_dir` placeholder).
        entries: Vec<(String, Option<std::fs::Metadata>)>,
        cursor: usize,
        dots_sent: bool,
    },
}

/// Per-session handler — one instance per SSH share-subsystem channel.
pub struct ShareHandler {
    config: Arc<ShareServerConfig>,
    handles: HashMap<String, HandleEntry>,
    next_handle: u64,
}

impl ShareHandler {
    pub fn new(config: ShareServerConfig) -> Self {
        Self {
            config: Arc::new(config),
            handles: HashMap::new(),
            next_handle: 0,
        }
    }

    fn alloc(&mut self, entry: HandleEntry) -> String {
        let id = self.next_handle;
        self.next_handle += 1;
        let key = format!("h{id}");
        self.handles.insert(key.clone(), entry);
        key
    }

    /// The generation of whatever a client-visible path currently names —
    /// shared by the `extended` generation batch handler. Root's own
    /// generation reuses [`INDEX_GENERATION`] (no real backing mtime to
    /// report; a synthetic namespace node, constant like the manifest).
    fn generation_of_route(route: &Route) -> io::Result<u64> {
        match route {
            Route::Root | Route::Index => Ok(INDEX_GENERATION),
            Route::ShareRoot(s) => Ok(generation_of(&std::fs::metadata(&s.root)?)),
            Route::InShare(s, rel) => {
                let real = resolve_follow(&s.root, rel)?;
                Ok(generation_of(&std::fs::metadata(&real)?))
            }
        }
    }
}

impl Handler for ShareHandler {
    type Error = StatusReply;

    fn unimplemented(&self) -> Self::Error {
        StatusReply::new(StatusCode::OpUnsupported)
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        let mut ext = HashMap::new();
        ext.insert(
            GENERATION_EXTENSION.to_string(),
            GENERATION_EXTENSION_VERSION.to_string(),
        );
        Ok(Version {
            version: russh_sftp::protocol::VERSION,
            extensions: ext,
        })
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let comps = clean_components(&path);
        let mut resolved = String::from("/");
        resolved.push_str(&comps.join("/"));
        Ok(Name {
            id,
            files: vec![File::dummy(resolved)],
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        match route(&self.config, &path)? {
            Route::Root => Ok(Attrs { id, attrs: dir_attrs(None) }),
            Route::Index => {
                let mut fa = FileAttributes::empty();
                fa.permissions = Some(0o444);
                fa.set_regular(true);
                fa.size = Some(self.config.manifest.len() as u64);
                Ok(Attrs { id, attrs: fa })
            }
            Route::ShareRoot(s) => {
                let meta = std::fs::metadata(&s.root).map_err(|e| io_status(&e))?;
                Ok(Attrs { id, attrs: dir_attrs(Some(&meta)) })
            }
            Route::InShare(s, rel) => {
                let real = resolve_lstat(&s.root, &rel).map_err(|e| io_status(&e))?;
                let meta = std::fs::symlink_metadata(&real).map_err(|e| io_status(&e))?;
                Ok(Attrs { id, attrs: attrs_from_lstat(&meta) })
            }
        }
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        match route(&self.config, &path)? {
            Route::Root => Ok(Attrs { id, attrs: dir_attrs(None) }),
            Route::Index => self.lstat(id, path).await,
            Route::ShareRoot(s) => {
                let meta = std::fs::metadata(&s.root).map_err(|e| io_status(&e))?;
                Ok(Attrs { id, attrs: dir_attrs(Some(&meta)) })
            }
            Route::InShare(s, rel) => {
                let real = resolve_follow(&s.root, &rel).map_err(|e| io_status(&e))?;
                let meta = std::fs::metadata(&real).map_err(|e| io_status(&e))?;
                Ok(Attrs { id, attrs: attrs_from_lstat(&meta) })
            }
        }
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        match self.handles.get(&handle) {
            Some(HandleEntry::Manifest) => {
                let mut fa = FileAttributes::empty();
                fa.set_regular(true);
                fa.size = Some(self.config.manifest.len() as u64);
                Ok(Attrs { id, attrs: fa })
            }
            Some(HandleEntry::File(f)) => {
                let meta = f.metadata().await.map_err(|e| io_status(&e))?;
                Ok(Attrs { id, attrs: attrs_from_lstat(&meta) })
            }
            Some(HandleEntry::Dir { .. }) => Ok(Attrs { id, attrs: dir_attrs(None) }),
            None => Err(StatusReply::new(StatusCode::Failure).with_message("bad handle")),
        }
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let entries: Vec<(String, Option<std::fs::Metadata>)> = match route(&self.config, &path)? {
            Route::Root => {
                let mut v: Vec<(String, Option<std::fs::Metadata>)> = self
                    .config
                    .shares
                    .iter()
                    .filter_map(|s| {
                        std::fs::metadata(&s.root).ok().map(|m| (s.name.clone(), Some(m)))
                    })
                    .collect();
                // The manifest has no real host file — a `None`-metadata
                // marker entry, always present (its attrs are synthesized
                // from the in-memory manifest in `readdir`; nothing on disk
                // to stat, nothing to silently fail).
                v.push((INDEX_NAME.to_string(), None));
                v
            }
            Route::Index => return Err(StatusReply::new(StatusCode::Failure).with_message("not a directory")),
            Route::ShareRoot(s) => real_entries(read_dir_lstat(&s.root).map_err(|e| io_status(&e))?),
            Route::InShare(s, rel) => {
                let real = resolve_follow(&s.root, &rel).map_err(|e| io_status(&e))?;
                real_entries(read_dir_lstat(&real).map_err(|e| io_status(&e))?)
            }
        };
        let handle = self.alloc(HandleEntry::Dir { entries, cursor: 0, dots_sent: false });
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let (batch, include_dots) = match self.handles.get_mut(&handle) {
            Some(HandleEntry::Dir { entries, cursor, dots_sent }) => {
                let include_dots = !*dots_sent;
                *dots_sent = true;
                let start = *cursor;
                let end = (start + READDIR_CHUNK).min(entries.len());
                let batch: Vec<(String, Option<std::fs::Metadata>)> = entries[start..end].to_vec();
                *cursor = end;
                if batch.is_empty() && !include_dots {
                    return Err(StatusReply::new(StatusCode::Eof));
                }
                (batch, include_dots)
            }
            _ => return Err(StatusReply::new(StatusCode::Failure).with_message("bad dir handle")),
        };

        let mut files = Vec::with_capacity(batch.len() + 2);
        if include_dots {
            files.push(File::new(".".to_string(), dir_attrs(None)));
            files.push(File::new("..".to_string(), dir_attrs(None)));
        }
        for (name, meta) in &batch {
            match meta {
                // The root's synthesized `/index` manifest entry (see
                // `HandleEntry::Dir` — only the root listing mints a
                // `None`). Keyed on the marker, NOT the name: a real file
                // named `index` inside a share carries `Some(meta)` below
                // and lists with its own attrs.
                None => {
                    let mut fa = FileAttributes::empty();
                    fa.permissions = Some(0o444);
                    fa.set_regular(true);
                    fa.size = Some(self.config.manifest.len() as u64);
                    files.push(File::new(name.clone(), fa));
                }
                Some(meta) => files.push(File::new(name.clone(), attrs_from_lstat(meta))),
            }
        }
        Ok(Name { id, files })
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let wants_write = pflags.intersects(
            OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::CREATE | OpenFlags::TRUNCATE,
        );
        if wants_write {
            return Err(StatusCode::PermissionDenied
                .with_message("shares are read-only in this build; write refused"));
        }

        match route(&self.config, &filename)? {
            Route::Index => Ok(Handle {
                id,
                handle: self.alloc(HandleEntry::Manifest),
            }),
            Route::Root | Route::ShareRoot(_) => {
                Err(StatusCode::Failure.with_message("is a directory"))
            }
            Route::InShare(s, rel) => {
                let file = open_file_beneath(&s.root, &rel).map_err(|e| io_status(&e))?;
                Ok(Handle {
                    id,
                    handle: self.alloc(HandleEntry::File(tokio::fs::File::from_std(file))),
                })
            }
        }
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let len = len.min(MAX_READ_LEN) as usize;
        match self.handles.get_mut(&handle) {
            Some(HandleEntry::Manifest) => {
                let bytes = &self.config.manifest;
                let start = (offset as usize).min(bytes.len());
                let end = (start + len).min(bytes.len());
                if start >= bytes.len() {
                    return Err(StatusReply::new(StatusCode::Eof));
                }
                Ok(Data { id, data: bytes[start..end].to_vec() })
            }
            Some(HandleEntry::File(f)) => {
                f.seek(std::io::SeekFrom::Start(offset))
                    .await
                    .map_err(|e| io_status(&e))?;
                let mut buf = vec![0u8; len];
                let n = f.read(&mut buf).await.map_err(|e| io_status(&e))?;
                if n == 0 {
                    return Err(StatusReply::new(StatusCode::Eof));
                }
                buf.truncate(n);
                Ok(Data { id, data: buf })
            }
            _ => Err(StatusReply::new(StatusCode::PermissionDenied).with_message("not a readable handle")),
        }
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        if self.handles.remove(&handle).is_none() {
            return Err(StatusReply::new(StatusCode::Failure).with_message("bad handle"));
        }
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_string(),
            language_tag: "en-US".to_string(),
        })
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        match route(&self.config, &path)? {
            Route::InShare(s, rel) => {
                let real = resolve_lstat(&s.root, &rel).map_err(|e| io_status(&e))?;
                let target = std::fs::read_link(&real).map_err(|e| io_status(&e))?;
                Ok(Name {
                    id,
                    files: vec![File::dummy(target.to_string_lossy().into_owned())],
                })
            }
            _ => Err(StatusCode::Failure.with_message("not a symlink")),
        }
    }

    async fn extended(
        &mut self,
        id: u32,
        request: String,
        data: Vec<u8>,
    ) -> Result<Packet, Self::Error> {
        if request != GENERATION_EXTENSION {
            return Err(StatusCode::OpUnsupported
                .with_message(format!("unsupported extension: {request}")));
        }
        let req: GenerationRequest = russh_sftp::de::from_bytes(&mut data.into())
            .map_err(|e| StatusCode::BadMessage.with_message(format!("bad generation request: {e}")))?;
        let mut generations = Vec::with_capacity(req.paths.len());
        for p in &req.paths {
            let r = route(&self.config, p)?;
            let generation = Self::generation_of_route(&r).map_err(|e| io_status(&e))?;
            generations.push(generation);
        }
        let bytes = russh_sftp::ser::to_bytes(&GenerationReply { generations })
            .map_err(|e| StatusCode::Failure.with_message(format!("generation reply encode: {e}")))?;
        Ok(Packet::ExtendedReply(russh_sftp::protocol::ExtendedReply {
            id,
            data: bytes.to_vec(),
        }))
    }

    // ── every mutating op: read-only in this slice, regardless of the
    // share's advertised `rw` flag (`docs/slash-r.md` slice 1 scope). ──

    async fn write(&mut self, _id: u32, _handle: String, _offset: u64, _data: Vec<u8>) -> Result<Status, Self::Error> {
        Err(StatusCode::PermissionDenied.with_message("shares are read-only in this build"))
    }
    async fn setstat(&mut self, _id: u32, _path: String, _attrs: FileAttributes) -> Result<Status, Self::Error> {
        Err(StatusCode::PermissionDenied.with_message("shares are read-only in this build"))
    }
    async fn fsetstat(&mut self, _id: u32, _handle: String, _attrs: FileAttributes) -> Result<Status, Self::Error> {
        Err(StatusCode::PermissionDenied.with_message("shares are read-only in this build"))
    }
    async fn mkdir(&mut self, _id: u32, _path: String, _attrs: FileAttributes) -> Result<Status, Self::Error> {
        Err(StatusCode::PermissionDenied.with_message("shares are read-only in this build"))
    }
    async fn rmdir(&mut self, _id: u32, _path: String) -> Result<Status, Self::Error> {
        Err(StatusCode::PermissionDenied.with_message("shares are read-only in this build"))
    }
    async fn remove(&mut self, _id: u32, _filename: String) -> Result<Status, Self::Error> {
        Err(StatusCode::PermissionDenied.with_message("shares are read-only in this build"))
    }
    async fn rename(&mut self, _id: u32, _oldpath: String, _newpath: String) -> Result<Status, Self::Error> {
        Err(StatusCode::PermissionDenied.with_message("shares are read-only in this build"))
    }
    async fn symlink(&mut self, _id: u32, _linkpath: String, _targetpath: String) -> Result<Status, Self::Error> {
        Err(StatusCode::PermissionDenied.with_message("shares are read-only in this build"))
    }
}

/// `read_dir`, reporting each entry's `lstat` (not following) — the listing
/// case, mirrors the forward adapter's directory-entry attrs (never resolves
/// a symlink just to list it).
fn read_dir_lstat(dir: &Path) -> io::Result<Vec<(String, std::fs::Metadata)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?; // std::fs::DirEntry::metadata is lstat-like (does not follow)
        out.push((entry.file_name().to_string_lossy().into_owned(), meta));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Lift a real directory listing into the `Option<Metadata>` entry shape —
/// every real entry is `Some`; only `opendir`'s root arm mints the `None`
/// manifest marker (see `HandleEntry::Dir`).
fn real_entries(
    listing: Vec<(String, std::fs::Metadata)>,
) -> Vec<(String, Option<std::fs::Metadata>)> {
    listing.into_iter().map(|(name, meta)| (name, Some(meta))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_path() {
        let a = parse_share_arg("/home/amy/Downloads").unwrap();
        assert_eq!(a.name, "Downloads");
        assert_eq!(a.path, PathBuf::from("/home/amy/Downloads"));
        assert!(!a.rw);
    }

    #[test]
    fn parses_explicit_name() {
        let a = parse_share_arg("dl=/home/amy/Downloads").unwrap();
        assert_eq!(a.name, "dl");
        assert_eq!(a.path, PathBuf::from("/home/amy/Downloads"));
    }

    #[test]
    fn parses_rw_suffix() {
        let a = parse_share_arg("/home/amy/src:rw").unwrap();
        assert_eq!(a.name, "src");
        assert!(a.rw);
    }

    #[test]
    fn parses_explicit_name_and_rw() {
        let a = parse_share_arg("code=/home/amy/src:rw").unwrap();
        assert_eq!(a.name, "code");
        assert_eq!(a.path, PathBuf::from("/home/amy/src"));
        assert!(a.rw);
    }

    #[test]
    fn empty_path_is_rejected() {
        assert!(parse_share_arg("name=").is_err());
        assert!(parse_share_arg("").is_err());
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let shares = vec![
            parse_share_arg("a/x").unwrap(),
            parse_share_arg("b/x").unwrap(),
        ];
        assert!(validate_unique_names(&shares).is_err());
    }

    #[test]
    fn distinct_explicit_names_are_accepted() {
        let shares = vec![
            parse_share_arg("first=a/x").unwrap(),
            parse_share_arg("second=b/x").unwrap(),
        ];
        assert!(validate_unique_names(&shares).is_ok());
    }

    #[test]
    fn clean_components_clamps_parent_at_root() {
        assert_eq!(clean_components("/.."), Vec::<String>::new());
        assert_eq!(clean_components("a/../b"), vec!["b".to_string()]);
        assert_eq!(clean_components("/a/b"), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn resolve_follow_refuses_a_symlink_escaping_the_jail() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("escape")).unwrap();

        let err = resolve_follow(&root, Path::new("escape")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn resolve_follow_allows_a_symlink_staying_inside_the_jail() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("real.txt"), b"hi").unwrap();
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();

        let real = resolve_follow(&root, Path::new("link.txt")).unwrap();
        assert_eq!(real, root.join("real.txt"));
    }

    #[test]
    fn resolve_lstat_reports_a_symlink_without_following_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("escape")).unwrap();

        // lstat must succeed (it's visible as a symlink)...
        let real = resolve_lstat(&root, Path::new("escape")).unwrap();
        let meta = std::fs::symlink_metadata(&real).unwrap();
        assert!(meta.file_type().is_symlink());

        // ...but following it (open/stat) must be refused.
        assert!(resolve_follow(&root, Path::new("escape")).is_err());
    }

    #[test]
    fn open_file_beneath_refuses_a_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let fifo_path = root.join("myfifo");
        let c_path = std::ffi::CString::new(fifo_path.to_str().unwrap()).unwrap();
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed");

        let err = open_file_beneath(&root, Path::new("myfifo")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    /// The PORTABLE fallback (live on non-Linux unix / pre-5.6 kernels; the
    /// openat2 path shadows it on this host) must not block on a FIFO: the
    /// old order stat-then-`File::open` left the open itself blocking when a
    /// FIFO appeared after the check. The fix opens with `O_NONBLOCK` and
    /// fstats the opened fd — this test HANGING (not merely failing) is the
    /// regression signal.
    #[test]
    fn fallback_open_refuses_a_fifo_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let fifo_path = root.join("fallbackfifo");
        let c_path = std::ffi::CString::new(fifo_path.to_str().unwrap()).unwrap();
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed");

        let err = open_file_fallback(&root, Path::new("fallbackfifo")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn open_file_beneath_refuses_a_path_escaping_the_jail() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"nope").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.join("escape.txt")).unwrap();

        let err = open_file_beneath(&root, Path::new("escape.txt")).unwrap_err();
        assert_ne!(err.kind(), io::ErrorKind::Other, "must be a clean, mapped error");
    }

    #[test]
    fn open_file_beneath_opens_a_regular_file_in_jail() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("ok.txt"), b"hello").unwrap();

        let mut f = open_file_beneath(&root, Path::new("ok.txt")).unwrap();
        use std::io::Read;
        let mut buf = String::new();
        f.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "hello");
    }

    fn one_share(dir: &std::path::Path) -> ShareServerConfig {
        let args = vec![ShareArg {
            name: "share".to_string(),
            path: dir.to_path_buf(),
            rw: false,
        }];
        ShareServerConfig::new(&args, "client-1", "test-nick").unwrap()
    }

    #[tokio::test]
    async fn manifest_round_trips_through_the_shared_format() {
        let dir = tempfile::tempdir().unwrap();
        let config = one_share(dir.path());
        let rows = kaijutsu_types::share::parse_manifest(&config.manifest).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "share");
        assert_eq!(rows[0].client_id, "client-1");
        assert_eq!(rows[0].nick, "test-nick");
        assert!(!rows[0].rw);
    }

    #[tokio::test]
    async fn root_lists_shares_and_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut handler = ShareHandler::new(one_share(dir.path()));

        let h = Handler::opendir(&mut handler, 1, "/".to_string()).await.unwrap();
        let name = Handler::readdir(&mut handler, 2, h.handle).await.unwrap();
        let names: Vec<&str> = name.files.iter().map(|f| f.filename.as_str()).collect();
        assert!(names.contains(&"share"));
        assert!(names.contains(&"index"));

        // Regression guard: `set_dir`/`set_regular` OR their type bit INTO
        // `permissions` (russh_sftp's `set_type`) — assigning `.permissions`
        // AFTER calling them clobbers the bit that was just set. This exact
        // bug shipped once (caught by the cross-crate registration test,
        // not this one — it never asserted on the type bits) and made every
        // share root's `is_dir()` read false, so registration refused every
        // real share as "not a directory."
        let share_entry = name.files.iter().find(|f| f.filename == "share").unwrap();
        assert!(share_entry.attrs.is_dir(), "share root must report is_dir()");
        let index_entry = name.files.iter().find(|f| f.filename == "index").unwrap();
        assert!(index_entry.attrs.is_regular(), "index must report is_regular()");
    }

    #[tokio::test]
    async fn lstat_and_stat_on_a_share_root_report_is_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut handler = ShareHandler::new(one_share(dir.path()));

        let lstat = Handler::lstat(&mut handler, 1, "/share".to_string()).await.unwrap();
        assert!(lstat.attrs.is_dir(), "lstat /share must report is_dir()");

        let stat = Handler::stat(&mut handler, 2, "/share".to_string()).await.unwrap();
        assert!(stat.attrs.is_dir(), "stat /share must report is_dir()");

        let lstat_index = Handler::lstat(&mut handler, 3, "/index".to_string()).await.unwrap();
        assert!(lstat_index.attrs.is_regular(), "lstat /index must report is_regular()");
    }

    /// Regression: the readdir attrs override for the root's synthesized
    /// `/index` entry used to key on the NAME — so a real file named `index`
    /// inside a share listed with the manifest's size/permissions instead of
    /// its own. The override is now keyed on the `None`-metadata marker only
    /// `opendir`'s root arm mints.
    #[tokio::test]
    async fn a_real_file_named_index_inside_a_share_lists_its_own_attrs() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"just a file"; // 11 bytes, unlike any manifest
        std::fs::write(dir.path().join("index"), body).unwrap();
        let config = one_share(dir.path());
        let manifest_len = config.manifest.len() as u64;
        assert_ne!(manifest_len, body.len() as u64, "test needs distinguishable sizes");
        let mut handler = ShareHandler::new(config);

        // Inside the share: the real file's own size, not the manifest's.
        let h = Handler::opendir(&mut handler, 1, "/share".to_string()).await.unwrap();
        let listing = Handler::readdir(&mut handler, 2, h.handle).await.unwrap();
        let entry = listing
            .files
            .iter()
            .find(|f| f.filename == "index")
            .expect("real index file listed");
        assert_eq!(
            entry.attrs.size,
            Some(body.len() as u64),
            "a real file named index must list with its own size"
        );

        // At the root: the synthesized manifest entry still reports the
        // manifest's size (and exists without any host stat backing it).
        let h = Handler::opendir(&mut handler, 3, "/".to_string()).await.unwrap();
        let root_listing = Handler::readdir(&mut handler, 4, h.handle).await.unwrap();
        let root_index = root_listing
            .files
            .iter()
            .find(|f| f.filename == "index")
            .expect("root index entry always present");
        assert_eq!(root_index.attrs.size, Some(manifest_len));
        assert!(root_index.attrs.is_regular());
    }

    #[tokio::test]
    async fn index_reads_back_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = one_share(dir.path()).manifest.clone();
        let mut handler = ShareHandler::new(one_share(dir.path()));

        let h = Handler::open(
            &mut handler,
            1,
            "/index".to_string(),
            OpenFlags::READ,
            FileAttributes::empty(),
        )
        .await
        .unwrap();
        let data = Handler::read(&mut handler, 2, h.handle, 0, 4096).await.unwrap();
        assert_eq!(data.data, *manifest);
    }

    #[tokio::test]
    async fn write_open_is_refused_regardless_of_rw_flag() {
        let dir = tempfile::tempdir().unwrap();
        let args = vec![ShareArg { name: "share".to_string(), path: dir.path().to_path_buf(), rw: true }];
        let config = ShareServerConfig::new(&args, "c", "n").unwrap();
        let mut handler = ShareHandler::new(config);

        let err = Handler::open(
            &mut handler,
            1,
            "/share/new.txt".to_string(),
            OpenFlags::WRITE | OpenFlags::CREATE,
            FileAttributes::empty(),
        )
        .await
        .expect_err("write must be refused even on an rw-labelled share");
        assert_eq!(err.status_code, StatusCode::PermissionDenied);
    }

    #[tokio::test]
    async fn read_through_a_share_returns_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), b"share bytes").unwrap();
        let mut handler = ShareHandler::new(one_share(dir.path()));

        let h = Handler::open(
            &mut handler,
            1,
            "/share/hello.txt".to_string(),
            OpenFlags::READ,
            FileAttributes::empty(),
        )
        .await
        .unwrap();
        let data = Handler::read(&mut handler, 2, h.handle, 0, 4096).await.unwrap();
        assert_eq!(data.data, b"share bytes");
    }

    #[tokio::test]
    async fn generation_extension_reports_host_mtime_nanos() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
        let mut handler = ShareHandler::new(one_share(dir.path()));

        let req = GenerationRequest { paths: vec!["/share/f.txt".to_string()] };
        let payload = russh_sftp::ser::to_bytes(&req).unwrap().to_vec();
        let packet = Handler::extended(&mut handler, 1, GENERATION_EXTENSION.to_string(), payload)
            .await
            .unwrap();
        let Packet::ExtendedReply(reply) = packet else { panic!("expected ExtendedReply") };
        let decoded: GenerationReply =
            russh_sftp::de::from_bytes(&mut reply.data.into()).unwrap();
        assert_eq!(decoded.generations.len(), 1);
        assert!(decoded.generations[0] > 0);
    }

    #[tokio::test]
    async fn init_advertises_the_generation_extension() {
        let dir = tempfile::tempdir().unwrap();
        let mut handler = ShareHandler::new(one_share(dir.path()));
        let version = Handler::init(&mut handler, 3, HashMap::new()).await.unwrap();
        assert_eq!(
            version.extensions.get(GENERATION_EXTENSION),
            Some(&GENERATION_EXTENSION_VERSION.to_string())
        );
    }
}
