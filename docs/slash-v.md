# The `/v` virtual filesystem: `/v/blobs`, `/v/ctx`, `/v/session`

*Design note. Status: proposed 2026-06-26 (extracted from `docs/sftp.md`; refined
via a kaibo cross-model round — gemini + deepseek — then code-verified). Redesigned
2026-06-27 with Amy and simplified hard: the per-session `bound` capability
apparatus is gone (it was SFTP-shaped scaffolding — see below), navigation is a TSV
`index` rather than human symlink farms, and the canonical pools are sharded.
Revised 2026-07-02: `/v/blobs` promoted from a passing mention to a first-class
plan (**track B**, the active work item — it unblocks client CAS sync for
`docs/pcm.md`'s clip prefetch), and the mount-table reality corrected below;
same day, a gemini-pro kaibo batch reviewed the plan and its verified blockers
were folded into the track B slices. Status at that revision: every `/v` mount
is unbuilt; of track B, the client half (`SftpClient`/`BlobResolver`,
`cca8ce7b`+`52d377e7`) is landed with named residue, and B0/B1/B2 are unstarted.
Track B lands first and is independent of the `/v/ctx`/`/v/session` track.*

`/v` is kaijutsu's virtual / CRDT namespace. This note plans three **sysfs-style**
surfaces under it:

- **`/v/blobs`** — the kernel's CAS object pool, rendered as immutable files
  (track B — lands first; the substrate for client CAS sync).
- **`/v/ctx`** — every context and its CRDT block log, rendered as files.
- **`/v/session`** — the live participants (app, MCP, SFTP) and, read-only, the
  context each one is currently acting as.

Because all three are ordinary `VfsBackend`s on the **kernel `MountTable`**, the
same trees are reachable from the Bevy app, kaish, the file tools, **and** SFTP —
build once, every surface gets the view. This is the "instrument you play" stance
made literal: `grep`, `less`, `ls -l` over live kernel state.

**The mount-table reality (verified against code 2026-07-02).** An earlier
revision claimed `/v` "already hosts `/v/docs`, `/v/input`, and `/v/blobs`."
Correcting the record:

- **`/v/blobs` does not exist.** The only `/v/blobs` strings in the tree are test
  literals for blob-valued shell variables; the `embedded_kaish.rs` module header
  sketches it as a future namespace. There is no backend, no mount.
- **`/v/docs` and `/v/input` are kaish-side mounts**
  (`embedded_kaish.rs:286` — `kaish_kernel::vfs::Filesystem` objects on each
  materialized kaish's own VFS), **not** kernel-`MountTable` backends. They are
  therefore *not* visible over SFTP today.
- The surfaces planned here mount on the **kernel `MountTable`** (like `/etc/rc`'s
  `ConfigCrdtFs`), which is the table every surface reaches: SFTP serves it
  directly (`SftpSession::new(principal, vfs)` takes the kernel table), and kaish
  reaches it through `MountBackend` longest-prefix routing — exactly how
  `/etc/rc` is already visible from kaish, the file tools, and SFTP at once.
  Note for implementers: the kernel mount table **freezes** after setup
  (`MountTable::freeze()`, called in the server bootstrap once all mounts are in
  place) — new mounts must land in that bootstrap sequence, before the freeze.

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

## Why this is its own doc, independent of SFTP

These are general surfaces with value far beyond SFTP — `awk` over a
`blocks/index`, `ls -l /v/session`, a context browser reading `index`, a clip
sink pulling a sample — so they get built and tested on their own (via
`kaish ls /v/ctx`, `kaish ls /v/blobs`, zero SFTP involved). SFTP is a
**read/view consumer**: it serves the same kernel mount table, so each tree
becomes remotely browsable/fetchable the moment it mounts. It is *not* the
capability driver it was in the first draft; see "Capability" below for why
that whole apparatus dissolved.

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

## `/v/blobs` — the CAS pool (read-only), and client sync — track B

*The pragmatic driver (2026-07-02): clips. A sink resolves a clip's `media` hash
from a local cache and pulls misses from the kernel under the prepare horizon
(`docs/pcm.md` "How it converges" + slice 5c's open clip half; `docs/clips.md`
"How a clip plays"). SFTP already speaks the VFS (`docs/sftp.md`, landed), so
what was missing was (a) a real `/v/blobs` backend on the kernel mount table
and (b) a client-side fetcher + cache. **No new RPC.** (b) landed 2026-07-02
(with residue — see B3); (a) is the open gap, slices B0–B2.*

### What exists to build on

The kernel already owns a CAS: `kaijutsu-cas`'s `FileStore` at `{data_dir}/cas/`
(`Kernel::cas_for_data_dir`, `kernel.rs:164`), with hashes 128-bit BLAKE3 as 32
hex chars (`ContentHash`, `crates/kaijutsu-cas/src/hash.rs`), disk layout
`objects/<2-hex-prefix>/<30-hex-remainder>` plus a parallel `metadata/` tree of
`{mime_type, size}` JSON and a `staging/` area for incremental ingest
(`CasConfig`, `config.rs`). `kj cas put/get/ls/info/rm` verbs exist
(`crates/kaijutsu-kernel/src/kj/cas.rs`). The trait boundary is
`ContentStore` (`store.rs:72` — `store`/`retrieve`/`exists`/`path`/`inspect`).

### The backend — `CasFs`

A read-only `VfsBackend` (sibling to `LocalBackend`/`MemoryBackend`, new file
`crates/kaijutsu-kernel/src/vfs/backends/cas.rs`) over the kernel's
`Arc<FileStore>`, mounted at `/v/blobs` in the server bootstrap (before the
freeze). Every mutating op returns `EROFS` from the backend itself — read-only
by construction, not by mount flag.

```
/v/blobs/
├── index            # TSV: hash  mime  size  path  — DESIGNED, DEFERRED (see B2)
└── <ab>/            # shard dirs — the hash's LEADING two hex chars
    └── <full-hash>  # the raw bytes, immutable; leaf name is the FULL 32-hex hash
```

*`index` is designed below but **not shipped** — see the B2 slice for why (no
consumer yet, and a walk-the-pool-per-read index is under-designed). The blob
leaves are the whole substrate the client resolver needs.*

- **Shard on the leading byte — deliberately unlike contexts.** The UUIDv7
  trailing-byte rule (below) exists because v7's leading bytes are a clock.
  BLAKE3 output is uniform in *every* byte, so `/v/blobs` shards on the leading
  two hex chars — matching the on-disk `objects/` layout one-to-one. Same
  bounded-directory goal, opposite end, for a stated reason (so nobody
  cargo-cults the trailing-byte rule onto hashes, or vice versa).
- **The leaf name is the full hash**, not the disk's 30-char remainder — paths
  are self-describing and copy-pastable (`grep <hash> /v/blobs/index` → the
  `path` column → `cat` it). The backend maps `<ab>/<full-hash>` →
  `objects/<ab>/<full-hash[2..]>`; a path whose shard doesn't match its hash
  prefix is `ENOENT`.
- **Objects are immutable, which makes every hard problem easy.** For *blob
  leaves*: `getattr` size comes from host-file metadata (O(1) — no
  `content_len`-style prerequisite here); `generation` is a constant (a hash
  names one byte string forever — a caching client never needs invalidation);
  reads are plain offset/length passthrough to the host file, no
  snapshot-at-open apparatus. There are no symlinks in this tree.
- **The `index` file is the one synthesized path, and it needs its own size
  discipline (gemini batch finding, 2026-07-02).** The default
  `VfsOps::read_all` bounds its read at `getattr().size` — exact for blob
  leaves, but the `index` has no host file to stat, so a dummy size would
  silently truncate it. Rule: `CasFs` generates the TSV once per operation and
  serves *both* `getattr` (exact generated byte length) and `read`/`read_all`
  from that generation; equivalently, override `read_all` for `index`. Never
  return a placeholder size.
- **`index`** carries `hash`, `mime` (from the `metadata/` JSON via
  `inspect()`; `-` when metadata is absent), `size`, and the canonical `path` —
  **absolute** (`/v/blobs/<ab>/<full-hash>`), so a script feeds it straight to
  `cat` with no prefixing. The resolver role, per the three-roles rule. First
  cut recomputes by walking `objects/` per read (same enumeration `kj cas ls`
  does); fine at current scale, and a cached index is the deferred optimization
  if the pool grows.
- **`real_path()` returns `None`.** The backend maps to real host files, but
  exposing the host path would let callers bypass the read-only virtual
  abstraction. Shard-dir `getattr`: a standard read-only directory when it
  exists on disk, `ENOENT` when it doesn't (never synthesize empty shards).
- **`staging/` and `metadata/` are deliberately not exposed.** Staging is a
  kernel-internal ingest mechanism; metadata is projected through `index`.
  `/v/blobs` is the object pool, nothing else (the junk-drawer rule).
- **readdir** of `/v/blobs` lists the shard dirs that exist on disk (plus
  `index` once B2 ships); readdir of a shard lists full-hash leaf names. Paged
  readdir (landed with SFTP) keeps a fat shard correct.

### Client sync — `SftpClient` + `BlobResolver` and the XDG cache

The client side of "sync the CAS," in `kaijutsu-client` so both the app and any
future peripheral get it. *(Landed 2026-07-02: `crates/kaijutsu-client/src/sftp.rs`
— `SftpClient` + `BlobResolver`, `cca8ce7b` + `52d377e7` — matches this shape;
still open in-slice: the sharded `blob_path()`, single-flight, and the
read-to-EOF check — see B3.)*

- **Transport: the standard `"sftp"` subsystem over the kaijutsu SSH client,
  on its own connection.** `connect_subsystem("sftp")`
  (`crates/kaijutsu-client/src/ssh.rs:210`) dials + authenticates + binds the
  named subsystem — same key material and server as the RPC channel, so no new
  auth surface and no new RPC; the workspace's `russh-sftp = "2.3"` ships the
  client half of the same crate the server uses. It is a *separate* SSH
  connection by design: the capnp RPC world is `!Send` and pinned to its
  dedicated thread, while SFTP futures are `Send` and ride the ambient async
  runtime / Bevy task pool — the blob path never touches the RPC actor's
  `spawn_local` world. (`sshfs` remains the human/prototyping path to the same
  tree.)
- **The cache is a `kaijutsu_cas::FileStore`** at the XDG cache dir
  (`dirs::cache_dir()/kaijutsu/cas`) — the same crate as the kernel store, so
  layout, hashing, and dedup come for free, and `exists()`/`path()` are the
  sink's lookup API. (`kaijutsu-client` already deps `kaijutsu-cas`, `russh-sftp`,
  and `dirs`.)
- **Verify on fetch, fail loud.** After pulling bytes, recompute
  `ContentHash::from_data(bytes)` and compare to the requested hash. Mismatch =
  hard error (`HashMismatch`), nothing written to the cache — crash over
  corruption; the CAS is self-verifying. (SFTP/SSH gives transport integrity,
  the re-hash gives end-to-end content integrity.) The fetch side is a
  `BlobFetch` trait so the cache-hit/verify/store logic unit-tests against a
  stub with no live server.
- **Single-flight per hash.** Concurrent requests for one hash coalesce into
  one fetch; a clip repeated across a vamp must not open N transfers.
- **When fetches run** is the *consumer's* policy, already decided in
  `docs/pcm.md`: warm the cache on the long **prepare** horizon (10–30 s, when
  a cell becomes known), never inside the short fire lead; late = skip-loud.
  The resolver exposes resolve-by-hash → local path/bytes; scheduling stays
  with the sink.

### Ingest — how bytes get in (today, and deferred)

- **Today, zero new code:** `kj cas put <path>` reads a kernel-VFS path. From a
  remote client: SFTP-upload the file to `/tmp` (a writable `LocalBackend`
  mount), then `kj cas put /tmp/sample.wav`. Two steps, works now, good enough
  for getting sample libraries onto the kernel.
- **Deferred: writable staging over SFTP** (upload into a `/v/blobs`-adjacent
  staging path, seal into CAS on `CLOSE`). Real design work hides there — SFTP
  writes arrive as arbitrary-offset chunks so the hash only exists at close;
  mime must be sniffed or declared; abandoned uploads need cleanup. None of it
  blocks the read path, and the `/tmp` two-step covers ingest meanwhile. Do not
  build speculatively.

### Track B slices

Independent of track V below (no `content_len`, no `/v/ctx` dependency).
Reviewed by a gemini-pro kaibo batch 2026-07-02 (two prompts, whole files);
verified findings are folded in below — B0 and the B3 hardening items are that
review's blockers.

- **B0 — `kaijutsu-cas` hardening (prerequisite for B3's multi-process cache).**
  - **Atomic `store()`.** `FileStore::store` is raw `fs::write(&obj_path, data)`
    (`store.rs:265`) — not atomic, while `seal_path` (`store.rs:206`) already
    has the staging + `fs::rename` shape one function away. The XDG cache is
    shared across OS processes, and a cache-hit `retrieve` never re-hashes, so
    a concurrent reader can get a torn object. Rewrite `store()` to write into
    `staging/` and atomically rename into `objects/` — verified real, blocker.
  - **`ContentHash` deserialization bypasses validation.** `#[serde(transparent)]`
    delegates to `String`, so a malformed hash deserializes fine and
    `prefix()`/`remainder()` panic on short strings. Today the explicit
    `Clip::parse` checks cover it (documented in `docs/pcm.md` 5b); move the
    guarantee into the type with `#[serde(try_from = "String")]` →
    `from_str_checked` so no future call site can forget. Defense-in-depth,
    should-fix.
- **B1 — `CasFs` + mount.** The backend above (unit-tested against a temp
  `FileStore`: getattr/readdir/read/EROFS/shard-mismatch-ENOENT), mounted at
  `/v/blobs` in the server bootstrap before the freeze. Verify from kaish
  (`ls /v/blobs`, `cat` a known hash) and over stock `sftp` (byte-identical
  round-trip against `kj cas get`).
- **B2 — `index` TSV. DEFERRED (2026-07-02, Amy) — do not ship yet.** The
  resolver file was designed here (mime joined from metadata, absolute `path`
  column), but there is **no consumer**: the client resolver addresses blobs by
  exact hash (`/v/blobs/<ab>/<hash>`), never by reading `index`; it is purely a
  human/script grep-the-pool convenience. And the first-cut shape — regenerate
  by walking `objects/` (an O(N) `stat`+`inspect` sweep) on *every* read, no
  cache — is under-designed: shipping it commits us to an ABI we'd outgrow.
  YAGNI. It lands with (a) a real consumer and (b) a cache keyed on a
  pool-version stamp (invalidate on store/remove), or a per-shard `index`
  (256-way) if a single roster ever gets large. Tracked in `docs/issues.md`.
  Until then `kj cas ls` already lists the pool for humans.
- **B3 — client resolver + XDG cache.** `SftpClient` + `BlobResolver` landed
  (`cca8ce7b` + hardening `52d377e7`); remaining in-slice work:
  - **align `blob_path()` to the sharded shape**
    (`format!("/v/blobs/{}/{}", hash.prefix(), hash)` — still flat today);
  - **single-flight per hash** — mandated above, not yet implemented; concurrent
    resolves for one hash must coalesce onto one wire transfer (a per-hash map
    of shared futures / `OnceCell`s; a vamp repeating one clip is the trigger);
  - **whole-blob reads must reach EOF** — SFTP caps a single `READ` at
    `MAX_READ_LEN` (256 KiB server-side); verify `russh-sftp`'s `read` loops
    offsets to EOF or loop explicitly, or every blob >256 KiB fails its hash
    verify. The re-hash makes truncation *loud*, never *corrupt* — but large
    media then never plays. (The deeper fix, streaming into CAS staging instead
    of whole-object `Vec<u8>`, is already recorded as the OOM-vector deferral in
    `52d377e7`.)
  - e2e test against a live server: put via `kj cas put`, fetch via
    `BlobResolver`, byte + hash match; corruption test (flip a byte in the
    server store fixture → resolve fails loud with `HashMismatch`, cache stays
    clean).
- **B4 — the consumer. ✅ landed (2026-07-02).** The app sink's CAS-payload
  warn-and-skip (`crates/kaijutsu-app/src/audio.rs`) is now a `BlobResolver`
  resolve: a `CuePayload::Cas` audio cue dispatches an off-thread resolve on a
  **dedicated tokio runtime** (the SFTP world is `Send` and must stay off the RPC
  actor's current-thread + `LocalSet` `!Send` runtime — `connection/bootstrap.rs`),
  lazily connecting one `SftpClient`/XDG-cache `BlobResolver` for the session;
  results cross back to Bevy on a crossbeam channel and play as `AudioPlayer`s.
  First cut is **fetch-on-cue** (the two-phase prepare-horizon prefetch and
  precise `lead` scheduling remain the `docs/pcm.md` follow-up); the clip-record
  path (parse Shape A → resolve `media`) is the next slice. Transport errors drop
  the connection so the next cue redials.

Open questions for track B: none blocking. Non-blocking notes: the naive
`index` walk (cache when it hurts); CAS garbage collection (`kj cas rm` is
unconditional and nothing refcounts blob references from clip records — a
*kernel* CAS concern, not a `/v/blobs` one; client caches are content-addressed
so a deleted kernel blob never corrupts them); the inline-vs-CAS size threshold
(`docs/pcm.md` residual, decided at the sink).

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

(**Blobs are the stated exception:** BLAKE3 hashes are uniform in every byte, so
`/v/blobs` shards on the *leading* two hex chars, matching the CAS disk layout —
see track B above. The trailing-byte rule is a UUIDv7 fact, not a house style.)

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
│   └── context -> /v/ctx/<ab>/<id>   # read-only: its *live* acting context (session registry, not KV)
├── <mcp-instance>/
│   └── … context -> /v/ctx/<ab>/<id>
└── <sftp-conn>/
    ├── kind        # "sftp"
    ├── principal   # <id>
    └── (no context — SFTP carries none; see Capability)
```

`ls -l /v/session/` (or one read of `index`) is a live roster of who's playing the
instrument and what context each is acting as.

**`context` is read-only observation, not a setter — and it renders *live* session
state, never KV.** It shows the participant's **current acting context** (the context
it has joined / `kj context switch`ed to) read from the live session state the kernel
already tracks: `SessionContextMap` (`runtime/context_engine.rs:31`, an ephemeral
`DashMap<SessionId, ContextId>` the shell resolves per-op). This is deliberately the
*ephemeral* value — a disconnected session has no live context, so there is nothing to
render. KV is **not** the source: an earlier draft read `client.current_context` from
`KvDocument`, but KV is being deleted (`docs/shared-state.md`, *Retiring KV*). KV's one
real job was the *orthogonal* concern of **durable** per-client restoration — "reopen the
context this app window had last time" — which survives disconnect and therefore lives in
a typed per-client store, **not** in `/v/session` (this tree only ever shows who is live
*right now*). There is no symlink-to-arm and no TTL — see why next.

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
- the **app** has its live current context (the durable per-client store, *Future:
  `/v/clients`* below — formerly KV),

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
  like `cd` — see `docs/ssh-shell.md`); MCP/app act as the context they have joined,
  tracked the same live way (not KV — see *Retiring KV* in `docs/shared-state.md`).
  Writing
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

## Future: `/v/clients` — durable per-client state, and steering it

`/v/session` is the *live* roster (who's connected **now**, ephemeral, read-only). Its
mirror-image sibling is **`/v/clients`** — the **durable** per-client state, keyed by the
stable installation client-id (`client_id.rs`), surviving disconnect. This is the typed
per-client store that replaces KV (`docs/shared-state.md`, *Retiring KV*), *projected* as
files the slash-v way: the canonical store is a normalized `KernelDb` row with a typed RPC,
and `/v/clients` is the introspection-and-control surface over it.

```
/v/clients/
├── index                         # TSV: client-id  last-seen  context  …
├── self -> <my-client-id>        # the durable id (cf. /v/session/self = the live instance)
└── <client-id>/
    ├── last_seen   # <iso>
    └── context -> /v/ctx/<ab>/<id>   # the context this client should be showing
```

The axis that makes it interesting: unlike `/v/ctx` and `/v/session` (strictly
read-only), **`/v/clients/<id>/context` is writable, and a write *steers* that client.**
The client watches its own row (a `generation` bump, like every other hot file here) and
follows the change. So `context` is simultaneously:

- **the client's own setter** — the app writes it on a local `kj context switch` (this is
  also the durable "reopen last context" restore value: setting it steers live *and*
  persists);
- **a remote steering surface** — *another* participant writes it to drive that display.

The motivating scene (Amy): **four tablets on the wall, each running `kaijutsu-app`.** An
orchestrator (a human at the console, or a context acting on its own) writes
`/v/clients/tablet-3/context` to swing tablet 3 onto the bass line while the others stay
on the score — *and* a player standing at tablet 3 can grab it and switch it themselves,
writing the same field. The instrument-you-play stance made multi-surface: many hands, one
keyboard, now literally across devices, with no bespoke protocol — it's a file write
landing on shared kernel state every surface already sees.

This rides the **per-operation-join** write model above: a steer is an ambient-context
write in the shared-trust kernel (anyone connected can drive a display; that's the
collaborative point, not a hole — there is no ownership). Open, for the dedicated session:
how a steered client *observes* the change (poll `generation` vs. a `kvWatch`-style
notify — note KV's `kvWatch` is being deleted, so this wants a successor or a plain poll);
whether `context` is the only steerable field (theme, layout, a "spotlight this block"
pointer all fit the same shape); and the exact typed-RPC vs. file-write split (the file is
the *projection*; the RPC is canonical — same as `/v/ctx` reflecting verbs). Reserved as a
direction now; `/v/ctx` + `/v/session` land first.

## Decisions (2026-06-27, track B added 2026-07-02)

- **`/v/blobs` is a read-only `CasFs` on the kernel `MountTable`** — sharded on
  the *leading* two hex chars (BLAKE3 is uniform; matches disk), full-hash leaf
  names, `staging/`+`metadata/` unexposed, constant `generation` (immutable
  objects). Track B, lands first. The `index` TSV (mime/size) is designed but
  **deferred** (B2) until it has a consumer and a cache.
- **Client CAS sync = the `sftp` subsystem on its own SSH connection (same keys,
  same server; the capnp RPC world is `!Send`) + an XDG `FileStore` cache +
  re-hash verification** — no new RPC, fail-loud on hash mismatch, single-flight
  per hash. Ingest stays `kj cas put` (SFTP→`/tmp` for remote files); writable
  staging over SFTP is deferred.
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

Two independent tracks. **Track B** (`/v/blobs` + client sync, slices B1–B4
above) is the active work item and touches none of the below — no `content_len`,
no block-store surface. **Track V** (`/v/ctx` + `/v/session`) is this section.
Both are independent of the remaining SFTP *adapter* work in `docs/sftp.md`; the
SFTP-specific *write/capability* work this surface used to drive is gone (SFTP
is a read consumer).

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
   Testable via `kaish ls /v/ctx` with no SFTP. Mounts on the kernel
   `MountTable` (see "The mount-table reality" — *not* the kaish-side layer
   `/v/docs`/`/v/input` live on).
2. **`/v/session` read-only roster.** View over the participant registry
   (`PeerRegistry` generalized to carry a session *kind* — `PeerInfo` has none today,
   `peers.rs:50`); rows render each session's **live** acting context from
   `SessionContextMap` as a read-only `context` edge (never KV — see the
   `/v/session` section); `self` resolution wired per surface; `index` TSV.
   (`conversation/` is namespace-reserved, not built here.)
3. **SFTP mounts `/v` read-only.** The SFTP adapter exposes the same backends so an
   sshfs session can browse context/block/session state. No write path, no guard
   injection — privileged writes stay lexically denied (`sftp.rs:234`) and happen via
   the shell/MCP where context is ambient. (The path-derived context-projected write
   view is a deferred future, only if a real SFTP-write need appears.)

**Dependency order:** track B: B0 → B3-hardening in the cas crate; B1 → B2 on
the kernel side; B3's e2e needs B1. Track V:
0 → 1 → 2; slice 3 (SFTP read mount of `/v/ctx`+`/v/session`) depends only on
1–2. The tracks share nothing; B lands first.

## File references

Track B (`/v/blobs`):

- `crates/kaijutsu-cas/src/{hash,store,config}.rs` — `ContentHash` (32-hex BLAKE3-128, `prefix()`/`remainder()`), `FileStore`/`ContentStore` (the backend's substrate **and** the client cache), `objects/`+`metadata/`+`staging/` layout
- `crates/kaijutsu-kernel/src/kernel.rs:164,685` — `cas_for_data_dir` (`{data_dir}/cas/`), `Kernel::cas()` accessor
- `crates/kaijutsu-kernel/src/kj/cas.rs` — `kj cas put/get/ls/info/rm` (the enumeration `index` reuses; the ingest path)
- `crates/kaijutsu-server/src/rpc.rs:1062–1075` — server bootstrap mount sequence (where the `/v/blobs` mount lands, before the freeze)
- `crates/kaijutsu-kernel/src/vfs/mount.rs:68` — `MountTable::freeze()`
- `crates/kaijutsu-server/src/ssh.rs:837` / `sftp.rs:121` — `SftpSession::new(principal, vfs)` serves the kernel `MountTable` (so `/v/blobs` is SFTP-visible on mount, zero adapter work)
- `crates/kaijutsu-client/src/ssh.rs:210` — `connect_subsystem` (dial + auth + bind a named subsystem; the `SftpClient` transport)
- `crates/kaijutsu-client/src/sftp.rs` — `SftpClient`/`BlobFetch`/`BlobResolver` (landed `cca8ce7b`+`52d377e7`; B3 residue: sharded `blob_path`, single-flight, read-to-EOF)
- `crates/kaijutsu-app/src/audio.rs:138` — the CAS-payload skip B4 replaces
- `Cargo.toml:60` — `russh-sftp = "2.3"` (workspace; has the client half)

Track V (`/v/ctx` + `/v/session`):

- `crates/kaijutsu-kernel/src/runtime/embedded_kaish.rs:286` — `/v/docs`, `/v/input` (**kaish-side** mounts, not kernel-`MountTable` — not SFTP-visible; see "The mount-table reality")
- `crates/kaijutsu-kernel/src/runtime/config_crdt_fs.rs:47` — `VfsBackend` pattern to mirror
- `crates/kaijutsu-types/src/ids.rs:52` — all ids are `Uuid::now_v7()` (the trailing-byte sharding rule)
- `crates/kaijutsu-kernel/src/peers.rs:50,103` — `PeerInfo` / `PeerRegistry` (the session seed; `PeerInfo` needs a `kind` field)
- `crates/kaijutsu-kernel/src/runtime/context_engine.rs:31` — `SessionContextMap` (the live acting-context source `context` renders; KV is retired, see `docs/shared-state.md`)
- `crates/kaijutsu-app/src/connection/actor_plugin.rs:319,534` — the app's *durable* per-client restore (today via KV; migrates to a typed per-client store, **not** `/v/session`)
- `crates/kaijutsu-types/src/block.rs:67,134` — `BlockId::to_key()`; `BlockHeader` (gains `content_len`)
- `crates/kaijutsu-crdt/src/block_store.rs:199` — `block_ids_ordered()` (per-context timeline truth → `blocks/index` order)
- `crates/kaijutsu-kernel/src/block_store.rs:182,153,1973` — `documents: DashMap<ContextId, DocumentEntry>`; `DocumentEntry::version()` (coherence stamp; bumped on local write + remote merge)
- `crates/kaijutsu-kernel/src/kernel_db.rs:1680,279` — `list_all_contexts()` (context roster → `index`); `contexts.label` UNIQUE (the `label` column)
- `crates/kaijutsu-kernel/src/file_tools/guard.rs:71` — `context_allows_rc_write` (keys on `ctx.context_id`; unchanged)
- `crates/kaijutsu-kernel/src/mcp/binding.rs:94` — `Capability` (`RcWrite`, `ConfigWrite`; unchanged)
- `crates/kaijutsu-server/src/sftp.rs:107,234` — `SftpSession`; `privileged_write_denied` (lexical deny SFTP keeps — no guard injection now)
- `crates/kaijutsu-kernel/src/vfs/types.rs` — `FileAttr.generation` coherence stamp
