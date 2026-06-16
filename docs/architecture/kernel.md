# The kernel

*Deep-dive companion to [README.md](README.md). Covers `kaijutsu-kernel` — the
largest crate (~82k LOC) and the orchestration hub. Code is truth; verified
2026-06-16.*

The kernel owns context data, model interactions, the VFS, and tools. It does
**not** run an LLM turn (the server does); it supplies everything a turn needs.

---

## `Kernel` (`src/kernel.rs:40`)

Every field is `Arc`/`OnceLock`-wrapped. The coordinator owns: `vfs:
Arc<MountTable>`, `state: RwLock<KernelState>`, `llm: RwLock<LlmRegistry>`,
`peers: RwLock<PeerRegistry>`, `consent_mode`, three `FlowBus`es (`block_flows`,
`turn_flows`, input via the broker), `drift: SharedDriftRouter`, `cas:
Arc<FileStore>`, `image_backends`, `broker: Arc<Broker>`, `timeouts`,
`file_cache: OnceLock<Arc<FileDocumentCache>>`, `nonce_stores`, `timelines:
DashMap<ContextId, SharedTimeline>`, `beat_ingress`, and `kv: OnceLock<Arc<Kv>>`.

Notably the `Kernel` **does not own a `BlockStore`** — it receives one at
`register_builtin_mcp_servers` (`kernel.rs:480`) and routes it into the broker.
Key methods: `dispatch_tool_via_broker` (`:296`), `attach_peer`/`invoke_peer`
(`:980`/`:994`), `arm_timeline`/`disarm_timeline` (`:680`/`:713`), `init_kv`
(`:747`). `Kernel` impls `VfsOps` by forwarding to `self.vfs` (`:1070`) so a
kernel can be mounted inside another — "everything is a kernel."

---

## Subsystems

### Persistence — `KernelDb` (`src/kernel_db.rs:849`)

SQLite (WAL, `rusqlite`), one `Connection` behind `Arc<Mutex<KernelDb>>`. Full
schema laid down on open (`SCHEMA`, `:250`); migrations are "bump = wipe" except
two additive `ALTER TABLE` guards (`:868`). ~20 tables:

| Table(s) | Purpose |
|---|---|
| `kernel` | singleton identity |
| `workspaces`, `workspace_paths` | named path collections |
| `presets`, `preset_args` | model/filter patches (full/window/spawn) |
| `documents` | CRDT document registry |
| `contexts`, `context_edges` | per-conversation metadata + DAG edges (fork/drift provenance) |
| `oplog`, `doc_snapshots` | append-only CRDT op journal + compaction checkpoints |
| `input_oplog`, `input_doc_snapshots` | same, for per-context compose input docs |
| `context_shell`, `context_env` | per-context cwd + env overrides |
| `context_bindings` (+5 children) | per-context capability allow-sets (deny by default) |
| `hooks`, `hook_scripts` | match-action hooks + shared kaish bodies |
| `cache_breakpoints` | per-context Claude cache targets (set by rc) |
| `context_hydration` | windowed hydration marker + window size |

### CRDT documents — `BlockStore` (`src/block_store.rs:180`)

Kernel-level wrapper around `kaijutsu_crdt`: a `DashMap<ContextId,
DocumentEntry>`, threaded with the `DbHandle` for journaling.
`create_document`/`fork_document`/`fork_document_filtered` (`:387`/`:535`/`:669`),
`insert_block`/`insert_tool_call`/`insert_tool_result`, `set_excluded` (`:1474`),
`edit_text` (`:1388`), `ops_since`/`merge_ops`, and cold-start
`load_from_db`/`load_one_from_db` (`:2142`/`:2271`).

### KV — `Kv` (`src/kv.rs:122`)

Persistent CRDT KV: a `KvDocument` (LWW per key) with values in a versioned JSON
`Envelope`. Journals to the oplog under a deterministic `root_context_id()`
(UUIDv5); compaction at 200 ops. `subscribe` yields a `broadcast::Receiver<KvChange>`.
In embedded/test kernels `kv()` returns `None` — callers must degrade.

### Context registry + drift — `DriftRouter` (`src/drift.rs:130`)

The **single source of truth for live contexts**: `contexts` map, `label_to_id`
index, a `staging` queue, a `dead_letter` list (after `MAX_DRIFT_RETRIES` = 5),
and a lazily-created `lost+found` sink. `register`/`register_fork`/`unregister`,
`stage`/`drain`/`requeue`, `resolve_context` (label / label-prefix / hex-prefix),
`adopt_lost_found` for cold-start recovery. The distillation-prompt builder lives
here too (`:602`).

### Events — `FlowBus<T>` (`src/flows.rs:514`)

