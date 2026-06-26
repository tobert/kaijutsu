# SFTP over the kaijutsu VFS

*Design note. Status: proposed 2026-06-25. The `generation` prerequisite is
**landed** (commits `70a589ea` / `7f38ff2b` / `f542c944`); the SSH-surface work
below is **not started**. Next step: the RPC-named migration (slice 0).*

Expose the kernel's virtual filesystem over SFTP so any off-the-shelf SFTP
client (sshfs, `sftp`, Nautilus, an editor's remote-FS plugin) can read and
write the unified tree — host FS, CRDT-backed `/etc/rc` and `/v/...`, and the
memory scratch at `/tmp` — through the same SSH server that already carries the
Cap'n Proto RPC channel.

This is plumbing, not new architecture. The VFS is already SFTP-shaped; the
work is a channel-dispatch scaffold on the SSH session-channel surface, an SFTP
adapter, and one genuine design decision about how an SFTP session reaches a
capability verdict. That scaffold is shared with two siblings — a cleaner
**named RPC** (today it's positional) and a future **debug kaish** shell — so
this doc covers the whole surface, not SFTP alone.

## Why this is mostly already done

- **`VfsOps` is path-based and async** (`crates/kaijutsu-kernel/src/vfs/ops.rs:20`).
  Its methods — `getattr`, `readdir`, `read(path, offset, size)`,
  `write(path, offset, data)`, `create`, `mkdir`, `unlink`, `rmdir`, `rename`,
  `truncate`, `setattr`, `symlink`, `link`, `readlink`, `statfs` — are nearly
  the full `SSH_FXP_*` opcode set. SFTP is path-based too, so there is no
  semantic model to invent.
- **`MountTable` already unifies the tree** (`crates/kaijutsu-kernel/src/vfs/mount.rs`).
  Longest-prefix routing means one SFTP client sees host FS, CRDT mounts, and
  memory scratch side by side, each op dispatched to the right backend with the
  mount prefix stripped. Backends (`LocalBackend`, `MemoryBackend`,
  `ConfigCrdtFs`) implement the full trait today.
- **CRDT write-through is already unified and crash-safe**
  (`crates/kaijutsu-kernel/src/runtime/mount_backend.rs:293`). A write to a
  CRDT mount flows through `FileDocumentCache` → `flush_one` → backend, with
  rollback-on-flush-error and mtime-staleness reload. An SFTP write is just
  another client of that cache — no new correctness work.
- **russh is already the SSH library** (0.61.x), and the per-connection
  threading pattern to copy already exists for the RPC channel
  (`crates/kaijutsu-server/src/ssh.rs:696`).

## The gap

`ConnectionHandler` (`crates/kaijutsu-server/src/ssh.rs:632`) implements only
`channel_open_session`, `auth_publickey`, and `channel_close`. The russh 0.61.1
`Handler` trait already exposes the methods we'd need —
`subsystem_request(channel: ChannelId, name: &str, session)` (the `subsystem
sftp` entry point), plus `shell_request` / `exec_request` / `pty_request` /
`data` for a future debug shell — but **none are implemented**. That handler
method is the one structural thing missing.

No SFTP crate is in the tree. `russh-sftp` is the natural fit; the one thing to
verify up front is its compatibility with the pinned russh version, since that
pairing has historically been version-sensitive. The alternative is parsing
`SSH_FXP_*` packets by hand over the channel stream — more code, no upside.

## The SSH session-channel surface (and why RPC migrates first)

SFTP doesn't land in isolation — it's the second tenant of the SSH
session-channel surface, and the first tenant is set up in a way that's worth
fixing on the way in.

**RPC today is positional, not named.** The client opens three plain session
channels in a fixed order — `control`, `rpc`, `events`
(`crates/kaijutsu-client/src/ssh.rs:235–248`) — and the server identifies the
RPC channel purely by **ordinal**: `channel_open_session` counts opens and only
channel index 1 gets the Cap'n Proto handler thread
(`crates/kaijutsu-server/src/ssh.rs:686`). There is no subsystem name, nothing
discoverable on the wire. Worse, `control` and `events` are **dead weight** —
`retain_ssh_channels` just stashes them in an `Arc` to hold them open
(`crates/kaijutsu-client/src/rpc.rs:101`); they're never read or written. The
real event stream flows as capnp *over the single RPC channel*
(`subscriptions.rs`). So two of the three channels exist only to pad the ordinal
scheme.

**The structural change SFTP needs is exactly the change RPC-named needs.** To
dispatch a channel by subsystem name, the server must *not* consume it via
`into_stream()` at open time (as it does now for index 1); it must **stash** the
`Channel<Msg>` in a per-connection `HashMap<ChannelId, Channel<Msg>>` and drain
it when the matching `*_request` arrives. That retention-and-dispatch scaffold
is shared by every named channel — RPC, SFTP, and a debug shell are all just
`match name` arms over it.

So the sequence is **RPC-named first**, because it builds and proves that
scaffold on the path we exercise constantly (regressions surface immediately),
after which SFTP is one more match arm plus the adapter — not a new architecture.

**RPC-named migration — sized (small/moderate, ~one focused session):**
- *Server (~40–60 lines, mostly relocation):* add the `HashMap<ChannelId,
  Channel<Msg>>` field; stash in `channel_open_session` instead of streaming
  index 1; delete the ordinal `channel_index == 1` logic; implement
  `subsystem_request` — on `"kaijutsu-rpc"` drain the channel, `into_stream()`,
  spawn the existing `run_rpc` thread block (`ssh.rs:715–745` moves here
  verbatim), and signal `session.channel_success(channel)`. Unknown name →
  `channel_failure`.
- *Client (~3 lines + a deletion):* after opening the one channel, call
  `rpc.request_subsystem(true, "kaijutsu-rpc")` before `into_stream()`
  (russh `channels/mod.rs:249`); delete `control`/`events`,
  `retain_ssh_channels`, and the two `SshChannels` fields (~30 lines of dead
  code gone).
- *Risks (minor):* relocating the `LocalSet`/thread spawn from open-time to
  subsystem-time is safe (`subsystem_request` fires after `channel_open_session`
  for the same channel); **must** signal `channel_success` or a `want_reply:
  true` client hangs; it's a **breaking client↔server wire change** — monorepo +
  early dev means a flag-day cutover, no compat shim. Verify the e2e live-eval
  harness reconnects.

The same surface later hosts a **debug kaish** (`shell_request` for interactive
`ssh kernel-host`, or a named `subsystem "kaish"`): retain the channel the same
way, wire its stream to an `EmbeddedKaish` bound to the authenticated principal
+ a context, capability-gated to operator/admin. Skip `pty_request` /
`window_change_request` for a line-oriented shell; add them for a full TTY. It
reuses the same binding decision SFTP raises (settle it once for both).

## Principal threading — the load-bearing decision

The handler already authenticates a `Principal`
(`crates/kaijutsu-server/src/ssh.rs:640`, the `identity` field set in
`auth_publickey`). The SFTP adapter must carry that exact `Principal` into every
`VfsOps` call so reads and writes act *as the authenticated user*, not as the
bare kernel identity. That much is straightforward — pass it into the adapter
struct the way `run_rpc` takes `principal`.

The subtlety: **capabilities in kaijutsu are bound to a *context loadout*, not
to a principal.** The rc-write gate is
`context_allows_rc_write(ctx: &ExecContext)`
(`crates/kaijutsu-kernel/src/file_tools/guard.rs:71`), which looks up
`get_context_binding(ctx.context_id)` and reads `binding.is_rc_write()`. A
`Principal` has `id`, `username`, `display_name`
(`crates/kaijutsu-types/src/principal.rs:16`) — and no loadout. So an SFTP
session authenticates a *who* but arrives without the *context* that the
existing capability machinery keys on. Plumbing the principal through is
necessary but not sufficient.

### How an SFTP session reaches a capability verdict

The two privileged write surfaces an SFTP write could hit are the same ones the
file tools gate today: `RcWrite` for `/etc/rc` and `ConfigWrite` for
`/etc/config` (`crates/kaijutsu-kernel/src/mcp/binding.rs:94`). Everything else
falls out of the mount's own `read_only()` flag, which the VFS already enforces.

We resolve the SFTP-session-to-binding question by giving each SFTP session a
**synthetic, per-principal loadout context** rather than borrowing some live
conversation context:

1. On `subsystem sftp`, the adapter creates (or reuses) a dedicated
   `ExecContext` whose `context_id` is derived deterministically from the
   principal — an `sftp:<principal-id>` context that exists only to carry a
   `ContextToolBinding`.
2. That binding's grants come from the principal's persisted authority set
   (the same SQLite-backed grants that gate `kj`), defaulting to **deny** for
   `RcWrite`/`ConfigWrite`. A human operator who already holds those grants
   keeps them over SFTP; a model principal that was never granted them cannot
   clobber a lifecycle script by sshfs-ing in.
3. Writes to `/etc/rc` / `/etc/config` route through the **same** guard the
   file tools use (`context_allows_rc_write`, and the `ConfigWrite` analogue),
   so there is exactly one enforcement point and SFTP cannot become a
   capability bypass.

This keeps the invariant that capabilities are an *ergonomic nudge in a
shared-trust kernel*, not a hard security wall — host `vim` and `kj rc` already
bypass these gates by design, and SFTP sits at the same trust level. The point
is that an SFTP write should be no *easier* a way to clobber a privileged file
than the file tools are, not that SFTP is a sandbox.

### Three options for the session-to-binding mapping

The Gemini review (below) pushed back on the synthetic context as a "parallel
capability system" and a static grant snapshot taken at connect time. Fair. The
options, ranked after that pass:

1. **Path-based context routing** *(new front-runner)*: don't bind the session
   at all. Expose contexts as directory prefixes — `/contexts/<id>/...` — and
   have the adapter extract `<id>` per call, bind the matching `ExecContext`,
   and strip the prefix before the `VfsOps` call. SFTP ops then reuse the
   **exact** capability path the file tools use — no shadow context, no stale
   snapshot, and one mount can span multiple contexts.

   *Caveat that keeps it from being a clean win:* the VFS tree is **global**,
   not per-context — only the *binding* is context-scoped. So
   `/contexts/<a>/etc/rc` and `/contexts/<b>/etc/rc` present the *same* files,
   differing only in which loadout gates a write. Defensible (the gate is the
   point), but it's a presentation oddity to settle before committing.

2. **Synthetic per-principal context** (the `sftp:<principal-id>` scheme above):
   simplest, but a parallel grant axis and a connect-time snapshot. Fallback if
   path-routing's global-tree framing confuses clients.

3. **Inherit a live context's loadout**: semantically clean but impractical —
   stock SFTP clients can't pass a context-id at connect time.

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
(now on `FileAttr` — see the prerequisite below) into `OpenFile` at `OPEN` and
re-verify it on every `WRITE`, failing the op if the underlying file was
replaced. Without this, SFTP is a corruption vector, not merely a coherence
question.

**Pipelining.** SFTP clients do *not* wait for a `WRITE` reply before sending
the next — they pipeline. Sequential processing throttles throughput to one
round-trip per block; concurrent processing forces interior mutability on the
handle map and hammers the CRDT cache with overlapping writes to one path. The
adapter has to choose deliberately, not fall into either by accident.

**Directory-handle leaks.** A cached `readdir` held across `READDIR` pages is a
memory-exhaustion vector if a client opens a dir, reads half, and never sends
`CLOSE`. Bound it (cap retained entries, or evict on session idle).

Beyond the per-handle stamp, concurrent-write coherence still leans on the
cache (mtime staleness + rollback-on-flush-failure) — which is exactly why the
mtime work below is a prerequisite, not an afterthought.

## Prerequisite (DONE): generation as the coherence primitive

SFTP makes the kernel's mtime semantics load-bearing in a way the in-app tools
never did, because caching clients (sshfs, editor indexers, `make`, `rsync`)
treat mtime as ground truth. mtime was **never** "a counter" — across backends
`FileAttr.mtime` is a real `SystemTime` — but the CRDT-backed `ConfigCrdtFs`
carried two footguns plus one latent coherence bug:

- Unwritten/seeded files reported `UNIX_EPOCH` (1970).
- `setattr(mtime)` was a silent no-op (`cp -p`/`tar -x`/rsync lost it).
- mtime was wall-clock `SystemTime::now()` on write, so two writes inside one
  clock tick (or a backward step) failed to advance it — and
  `FileDocumentCache`'s staleness reload relies on a strict advance.

**Resolved 2026-06-25** by adding a `generation: u64` field to `FileAttr`
(`crates/kaijutsu-kernel/src/vfs/types.rs`) — a strictly-advancing,
content-tied coherence stamp distinct from the (now display-only) wall-clock
mtime:

- `ConfigCrdtFs` and `MemoryBackend` source generation from a monotonic
  per-backend counter, bumped on every content mutation (write/create/truncate),
  so it advances even within one mtime tick. `LocalBackend` derives it from host
  mtime-nanos (advances with external edits, matches prior behaviour).
- `FileDocumentCache` now compares **generation**, not mtime
  (`file_tools/cache.rs` — `loaded_generation`, the `d > l` staleness check).
- `setattr(mtime)` is honored for display but deliberately does **not** bump
  generation, so a pure attribute touch never triggers a needless reload. The
  `UNIX_EPOCH` default is gone (seeds a real backend-creation timestamp).

This same `generation` is what an SFTP `OPEN` captures for the TOCTOU re-verify
above — so the handle guard and the cache now share one primitive.

## Security posture

- SFTP is reachable only after the existing pubkey auth succeeds; there is no
  new authentication surface.
- Capability binding (whichever option lands) means privileged-path writes honor
  the same deny-by-default grants as the rest of the system.
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
  build its search index — pulling every `/tmp` blob and CRDT doc over the
  channel, with potential to OOM the kernel or saturate the link. This needs
  **rate-limiting and traversal-depth/size limits** on the adapter; the
  "ergonomic nudge" framing does not cover it.
- Per-connection limits (the active-connection counter at
  `crates/kaijutsu-server/src/ssh.rs:650`) already apply, since SFTP rides the
  same connection accounting.

## Implementation slices

0. **RPC → named subsystem + channel-retention scaffold (~one session).** Build
   the `HashMap<ChannelId, Channel<Msg>>` retention + `subsystem_request`
   dispatch (see "The SSH session-channel surface" above), migrate RPC to
   `"kaijutsu-rpc"`, drop the dead `control`/`events` channels. This is the
   shared scaffold; doing it first de-risks everything below on a path we
   already exercise. Flag-day client↔server cutover.
1. **SFTP subsystem dispatch (~1 match arm).** With the scaffold in place,
   `subsystem_request` gains a `name == "sftp"` arm that spawns the SFTP task
   with the channel stream + the authenticated `Principal`. Reject if `identity`
   is `None`.
2. **Adapter (budget ~1,000+ lines, not 200–300).** The first estimate was
   naïve. Bridging onto `VfsOps` also means:
   - `SSH_FXP_REALPATH` and `.`/`..` canonicalization — and getting it right
     *without* string manipulation that lets a client escape the mount boundary
     (directory-traversal risk).
   - OpenSSH extensions stock clients depend on: `posix-rename@openssh.com`
     (plain `RENAME` fails if the target exists; sshfs needs overwrite),
     `statvfs@openssh.com` (sshfs checks free space and refuses to write if it's
     missing), `fsync@openssh.com`, `hardlink@openssh.com`.
   - Strict VFS-error → `SSH_FX_*` mapping (`NO_SUCH_FILE`, `PERMISSION_DENIED`,
     `EOF`, …); botching `EOF` on reads hangs clients.
   - The handle map, pipelining/backpressure, and `FileAttr`↔SFTP-attr
     conversions.
3. **Capability binding (~100–150 lines).** Resolve the chosen mapping (path-
   based routing preferred) to an `ExecContext` + binding, and route `/etc/rc` /
   `/etc/config` writes through the shared guard.
4. **Adapter-level limits.** Rate-limiting and traversal-depth/size caps to
   survive editor-indexer crawls; directory-handle eviction.
5. **Tests.** A live test that mounts the SFTP endpoint, reads a host file,
   writes a `/tmp` file, confirms a CRDT round-trip (`/etc/rc` write visible to
   `kj rc`/kaish), confirms an ungranted principal is denied `/etc/rc` writes,
   and exercises the rename-replace TOCTOU guard. Grow it per slice, the way the
   e2e live-eval harness does.

**Dependency order:** the mtime/generation rework is **done** (see the
prerequisite section). Next is slice 0 (RPC-named + retention scaffold), then
the SFTP arm + adapter + binding; limits and the TOCTOU guard are not optional
polish. A debug-kaish `shell`/`subsystem` is a later tenant of the same
scaffold.

## File references

- `crates/kaijutsu-kernel/src/vfs/ops.rs:20` — `VfsOps` trait
- `crates/kaijutsu-kernel/src/vfs/mount.rs` — `MountTable` routing
- `crates/kaijutsu-kernel/src/runtime/mount_backend.rs:293` — CRDT write-through
- `crates/kaijutsu-server/src/ssh.rs:632` — `ConnectionHandler` (subsystem gap)
- `crates/kaijutsu-server/src/ssh.rs:686` — ordinal `channel_index == 1` RPC selection (to be replaced)
- `crates/kaijutsu-server/src/ssh.rs:696` — RPC per-connection thread pattern
- `crates/kaijutsu-client/src/ssh.rs:235` — client opens control/rpc/events in order
- `crates/kaijutsu-client/src/rpc.rs:101` — `retain_ssh_channels` holds the dead control/events channels
- russh 0.61.1 `server/mod.rs:633` (`subsystem_request`) / `channels/mod.rs:249` (`request_subsystem`)
- `crates/kaijutsu-kernel/src/mcp/binding.rs:94` — `Capability` (`RcWrite`, `ConfigWrite`)
- `crates/kaijutsu-kernel/src/file_tools/guard.rs:71` — `context_allows_rc_write`
- `crates/kaijutsu-types/src/principal.rs:16` — `Principal`
- `crates/kaijutsu-kernel/src/runtime/config_crdt_fs.rs:199,230,258,605` — CRDT mtime (now-on-write, epoch default, setattr no-op)
- `crates/kaijutsu-kernel/src/vfs/backends/local.rs:141` / `memory.rs:451` — host mtime / `MemoryBackend` honoring `setattr(mtime)`

---

*Reviewed by Gemini Pro (gpal batch, 2026-06-25). Findings folded in: TOCTOU
handle hazard, sshfs caching vs. CRDT mtime, the adapter/effort underestimate
(REALPATH, OpenSSH extensions, error mapping, pipelining), editor-indexer crawl
DoS, and path-based context routing as the preferred binding option. Pushed back
on the host-FS RCE claim — root `/` is read-only, so it doesn't hold as stated.*
