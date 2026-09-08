# SFTP over the kaijutsu VFS

The kernel's virtual filesystem is exposed over SFTP, so any off-the-shelf
SFTP client (sshfs, `sftp`, Nautilus, an editor's remote-FS plugin) can read
and write the unified tree — host FS (including `/config/rc` and
`/config/kernel`), `/v/cas`, and the memory scratch at `/tmp` — through the
same SSH server that carries the Cap'n Proto RPC channel and the `/r` share
role-swap (`docs/slash-r.md`). SFTP needs no capability verdict of its own: a
write to any mount is governed only by that mount's own `read_only()` flag,
same as `LocalBackend`, host `vim`, or the file tools (`docs/config-namespace.md`,
`docs/slash-v.md` "Capability"). The first real consumer is client CAS sync
against `/v/cas` (`docs/slash-v.md` track B).

## Why this is mostly already done

- **`VfsOps` is path-based and async** (`crates/kaijutsu-kernel/src/vfs/ops.rs:29`).
  Its methods — `getattr`, `readdir`, `read(path, offset, size)`,
  `write(path, offset, data)`, `create`, `mkdir`, `unlink`, `rmdir`, `rename`,
  `truncate`, `setattr`, `symlink`, `link`, `readlink`, `statfs` — are nearly
  the full `SSH_FXP_*` opcode set. SFTP is path-based too, so there is no
  semantic model to invent.
- **`MountTable` already unifies the tree** (`crates/kaijutsu-kernel/src/vfs/mount.rs`).
  Longest-prefix routing means one SFTP client sees host FS, kernel-owned
  mounts, and memory scratch side by side, each op dispatched to the right
  backend with the mount prefix stripped. `LocalBackend`, `MemoryBackend`,
  `CasFs`, and `RosterFs` implement the full trait.
- **Kernel-owned-mount write-through is unified and crash-safe**
  (`crates/kaijutsu-kernel/src/runtime/mount_backend.rs:294`). A write to a
  kernel-owned mount flows through `FileDocumentCache` → `flush_one` → backend, with
  rollback-on-flush-error and mtime-staleness reload. An SFTP write is just
  another client of that cache — no new correctness work.
- **russh is the SSH library** (0.61.x). `ConnectionHandler`
  (`crates/kaijutsu-server/src/ssh.rs:480`) implements `subsystem_request`,
  which stashes each opened channel until the client names a subsystem, then
  dispatches by name: `kaijutsu-rpc` spawns the existing per-connection RPC
  thread, `sftp` spawns `SftpSession`, and `kaijutsu-share` spawns the `/r`
  role-swap (`docs/slash-r.md`). All three are `match name` arms over one
  retention-and-dispatch scaffold — adding a future debug-kaish subsystem is
  one more arm.

## The `/v` surfaces live in `docs/slash-v.md`

`/v` VFS surfaces are general, every-surface backends, not SFTP details, so
their design lives in **`docs/slash-v.md`**: **`/v/cas`** (the CAS pool,
shipped — SFTP is how clients sync it) and **`/v/ctx`** (context + block-log
introspection, designed but unbuilt). The live-participant roster that doc
once sketched as `/v/session` shipped instead as `/run/roster`, outside `/v`
(`docs/slash-v.md`). Every kernel-`MountTable` mount is SFTP-visible the
moment it lands — SFTP is a *consumer* of these trees, never their capability
driver (`docs/slash-v.md` "Capability").

## Principal — carried for attribution, not for a capability check

`SftpSession` is constructed with the connection's authenticated `PrincipalId`
(`crates/kaijutsu-server/src/ssh.rs:958`, from `ConnectionHandler.identity`)
and keeps it for logging and tracing (`sftp.rs:111`, `.short()` in every
span) — it is never threaded into a `VfsOps` call, because no `VfsOps` method
takes a principal. Capabilities in kaijutsu are bound to a *context loadout*,
not to a principal, and SFTP needs no capability verdict at all: a write to
any mount is governed only by that mount's own `read_only()` flag, exactly
like `LocalBackend`, host `vim`, or the file tools
(`docs/config-namespace.md`). An earlier design routed SFTP writes through a
per-connection `bound` context set by an arming symlink; Amy's guidance
dissolved it (`docs/slash-v.md` "Capability — per-operation join, not
per-session binding") because every *real* write surface already carries its
acting context ambiently, and SFTP alone needing a stashed capability grant
was scaffolding for a problem the other surfaces don't have.

## Handle mapping — the one real impedance mismatch

SFTP is stateful: `OPEN` returns a handle, then `READ`/`WRITE`/`CLOSE` operate
on it. `VfsOps` is stateless-per-call (path + offset + length). The adapter
keeps a `HashMap<Handle, OpenFile>` where `OpenFile` holds the resolved path,
the open flags, and a running offset. `OPEN` allocates an entry, `READ`/`WRITE`
translate to `vfs.read`/`vfs.write` at the tracked offset, `CLOSE` drops it.
Directory handles map to a paged `readdir` result.

**TOCTOU hazard — the part the first draft glossed.** Storing only `(path,
offset)` is unsafe: SFTP clients expect a handle to pin the *file object*, not
the path string. If client X opens `A` (handle 1), and meanwhile `A` is renamed
away and a *new* `A` is created, X's subsequent writes — translated to
`write("A", …)` — silently land in the wrong file. `VfsOps` cannot pin an
inode, so the adapter must compensate: capture the file's `generation` stamp
(`FileAttr` — see "`generation`, not mtime" below) into `OpenFile` at `OPEN` and
re-verify it on every `WRITE`, failing the op if the underlying file was
replaced. Without this, SFTP is a corruption vector, not merely a coherence
question.

**Pipelining.** SFTP clients do *not* wait for a `WRITE` reply before sending
the next — they pipeline. Sequential processing throttles throughput to one
round-trip per block; concurrent processing forces interior mutability on the
handle map and hammers the document cache with overlapping writes to one path. The
adapter has to choose deliberately, not fall into either by accident.

**Directory-handle leaks.** A cached `readdir` held across `READDIR` pages is a
memory-exhaustion vector if a client opens a dir, reads half, and never sends
`CLOSE`. Bound it (cap retained entries, or evict on session idle).

Beyond the per-handle stamp, concurrent-write coherence still leans on the
cache (mtime staleness + rollback-on-flush-failure) — which is exactly why the
mtime work below is a prerequisite, not an afterthought.

## `generation`, not mtime, is the coherence primitive

SFTP makes coherence semantics load-bearing in a way the in-app tools never
did, because caching clients (sshfs, editor indexers, `make`, `rsync`) treat
mtime as ground truth. mtime stays a real, display-only `SystemTime` on every
backend; `FileAttr.generation` (`crates/kaijutsu-kernel/src/vfs/types.rs`) is
the strictly-advancing, content-tied stamp `FileDocumentCache` actually
compares (`file_tools/cache.rs` — `loaded_generation`, the `d > l` staleness
check), and the same stamp an SFTP `OPEN` captures for its TOCTOU re-verify
above — the handle guard and the cache share one primitive. Every backend
sources it differently: `LocalBackend` derives it from host mtime-nanos
(advances with external edits); `MemoryBackend` bumps a monotonic per-backend
counter on every content mutation, so it advances even within one mtime tick.
`setattr(mtime)` is honored for display but deliberately never bumps
generation, so a pure attribute touch never triggers a needless reload.

## Security posture

- SFTP is reachable only after the existing pubkey auth succeeds; there is no
  new authentication surface.
- There is no lexical deny on `/config/rc` or `/config/kernel` — every player is
  inside one trust boundary, and capabilities are ergonomic nudges, not a
  security control (CLAUDE.md "Shared trust, crosstalk-as-feature"). A write
  there over SFTP is governed the same way a write anywhere else is: the
  mount's own `read_only()` flag.
- Mount `read_only()` flags are enforced by the VFS regardless of principal, so
  read-only mounts stay read-only over SFTP for free. **Note:** root `/` is a
  *read-only* host anchor (`LocalBackend::read_only("/")`); only the project
  tree is writable. So the "SFTP write to `~/.ssh/authorized_keys` → host RCE"
  bypass does **not** hold unless a home directory is explicitly mounted
  writable — the writable host surface is whatever the mount table exposes, and
  today that is narrow. Worth re-checking whenever mounts change.
- **The access-pattern shift is the real new risk.** Shared-trust works for CLI
  tools because a human drives them intentionally. SFTP turns that into an
  *unconstrained programmatic crawl*: mount via sshfs in VS Code/IntelliJ and
  the background indexer immediately walks the whole tree, reading every file to
  build its search index — pulling every `/tmp` blob and kernel document over the
  channel, with potential to OOM the kernel or saturate the link. This needs
  **rate-limiting and traversal-depth/size limits** on the adapter; the
  "ergonomic nudge" framing does not cover it.
- Per-connection limits (the active-connection counter at
  `crates/kaijutsu-server/src/ssh.rs:650`) already apply, since SFTP rides the
  same connection accounting.

## Implementation status

Subsystem dispatch, the adapter (`sftp.rs`, `SSH_FXP_REALPATH` and `.`/`..`
canonicalization without an escape hatch out of the mount boundary, the
OpenSSH extensions stock clients depend on — `posix-rename@openssh.com`,
`statvfs@openssh.com`, `fsync@openssh.com`, `hardlink@openssh.com` — strict
VFS-error → `SSH_FX_*` mapping, and the handle map with its TOCTOU guard) are
built and covered by `crates/kaijutsu-server/tests/sftp_adapter.rs` and
`sftp_transport.rs`, including a live test confirming a `/config/rc` write is
an ordinary host-file write and one exercising the rename-replace TOCTOU
guard.

Still open:

- **Adapter-level limits.** Rate-limiting and traversal-depth/size caps to
  survive editor-indexer crawls; directory-handle eviction — `opendir`
  materializes a whole `readdir` per handle with no pagination
  (`docs/issues.md`, "SFTP over the VFS: appends can clobber each other").
- **Append races.** `write`'s generation guard and its `APPEND` offset share
  one `getattr`, so two cross-session appenders can both read the same
  generation and lose an update — `VfsOps` has no atomic-append primitive
  (`docs/issues.md`, "SFTP over the VFS: appends can clobber each other").

The active consumer is **`/v/cas` client CAS sync** (`docs/slash-v.md` track
B) — the SFTP adapter needs nothing further for it beyond the mount existing.
The limits above and the append-race fix are not optional polish. A future
debug-kaish subsystem is a later tenant of the same channel-retention scaffold.

## File references

- `crates/kaijutsu-kernel/src/vfs/ops.rs:29` — `VfsOps` trait
- `crates/kaijutsu-kernel/src/vfs/mount.rs:57` — `MountTable`
- `crates/kaijutsu-kernel/src/runtime/mount_backend.rs:294` — kernel-document write-through
- `crates/kaijutsu-server/src/ssh.rs:480` — `ConnectionHandler`; `:892` — `subsystem_request`; `:535` — `spawn_rpc_thread`
- `crates/kaijutsu-client/src/ssh.rs:210` — `connect_subsystem`
- russh 0.61.1 `server/mod.rs:633` (`subsystem_request`) / `channels/mod.rs:249` (`request_subsystem`)
- `crates/kaijutsu-kernel/src/mcp/binding.rs` — `Capability` (`ConfigWrite` remains but no longer gates file writes; `RcWrite` and `context_allows_rc_write` are deleted, see `docs/config-namespace.md`)
- `crates/kaijutsu-types/src/ids.rs:20` — `PrincipalId` (the whole of what SFTP authenticates; no `username`/`display_name` — names live on `characters`, see `docs/character.md`)
- `crates/kaijutsu-server/src/sftp.rs:107` — `SftpSession`
- `crates/kaijutsu-kernel/src/vfs/types.rs` — `FileAttr::generation`
- `crates/kaijutsu-kernel/src/vfs/backends/local.rs` / `memory.rs` — `LocalBackend` / `MemoryBackend` generation sourcing