Topic-partitioned pub/sub (`async-broadcast`, NATS-style `*`/`>` wildcards).
Three buses: **block** (`BlockFlow`: Inserted, TextOps, Deleted, StatusChanged,
CollapsedChanged, ExcludedChanged, Moved, SyncReset, Output, Metadata,
ContextSwitched — each carrying `OpSource` Local/Remote to break echo loops),
**input-doc**, and **turn** (`turn.requested`/`completed`/`failed`).

### Peers — `PeerRegistry` (`src/peers.rs:71`)

Named RPC callbacks (the Bevy app, external MCP). Attach with a `nick` and an
`mpsc::Sender<InvokeRequest>`; re-attach replaces. `invoke_peer` dispatches via a
oneshot with a timeout.

### Misc

`KernelState` (`state.rs:16`) — **in-memory only** vars/history/checkpoints (lost
on restart). `execution.rs` — `ExecContext`/`ExecResult` data shims.
`input_doc.rs` — per-context compose scratchpad (a single-text DTE doc, separate
oplog tables). `config_backend.rs` (`:193`) — theme/models/mcp/system.md as CRDT
docs, debounced flush to disk + `notify` watcher reload. `seed_presets.rs`,
`seed_scripts.rs` — idempotent boot-time seeding (presets; the `/etc/rc` tree via
`include_dir!`).

---

## The embedded shell + VFS

