# Foundation: types, the block/document model, and the wire schema

*Deep-dive companion to [README.md](README.md). Covers `kaijutsu-types`,
`kaijutsu_kernel::blocks`, and `kaijutsu.capnp`. Code is truth; verified
2026-08-16.*

These three things are the shared vocabulary every other crate builds on:
`kaijutsu-types` defines the identities and data shapes, `kaijutsu.capnp` is how
they travel on the wire, and `kaijutsu_kernel::blocks` holds them per context.

---

## `kaijutsu-types` — the leaf

A dependency-free foundation crate (no in-repo deps; `lib.rs:5`). Every identity,
block, and block-metadata shape lives here as a plain Rust type so the workspace
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
  Image. For `Image`, the `content` string holds a 32-char CAS hash, not bytes.
- **`BlockSnapshot`** (`block.rs:1172`) — the serializable replication unit: a flat
  struct with all fields present, mechanism-specific fields (`tool_*`, `drift_*`,
  `error`, `notification`, `resource`, `file_*`) as `Option`, plus `parent_id`,
  `order_key`, `tick`, `created_at`, and `updated_at` (wall-clock millis).
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
`NotificationPayload`, `ResourcePayload`, `VersionSnapshot` (line 503),
`TimeoutPolicy`.

Interfaces:

- **`World`** (line 879) — entry point: `whoami`, `listKernels`, `bindKernel`.
- **`Kernel`** (line 1576) — the main surface: kaish exec, VFS, block queries,
  subscriptions, MCP, peers, timeline nav, context lifecycle. **There is no
  client-facing method for editing block text** — clients follow a context
  through `subscribeContext` and mutate by asking the kernel to run something
  (`docs/change-feed.md`).
- **`ContextObserver`** (line 472) — the per-context change feed a client
  subscribes to.
- **`BlockEvents`** (line 560) — server→client callback; carries `seqNum` for
  dropped-event detection.
- **`Vfs`** (line 1100) — filesystem interface.
- **`PeerCommands`**, plus MCP callbacks (`ResourceEvents`, `ProgressEvents`,
  `ElicitationEvents`, `LoggingEvents`).

Evolution is tracked **only in comments** (no `@version`): see lines 921, 933,
1169 documenting removed methods whose ordinals were *renumbered/reused* — flagged
in [issues](../issues.md) because Cap'n Proto treats ordinals as permanent.

---

## `kaijutsu_kernel::blocks` — the block/document model

One ordered block log per context. The kernel is the sole sequencer for every
mutation, so nothing ever needs concurrent-branch reconciliation. Block order is
fractional indexing, metadata in `BlockHeader` is plain data, and text is a
plain `String`.

**Do not reintroduce a text CRDT for block content.** Streaming is 100% append
and `push_str` is amortized O(1), while per-block merge metadata measured about
4x the size of the text it represented (`docs/crdt-position-2026-08.md`).

### One storage impl: `BlockDocument`

`BlockDocument` (`blocks/block_store.rs:78`) is the single per-context storage
path — a `BTreeMap<BlockId, BlockContent>` where each block owns its content
as a plain `String`. Manages per-principal `seq_lanes`, a monotonic
`next_tick`, and a `version` counter. `block_ids_ordered()`
(`blocks/block_store.rs:238`) sorts by `order_key` (tiebreak `BlockId`) — never
iterate the `BTreeMap` for timeline order, it's principal-major. Append
`order_key` is the *successor* of the predecessor's key (`blocks/content.rs`),
decoupled from `tick` to avoid stale-counter mis-sorts. Sync via
`ops_since(frontiers) → SyncPayload` / `merge_ops`
(`blocks/block_store.rs:1232`, `:1273`) serves the client mirror's
incremental-fetch path only. **Concurrent merge is structurally impossible** —
`merge_ops` has no concurrent caller, and replay is sequential
self-application, so code that reasons about conflict resolution here is
reasoning about a state the system cannot reach. Persistence via `StoreSnapshot`
(`Vec<BlockSnapshot>`, CBOR).

`BlockDocument` is one context's block log. The kernel's own `BlockStore` is the
documents map, persistence, journaling, and flows wrapper around it — two
different types, one letter apart.

`BlockContent` (`blocks/content.rs:204`) is the per-block unit: `content:
String`, the `order_key`, an `Option<Tick>`, an `Option<TrackId>`, and
write-once snapshot fields.

### Other documents

- **`ConversationDAG`** (`dag.rs:15`) — an *ephemeral computed index*, not durable storage,
  over an ordered `Vec<BlockSnapshot>`; DFS/BFS, subtree, ancestors, depth, all
  circuit-broken at `MAX_DAG_DEPTH`.

### Document kinds

`DocKind` is defined in `kaijutsu-types` (`enums.rs`, three variants: `Conversation`, `File`, `Symlink`) but *implemented* here —
`kaijutsu_kernel::blocks` doesn't map kinds to backends; the kernel does.
Variants: **Conversation** (the dialog block log), **Code** (file-tool cache, one
doc per tracked file), **Text** (static markdown), **Config** (theme/models TOML),
**Symlink** (content *is* the link target). Legacy string aliases map onto these;
the retired `kv` tag is deliberately never reused (see the tombstone in
`enums.rs`).

### Smells (not fixed — see [issues](../issues.md))

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
