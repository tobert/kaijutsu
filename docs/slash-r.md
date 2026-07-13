# The `/r` remote-mount namespace: client shares over reverse SFTP

*Design note. Proposed 2026-07-13 (Amy + Claude co-design session); reviewed
pre-build by DeepSeek + Gemini Pro (provenance footers). **Slices 0+1 +
stitch SHIPPED same day** (merges `ad4b212e` pump, `99d4e5cd` share — two
Sonnet-subagent worktree lanes + a post-build deepseek review round; sections
below updated to the as-built reality, deviations marked). Not yet
live-verified on a real kernel; remaining slices (kj share verbs, `:rw`,
notify) tracked in `docs/issues.md`. Companions: `docs/sftp.md` (the landed
forward direction), `docs/slash-v.md` (the `/v` surfaces + the index-TSV
house style this reuses), `docs/mounts.md` (the opaque-host inversion `/r`
sits beside).*

A client shares a slice of its local filesystem into the kernel's VFS —
the exact reverse of the SFTP we already ship. `kaijutsu-app --share ~/Downloads`
and the directory appears under `/r/<client>/downloads` on the kernel mount
tree, reachable from kaish, the file tools, MCP, the FSN world, and (forwarded)
other clients. The gesture is `code .` for the instrument: point the app at a
directory and that directory joins the session, the way a MIDI device joins
the patch graph.

Sharing is **explicit, client-initiated, per-invocation**. No CLI arg, no
share. Reconnect re-offers automatically because the arg is still there.

## Decisions (2026-07-13, from the design conversation)

- **Heavy IO stays off capnp; file data rides SFTP.** The rule is not
  control-plane purity — capnp carries control verbs and light metadata fine —
  but bulk bytes go where a mature subsystem already exists (Amy, 2026-07-13).
  No file bytes ever cross the capnp channel or an MCP tool result (unless a
  caller deliberately `read`s content into a conversation). This is also why
  the share manifest riding the SFTP session is unremarkable: it's small
  metadata on the data plane, chosen for the pairing win, not a rule bend.
- **Reverse SFTP, not a capnp filesystem capability.** Same wire protocol as
  the forward direction, roles swapped. Chosen over a capnp `FileSystem`
  capability because SFTP futures are `Send` and ride the ambient runtime,
  while capnp is `!Send` and pinned to a dedicated thread — a remote mount's
  `VfsOps` calls come from anywhere in the kernel's multithreaded runtime, and
  a capnp-backed backend would pay the LocalSet-hop tax on every hot
  filesystem op. Both halves of `russh-sftp` (2.3) are already workspace deps.
- **`/r/<client-id>/<share-name>` namespace**, sibling of `/v`. Over time all
  special virtual filesystems consolidate under `/v`; remote mounts get `/r`.
- **One channel per client, N shares.** The client opens a single
  `kaijutsu-share` subsystem channel and serves all its shares as top-level
  directories of that one reverse-SFTP session.
- **Read-only by default; `:rw` is per-share opt-in.**
- **Disconnect = unmount, loud.** Shares vanish from `readdir` the moment the
  channel drops; in-flight ops fail with a distinct error. No stale mounts, no
  silent fallback.
- **The streaming-copy primitive lands first** (slice 0) — `cp` across
  mounts, CAS ingest, and share sync all sit on it.

## Why "kaijutsu-share" and not the `sftp` subsystem

On the wire the two are identical — plain SFTP packets. The differences are
pure negotiation:

- SSH subsystem requests only travel client→server, so the client must open
  the channel no matter what. The trick is a **role swap**: after the
  subsystem request, the client speaks the SFTP *server* role and the kernel
  speaks the *client* role on the same channel.
- The name `sftp` is taken with the opposite meaning: the kernel's existing
  handler (`subsystem_request` dispatch, `crates/kaijutsu-server/src/ssh.rs:764`,
  match at `:802`) serves the kernel VFS on it. `kaijutsu-share` is one more
  match arm on the landed named-subsystem scaffold and a new constant beside
  `SSH_RPC_SUBSYSTEM`/`SSH_SFTP_SUBSYSTEM` (`crates/kaijutsu-types/src/lib.rs:69`).

## Session shape: self-describing, manifest-in-band

The client dials its own SSH connection (the pattern `SftpClient` set:
`connect_subsystem`, `crates/kaijutsu-client/src/ssh.rs:210` — same keys, same
server, no new auth surface), requests `kaijutsu-share`, and serves:

```
/                       # the reverse-SFTP session root (client-synthesized)
├── index               # manifest TSV: name  rw  client-id  nick
├── downloads/          # each offered share, rooted+jailed
└── src/
```

