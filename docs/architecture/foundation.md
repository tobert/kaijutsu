# Foundation: types, CRDT, and the wire schema

*Deep-dive companion to [README.md](README.md). Covers `kaijutsu-types`,
`kaijutsu-crdt`, and `kaijutsu.capnp`. Code is truth; verified 2026-06-16.*

These three things are the shared vocabulary every other crate builds on:
`kaijutsu-types` defines the identities and data shapes, `kaijutsu.capnp` is how
they travel on the wire, and `kaijutsu-crdt` is how they converge across writers.

---

## `kaijutsu-types` — the leaf

A dependency-free foundation crate (no in-repo deps; `lib.rs:5`). Every identity,
block, and CRDT-metadata shape lives here as a plain Rust type so the workspace
DAG has no cycles.

### Identities (`ids.rs`)

`ContextId`, `KernelId`, `PrincipalId`, `SessionId`, `WorkspaceId`, `PresetId` are
all newtypes over `uuid::Uuid` (`ids.rs:18`), generated as **UUIDv7**
(time-ordered). `PrincipalId::system()` and `PrincipalId::beat()` are deterministic
UUIDv5 sentinels (`ids.rs:194`, `ids.rs:218`) — `beat()` is the author lane for
machine-generated timeline fallbacks. The `impl_typed_id!` macro (`ids.rs:47`)
gives every id the same surface (`new`, `short`, `to_hex`, `parse`, `nil`); a
`PrefixResolvable` trait (`ids.rs:246`) enables generic prefix lookup.

Birth-certificate structs: `Context` (`context.rs:16`, with `forked_from` +
`fork_lineage()`), `Principal` (`principal.rs:16`), `Kernel` (`kernel.rs:18`),
`Session` (`session.rs:15`).

### The block (`block.rs`)

- **`BlockId`** (`block.rs:38`) = `(context_id, principal_id, seq)`. Identity, not
  position. Key form `"{ctx_hex}_{principal_hex}_{seq}"` with a legacy `:`
  delimiter still accepted (`block.rs:80`).
- **`BlockKind`** (`block.rs:945`) — 10 structural variants: Text, Thinking,
  ToolCall, ToolResult, Drift, File, Error, Notification, Resource, Trace.
- **`ContentType`** (`block.rs:310`) — 5 render hints: Plain, Markdown, Svg, Abc,
  Image. LWW-merged via Lamport timestamp; richer discriminant wins ties. For
  `Image`, the `content` string holds a 32-char CAS hash, not bytes.
- **`BlockSnapshot`** (`block.rs:1172`) — the serializable replication unit: a flat
  struct with all fields present, mechanism-specific fields (`tool_*`, `drift_*`,
  `error`, `notification`, `resource`, `file_*`) as `Option`, plus per-field LWW
  timestamps, `parent_id`, `order_key`, `tick`, and `updated_at` (the aggregate
  `max` of all field timestamps).
- **`BlockHeader`** (`block.rs:134`) — a `Copy` ~99-byte subset for DAG traversal
  without content.

**`BlockKind` vs `ContentType`** are orthogonal: kind = what the event *is*,
content-type = how its text *renders*. **`BlockId` vs `tick` vs `order_key`**:
identity vs shared timeline position (ties allowed) vs sibling sort order. See the
overview's [data model](README.md#blocks-ids-ticks-and-order) for the why.

### Timeline algebra (`tick.rs`, `track.rs`)

`Tick` is an absolute point, `TickDelta` a signed offset; `Tick + Tick` is a
compile error by design (affine algebra). `Span` is a half-open `(start, len)`
interval. No wall-clock is carried — mapping to seconds happens at the
driver/PPQ boundary. `TrackId` (`track.rs:21`) is a slugified lane identity
(`[a-z0-9_-]`, 1–64 chars); one track spans multiple principals (a player plus
`beat()` for fallbacks).

### Codec (`codec.rs`)

Versioned CBOR: one format byte (`FORMAT_V1 = 0x01`) then ciborium CBOR.
`encode`/`decode` are canonical. Additive evolution is safe because nothing uses
`deny_unknown_fields`; a frozen binary regression test pins the contract for
`BlockSnapshot.track` (`codec.rs:124`).

---

## `kaijutsu.capnp` — the wire schema

Schema id `@0xb8e3f4a9c2d1e0f7`, ~1,260 lines. Major shapes mirror the Rust types
1:1: `BlockId` (line 35), `BlockSnapshot` (line 131, 41 fields with `has*`
sentinels for null-less value types), `BlockMetadata`, `ErrorPayload`,
`NotificationPayload`, `ResourcePayload`, `ContextState` (full CRDT sync: blocks +
ops + version), `TimeoutPolicy`.

Interfaces:

- **`World`** (line 879) — entry point: `whoami`, `listKernels`, `bindKernel`.
- **`Kernel`** (line 888) — the main surface, ~84 methods: kaish exec, VFS, CRDT
  sync (`pushOps`/`getContextSync`), block queries, subscriptions, MCP, peers,
  timeline nav, KV, context lifecycle.
