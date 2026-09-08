# Foundation: types, the block/document model, and the wire schema

*Deep-dive companion to [README.md](README.md). Covers `kaijutsu-types`,
`kaijutsu_kernel::blocks`, and `kaijutsu.capnp`. Code is truth: every pointer
below names a symbol — `grep` it.*

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
all newtypes over `uuid::Uuid` (`ids.rs:30`), generated as **UUIDv7**
(time-ordered). `PrincipalId::system()` and `PrincipalId::beat()` are deterministic
UUIDv5 sentinels (`ids.rs:229`, `ids.rs:248`) — `beat()` is the author lane for
machine-generated timeline fallbacks. The `impl_typed_id!` macro (`ids.rs:62`)
gives every id the same surface (`new`, `short`, `to_hex`, `parse`, `nil`); a
`PrefixResolvable` trait (`ids.rs:211`) enables generic prefix lookup.

Birth-certificate structs: `Context` (`context.rs:24`, with `forked_from` +
`fork_lineage()` at `:73`), `Kernel` (`kernel.rs:18`), `Session`
(`session.rs:15`). There is no `Principal` struct with a name on it —
`principal.rs` holds only `Credential` (`kind`, `fingerprint`, `principal_id`,
no name field) and `CredentialKind`. A principal is a bare `PrincipalId`; the
given name a player reads is `characters.name`, resolved through
`KernelDb::name_for` (`docs/character.md`, "`auth.db` is a keyring").

### The block (`block.rs`)

- **`BlockId`** (`block.rs:38`) = `(context_id, principal_id, seq)`. Identity, not
  position. Key form `"{ctx_hex}_{principal_hex}_{seq}"` with a legacy `:`
  delimiter still accepted (`block.rs:78`).
- **`BlockKind`** (`block.rs:1190`) — 11 structural variants: Text, Thinking,
  ToolCall, ToolResult, Drift, File, Error, Notification, Resource, Trace, Task
  (the household-agent surface, `builtin.tasks`).
- **`ContentType`** (`block.rs:294`) — 6 render hints: Plain, Markdown, Svg, Abc,
  Diff, Image. For `Image`, the `content` string holds a 32-char CAS hash, not
  bytes.
- **`BlockSnapshot`** (`block.rs:1637`) — the serializable projection unit: a flat,
  38-field struct with mechanism-specific fields (`tool_*`, `drift_*`, `error`,
  `notification`, `resource`, `file_*`) as `Option`, plus `parent_id`,
  `order_key`, `tick`, `created_at`, and `updated_at` (wall-clock millis).
- **`BlockHeader`** (`block.rs:134`) — a `Copy` subset for DAG traversal without
  content.

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
`BlockSnapshot.track` (`codec.rs:140`).

---

## `kaijutsu.capnp` — the wire schema

Schema id `@0xb8e3f4a9c2d1e0f7`, ~2,540 lines. Major shapes mirror the Rust types
1:1: `BlockId` (line 26), `BlockSnapshot` (line 135, 45 fields with `has*`
sentinels for null-less value types), `BlockMetadata`, `ErrorPayload`,
`NotificationPayload`, `ResourcePayload`, `VersionSnapshot` (line 754),
`TimeoutPolicy`.

Interfaces:

- **`World`** (line 1837) — entry point: `whoami`, `listKernels`, `bindKernel`.
- **`Kernel`** (line 1858) — the main surface: kaish exec, VFS, block queries,
  subscriptions, MCP, peers, timeline nav, context lifecycle. **There is no
  client-facing method for editing block text** — clients follow a context
  through `subscribeContext` and mutate by asking the kernel to run something
  (`docs/change-feed.md`).
- **`ContextObserver`** (line 723) — the per-context change feed a client
  subscribes to.
- **`BlockEvents`** (line 811) — server→client callback; carries `seqNum` for
  dropped-event detection.
- **`Vfs`** (line 1390) — filesystem interface. Thirteen of its original
  seventeen methods are retired: general filesystem access was superseded by
  SFTP, and the four that remain are the ones the app actually uses.