The kernel, on the subsystem request, runs an `SftpSession` (client role) over
the channel, reads `/index`, and registers the shares. The manifest riding the
data plane makes the session **self-describing** — no token correlation
between a capnp call and a separately-dialed SSH connection, no pairing state
machine. This is the slash-v ethos ("the index is the resolver") applied to
negotiation. Identity is not client-asserted where it matters: the
*authenticated principal* comes from SSH auth on the share connection itself;
`client-id` (the stable installation id, `client_id.rs::load_or_seed`) is
**namespace, not authority** (gemini flagged the spoof: a manifest could
claim another machine's id). It has to be manifest-supplied — SSH auth
yields a principal, not an installation, and nothing kernel-side knows the
id first — so the guards are: registry rows always carry the server-stamped
principal beside the claimed id; a claim of a client-id **already live**
from a different connection is rejected loudly (no silent rebind of
`/r/<id>` mid-session); and claiming an id grants nothing — the share server
only ever serves its own jail. Cross-*reconnect* squatting remains possible
and is shared-trust crosstalk, not an enforced boundary; a durable
principal↔client-id pin via the `/etc/client` namespace is the escalation
if it ever bites in practice.

Control operations stay capnp/kj: `kj share ls` (roster), a future
`kj share eject <id>/<name>` (kernel-side force-detach). The rejected
alternative — capnp `offerShares() -> token` with the token smuggled in-band
before SFTP `INIT` — adds a handshake frame and ordering rules for zero gain.

## Kernel side: one backend under the frozen mount table

The kernel `MountTable` **freezes after bootstrap** (`MountTable::freeze()`,
`crates/kaijutsu-kernel/src/vfs/mount.rs:119`) — per-share mounts are
impossible and undesirable. So `/r` is **one `ShareFs` backend**, mounted in
the server-bootstrap sequence before the freeze (like `/v/cas`), routing
internally:

- `readdir /r` → live client ids (from the share registry; empty when nobody
  shares — the dir always exists).
- `readdir /r/<id>` → that client's share names.
- `/r/<id>/<share>/...` → translated to `/<share>/...` on that client's
  reverse-SFTP session.
- `/r/index` → registry TSV: `client  nick  share  rw  attached  path`
  (slash-v index style; `kj share ls` renders the same rows).

The registry is session-scoped and in-memory, `/v/session`-flavored: rows
appear on channel-up, vanish on channel-down. Nothing durable — the client's
CLI arg is the durable intent. On disconnect, unregistration walks the
routing table, removes the rows, then drops the `SftpSession` — the drop
cancels its pending futures, so **in-flight ops fail** with a dedicated
error (`VfsError` variant, mapped loudly everywhere it surfaces) — a
half-written file plus a vanished mount beats a hung `cp`. Unregistration
is token-guarded so a stale session's cleanup can never evict a fresh
reconnect's registration. **As built, one addition** (deepseek post-build
review): a clean channel close is observed immediately, but a silently-dead
peer (partition, suspended laptop — no FIN) generates no traffic to trip
on, so the session task runs a **keepalive** — every
`SHARE_KEEPALIVE_INTERVAL` (15 s), one cheap timed `LSTAT /index`, raced
against the close signal; a failed ping evicts the session. "Vanish the
moment the channel drops" is honest for idle and wedged peers alike.

Backend details that are decisions, not chores:

- **The session is a serialization point** (deepseek review, 2026-07-13).
  `russh_sftp::client::SftpSession` is not internally concurrent, and
  `ShareFs` ops arrive from anywhere in the kernel's multithreaded runtime —
  so each client's session sits behind a per-client lock (or a bounded
  request queue). Reads/writes to one client's shares are serialized; that's
  also the natural throttle. A bounded in-flight cap + queue is the shape if
  serial latency stacks up; multiple channels per client is the bigger hammer
  we don't reach for yet.
- **Every remote op gets a timeout.** `SftpSession` has no built-in timeouts;
  a hung laptop must not park kernel tasks forever. `tokio::time::timeout`
  around each wire op, mapped to the share error.
- **`read_all` override** — the default sizes from `lstat` and truncates
  followed symlinks (the standing gotcha); `ShareFs` overrides it to loop
  `read` to EOF, exactly like the client CAS fetcher does past the 256 KiB
  SFTP `READ` cap.