- **`BlockEvents`** (line 373) — server→client callback (13 events; carries
  `seqNum` for dropped-event detection).
- **`Vfs`** (line 1237) — 16-method filesystem interface.
- **`PeerCommands`**, **`KvEvents`**, plus MCP callbacks (`ResourceEvents`,
  `ProgressEvents`, `ElicitationEvents`, `LoggingEvents`).

Evolution is tracked **only in comments** (no `@version`): see lines 921, 933,
1169 documenting removed methods whose ordinals were *renumbered/reused* — flagged
in [issues](../issues.md) because Cap'n Proto treats ordinals as permanent.

---

## `kaijutsu-crdt` — convergence

The CRDT layer: an ordered, multi-writer-safe block log per context, built on
`diamond-types-extended` (a fork of diamond-types). Text is a character-level
CRDT; block order is fractional indexing; metadata is per-field LWW.

### Two storage impls (one is legacy)

- **`BlockStore`** (`block_store.rs:76`) — **the target architecture.** A
  `BTreeMap<BlockId, BlockContent>` where **each block owns its own DTE
  document** for content. Manages a Lamport clock (LWW), per-principal `seq_lanes`,
  a monotonic `next_tick`, and a `version` counter. `block_ids_ordered()`
  (`block_store.rs:199`) sorts by `order_key` (tiebreak `BlockId`) — never iterate
  the `BTreeMap` for timeline order, it's principal-major. Append `order_key` is
  the *successor* of the predecessor's key (`content.rs:134`), decoupled from
  `tick` to avoid stale-counter mis-sorts. Sync via `ops_since(frontiers) →
  SyncPayload` / `merge_ops` (`block_store.rs:1193`, `:1242`); persistence via
  `StoreSnapshot` (parallel `Vec<BlockSnapshot>` + per-block DTE history, CBOR).
- **`BlockDocument`** (`document.rs:114`) — **legacy.** A single shared DTE
  document holding all blocks as paths. Still `pub` and in use during an
  unfinished migration; its `get_block_snapshot` returns newer fields
  (`ephemeral`, `stderr`, `signature`, `track`, …) as hardcoded `None`/`false`.
  Any code path through the legacy doc silently drops those fields.

`BlockContent` (`content.rs:178`) is the per-block unit: a DTE doc scoped to one
block's `content`, the `order_key`, an `Option<Tick>`, an `Option<TrackId>`, and
write-once snapshot fields. DTE ops are wrapped in `catch_unwind`
(`content.rs:628`) to turn causal-graph panics into `CrdtError::Internal`.

### Other documents

- **`KvDocument`** (`kv_document.rs:28`) — flat `key → String` LWW map (DTE),
  `Nil`-tombstone deletes; backs the kernel KV store. Snapshot sorts by key for
  deterministic rebuild.
- **`ConversationDAG`** (`dag.rs:15`) — an *ephemeral computed index* (not a CRDT)
  over an ordered `Vec<BlockSnapshot>`; DFS/BFS, subtree, ancestors, depth, all
  circuit-broken at `MAX_DAG_DEPTH`.

### Document kinds

`DocKind` is defined in `kaijutsu-types` (`enums.rs:184`) but *implemented* here —
a conceptual gap (the CRDT crate doesn't map kinds to backends; the kernel does).
Variants: **Conversation** (the dialog block log), **Code** (file-tool cache, one
doc per tracked file), **Text** (static markdown), **Config** (theme/models TOML),
**Kv** (the degenerate flat-map). Legacy string aliases map onto these.

### Smells (not fixed — see [issues](../issues.md))

- Two live storage impls; the legacy one drops fields, retains the old
  duplicate-block seq bug (`document.rs:892`), and diverges on `set_collapsed`
  semantics.
- `calc_order_key` calls `block_ids_ordered()` (an O(N) sort) on **every** insert
  (`block_store.rs:390`); the bench that exposes it is `#[ignore]`d.
- `StoreSnapshot` has a breaking-format note ("delete existing databases when
  upgrading", `block_store.rs:1680`) with no migration path.
- Tombstones aren't a first-class `BlockSnapshot` property; they ride a side
  `deleted_blocks` list re-applied by hand (`content.rs:388`, `block_store.rs:1637`).

### Types-crate smells

- `ThemeData` (`theme.rs:59`) — a ~60-field visual struct with an `include_str!`
  to `assets/defaults/theme.toml` lives in the *foundation* crate: a layering
  violation.
- `BlockSnapshot` is a 30+-field flat struct with no discriminated union; invalid
  field combinations aren't type-prevented on deserialize.
- Vestigial/dead: `is_error` flagged "legacy" (`block.rs:1237`),
  `DriftKind::Notification` vs `BlockKind::Notification` name collision, the
  `ephemeral` dual-use deferred to "batch 2". (`DriftKind::Commit`, the dead git
  variant, was removed 2026-06-16 — including from the capnp wire enum.)
