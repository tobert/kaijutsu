# The `/v` virtual filesystem: `/v/cas`, `/v/ctx`, `/v/session`

*Design note. Proposed 2026-06-26 (extracted from `docs/sftp.md`); redesigned
2026-06-27 with Amy and simplified hard — the per-session `bound` capability
apparatus is gone (see "Capability"), navigation is a TSV `index` rather than human
symlink farms, and the canonical pools are sharded. **Track B (`/v/cas` + client
CAS sync) shipped, live-verified 2026-07-02**; its `index` TSV (B2) was deliberately
deferred by Amy — see track B. Track V (`/v/ctx` + `/v/session`) remains the active
unbuilt plan. Compressed 2026-07-04: `docs/devlog.md` carries the track-B story, git
history keeps every cut line.*

`/v` is kaijutsu's virtual / CRDT namespace. This note covers three **sysfs-style**
surfaces under it:

- **`/v/cas`** — the kernel's CAS object pool, rendered as immutable files
  (**shipped** — the substrate for client CAS sync).
- **`/v/ctx`** — every context and its CRDT block log, rendered as files (planned).
- **`/v/session`** — the live participants (app, MCP, SFTP) and, read-only, the
  context each one is currently acting as (planned).

Because all three are ordinary `VfsBackend`s on the **kernel `MountTable`**, the same
trees are reachable from the Bevy app, kaish, the file tools, **and** SFTP — build
once, every surface gets the view. This is the "instrument you play" stance made
literal: `grep`, `less`, `ls -l` over live kernel state.