- **Coherence stamp: a vendor extension, because we own both ends**
  (gemini review, superseding the first two plans — remote-mtime was weak,
  a kernel-side write counter was blind to client-local edits). SFTP v3
  attrs carry mtime as **u32 seconds**, useless as a generation, and
  `FileDocumentCache` staleness needs a *strictly advancing* stamp. Kaijutsu
  ships both halves of this protocol, so the client serves
  `kaijutsu-generation@kaijutsu.dev`, carrying host mtime-**nanos** —
  exactly `LocalBackend`'s own generation rule, now crossing the wire.
  Strictly monotonic on day one, covering client-local edits at stat time.
  **As built (deviation from the ATTRS plan):** russh-sftp 2.3's
  `FileAttributes` has no extended-attribute slot (its `Serialize` says
  `// todo`), so the stamp rides a sibling `SSH_FXP_EXTENDED`
  request/reply (`kaijutsu-types/src/share.rs` — batched `paths: Vec` on
  the wire, though `getattr` currently sends one path per call: a known
  perf follow-up, 2 RTTs per stat). The **required** check moved to a
  better home than per-file attrs: the client advertises the extension in
  its SFTP `INIT` version reply, and registration refuses a session
  without it, loudly, as version skew (monorepo flag-day culture, no
  compat shim). Remote mtime stays display-only. The notify slice then
  becomes about *push* — invalidation without polling, FSN heat — not
  about stat correctness.
- **Writable shares enforce at both ends**: the kernel checks the registry
  `rw` flag before issuing a mutating op (fail fast, good errors), and the
  client's share server rejects mutations on an ro share regardless (its
  disk, its final say). Until notify lands, `:rw` is honest-labelled: the
  TOCTOU guard covers kernel-vs-kernel replacement, not a concurrent local
  editor on the client.
- **Attrs are translated, not passed through**: remote uid/gid are squashed
  to the authenticated principal (a laptop's numeric ids are meaningless
  kernel-side and would confuse anything that reads them), and mode bits are
  synthesized from the share's ro/rw state rather than trusted from the
  remote stat.

## The crawl boundary: `/r` is never swept