- **`PeerCommands`** (line 1630), plus MCP callbacks (`ResourceEvents`,
  `ProgressEvents`, `ElicitationEvents`, `LoggingEvents`).

Evolution is tracked **only in comments** (no `@version`), but the convention is
strict and enforced by the comments themselves: every interface's header says
"ordinals are dense and permanent — never reuse one," and retiring a method
leaves a `retiredNN @NN ();` stub rather than renumbering or reusing its slot.
There is no live case of ordinal reuse in this schema today.

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

`BlockDocument` (`blocks/block_store.rs:74`) is the single per-context storage
path — a `BTreeMap<BlockId, BlockContent>` where each block owns its content
as a plain `String`. Manages per-principal `seq_lanes`, a monotonic
`next_tick`, and a `version` counter. `block_ids_ordered()`
(`blocks/block_store.rs:213`) sorts by `order_key` (tiebreak `BlockId`) — never
iterate the `BTreeMap` for timeline order, it's principal-major. Append
`order_key` is the *successor* of the predecessor's key (`blocks/content.rs`),
decoupled from `tick` to avoid stale-counter mis-sorts. `ops_since(known) →
SyncPayload` / `merge_ops` (`blocks/block_store.rs:1248`, `:1289`) exist, but
serve neither the wire nor the client mirror today — the change feed carries
`TextAppended`/`TextReplaced` classification instead, and `ops_since` has no
caller outside its own tests. `merge_ops` still runs, but only for sequential
self-application during cold-start oplog replay. **Concurrent merge is
structurally impossible** — there is no concurrent caller, so code that
reasons about conflict resolution here is reasoning about a state the system
cannot reach. Persistence via `StoreSnapshot` (`Vec<BlockSnapshot>`, CBOR).

`BlockDocument` is one context's block log. The kernel's own `BlockStore` is the
documents map, persistence, journaling, and flows wrapper around it — two
different types, one letter apart.

`BlockContent` (`blocks/content.rs:195`) is the per-block unit: `content:
String`, the `order_key`, an `Option<Tick>`, an `Option<TrackId>`, and
write-once snapshot fields.

### Other documents

- **`ConversationDAG`** (`kaijutsu-types/src/dag.rs:18`) — an *ephemeral
  computed index*, not durable storage, over an ordered `Vec<BlockSnapshot>`;
  DFS/BFS, subtree, ancestors, depth, all circuit-broken at `MAX_DAG_DEPTH`
  (512). Lives in `kaijutsu-types`, not `kaijutsu_kernel::blocks`.

### Document kinds

`DocKind` is defined in `kaijutsu-types` (`enums.rs:174`, three variants) but
*implemented* here — `kaijutsu_kernel::blocks` doesn't map kinds to backends,
the kernel does. **Conversation** (the dialog block log) and **Symlink**
(content *is* the link target; no backend creates one today — `/config` and
rc are ordinary host directories — but the kind is a persisted enum kept for
any future document-backed mount that wants it) are single-purpose. **File**
collapsed what used to be four separate kinds — source, markdown, and config
were once distinct because nothing else carried the distinction; now
`language` does, so `code`/`text`/`markdown`/`config` are retired string
spellings that still parse into `File` for a row written before the
collapse.

### Smells (not fixed — see [issues](../issues.md))

- `calc_order_key` calls `block_ids_ordered()` (an O(N) sort) on **every** insert
  (`block_store.rs:426`).
- Tombstones aren't a first-class `BlockSnapshot` property; they ride a
  `deleted_blocks: Vec<BlockId>` side list on `SyncPayload`/`StoreSnapshot`
  instead (`block_store.rs:1704`, `:1742`).

### Types-crate smells

- `ThemeData` (`theme.rs:67`) — a large visual struct with an `include_str!`
  to `assets/defaults/theme.toml` lives in the *foundation* crate: a layering
  violation.
- `BlockSnapshot` is a 38-field flat struct with no discriminated union; invalid
  field combinations aren't type-prevented on deserialize.
- Vestigial/dead: `is_error` flagged "legacy" (`block.rs:1696`),
  `DriftKind::Notification` vs `BlockKind::Notification` name collision.