**The mount-table reality.** Two layers exist and they are not the same. `/v/docs`
and `/v/input` are **kaish-side** mounts (`embedded_kaish.rs:300` — objects on each
materialized kaish's own VFS), *not* kernel-`MountTable` backends, and therefore not
visible over SFTP. The surfaces in this doc mount on the **kernel `MountTable`**
(like `/etc/rc`'s `ConfigCrdtFs` and the shipped `/v/cas` `CasFs`), the table every
surface reaches: SFTP serves it directly (`SftpSession::new(principal, vfs)`), kaish
through `MountBackend` longest-prefix routing. Note for implementers: the kernel
mount table **freezes** after setup (`MountTable::freeze()`) — new mounts must land
in the server-bootstrap sequence, before the freeze.

## Orientation: script-first, three clean roles

The earlier draft leaned on human-typeable symlink farms (`live/<label>`,
`by-type/<type>`, `by-time/NNNN`, `by-lineage/`). They're cute but they don't scale,
and they bias the surface toward a human at a prompt. This surface is mostly for
**scripts and expert debugging**, so navigation collapses into three roles that never
overlap:

1. **`index` — the resolver.** One greppable TSV table per collection, carrying
   `label`, `type`, and a `path` column: a caller does
   `grep feature-x /v/ctx/index`, reads `path`, and `cd`s there. Nobody computes a
   shard path, nobody walks a farm. The index *is* `by-type` (a column), `live` (a
   column), and `by-time` (row order).
2. **Sharded pools — pure storage.** `/v/ctx/<shard>/<full-id>/`. You reach a dir by
   resolving a known id (from the index, or from an edge), never by browsing.
3. **Symlink edges — pure graph.** `parent`, `head`, `children/`, and a tool-result's
   `call ->` stay as real symlinks (the kobject model — relationships are links).
   Edge targets carry the full shard path, so traversal never makes a caller compute
   a shard; only the *initial* label lookup touches the index.

## Why this is its own doc, independent of SFTP

These are general surfaces with value far beyond SFTP — `awk` over a `blocks/index`,
`ls -l /v/session`, a context browser reading `index`, a clip sink pulling a sample —
so they get built and tested on their own (via `kaish ls /v/ctx`, zero SFTP
involved). SFTP is a **read/view consumer**: it serves the same kernel mount table,
so each tree becomes remotely browsable the moment it mounts — not the capability
driver it was in the first draft (see "Capability").

## Design principles (lessons from `/proc` and `/sysfs`)

1. **One value per file** (sysfs's cardinal rule). The anti-pattern is
   `/proc/<pid>/stat`: 50 positional fields nobody can extend safely. Scalars are
   their own files (`role`, `kind`, `status`), greppable line by line.
2. **…but ship aggregates**, because N tiny files is a `stat`-storm over a network
   FS. Two tiers: each object offers a `json` file (one read, whole object), each
   collection a TSV `index` (one read, whole roster, ordered). Humans use the scalar
   files; scripts read `index`/`json`. Aggregates are also how we evolve without
   breaking ABI — new fields land in `json`, new columns append to `index`.
3. **Directories are objects, edges are symlinks** (the kobject model). Contexts and
   blocks are dirs; `parent`, `head`, `children`, and a tool-result's `call` are
   symlinks. We **do not** ship symlink *farms* for naming convenience — `live`,
   `by-type`, `by-lineage` are index columns, not directory trees.
4. **Stable identity, separate from ordering and views.** `/proc`'s pid-reuse footgun
   (a stale path silently addressing a different process) is our timeline-ordinal
   instability. Identity is the stable `BlockId` key (the shard path); ordering is a
   *view*, living in `blocks/index` row order — not a `by-time/NNNN` farm whose
   ordinals shift under every insert.
5. **`self`** — `/proc/self`. A per-caller magic name resolved by whichever surface
   the caller arrived on. Introspection only now (it no longer gates writes).
6. **Don't make the front door the firehose.** You never `ls -R /sys`. Living under
   `/v` (a deliberate destination a project crawler never wanders into) is what lets
   `/v/ctx` hold *all* contexts safely.
7. **Resist the `/proc` junk-drawer.** `/v/ctx` is *only* the context/block model;
   `/v/session` is *only* live participants. Config stays at `/etc/config`.
8. **The ABI is forever.** Conservative names, additive-only; `json` absorbs new
   object fields and `index` appends columns (never reorders).
9. **Hot vs cold reads.** A streaming block is "hot" — its `content` grows and
   `generation` advances, invalidating a caching reader every poll (we *want* that).
   `status` makes hot/cold legible so an indexer can skip `running` blocks.
10. **Text, line-oriented, greppable** — the whole proc/sysfs ethos, and the payoff
    for the instrument: `awk -F'\t' '$6==1' /v/ctx/*/*/blocks/index` lists every
    excluded block (the `excl` column is `0`/`1`), one read per context.

## `/v/cas` — the CAS pool (read-only) + client sync — track B, SHIPPED

*Renamed from `/v/blobs` to `/v/cas` on 2026-07-06 for naming consistency with the
rest of the CAS surface (the crate, the store, `kj cas`) — no design change, same
`CasFs` backend.*

*Landed and live-verified 2026-07-02: `kj cas put` → SFTP fetch → hash-verified XDG
cache → speakers. The driver was clips — a sink resolves a clip's `media` hash
locally and pulls misses from the kernel under the prepare horizon (`docs/pcm.md`,
`docs/clips.md`). No new RPC. The slice-by-slice execution plan (B0–B4) lives in git
history; `docs/devlog.md` tells the story. The landed design:*

**`CasFs`** (`crates/kaijutsu-kernel/src/vfs/backends/cas.rs`) — a read-only
`VfsBackend` over the kernel's `Arc<FileStore>` (`kaijutsu-cas`: 128-bit BLAKE3 as 32
hex chars, disk layout `objects/<2-hex>/<30-hex>` plus `metadata/` and `staging/`),
mounted at `/v/cas` in the server bootstrap before the freeze (`rpc.rs:1167`).
Every mutating op returns `EROFS` from the backend itself — read-only by
construction, not by mount flag. It shards on the **leading two hex chars**, matching
the on-disk `objects/` layout one-to-one — deliberately **not** the UUIDv7
trailing-byte rule (see Sharding): BLAKE3 is uniform in *every* byte, v7's leading
bytes are a clock. The leaf name is the **full 32-hex hash** (self-describing; a
shard/prefix mismatch is `ENOENT`; shard dirs render only when they exist on disk).
Immutability makes the hard problems easy: `getattr` size from host-file metadata
(O(1)), `generation` a nonzero **constant** (a hash names one byte string forever —
caching clients never invalidate), reads plain passthrough, no symlinks.
`real_path()` returns `None` (the host path would bypass the read-only abstraction);
`staging/` and `metadata/` are not exposed (the junk-drawer rule).

**Client resolver** (`crates/kaijutsu-client/src/sftp.rs` — `SftpClient` +
`BlobFetch` + `BlobResolver`): the standard `"sftp"` subsystem **on its own SSH
connection** (`connect_subsystem`, client `ssh.rs:210` — same keys, same server, no
new auth surface). Separate by design: the capnp RPC world is `!Send` and pinned to
its dedicated thread, while SFTP futures are `Send` and ride the ambient runtime —
fetching an object never touches the RPC actor's `spawn_local` world. The cache is a
`kaijutsu_cas::FileStore` at the XDG cache dir (the kernel store's crate —
layout/hashing/dedup for free), hardened for the multi-process cache: `store()`
writes into `staging/` and atomically renames into `objects/`, and `ContentHash`
deserialization is validated (`try_from` → `from_str_checked`). **Verify on fetch,
fail loud:** pulled bytes are re-hashed; mismatch = hard `HashMismatch`, nothing
cached — crash over corruption. Reads loop offsets to EOF past the 256 KiB per-packet
SFTP `READ` cap, so truncation can't slip past the hash. **Single-flight per hash** —
concurrent resolves coalesce onto one wire transfer, the lock entry pruned even if a
fetch panics. Scheduling stays with the consumer (`docs/pcm.md`): the app sink
(`kaijutsu-app/src/audio.rs`) resolves `CuePayload::Cas` cues off a dedicated tokio
runtime, fetch-on-cue first; prepare-horizon prefetch and the clip-record path are
the follow-ups.

**Ingest** stays `kj cas put <path>` (remote: SFTP-upload to `/tmp`, then put).
Writable staging over SFTP is deferred until a real need — hash-at-close, mime
sniffing, and abandoned-upload cleanup hide in there; none blocks the read path.

**B2 — the `index` TSV: designed, DEFERRED on purpose (2026-07-02, Amy).** The
resolver file (`hash  mime  size  path`, absolute paths, mime joined from
`metadata/`) has **no consumer** — the client resolver addresses objects by exact
hash — and the first-cut shape (an O(N) walk of `objects/` on every read, no cache)
is under-designed: shipping it commits an ABI we'd outgrow. It lands with a real
consumer *and* a cache keyed on a pool-version stamp (or a per-shard `index` if a
roster gets large). **Do not rebuild it naive.** One fact worth keeping: `index` is
synthesized (no host file to stat), so `CasFs` must generate the TSV once per
operation and serve *both* `getattr` (exact byte length) and `read`/`read_all` from
that generation — never a placeholder size, which the default `read_all` would
silently truncate to. Until then `kj cas ls` lists the pool for humans.

**Known papercut:** kaish's overlay *reserves* `/v/cas`, so `kaish ls /v/cas`
shows an **empty shadow** while SFTP serves the real pool — verify over stock `sftp`,
not kaish (`docs/devlog.md`, July 2).

Residual non-blockers: CAS garbage collection (`kj cas rm` is unconditional; nothing
refcounts object references from clip records — a kernel CAS concern; content-addressed
client caches never corrupt) and the inline-vs-CAS size threshold (`docs/pcm.md`,
decided at the sink).

## `/v/ctx` — context + block introspection (read-only)

A new `VfsBackend` (sibling to `ConfigCrdtFs`,
`crates/kaijutsu-kernel/src/runtime/config_crdt_fs.rs:47`) that synthesizes
`getattr`/`readdir`/`read`/`readlink` from the kernel's context + block stores and
returns `EROFS` on every write.

```
/v/ctx/
├── index                         # resolver TSV: id  type  label  created  parent  head  nblocks  path
├── <ab>/<full-context-id>/       # 256 shards on the LAST byte of the uuid (see Sharding)
│   ├── type created principal label      # one value per file
│   ├── parent   -> ../../<cd>/<parent-id>        # fork ancestry up (edge)
│   ├── head     -> blocks/<head-key>             # newest block (edge)
│   ├── children/<child-id> -> ../../../<ef>/<child-id>  # fork tree down (edges; one level deeper than parent)
│   ├── json                                      # opt-in aggregate of the scalars
│   └── blocks/
│       ├── index                 # timeline-ordered TSV: seq  tick  role  kind  status  excl  bytes  key
│       └── <block-key>/          # flat, NOT sharded (see Sharding); <key> = BlockId.to_key()
│           ├── role kind status tick seq principal excluded ephemeral created
│           ├── content                   # the Text CRDT body; size = BlockHeader.content_len (O(1))
│           ├── json                       # opt-in: scalars + content pointer
│           ├── parent  -> ../<parentkey>  # DAG edge
│           │   # flat, kind-conditional — readdir only shows files that apply:
│           ├── (tool-call blocks)   tool_name tool_input tool_use_id
│           └── (tool-result blocks) exit_code is_error  call -> ../<callkey>
```

**Block attributes are flat and kind-conditional.** Every block exposes the scalar
files plus `content`/`json`; a *tool-call* block adds
`tool_name`/`tool_input`/`tool_use_id`, a *tool-result* block adds
`exit_code`/`is_error` and a `call ->` symlink back to its call. `readdir` shows only
the files that apply. Preferred over nested `tool/`·`error/` subdirs (depth + empty
dirs on text blocks).

**Block addressing — stable id, ordered view.** A block has two names that disagree:
its `BlockId` (`{ctx}_{principal}_{seq}`, `crates/kaijutsu-types/src/block.rs:67` —
stable and unique, but *principal-major*) and its timeline position (a derived view
that shifts on every insert/exclude). Timeline order comes from
`block_ids_ordered()` (`crates/kaijutsu-crdt/src/block_store.rs:199`) and must never
be confused with `BlockId` iteration order — a standing gotcha. So the block-key dir
is canonical and **the ordered view is `blocks/index` row order** — no `by-time/NNNN`
farm whose ordinals would be unstable under multi-writer.

**Enumeration — per-context stores, not a global filter.** The kernel's
`SharedBlockStore` is a `DashMap<ContextId, DocumentEntry>`
(`crates/kaijutsu-kernel/src/block_store.rs:182`) — **one inner CRDT `BlockStore`
per context**, each with its own `block_ids_ordered()`; there is no global block
list to filter. The canonical context roster is `KernelDb::list_all_contexts()`
(`kernel_db.rs:1823`), which also surfaces non-resident/archived contexts the
resident-only `DriftRouter.contexts` would miss; fall back to the resident
`documents` keys when no DB is configured (test mode). The fork tree (`children/`,
the `parent` column) resolves from the `context_edges` table via
`KernelDb::get_active_structural_children` — also DB-gated.

**`blocks/index` recompute** is `block_ids_ordered()` once per read (it re-sorts the
whole context and caches nothing). Slice 1 ships it naive-correct; a backend cache of
the ordered `Vec<BlockId>` keyed on `DocumentEntry::version()` is the deferred
optimization (tracked in `docs/issues.md`). The `content_len`-on-`BlockHeader` change
is **not** deferred — it is a data-model prerequisite (slice 0).

**Booleans use the sysfs `0`/`1` convention** (`excluded`, `ephemeral`), not
`true`/`false` or presence/absence — in the scalar files and the `excl` column.

## Sharding — UUIDv7 shards on the *trailing* byte

Big histories need bounded directories. The canonical context pool is sharded
256-way — but **not on the leading bytes.** Every id is `uuid::Uuid::now_v7()`
(`crates/kaijutsu-types/src/ids.rs:54`), laid out
`[48-bit ms timestamp][version][rand_a][variant][rand_b]`. The *leading* hex is a
millisecond clock — monotonic, not uniform (the first byte is fixed for ~decades,
the first two turn over every ~49 days), so sharding there piles every
recently-created context into one hot bucket. The *trailing* bytes are `rand_b` (62
uniform random bits), so:

- **Contexts shard on the last byte** (`id[-2:]` of the hex) — free, uniform,
  256-way. The `index` resolves label/type → `path`.
- **Blocks are NOT sharded.** A context's block count is bounded by model context
  length; even a long no-compaction conversation stays a reasonable single
  `readdir`, and `blocks/index` is the real interface anyway. (A block key
  `{ctx}_{principal}_{seq}` has a *constant* prefix and *sequential* suffix —
  neither end uniform — so it would have to shard on `hash(key)`. We don't.)

Paged `readdir` (shipped in `docs/sftp.md`'s slice 2) covers any fat directory for
correctness.

(**Objects are the stated exception:** BLAKE3 hashes are uniform in every byte, so
`/v/cas` shards on the *leading* two hex chars, matching the CAS disk layout — see
track B. The trailing-byte rule is a UUIDv7 fact, not a house style.)

## Coherence, size, and read semantics

**Coherence stamp — reuse `DocumentEntry::version()`.** A live `content` file grows
as a block streams; there is no change notification, so a re-`stat` is how a reader
learns of growth. Map `FileAttr.generation` straight to `DocumentEntry::version()`
(`block_store.rs:153`) — an `AtomicU64` bumped on every local write (`touch()`)
**and** restored from the doc version on remote `merge_ops` (`block_store.rs:2020`),
so it advances on local edits *and* sync. O(1) and free. (`sync_generation` is a
narrower sync-protocol counter — wrong for this.) *Verify at impl* with a TDD pair: a
streaming append strictly increases a hot block's generation; a `done` block's is
stable across two stats. The wrong wiring (to `sync_generation`) fails both.

**`content` size is O(1) via `BlockHeader.content_len` (slice 0).** A block's byte
size isn't stored today, so synthesizing it in `getattr` would force `text()` to
materialize the whole CRDT body — a 5 MB tool result re-allocated to fill an `ls -l`
size. Add a `content_len` field to `BlockHeader`
(`crates/kaijutsu-types/src/block.rs:134`, absent today), set on write/merge: a
core-type field + additive CBOR schema evolution (fail-loud per
`kaijutsu_types::codec`) + the block-construction sites. The backend must still
**override `read_all`** (the default sizes from `getattr` and would truncate a
followed symlink — the `read_all`/symlink-sizing gotcha).

**`content` reads snapshot at open** — `text()` taken once at open, so a `cat` over a
streaming block reads a coherent body, not one spliced across a mid-read CRDT merge.
Eventual consistency across *separate* reads is the natural state of this FS — you
poll again to see growth (that's what `generation` is for). A free-running tail is a
possible explicit future mode, not the default.

**Read-only.** A writable-attribute future (`echo 1 > .../excluded` →
`block exclude`; edit `content` → `block edit`, landing at the next hydrate boundary
per the context/conversation split) is noted but out of scope. Today you remediate
via the existing verbs (`block exclude <key>`, then `kj fork`); `/v/ctx` *reflects*
the result immediately.

## `/v/session` — live participants (read-only roster)

A `VfsBackend` view over the kernel's **live participant registry**, `/proc`-style
(in-memory; entries appear on attach, vanish on disconnect). The seed exists:
`PeerRegistry` (`crates/kaijutsu-kernel/src/peers.rs:103`) already tracks the app and
MCP servers with `nick`, a unique-per-process `instance`, a **server-stamped
`principal`** (never trusted from the client), and `attached_at` (`PeerInfo`,
`peers.rs:50`); it gains a session *kind* field (none today). SFTP and SSH-shell
connections register as new kinds (`docs/ssh-shell.md`).

```
/v/session/
├── index                         # roster TSV: instance  kind  principal  attached  context
├── self -> <my-instance>         # resolved per-caller (/proc/self); introspection only
├── <app-instance>/
│   ├── kind        # "app"
│   ├── principal   # <id>        (server-stamped)
│   ├── attached    # <iso>
│   └── context -> /v/ctx/<ab>/<id>   # read-only: its *live* acting context (session registry, not KV)
├── <mcp-instance>/…                  # same shape
└── <sftp-conn>/
    ├── kind principal                # "sftp"; <id>
    └── (no context — SFTP carries none; see Capability)
```

`ls -l /v/session/` (or one read of `index`) is a live roster of who's playing the
instrument and what context each is acting as.

**`context` is read-only observation, not a setter — and it renders *live* session
state, never KV.** It shows the participant's **current acting context** read from
the live state the kernel already tracks: `SessionContextMap`
(`runtime/context_engine.rs:31`, an ephemeral `DashMap<SessionId, ContextId>` the
shell resolves per-op). Deliberately the *ephemeral* value — a disconnected session
has no live context, nothing to render. KV is **not** the source (KV is being
retired — `docs/shared-state.md`); KV's one real job, **durable** per-client
restoration, survives disconnect and therefore lives in a typed per-client store
(*Future: `/v/clients`* below), not here — this tree only ever shows who is live
*right now*. No symlink-to-arm, no TTL — see "Capability".

**`conversation/` (deferred — omitted from the tree on purpose).** The
context/conversation split (durable multi-writer context vs. the append-only sequence
actually shipped to the LLM) is currently invisible. Each session that *runs* a
conversation (app / MCP / SSH-shell, **not** SFTP) would gain a `conversation/`
subdir for the hydrated sequence — append-only, ordinal-stable, read-only, with its
own `index` — so a debugger can diff it against `/v/ctx/<id>/blocks/index` and *see*
what a pending `block exclude` will drop at the next fork. Namespace reserved now;
built later.

### `self` resolution

`self` is resolved **at adapter altitude**, where the caller's identity exists — each
surface rewrites `self` → its own session key before the backend sees the path
(the SFTP adapter holds the connection's key; kaish / file-tools derive it from
`ExecContext`). Preferred over threading a caller parameter through all of `VfsOps`.

## Capability — per-operation join, not per-session binding

The first draft gave each session a durable `bound` context with a symlink setter, a
sliding TTL, and default-deny-until-armed, and sold it as "the capability
unification." On reflection (with Amy, 2026-06-27) that whole apparatus was
**SFTP-shaped scaffolding**: it existed only because SFTP is a bare file protocol
with no `ExecContext`. Every *real* write surface already carries its acting context
ambiently — the **kaish shell** has `ExecContext.context_id`, **MCP** has it, the
**app** has its live current context — and the guard `context_allows_rc_write(ctx)`
(`crates/kaijutsu-kernel/src/file_tools/guard.rs:71`) already keys on
`ctx.context_id` alone. The "one axis, one guard" unification was *already true* for
the real write paths; `bound` was a wart on it, not the prize.

So the model is **per-operation join**: a privileged write joins a context as a
transient CRDT co-writer for that one op, then leaves. The context comes from where
the operation runs — the session's *current* context, read at the moment of the op —
not from a separate stashed capability grant. (The rejected `bound` was such a grant;
the current context is ordinary ambient state, like cwd, not a capability token.)

- **Shell / MCP / app** — context is ambient. The shell resolves its current context
  **live** from `SessionContextMap` per operation (a mid-line `kj attach` chains like
  `cd` — `docs/ssh-shell.md`); MCP/app act as the context they have joined, tracked
  the same live way. Writing `/etc/rc/coder/create/S00-stance.kai` while acting as a
  privileged context routes `context_allows_rc_write(ctx)` and lands the write. No
  binding, no TTL, no arm.
- **SFTP** — read/view, by design. It keeps its lexical deny on privileged paths
  (`privileged_write_denied`, `crates/kaijutsu-server/src/sftp.rs:234`). *If* SFTP
  ever needs to write a privileged tree, the per-operation join is **path-derived**:
  a context-projected writable view (e.g. `/v/ctx/<ab>/<id>/rc/...`) where the
  `context_id` falls out of the path and routes the same guard — privileged context →
  writable, everyone else → `EROFS`, fail-loud. Deferred until a real need appears;
  it needs no session state.

This deletes the `bound` field, the arm symlink, the sliding TTL,
default-deny-until-armed, and the slice-3 `SftpSession` guard-injection complexity —
while *keeping* the unification (one guard, one `context_id` axis). `guard.rs` /
`binding.rs` need no change.

### No ownership

kaijutsu doesn't model ownership — if you can connect you have broadly the same
privileges; the constraint is for safe operation. A non-privileged context simply
can't write the privileged trees (the guard denies, fail-loud, naming the path).
Identity follows the Unix model: a session's `principal` is the **authenticated
user**; multiple sessions by one user share that principal and authorship lane
(`BlockId.principal_id`). The per-connection `instance` distinguishes `/v/session`
rows and rides traces but never enters authorship — two logins are two ttys for one
uid.

## Future: `/v/clients` — durable per-client state, and steering it

`/v/session` is the *live* roster (who's connected **now**, ephemeral, read-only).
Its mirror-image sibling is **`/v/clients`** — the **durable** per-client state,
keyed by the stable installation client-id (`client_id.rs`), surviving disconnect:
the typed per-client store that replaces KV (`docs/shared-state.md`, *Retiring KV*),
*projected* as files the slash-v way — the canonical store is a normalized `KernelDb`
row with a typed RPC; `/v/clients` is the introspection-and-control surface over it.

```
/v/clients/
├── index                         # TSV: client-id  last-seen  context  …
├── self -> <my-client-id>        # the durable id (cf. /v/session/self = the live instance)
└── <client-id>/
    ├── last_seen   # <iso>
    └── context -> /v/ctx/<ab>/<id>   # the context this client should be showing
```

The axis that makes it interesting: unlike `/v/ctx` and `/v/session` (strictly
read-only), **`/v/clients/<id>/context` is writable, and a write *steers* that
client.** The client watches its own row (a `generation` bump, like every other hot
file here) and follows the change. So `context` is simultaneously **the client's own
setter** — the app writes it on a local `kj context switch` (also the durable "reopen
last context" restore value: setting it steers live *and* persists) — and **a remote
steering surface**: *another* participant writes it to drive that display.

The motivating scene (Amy): **four tablets on the wall, each running
`kaijutsu-app`.** An orchestrator — a human at the console, or a context acting on
its own — writes `/v/clients/tablet-3/context` to swing tablet 3 onto the bass line
while the others stay on the score; a player standing at tablet 3 can grab it and
switch it themselves, writing the same field. Many hands, one keyboard, now literally
across devices, with no bespoke protocol — a file write landing on shared kernel
state every surface already sees.

This rides the **per-operation-join** write model above: a steer is an
ambient-context write in the shared-trust kernel (anyone connected can drive a
display; that's the collaborative point, not a hole — there is no ownership). Open,
for the dedicated session: how a steered client *observes* the change (poll
`generation` vs. a notify — KV's `kvWatch` is being deleted); whether `context` is
the only steerable field (theme, layout, a "spotlight this block" pointer all fit);
and the exact typed-RPC vs. file-write split (the file is the *projection*; the RPC
is canonical). Reserved as a direction now; `/v/ctx` + `/v/session` land first.

## Decisions (2026-06-27; track B 2026-07-02, landed)

- **`/v/cas` = read-only `CasFs` on the kernel `MountTable`** — leading two-hex
  shards, full-hash leaves, `staging/`+`metadata/` unexposed, constant `generation`.
  **Landed**; the `index` TSV (B2) **deferred** until it has a consumer and a cache.
- **Client CAS sync = the `sftp` subsystem on its own SSH connection + XDG
  `FileStore` cache + re-hash verification** — no new RPC, fail-loud on mismatch,
  single-flight per hash. **Landed.** Ingest stays `kj cas put`; writable SFTP
  staging deferred.
- **Script-first, three roles** — `index` resolves, sharded pools store, symlink
  edges graph. Human farms are gone; their information is index columns / row order
  / edges.
- **`index` is TSV** — greppable, line-per-object, with a `path` column so callers
  never compute a shard. Per-object `json` stays for structured reads.
- **Sharding on the UUIDv7 trailing byte** — contexts shard 256-way on `id[-2:]`;
  the leading timestamp bytes would cluster. **Blocks are not sharded.**
- **No per-session binding** — per-operation join; the guard keys on the ambient (or
  path-derived) `context_id`. SFTP is read/view.
- **`content_len`** on `BlockHeader` for O(1) `getattr` size — slice 0, foundational.
- **Coherence stamp** — `FileAttr.generation` ← `DocumentEntry::version()`.
- **`content` reads snapshot at open**; cross-poll eventual consistency is fine.
- **`blocks/index` ordered-id cache** — deferred; slice 1 is naive-correct.

## Open questions

- **Huge `content`** — a giant `tool_result` body via one `content` file; lean is no
  size cap, chunked reads, `json`/`index` omitting the body. Range-read discipline
  vs. a cap still unsettled (sysfs's PAGE_SIZE problem, our version).
- **`conversation/` home** — confirmed under `/v/session/<id>/`, but the hydration
  boundary semantics (one conversation per running loop? per fork?) want pinning
  when it's built.
- **Reconnect flicker** — peer entries can flicker on reconnect
  (`[[tech_debt_peer_reattach_on_reconnect]]`); that churn will be visible here.
  Acceptable for `/proc`-style state.

## Implementation slices

**Track B (`/v/cas` + client sync) SHIPPED 2026-07-02** — B0 (cas-crate
hardening), B1 (`CasFs` + mount), B3 (client resolver + XDG cache), and B4 (the
app-sink consumer) all landed; **B2 (the `index` TSV) is deliberately deferred** (see
track B). What remains is **track V**, which shares nothing with track B and is
independent of the remaining SFTP *adapter* work in `docs/sftp.md` (the SFTP-specific
write/capability work this surface used to drive is gone — SFTP is a read consumer).

0. **`content_len` on `BlockHeader` (prerequisite).** Additive field set on
   write/merge; enables O(1) `getattr` size in slice 1. Touches `kaijutsu-types` +
   the block-construction sites + a CBOR schema bump (additive, fail-loud).
1. **`/v/ctx` read-only backend.** New `VfsBackend` rendering the tree above:
   256-shard context pool on the trailing byte; flat kind-conditional block dirs +
   `json`; `index` TSV at `/v/ctx` and per-context `blocks/index`;
   `parent`/`head`/`children`/`call` edges; `EROFS` on writes; size from
   `content_len`; `read_all` override; `generation` ← `DocumentEntry::version()`;
   `content` snapshot-at-open. Contexts from `list_all_contexts()`, blocks from each
   per-context store's `block_ids_ordered()` (naive — no ordered-id cache yet).
   Testable via `kaish ls /v/ctx` with no SFTP. Mounts on the kernel `MountTable`
   (see "The mount-table reality"); mind the kaish `/v/cas` shadow-overlay
   papercut in track B — verify the mount is not similarly shadowed.
2. **`/v/session` read-only roster.** View over the participant registry
   (`PeerRegistry` generalized to carry a session *kind* — `PeerInfo` has none today,
   `peers.rs:50`); rows render each session's **live** acting context from
   `SessionContextMap` as a read-only `context` edge (never KV); `self` resolution
   wired per surface; `index` TSV. (`conversation/` is namespace-reserved, not built
   here.)
3. **SFTP mounts `/v` read-only.** The SFTP adapter exposes the same backends so an
   sshfs session can browse context/block/session state. No write path, no guard
   injection — privileged writes stay lexically denied (`sftp.rs:234`) and happen via
   the shell/MCP where context is ambient.

**Dependency order:** 0 → 1 → 2; slice 3 depends only on 1–2.

## File references

Track B (`/v/cas`, landed):

- `crates/kaijutsu-cas/src/{hash,store,config}.rs` — `ContentHash` (32-hex BLAKE3-128, `prefix()`/`remainder()`, validated `try_from` deserialization), `FileStore`/`ContentStore` (the backend's substrate **and** the client cache; atomic staging+rename `store()`), `objects/`+`metadata/`+`staging/` layout
- `crates/kaijutsu-kernel/src/vfs/backends/cas.rs:59` — `CasFs` (EROFS, leading-two-hex shards, constant generation, `real_path` None)
- `crates/kaijutsu-kernel/src/kernel.rs:164,697` — `cas_for_data_dir` (`{data_dir}/cas/`), `Kernel::cas()`; `kj/cas.rs` — `kj cas put/get/ls/info/rm` (the ingest path)
- `crates/kaijutsu-server/src/rpc.rs:1167–1179` — server bootstrap: `/v/cas` mount, then `freeze_mounts()` (`MountTable::freeze()`, `vfs/mount.rs:68`)
- `crates/kaijutsu-server/src/ssh.rs:832` / `sftp.rs:121` — `SftpSession::new(principal, vfs)` serves the kernel `MountTable` (so `/v/cas` is SFTP-visible, zero adapter work)
- `crates/kaijutsu-client/src/ssh.rs:210` — `connect_subsystem` (the `SftpClient` transport); `sftp.rs` — `SftpClient`/`BlobFetch`/`BlobResolver` (sharded `blob_path`, single-flight, read-to-EOF, `HashMismatch`)
- `crates/kaijutsu-app/src/audio.rs` — the B4 consumer; `Cargo.toml:63` — `russh-sftp = "2.3"` (workspace; client + server halves of one crate)

Track V (`/v/ctx` + `/v/session`):

- `crates/kaijutsu-kernel/src/runtime/embedded_kaish.rs:300` — `/v/docs`, `/v/input` (**kaish-side** mounts, not kernel-`MountTable` — not SFTP-visible; see "The mount-table reality")
- `crates/kaijutsu-kernel/src/runtime/config_crdt_fs.rs:47` — `VfsBackend` pattern to mirror
- `crates/kaijutsu-types/src/ids.rs:54` — all ids are `Uuid::now_v7()` (the trailing-byte sharding rule)
- `crates/kaijutsu-kernel/src/peers.rs:50,103` — `PeerInfo` / `PeerRegistry` (the session seed; `PeerInfo` needs a `kind` field)
- `crates/kaijutsu-kernel/src/runtime/context_engine.rs:31` — `SessionContextMap` (the live acting-context source `context` renders; KV is retired, see `docs/shared-state.md`)
- `crates/kaijutsu-app/src/connection/actor_plugin.rs:301,319` — the app's *durable* per-client restore (today via KV; migrates to a typed per-client store, **not** `/v/session`)
- `crates/kaijutsu-types/src/block.rs:67,134` — `BlockId::to_key()`; `BlockHeader` (gains `content_len`)
- `crates/kaijutsu-crdt/src/block_store.rs:199` — `block_ids_ordered()` (per-context timeline truth → `blocks/index` order)
- `crates/kaijutsu-kernel/src/block_store.rs:182,153,2020` — `documents: DashMap<ContextId, DocumentEntry>`; `DocumentEntry::version()` (coherence stamp; bumped on local write, restored on remote `merge_ops`)
- `crates/kaijutsu-kernel/src/kernel_db.rs:1823,284` — `list_all_contexts()` (context roster → `index`); `contexts.label` UNIQUE (the `label` column)
- `crates/kaijutsu-kernel/src/file_tools/guard.rs:71` — `context_allows_rc_write` (keys on `ctx.context_id`; unchanged)
- `crates/kaijutsu-kernel/src/mcp/binding.rs:94` — `Capability` (`RcWrite`, `ConfigWrite`; unchanged)
- `crates/kaijutsu-server/src/sftp.rs:107,234` — `SftpSession`; `privileged_write_denied` (lexical deny SFTP keeps — no guard injection)
- `crates/kaijutsu-kernel/src/vfs/types.rs` — `FileAttr.generation` coherence stamp