The forward-SFTP doc flagged editor-indexer crawls as the real risk of
mounting kernel state into an IDE. Reversed, the risk reverses: the **kernel's
own ambient machinery** must not crawl a client's disk over the network.
`MountTable::snapshot` (the FSN backdrop walk), the semantic index, and any
future project crawler **skip `/r` by construction** — a backend-level
`opaque-to-sweeps` flag, not a path blocklist. Slash-v principle 6 ("don't
make the front door the firehose") applies with teeth: every `readdir` here
is a network round trip to somebody's laptop.

The FSN world still gets its ambient reading for free: kernel-*mediated* ops
on `/r` paths bump the activity digests like any other mount, so a share
district lights up when the session actually touches it — which is the honest
signal anyway.

## Client side: a jailed, trivial file server

The share server is the easy half — real files, no VFS underneath: a
`russh_sftp::server::Handler` over `tokio::fs`, plus the manifest synthesizer.
The jail is the one part that must be right, because the client machine is
**outside the kernel's shared-trust unix boundary** — sharing `~/Downloads`
must not leak `~/.ssh`:

- Canonicalize the share root once at startup (symlinks resolved; the root is
  what it resolves to).
- Serve by `lstat`; follow a symlink only if its **resolved** target stays
  under the root. A link pointing out of the jail lists as a symlink but
  refuses to open — visible, not followed. Resolve targets with a real
  `canonicalize`, not string prefixing — case-insensitive filesystems make
  string checks lie (deepseek review).
- **Refuse special files.** Devices, FIFOs, and sockets refuse to open — a
  FIFO served over the wire blocks a kernel task indefinitely. Regular files,
  dirs, and in-jail symlinks only.
- `..` and absolute paths normalize inside the jail (the landed kernel
  adapter's REALPATH discipline, reused in miniature).
- **Close the lstat→open race where the OS can**: on Linux, open through
  `openat2` with `RESOLVE_BENEATH` (gemini review) so the kernel enforces
  the jail atomically — no window between check and open; `O_NOFOLLOW`
  discipline as the portable fallback. Residual where unsupported: a local
  process swapping a symlink mid-serve — a hostile local actor on the
  client's own box, outside this design's threat model. Same verdict for
  hardlinks (out-of-jail *content* under an in-jail name): physically
  indistinguishable from a regular file, documented, accepted.

Sizing honesty: the forward adapter came in at ~1,300 lines against a
200–300 estimate. The reverse handler is smaller in kind — real files, no
CRDT write-through, and the kernel is the **only** client, so the
sshfs-appeasement extensions (`posix-rename@`, `statvfs@`, …) are simply not
implemented — but it is still a real piece of code, not an afternoon.

## Slice 0 — the streaming pump (built first, everything sits on it)

kaish's `cp` slurps whole files — `backend.read(src, None)` then write
(kaish-kernel 0.12 `tools/builtin/cp.rs:202`). Fine for configs; wrong for a
4 GB ISO crossing two network channels through kernel memory. `VfsOps` already
has `read(path, offset, size)` / `write(path, offset, data)`
(`crates/kaijutsu-kernel/src/vfs/ops.rs:20`), so the primitive is a chunked
pump in `kaijutsu-kernel/src/vfs/` :

- **The pump rides a streaming read primitive, not raw `read` calls**
  (gemini review — the RTT-amplification catch). `VfsOps::read` is stateless
  per call; SFTP is stateful (`OPEN`/`READ`/`CLOSE`). A pump looping bare
  `read(path, offset, size)` against `ShareFs` would open and close an SFTP
  handle *per 256 KiB chunk* — three round trips each, ~1.7 MB/s at 50 ms
  latency, the 4 GB ISO case dead on arrival. So slice 0 adds a streaming
  primitive to `VfsOps` (`open_read_stream(path)` returning a chunk stream):
  the **default impl** loops the backend's own `read` (free and correct for
  local/memory/CRDT backends), and `ShareFs` **overrides** it to hold one
  SFTP handle open, pipeline `READ`s, and close on drop. The pump consumes
  the stream; sinks stay generic: write-at-offset to another `VfsOps` path
  (`cp`), a hashing CAS stage (`cas put`), or both. 256 KiB chunks match the
  SFTP `READ` window both adapters already use.
- **Read contract, pinned** (deepseek review): a zero-length `read` return is
  EOF, stop; a *short* read before EOF is legal — advance the offset by the
  returned length, never assume the full chunk arrived. Both are test cases,
  not comments.
- **No mid-pump source consistency.** A source mutated during the pump yields
  a spliced copy — documented, not defended. The CAS sink is the honest path
  when it matters (the hash covers exactly the bytes streamed); plain `cp`
  makes no promise a local `cp` doesn't make.
- **CAS ingest streams**: `FileStore` gains a streaming store — hash
  incrementally while writing into `staging/`, atomic rename into `objects/`
  at close; the streaming writer *is* a sink and exposes the final
  `ContentHash` at close (never a re-hash pass after the write). Then
  `kj cas put /r/<id>/downloads/render.wav` pulls client→kernel→CAS in one
  pass with bounded memory, and the client's existing XDG cache
  (`CasResolver`) resolves the hash back to its local copy — the bytes make
  one trip.
- **Consumers**: a `kj cp` (kernel builtin, ours) uses it immediately;
  kaish's `cp` keeps its slurp until a kaish release picks the primitive up
  (upstream candidate, noted in issues).
- **Interruption is loud**: a mid-pump disconnect surfaces the share error and
  reports bytes-written; no partial file is silently blessed. CAS staging
  discards on error by construction (nothing renames without a verified
  close) — and *actively*: the streaming writer's drop path unlinks its
  partial `staging/` file rather than leaving garbage for a restart sweep
  (gemini review).

This is TDD-friendly in isolation: two `MemoryBackend`s and a fault-injecting
wrapper cover chunking, short reads, sink errors, and the interruption
contract before any network exists.

## What this enables (the MCP seat)

Both machines are just paths in one tree. From kaish/MCP:

```
cp /r/3f2a/downloads/demo.wav /r/9c81/inbox/     # client A → kernel → client B
kj cas put /r/3f2a/src/render.wav                 # share → CAS, hashed in flight
```

capnp and the MCP channel carry only commands and results; a model moving
files between two laptops never holds the bytes in context. This is the
midi/audio-routing posture applied to files: the kernel is the patch bay,
shares are cables the clients plug in.

## Implementation slices

0. **Streaming pump + streaming CAS store + the `VfsOps` stream primitive.**
   ✅ **SHIPPED 2026-07-13** (`ad4b212e`): `open_read_stream` + loop-`read`
   default + `MountTable` delegation, `vfs/pump.rs` (`PumpSink`/`VfsSink`/
   `CasSink`), `StreamingWriter` (drop-unlinks-staging), `kj cp`, `kj cas
   put` streams; fault-injection tests.
1. **Reverse-SFTP share, read-only.** ✅ **SHIPPED 2026-07-13** (`99d4e5cd`,
   including the stitch: `ShareFs::open_read_stream` holds ONE remote handle
   per transfer with per-chunk lock scoping + a drop-guard `CLOSE`; proven
   by a counting harness). All of: subsystem constant + arm, jailed client
   share server + manifest + generation extension, repeatable `--share
   [name=]path`, `ShareFs` pre-freeze with registry/timeouts/keepalive,
   `/r/index`, `read_all` override, attr squashing, crawl opacity.
   **Not yet live-verified** on a real kernel — kaish `ls /r`, `cp` out of a
   share, and the kaish shadow-overlay papercut check (which bit `/v/cas`)
   are the outstanding verification steps.
2. **`kj share` verbs + polish.** `kj share ls` (registry TSV), eject;
   `/v/session` rows gain the share channel kind.
3. **Writable shares.** `:rw` suffix, both-ends enforcement, TOCTOU stance
   documented against the weak generation stamp.
4. **Notify push.** Client-side `notify` watcher batches change events up the
   control plane; kernel bumps generations + activity digests so FSN heat
   shows client-local edits and caches invalidate without polling.

## Open questions

- **Share-name collisions** within one client (`--share a/x --share b/x`) —
  reject at CLI parse (make the second name explicit) is the lean answer.
- **Headless sharer.** A tiny `kaijutsu-client`-based binary that *only*
  serves shares (no Bevy) would make "join a directory from any box" one
  command. Worth it the first time we want a share from a machine with no GPU.
- **Large-directory `readdir` over the wire** — the kernel adapter pages at
  64 entries; the client server should too. Do we need a depth/size budget on
  `/r` per-op beyond crawl opacity?
- **`kj cp` progress** — the pump knows bytes/total; where does progress
  surface (trace block? kj output stream)?

## File references

- `crates/kaijutsu-kernel/src/vfs/ops.rs:20` — `VfsOps` (the pump's substrate)
- `crates/kaijutsu-kernel/src/vfs/mount.rs:119,134` — `MountTable::freeze` / `mount` (why `/r` is one backend)
- `crates/kaijutsu-server/src/ssh.rs:764,802` — named-subsystem dispatch (the new match arm)
- `crates/kaijutsu-server/src/sftp.rs` — the forward adapter (jail/REALPATH discipline, 256 KiB `MAX_READ_LEN`, 64-entry `READDIR_CHUNK` to mirror)
- `crates/kaijutsu-client/src/ssh.rs:210` — `connect_subsystem` (own-connection pattern)
- `crates/kaijutsu-client/src/sftp.rs` — `SftpClient` (read-to-EOF loop the `ShareFs` `read_all` override mirrors)
- `crates/kaijutsu-types/src/lib.rs:69,76` — subsystem name constants (gains `SSH_SHARE_SUBSYSTEM`)
- `crates/kaijutsu-app/src/connection/client_id.rs:45` — the stable installation id naming `/r/<id>`
- `crates/kaijutsu-cas/src/store.rs` — `FileStore` staging+rename (gains the streaming store)
- kaish-kernel 0.12 `tools/builtin/cp.rs:202` — the whole-file slurp the pump replaces (upstream candidate)
- `Cargo.toml` — `russh-sftp = "2.3"` (client + server halves of one crate, both directions covered)

---

*Reviewed by DeepSeek (kaibo oneshot, whole docs + code attached, 2026-07-13).
Findings folded in: per-client session serialization + bounded in-flight cap,
per-op timeouts, generation flipped to a kernel-side write counter (mtime
demoted to display), special-file refusal + canonicalized symlink targets in
the jail, the pump's EOF/short-read contract and no-mid-pump-consistency
note, the hash-at-close sink shape, and the handler sizing honesty. Confirmed
sound: the role swap and INIT direction (kernel initiates the handshake, no
deadlock), manifest-in-band over a capnp token, one pre-freeze backend, and
the client-id-is-cosmetic trust posture (SSH auth is the identity).*

*Reviewed by Gemini Pro (kaibo batch, same attached surface, 2026-07-13).
Findings folded in: the **RTT-amplification catch** (stateless `VfsOps::read`
= OPEN/READ/CLOSE per chunk over SFTP → the `open_read_stream` primitive with
loop-`read` default + held-handle override, now part of slice 0), the
`kaijutsu-generation@` ATTRS extension (we own both protocol ends — strictly
monotonic nanosecond generations on day one, superseding both the mtime and
kernel-counter plans), client-id spoof guard (live-duplicate rejection +
principal-stamped rows; resolved namespace-not-authority rather than gemini's
derive-from-connection, since SSH auth yields a principal, not an
installation id), `openat2`/`RESOLVE_BENEATH` jail atomicity, active staging
cleanup on drop, and uid/gid/mode squashing. Independently confirmed
deepseek's timeouts, special-file refusal, and short-read findings, and the
manifest-in-band choice ("correct architectural move").*