The old design ran kaish as a separate sandboxed process over a socket. **The code
embeds it.** See [overview](README.md#process--transport-model).

### `EmbeddedKaish` (`src/runtime/embedded_kaish.rs:56`)

Owns one `kaish_kernel::Kernel`, a `SessionContextMap`, a `SessionId`, and the
timeout policy. `with_identity_mode` (`:166`) is the builder: registers the
session→context pair, builds the CRDT input filesystem, gets the shared
`FileDocumentCache`, builds `KaijutsuBackend`, clones the `Arc<MountTable>`, wraps
it in a `MountBackend` (writable or read-only), and constructs the kaish kernel
with `/v/docs` and `/v/input` mounted. `execute_with_options` (`:309`) is the
single entry. cwd persists in the kaish kernel and is restored from the DB via the
**backend namespace**, not host-FS `is_dir()` (`restore_cwd_from_db`, `:399`).

Backends: **`KaijutsuBackend`** (routes `/docs/{ctx}/{block}` into the CRDT store +
tool dispatch), **`MountBackend`** (the primary `KernelBackend`; routes file I/O
through the CRDT cache on writable mounts, raw VFS otherwise; `deny_if_read_only`
gates mutations), **`KaijutsuFilesystem`** / **`InputFilesystem`** (adapt CRDT
docs/input to the kaish `Filesystem` trait), **`ReadOnlyFs`** (refuses all
mutations). `SessionContextMap` is a global `DashMap<SessionId, ContextId>`.

### VFS (`src/vfs/`)

`VfsOps` (`ops.rs:20`) — path-based async ops, no inodes; `real_path` returns
`Some` for Local, `None` for Memory. `MountTable` (`mount.rs:34`) impls `VfsOps`,
routes by longest-prefix match, errors on cross-mount rename, and can `freeze()`
(after which mount/unmount are rejected — mounts are fixed at startup).
`LocalBackend` (real FS, canonicalized + root-jailed) and `MemoryBackend`
(in-memory; note it uses a *blocking* `std::sync::RwLock`). Server mount layout
(`rpc.rs:1019`): read-only `/`, read-write `~/src`, `/tmp`, and `/etc/rc`; then
frozen.

### File cache (`src/file_tools/cache.rs:45`)

`FileDocumentCache` is the bridge that makes shell builtins and MCP file tools
share **one CRDT document per real file**. Key = `file_context_id(path)` (UUIDv5
over `"kaijutsu:file:{path}"`) after lexical canonicalization (`path.rs`), so
`foo.rs`, `./foo.rs`, `/abs/foo.rs` all collapse to one key. Cache miss loads via
VFS → creates a `DocKind::Code` doc; hits check `disk_mtime > loaded_mtime` and
reload if stale (dirty entries are never refreshed — local edits win). Write-through
is `create_or_replace → mark_dirty → flush_one`; flush stamps `loaded_mtime` from
the post-write `getattr` so the flush isn't mistaken for an external change. LRU
cap 64 (dirty never evicted). File-tool engines (read/edit/write/glob/grep) all
hold this cache + an optional `WorkspaceGuard` (KernelDb path bounds).

---

## LLM, MCP broker, kj

These three subsystems are detailed in the [server](server.md) (streaming path)
and below.

### LLM providers (`src/llm/`)

No trait — a closed `enum Provider` (`mod.rs:353`) with exhaustive `match`
dispatch: `Claude` (real), `OpenAi` (real, generic compat core), `DeepSeek` (thin
preset over OpenAi, `reasoning_required: true`), `Gemini` (**stub** — returns
`Unavailable`), `Mock` (test). `LlmRegistry` (`mod.rs:611`) holds named providers,
default, aliases, per-provider config. Streaming: `provider.stream(...) →
ProviderStream` whose `next_event()` yields provider-agnostic `StreamEvent`s
(Thinking/Text start-delta-end, ToolUse, Done, Error). Cancellation via
`CancellationToken` in a `biased` select. Reasoning continuity:
`ContentBlock::Reasoning { text, signature }` re-emits each thinking block with its
exact provider signature on tool-use turns. Credential resolution
(`config.rs:106`): inline key → `api_key_file` (`~`-expanded) → env var; OpenAI
family allows no key.

### MCP broker (`src/mcp/`)

See [overview](README.md#tool-dispatch-the-mcp-broker). `McpServerLike`
(`server_like.rs:41`) is the one interface. `Broker` (`broker.rs:122`) holds
instances, bindings, per-instance policies + semaphores, hook tables, a
notification fan-out, and the block store (to emit `Notification` blocks).
`list_visible_tools` (`:1081`) filters by binding then resolves visible names
(unqualified if unique, else `instance__tool`, cleaned to Anthropic's pattern,
sticky once set). `call_tool` (`:1184`): binding check → semaphore → PreCall hooks
→ call raced against timeout+cancel → truncate → PostCall → OnError. External
servers (`servers/external.rs`) wrap `rmcp` over stdio/HTTP and inject identity +
W3C trace into `_meta`; reconnect is manual-only (Phase 1).

### kj surface (`src/kj/`)

`KjDispatcher` (`mod.rs:234`) holds drift router, block store, KernelDb, kernel,
and the semantic index. `dispatch` (`:323`) string-matches `argv[0]`; some verbs
require an active context. `kj_command()` (`:591`) builds a full clap tree **for
schema reflection only** — routing stays in `dispatch` (the single-source caveat is
manual). `require_cap` (`:474`) reads the binding from KernelDb and gates the five
authorities: `drive`→`Capability::Drive`, `fork`→`Fork`, `drift`→`Drift`,
`transport`→`Transport`, lifecycle verbs→`Operator`. `KjCaller` carries a
`privileged` flag stamped at construction (never derived from agent-settable env),
and `rc_depth` capped to prevent runaway recursion.

### Block tools + image (`src/block_tools/`, `src/image/`)

`block_tools` has 9 structural engines (create/append/edit/splice/read/search/
list/status + cross-block `kernel_search`) and 4 content engines (`svg_block` with
`usvg` validation, `abc_block`, `img_block`, `img_block_from_path`), wrapped by
`BlockToolsServer`. `image` is an `ImageBackend` trait + `ImageBackendRegistry`
(streaming byte generation); the actual generate-image-block pipeline lives in the
server.

---

## Lifecycle: how fork/new/drift hook in

Context creation writes `KernelDb` rows, then `DriftRouter::register[_fork]`, then
runs the **rc lifecycle scripts** under `/etc/rc/<context_type>/<verb>/` (kaish
scripts, sort-key order): `create` on new, `fork` on fork, `drift` on drift,
`tick` on each beat. These set cache breakpoints, tool bindings, and the
hydration marker. On fork, `fork_document_filtered` copies the parent CRDT doc,
applying `ForkBlockFilter` to drop curated-out blocks — which is why exclude/edit
take effect "at fork."

---

## Smells (not fixed — see [issues](../issues.md))

- **`KernelDb` god-table** — ~5,900 lines, ~20 tables, every write behind one
  full-DB mutex (write-concurrency bottleneck).
- **Silent fallbacks** counter to the project stance: `list_tool_defs_via_broker`
  returns `Vec::new()` on any broker error (`kernel.rs:467`); binding resolve
  `unwrap_or_default()`s to deny-all (`kernel.rs:346`); additive migrations
  `let _ =` swallow SQL errors (`kernel_db.rs:873`); `MountBackend::read` falls
  through to raw VFS on *any* cache error, risking stale on-disk content.
- **Dual kernel identity** — `Kernel.id` vs legacy `KernelState.id` (`kernel.rs:768`).
- **UTF-8 offset hazard** — `EditEngine` passes byte offsets/lengths to
  `edit_text` (`file_tools/edit.rs:132`) while `FileDocumentCache` carefully uses
  char counts (`cache.rs:276`); multi-byte content can corrupt the splice.
- **`new_ephemeral` leaks** a `/tmp/kj-eph-*` dir on panic/`exit` before drop.
- **`LocalBackend::setattr` mtime is a no-op** (`local.rs:354`) yet mtime is
  load-bearing for cache staleness.
- **Kernel-facade vs MountTable** — some callers go through `Kernel::mount`, others
  grab the raw `Arc<MountTable>`; no invariant about which is authoritative.
