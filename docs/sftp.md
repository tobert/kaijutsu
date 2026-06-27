# SFTP over the kaijutsu VFS

*Design note. Status: slices 0–2 + OpenSSH extensions + tracing **landed**
(2026-06-26; commit trail in `signoff.md`). The `generation` prerequisite is
landed. Slice 3 (capability binding) was **redesigned 2026-06-26** and its
substrate extracted to **`docs/slash-v.md`**: a read-only `/v/ctx` introspection
tree + a `/v/session` participant registry whose per-session `bound` context the
guard keys on. SFTP becomes a consumer (sets `bound` via a symlink). The old
"three options" are resolved (the arming symlink is option 3 made practical).*

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

## Context + session surfaces live in `docs/slash-v.md`

Slice 3's capability binding rests on two new `/v` VFS surfaces — **`/v/ctx`**
(context + block-log introspection) and **`/v/session`** (live participants and
each one's *bound context*). They are general, every-surface backends, not SFTP
details, so their design lives in **`docs/slash-v.md`** and can land **ahead** of
this work. SFTP is a *consumer*: it registers each connection as a session and
sets that session's `bound` via a symlink (see the arming subsection below).

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

### How an SFTP session reaches a capability verdict — the arming symlink

The verdict mechanism is defined in `docs/slash-v.md` (`/v/session` → "`bound` is
one field — the capability unification"). SFTP's part is small:

- **Register the connection** as a session (kind `sftp`, the server-stamped
  `Principal`) in the participant registry, so it appears under `/v/session` and
  `self` resolves to it.
- **Intercept `symlink`/`readlink` on `/v/session/self/bound`.**
  `ln -sf /v/ctx/by-id/<id> /v/session/self/bound` records the per-connection arm
  `armed: Option<(ContextId, Instant)>`; `readlink` returns it (or `ENOENT` when
  unset/expired). A post-connect op stock clients support — which is why the old
  "three options" collapse to one (it's option 3, "inherit a live context", made
  practical).
- **Route privileged writes** (`/etc/rc`→`RcWrite`, `/etc/config`→`ConfigWrite`,
  `crates/kaijutsu-kernel/src/mcp/binding.rs:94`) through the bound context to the
  **one** shared guard `context_allows_rc_write`
  (`crates/kaijutsu-kernel/src/file_tools/guard.rs:71`). Everything else falls out
  of the mount's `read_only()` flag. One enforcement point; SFTP can't become a
  capability bypass — and the gate now sits **below** symlink resolution, fixing
  the deny-altitude bug (the slice-2 lexical deny sat above it).

**Per-connection sliding TTL (~15m), default-deny, fail-loud.** The arm lapses on
idle so an idle sshfs mount disarms (a *safe-operation* constraint, not security);
until armed, a session can browse `/v/ctx` read-only and set its `bound` symlink,
nothing else on the privileged paths — no "most-recent-context" guess. The
`SSH_FXP_STATUS` deny names the remedy:

```
SSH_FX_PERMISSION_DENIED
"session not attached; `ln -s /v/ctx/by-id/<id> /v/session/self/bound` to arm writes (expires in 15m)"
```

This keeps capabilities an *ergonomic nudge in a shared-trust kernel* — host
`vim`/`kj rc` bypass these gates by design and SFTP sits at the same trust level;
an SFTP write is no *easier* a way to clobber a privileged file than the file
tools, not a sandbox.

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
3. **Capability binding (consumer of `/v/session`).** Prereq: the `/v/ctx` +
   `/v/session` backends land first — their own work, in `docs/slash-v.md`
   (slices V1–V3), with no SFTP dependency. SFTP's part: register each connection
   as a session, intercept `symlink`/`readlink` on `/v/session/self/bound` to set
   the per-connection arm (`armed: Option<(ContextId, Instant)>`, sliding TTL),
   route privileged writes through the bound context to the shared guard, and
   deny-with-message otherwise. Replaces the slice-2 `/etc/rc` fail-loud stopgap.
4. **Adapter-level limits.** Rate-limiting and traversal-depth/size caps to
   survive editor-indexer crawls (the `/v/ctx` tree makes this sharper);
   directory-handle eviction.
5. **Tests.** A live test that mounts the SFTP endpoint, reads a host file,
   writes a `/tmp` file, confirms a CRDT round-trip (`/etc/rc` write visible to
   `kj rc`/kaish), confirms an ungranted principal is denied `/etc/rc` writes,
   and exercises the rename-replace TOCTOU guard. Grow it per slice, the way the
   e2e live-eval harness does.

**Dependency order:** slices 0–2 + extensions + tracing are **done**. The `/v`
surfaces (`docs/slash-v.md`, V1–V3) are next and can land ahead — they're
self-contained read-only-then-writable backends with no SFTP dependency. SFTP
slice 3 then consumes `/v/session`; limits (4) and the TOCTOU guard are not
optional polish. A debug-kaish `shell`/`subsystem` is a later tenant of the same
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
DoS, and path-based context routing as a candidate binding option. Pushed back
on the host-FS RCE claim — root `/` is read-only, so it doesn't hold as stated.*

*Binding superseded 2026-06-26 (design session with Amy): the three options
collapse to a per-connection arming symlink (`/v/session/self/bound`) over the
`/v/ctx` + `/v/session` surfaces — option 3 made practical, no parallel grant
axis, one guard. Full design extracted to `docs/slash-v.md`; SFTP is a consumer.*
