# The `/v` virtual filesystem: `/v/ctx` and `/v/session`

*Design note. Status: proposed 2026-06-26 (extracted from `docs/sftp.md`; refined
via a kaibo cross-model round — gemini + deepseek — then code-verified). Redesigned
2026-06-27 with Amy and simplified hard: the per-session `bound` capability
apparatus is gone (it was SFTP-shaped scaffolding — see below), navigation is a TSV
`index` rather than human symlink farms, and the canonical pools are sharded. Stands
alone and can land **ahead** of the remaining SFTP work — these are real VFS backends
every surface sees.*

`/v` is kaijutsu's virtual / CRDT namespace. It already hosts `/v/docs` and
`/v/input` (`crates/kaijutsu-kernel/src/runtime/embedded_kaish.rs:286`) and
`/v/blobs` (`crates/kaijutsu-server/src/rpc.rs:7968`). This note adds two
read-mostly, **sysfs-style** surfaces under it:

- **`/v/ctx`** — every context and its CRDT block log, rendered as files.
- **`/v/session`** — the live participants (app, MCP, SFTP) and, read-only, the
  context each one is currently acting as.

Because both are ordinary `VfsBackend`s, the same trees are reachable from the
Bevy app, kaish, the file tools, **and** SFTP — build once, every surface gets
context + session introspection. This is the "instrument you play" stance made
literal: `grep`, `less`, `ls -l` over live kernel state.

## Orientation: script-first, three clean roles

The earlier draft leaned on human-typeable symlink farms (`live/<label>`,
`by-type/<type>`, `by-time/NNNN`, `by-lineage/`). They're cute but they don't scale
and they bias the surface toward a human at a prompt. This surface is mostly for
**scripts and expert debugging**, so navigation collapses into three roles that
never overlap:

1. **`index` — the resolver.** One greppable TSV table per collection. It carries
   `label`, `type`, and a `path` column (the canonical shard-relative path), so a
   caller does `grep feature-x /v/ctx/index`, reads the `path` field, and `cd`s
   there. Nobody computes a shard path, nobody walks a farm. The index *is*
   `by-type` (a column), `live` (a column), and `by-time` (row order).
2. **Sharded pools — pure storage.** `/v/ctx/<shard>/<full-id>/`. You reach a dir
   by resolving a known id (from the index, or from an edge), never by browsing.
3. **Symlink edges — pure graph.** `parent`, `head`, `children/`, and a
   tool-result's `call ->` stay as real symlinks (the kobject model — relationships
   are links). `ls -l` still shows the DAG and `readlink` is script-friendly; edge
   targets carry the full shard path, so graph traversal never makes a caller
   compute a shard. Only the *initial* label lookup touches the index.

## Why this is its own doc, and lands first

These are general introspection surfaces with value far beyond SFTP — `awk` over a
`blocks/index`, `ls -l /v/session`, a context browser reading `index` — so they
get built and tested on their own (via `kaish ls /v/ctx`, zero SFTP involved). SFTP
is a **read/view consumer**: it mounts the same trees so an sshfs session can browse
context and block state. It is *not* the capability driver it was in the first
draft; see "Capability" below for why that whole apparatus dissolved.

## Design principles (lessons from `/proc` and `/sysfs`)

1. **One value per file** (sysfs's cardinal rule). The anti-pattern is
   `/proc/<pid>/stat`: 50 positional fields nobody can extend safely. Scalars are
   their own files (`role`, `kind`, `status`), greppable line by line.
2. **…but ship aggregates**, because N tiny files is a `stat`-storm over a network
   FS. Two tiers: each object offers a `json` file (one read returns the whole
   object) and each collection offers a TSV `index` (one read returns the whole
   roster, ordered, with the per-row scalars inline). Humans use the scalar files;
   scripts read `index`/`json`. Aggregates are also how we evolve without breaking
   ABI — new fields land in `json` / new columns append to `index`.
3. **Directories are objects, edges are symlinks** (the kobject model). Contexts and
   blocks are dirs; `parent`, `head`, `children`, and a tool-result's `call` are
   symlinks. We **do not** ship symlink *farms* for naming convenience — `live`,
   `by-type`, `by-lineage` are index columns, not directory trees.
4. **Stable identity, separate from ordering and views.** `/proc`'s pid-reuse
   footgun (a stale path silently addresses a different process) is our
   timeline-ordinal instability. So identity is the stable `BlockId` key (the shard
   path) and ordering is a *view* — but the view lives in `blocks/index` row order,
   not a `by-time/NNNN` symlink farm whose ordinals shift under every insert.
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
files plus `content`/`json`; a *tool-call* block additionally exposes
`tool_name`/`tool_input`/`tool_use_id`, and a *tool-result* block exposes
`exit_code`/`is_error` and a `call ->` symlink back to its call. `readdir` shows only
the files that apply — a plain `Text` block carries no `exit_code`. Preferred over
nested `tool/`·`error/` subdirs (depth + empty dirs on text blocks).

**Block addressing — stable id, ordered view.** A block has two names that disagree:
its `BlockId` (`{ctx}_{principal}_{seq}`, `crates/kaijutsu-types/src/block.rs:67` —
stable and unique, but *principal-major*) and its timeline position (a derived view
that shifts on every insert/exclude). Timeline order comes from
`block_ids_ordered()` (`crates/kaijutsu-crdt/src/block_store.rs:199`) and must never
be confused with `BlockId` iteration order — a standing gotcha. So the block-key dir
is canonical and **the ordered view is `blocks/index` row order** — there is no
`by-time/NNNN` farm whose ordinals would be unstable under multi-writer.

**Enumeration — per-context stores, not a global filter.** The kernel's
`SharedBlockStore` is a `DashMap<ContextId, DocumentEntry>`
(`crates/kaijutsu-kernel/src/block_store.rs:182`) — **one inner CRDT `BlockStore`
per context**, each with its own `block_ids_ordered()`. So `/v/ctx` *locates the
per-context store and reads its ordered blocks*; there is no global block list to
filter. The canonical context roster is `KernelDb::list_all_contexts()`
(`kernel_db.rs:1680`), which also surfaces non-resident/archived contexts the
resident-only `DriftRouter.contexts` would miss; fall back to the resident
`documents` keys when no DB is configured (test mode). The fork tree (`children/`,
and the `parent` column) resolves from the `context_edges` table via
`KernelDb::get_active_structural_children` — also DB-gated.

**`blocks/index` recompute** is `block_ids_ordered()` once per read (it re-sorts the
whole context and caches nothing). Slice 1 ships it naive-correct; a backend cache of
the ordered `Vec<BlockId>` keyed on `DocumentEntry::version()` is the deferred
optimization (tracked in `docs/issues.md`). The `content_len`-on-`BlockHeader` change
is **not** deferred — it is a data-model prerequisite (slice 0).

**Booleans use the sysfs `0`/`1` convention** (`excluded`, `ephemeral`), not
`true`/`false` or presence/absence — in both the scalar files and the `excl` column.

## Sharding — UUIDv7 shards on the *trailing* byte

Big histories need bounded directories. The canonical context pool is sharded
256-way — but **not on the leading bytes.** Every id is `uuid::Uuid::now_v7()`
(`crates/kaijutsu-types/src/ids.rs:52`), laid out
`[48-bit ms timestamp][version][rand_a][variant][rand_b]`. The *leading* hex is a
millisecond clock — monotonic, not uniform: the first byte is fixed for ~decades and
the first two bytes turn over every ~49 days, so sharding there piles every
recently-created context into one hot bucket. The *trailing* bytes are `rand_b` (62
uniform random bits), so:

- **Contexts shard on the last byte** (`id[-2:]` of the hex) — free, uniform,
  256-way. `live/` and `by-type/` farms are gone; the `index` resolves
  label/type → `path`.
- **Blocks are NOT sharded.** A context's block count is bounded by model context
  length; even a long no-compaction conversation stays a reasonable single
  `readdir`, and `blocks/index` is the real interface anyway. (If a block pool ever
  measured large, note that its key `{ctx}_{principal}_{seq}` has a *constant*
  prefix and a *sequential* suffix — neither end is uniform, so it would have to
  shard on `hash(key)`, unlike contexts. We don't.)

Paged `readdir` (shipped in `docs/sftp.md`'s slice 2) covers any fat directory for
correctness.

## Coherence, size, and read semantics

**Coherence stamp — reuse `DocumentEntry::version()`.** A live `content` file grows
as a block streams; there is no change notification, so a re-`stat` is how a reader
learns of growth. Map `FileAttr.generation` straight to `DocumentEntry::version()`
(`block_store.rs:153`) — an `AtomicU64` bumped on every local write (`touch()`)
**and** restored from the doc version on remote `merge_ops` (`block_store.rs:1973`),
so it advances on local edits *and* sync. O(1) and free. (`sync_generation` is a
narrower sync-protocol counter — wrong for this.) *Verify at impl* with a TDD pair: a
streaming append strictly increases a hot block's `getattr().generation`, and a
`done` block's generation is stable across two stats (so an indexer can trust
`status=done` to skip). The wrong wiring (to `sync_generation`) fails both.

**`content` size is O(1) via `BlockHeader.content_len` (slice 0).** A block's byte
size isn't stored today, so synthesizing it in `getattr` would force `text()` to
materialize the whole CRDT body — a 5 MB tool result re-allocated to fill an `ls -l`
size. Add a `content_len` field to `BlockHeader`
(`crates/kaijutsu-types/src/block.rs:134`, absent today), set on write/merge. Cost: a
core-type field + additive CBOR schema evolution (fail-loud per
`kaijutsu_types::codec`) + the block-construction sites. The backend must still
**override `read_all`** (the default sizes from `getattr` and would truncate a
followed symlink otherwise — the `read_all`/symlink-sizing gotcha).

**`content` reads snapshot at open.** A `content` read takes `text()` once at open
and serves that snapshot, so a `cat`/`less` over a streaming block reads a coherent
body, not one spliced across a mid-read CRDT merge. A little eventual consistency
across *separate* reads is the natural state of this FS — you poll again to see
growth (that's what `generation` is for). A free-running tail is a possible explicit
future mode, not the default.

**Read-only.** A writable-attribute future (`echo 1 > .../excluded` →
`block exclude`; edit `content` → `block edit`, taking effect at the next hydrate
boundary per the context/conversation split) is noted but out of scope — the
read-only tree needs only the mount's `read_only()` flag. Today you remediate via the
existing verbs (`block exclude <key>`, then `kj fork`); `/v/ctx` *reflects* the
result immediately.

## `/v/session` — live participants (read-only roster)

A `VfsBackend` view over the kernel's **live participant registry**, `/proc`-style
(in-memory; entries appear on attach and vanish on disconnect). The seed exists:
`PeerRegistry` (`crates/kaijutsu-kernel/src/peers.rs:103`) already tracks the app and
MCP servers with `nick`, a unique-per-process `instance`, a **server-stamped
`principal`** (never trusted from the client), and `attached_at` (`PeerInfo`,
`peers.rs:50`). It gains a session *kind* field (none today). SFTP and SSH-shell
connections register as new kinds (`docs/ssh-shell.md`).

```
/v/session/
├── index                         # roster TSV: instance  kind  principal  attached  context
├── self -> <my-instance>         # resolved per-caller (/proc/self); introspection only
├── <app-instance>/
│   ├── kind        # "app"
│   ├── principal   # <id>        (server-stamped)
│   ├── attached    # <iso>
│   └── context -> /v/ctx/<ab>/<id>   # read-only: its KV current_context (set via `kj context switch`)
├── <mcp-instance>/
│   └── … context -> /v/ctx/<ab>/<id>
└── <sftp-conn>/
    ├── kind        # "sftp"
    ├── principal   # <id>
    └── (no context — SFTP carries none; see Capability)
```

`ls -l /v/session/` (or one read of `index`) is a live roster of who's playing the
instrument and what context each is acting as.

**`context` is read-only observation, not a setter.** For app/MCP it renders
`client.current_context` from KV (`crates/kaijutsu-kernel/src/kv.rs:53`), which `kj
context switch` already writes. There is no symlink-to-arm and no TTL — see why next.

**`conversation/` (deferred — omitted from the tree above on purpose).** The
context/conversation split (durable multi-writer context vs. the append-only sequence
actually shipped to the LLM) is currently invisible. Each session that *runs* a
conversation (app / MCP / SSH-shell, **not** SFTP) would gain a `conversation/`
subdir reserving the home for the hydrated sequence — append-only, ordinal-stable,
read-only, with its own `index` — so a debugger can diff it against
`/v/ctx/<id>/blocks/index` and *see* what a pending `block exclude` will drop at the
next fork. Namespace reserved now; built later, which is why no `conversation/`
appears in the tree yet.

### `self` resolution

`self` is resolved **at adapter altitude**, where the caller's identity exists — each
surface rewrites `self` → its own session key before the backend sees the path,
keeping `/v/session` caller-agnostic over the registry. The SFTP adapter holds the
connection's key; the kaish / file-tool path has an `ExecContext` to derive it.
Preferred over threading a caller parameter through all of `VfsOps`.

## Capability — per-operation join, not per-session binding

The first draft gave each session a durable `bound` context with a symlink setter, a
sliding TTL, and default-deny-until-armed, and sold it as "the capability
unification." On reflection (with Amy, 2026-06-27) that whole apparatus was
**SFTP-shaped scaffolding**: it existed only because SFTP is a bare file protocol
with no `ExecContext`. Every *real* write surface already carries its acting context
ambiently —

- the **kaish shell** has `ExecContext.context_id`,
- **MCP** has it,
- the **app** has KV `current_context`,

and the guard `context_allows_rc_write(ctx)`
(`crates/kaijutsu-kernel/src/file_tools/guard.rs:71`) already keys on
`ctx.context_id` alone. The "one axis, one guard" unification was *already true* for
the real write paths; `bound` was a wart on it, not the prize.

So the model is **per-operation join**: a privileged write joins a context as a
transient CRDT co-writer for that one op, then leaves. The context comes from where
the operation runs — the session's *current* context, read at the moment of the op —
not from a separate stashed capability grant. (The rejected `bound` was such a grant;
the current context is ordinary ambient state, like cwd, not a capability token.)

- **Shell / MCP / app** — context is ambient. The shell resolves its current context
  **live** from `SessionContextMap` per operation (so a mid-line `kj attach` chains
  like `cd` — see `docs/ssh-shell.md`); MCP/app read KV `current_context`. Writing
  `/etc/rc/coder/create/S00-stance.kai` while acting as a privileged context routes
  `context_allows_rc_write(ctx)` and lands the write. No binding, no TTL, no arm.
- **SFTP** — read/view, by design ("not everything needs to work over SFTP"). It
  keeps its current lexical deny on privileged paths
  (`privileged_write_denied`, `crates/kaijutsu-server/src/sftp.rs:234`). *If* SFTP
  ever needs to write a privileged tree, the per-operation join is **path-derived**:
  a context-projected writable view (e.g. `/v/ctx/<ab>/<id>/rc/...`) where the
  `context_id` falls out of the path and routes the same guard — privileged context
  → writable, everyone else → `EROFS`, fail-loud, no special-casing. That's deferred
  until a real need appears; it needs no session state.

This deletes the `bound` field, the arm symlink, the sliding TTL, default-deny-until-
armed, and the slice-3 `SftpSession` guard-injection complexity — while *keeping* the
unification (one guard, one `context_id` axis) and making it cleaner, because nothing
pretends to be that axis anymore. `guard.rs` / `binding.rs` need no change.

### No ownership

kaijutsu doesn't model ownership — if you can connect you have broadly the same
privileges, and the constraint is for safe operation. A non-privileged context simply
can't write the privileged trees (the guard denies, fail-loud, naming the path).

Identity follows the Unix model: a session's `principal` is the **authenticated
user**, and multiple sessions by one user (app windows, SSH logins) share that one
principal and authorship lane (`BlockId.principal_id`). The per-connection `instance`
distinguishes `/v/session` rows and rides traces, but never enters authorship — two
logins are two ttys for one uid.

## Decisions (2026-06-27)

- **Script-first, three roles** — `index` resolves, sharded pools store, symlink
  edges graph. Human farms (`live`, `by-type`, `by-time`, `by-lineage`) are gone;
  their information is index columns / row order / edges.
- **`index` is TSV** — greppable, `cut`-able, line-per-object; carries a `path`
  column so callers never compute a shard. Per-object `json` stays for structured
  reads.
- **Sharding on the UUIDv7 trailing byte** — contexts shard 256-way on `id[-2:]`
  (the random tail); the leading timestamp bytes would cluster. **Blocks are not
  sharded** (bounded by context length).
- **No per-session binding** — per-operation join; the guard keys on the ambient (or
  path-derived) `context_id`. SFTP is read/view.
- **`content_len`** — added to `BlockHeader` for O(1) `getattr` size. Slice 0,
  foundational.
- **Coherence stamp** — `FileAttr.generation` ← `DocumentEntry::version()`.
- **`content` reads snapshot at open**; cross-poll eventual consistency is fine.
- **`blocks/index` ordered-id cache** — deferred; slice 1 is naive-correct.

## Open questions

- **Huge `content`** — a giant `tool_result` body via one `content` file; lean is no
  size cap, reads are chunked, `json`/`index` omit the body. Range-read discipline
  vs. a cap still unsettled (sysfs's PAGE_SIZE problem, our version).
- **`conversation/` home** — confirmed it hangs off `/v/session/<id>/` as observed
  per-session state, but the hydration boundary semantics (one conversation per
  running loop? per fork?) want pinning when it's built.
- **Reconnect flicker** — `/v/session` reflects live attachments, and
  `[[tech_debt_peer_reattach_on_reconnect]]` means peer entries can flicker on
  reconnect; that churn will be visible here. Acceptable for `/proc`-style state.

## Implementation slices

Independent of the remaining SFTP *adapter* work in `docs/sftp.md`; the SFTP-specific
*write/capability* work this surface used to drive is gone (SFTP is a read consumer).

0. **`content_len` on `BlockHeader` (prerequisite).** Additive field set on
   write/merge; enables O(1) `getattr` size in slice 1. Touches `kaijutsu-types` +
   the block-construction sites + a CBOR schema bump (additive, fail-loud). A general
   data-model improvement slice 1 depends on.
1. **`/v/ctx` read-only backend.** New `VfsBackend` rendering the tree above:
   256-shard context pool on the trailing byte; flat block-key dirs; flat
   kind-conditional scalar files + `json`; `index` TSV at `/v/ctx` and per-context
   `blocks/index`; `parent`/`head`/`children`/`call` edges; `EROFS` on writes; size
   from `content_len`; `read_all` override; `generation` ← `DocumentEntry::version()`;
   `content` snapshot-at-open. Contexts from `list_all_contexts()`, blocks from each
   per-context store's `block_ids_ordered()` (naive — no ordered-id cache yet).
   Testable via `kaish ls /v/ctx` with no SFTP. Mount alongside `/v/docs`/`/v/input`.
2. **`/v/session` read-only roster.** View over the participant registry
   (`PeerRegistry` generalized to carry a session *kind* — `PeerInfo` has none today,
   `peers.rs:50`); app/MCP rows render their KV `current_context` as a read-only
   `context` edge; `self` resolution wired per surface; `index` TSV. (`conversation/`
   is namespace-reserved, not built here.)
3. **SFTP mounts `/v` read-only.** The SFTP adapter exposes the same backends so an
   sshfs session can browse context/block/session state. No write path, no guard
   injection — privileged writes stay lexically denied (`sftp.rs:234`) and happen via
   the shell/MCP where context is ambient. (The path-derived context-projected write
   view is a deferred future, only if a real SFTP-write need appears.)

**Dependency order:** 0 → 1 → 2; slice 3 (SFTP read mount) depends only on 1–2.

## File references

- `crates/kaijutsu-kernel/src/runtime/embedded_kaish.rs:286` — existing `/v/docs`, `/v/input` mounts
- `crates/kaijutsu-server/src/rpc.rs:7968` — existing `/v/blobs`
- `crates/kaijutsu-kernel/src/runtime/config_crdt_fs.rs:47` — `VfsBackend` pattern to mirror
- `crates/kaijutsu-types/src/ids.rs:52` — all ids are `Uuid::now_v7()` (the trailing-byte sharding rule)
- `crates/kaijutsu-kernel/src/peers.rs:50,103` — `PeerInfo` / `PeerRegistry` (the session seed; `PeerInfo` needs a `kind` field)
- `crates/kaijutsu-kernel/src/kv.rs:53` — `client.current_context` (app/MCP's read-only `context`)
- `crates/kaijutsu-types/src/block.rs:67,134` — `BlockId::to_key()`; `BlockHeader` (gains `content_len`)
- `crates/kaijutsu-crdt/src/block_store.rs:199` — `block_ids_ordered()` (per-context timeline truth → `blocks/index` order)
- `crates/kaijutsu-kernel/src/block_store.rs:182,153,1973` — `documents: DashMap<ContextId, DocumentEntry>`; `DocumentEntry::version()` (coherence stamp; bumped on local write + remote merge)
- `crates/kaijutsu-kernel/src/kernel_db.rs:1680,279` — `list_all_contexts()` (context roster → `index`); `contexts.label` UNIQUE (the `label` column)
- `crates/kaijutsu-kernel/src/file_tools/guard.rs:71` — `context_allows_rc_write` (keys on `ctx.context_id`; unchanged)
- `crates/kaijutsu-kernel/src/mcp/binding.rs:94` — `Capability` (`RcWrite`, `ConfigWrite`; unchanged)
- `crates/kaijutsu-server/src/sftp.rs:107,234` — `SftpSession`; `privileged_write_denied` (lexical deny SFTP keeps — no guard injection now)
- `crates/kaijutsu-kernel/src/vfs/types.rs` — `FileAttr.generation` coherence stamp
