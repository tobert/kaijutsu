# The kernel

*Deep-dive companion to [README.md](README.md). Covers `kaijutsu-kernel` — the
largest crate (~82k LOC) and the instrument's body. Code is truth: every
pointer below names a symbol, not a line to trust blindly — `grep` it.*

The kernel owns context data, model interactions, the VFS, and tools. It does
**not** run an LLM turn (the server does); it supplies everything a turn needs.

---

## `Kernel` (`src/kernel.rs:41`)

Every field is `Arc`/`OnceLock`-wrapped. The coordinator owns: `vfs:
Arc<MountTable>`, `state: RwLock<KernelState>`, `llm: RwLock<LlmRegistry>`,
`peers: RwLock<PeerRegistry>`, `consent_mode`, three `FlowBus`es (`block_flows`,
`turn_flows`, input via the broker), `drift: SharedDriftRouter`, `cas:
Arc<FileStore>`, `image_backends`, `broker: Arc<Broker>`, `timeouts`,
`file_cache: OnceLock<Arc<FileDocumentCache>>`, `nonce_stores`, `timelines:
DashMap<ContextId, SharedTimeline>`, and `beat_ingress`.

Notably the `Kernel` **does not own a `BlockStore`** — it receives one at
`register_builtin_mcp_servers` (`kernel.rs:797`) and routes it into the broker.
Key methods: `dispatch_tool_via_broker` (`:539`), `attach_peer`/`invoke_peer`
(`:2064`/`:2078`), `arm_timeline`/`disarm_timeline` (`:1139`/`:1172`). `Kernel`
impls `VfsOps` by forwarding to `self.vfs` (`:2300`) so a kernel can be mounted
inside another — "everything is a kernel." (There is no `init_kv` — the KV
store it once seeded was demolished; kaish's VFS is the shared-state path now.)

---

## Subsystems

### Persistence — `KernelDb` (`src/kernel_db.rs:1824`)

SQLite (WAL, `rusqlite`), one `Connection` behind `Arc<Mutex<KernelDb>>`. Full
schema laid down on open (`SCHEMA`, `:500`); migrations are "bump = wipe" except
additive `ALTER TABLE` guards (`apply_additive_migrations`, `:2012`) that
propagate a genuine failure instead of swallowing it — only SQLite's
"duplicate column" case is treated as already-migrated. ~20 tables:

| Table(s) | Purpose |
|---|---|
| `kernel` | singleton identity |
| `workspaces`, `workspace_paths` | named path collections |
| `presets`, `preset_args` | model/filter patches (full/window/spawn) |
| `documents` | document registry |
| `contexts`, `context_edges` | per-conversation metadata + DAG edges (fork/drift provenance) |
| `oplog`, `doc_snapshots` | append-only op journal + compaction checkpoints |
| `input_oplog`, `input_doc_snapshots` | same, for per-context compose input docs |
| `context_shell`, `context_env` | per-context cwd + env overrides |
| `context_bindings` (+5 children) | per-context capability allow-sets (deny by default) |
| `hooks`, `hook_scripts` | match-action hooks + shared kaish bodies |
| `cache_breakpoints` | per-context Claude cache targets (set by rc) |
| `context_hydration` | windowed hydration marker + window size |

#### Backup & restore

`kernel.db` is WAL-mode SQLite, so a bare `cp` is quietly unsafe: committed
history can live in `kernel.db-wal` while the main file lags, producing a
torn or stale copy that *looks* fine. Two `kj db` verbs address this
(`src/kj/db.rs`):

- **`kj db backup <path>`** — `KernelDb::vacuum_into` (`kernel_db.rs`, next to
  `checkpoint()`) runs SQLite-native `VACUUM INTO ?1`: one consistent,
  already-compacted snapshot file, safe against a live writer, no quiesce
  needed first. `<path>` must be absolute (the kernel process's cwd isn't
  the caller's shell cwd, so a relative path can't be resolved
  predictably) and must not already exist — SQLite refuses to overwrite,
  and the verb will not pre-delete a same-named file to make room.
- **`kj db checkpoint`** — wraps the existing `KernelDb::checkpoint()`
  (`PRAGMA wal_checkpoint(TRUNCATE)`), reporting the busy case honestly.
  This is the quiesce step for people doing their own btrfs/ZFS snapshot or
  file copy: after a clean (non-busy) checkpoint, `kernel.db` alone
  reflects all committed history and `kernel.db-wal` is empty.

**Restore is deliberately not a `kj` verb.** The kernel holds in-memory
state (`BlockStore` documents, registries) that a live file swap would
desynchronize. To restore a backup: stop the kernel, replace `kernel.db`
(removing any `-wal`/`-shm` siblings alongside it), then start the kernel.

### Block documents — `BlockStore` (`src/block_store.rs:189`)

Kernel-level wrapper around `crate::blocks::BlockDocument`: a
`DashMap<ContextId, DocumentEntry>`, threaded with the `DbHandle` for
journaling.
`create_document`/`fork_document`/`fork_document_filtered` (`:428`/`:692`/`:826`),
`insert_block`/`insert_tool_call`/`insert_tool_result`, `set_excluded` (`:1618`),
`edit_text` (`:1481`), and cold-start `load_from_db`/`load_one_from_db`
(`:2437`/`:2672`). `BlockDocument::ops_since`/`merge_ops` (`blocks/block_store.rs`)
still exist, but `ops_since` has no caller outside its own tests and
`merge_ops` runs only sequential self-application during oplog replay —
concurrent merge is structurally impossible, not merely unobserved
(`docs/crdt-position-2026-08.md`).

### Context registry + drift — `DriftRouter` (`src/drift.rs:290`)

The **single source of truth for live contexts**: `contexts` map, `label_to_id`
index, a `staging` queue, a `dead_letter` list (after `MAX_DRIFT_RETRIES` = 5),
and a `lost+found` sink. `register`/`register_fork`/`unregister`,
`stage`/`drain`/`requeue`, `resolve_context` (label / label-prefix / hex-prefix),
`adopt_lost_found` for cold-start recovery. The router never *creates* the
lost+found context — `claim_lost_found` only claims an id whose DB row the caller
(`KjDispatcher::ensure_lost_found_context`) has already written, so a registered
handle always implies a KernelDb row; `restore_dead_letters` puts drained items
back when a write into the sink fails, so the flush cannot lose them.
`build_distillation_prompt` (`:1366`) lives here too.

### Events — `FlowBus<T>` (`src/flows.rs:1006`)

Topic-partitioned pub/sub (`async-broadcast`, NATS-style `*`/`>` wildcards).
Three buses: **block** (`BlockFlow`: Inserted, TextAppended, TextReplaced,
Deleted, StatusChanged, CollapsedChanged, ExcludedChanged, Moved,
OutputChanged, MetadataChanged, SpansChanged, ContextSwitched, RenderCue,
BeatSync — each carrying `OpSource` Local/Remote to break echo loops).
`TextAppended`/`TextReplaced` are the append-or-replace classification the
per-context change feed carries instead of decoded text-engine operations
(`docs/change-feed.md`); there is no `SyncReset` — a replica has nothing to
reset. Also **input-doc**, and **turn** (`turn.requested`/`completed`/`failed`).

### Peers — `PeerRegistry` (`src/peers.rs:115`)

Named RPC callbacks (the Bevy app, external MCP). Attach with a `nick` and an
`mpsc::Sender<InvokeRequest>`; re-attach replaces. `invoke_peer` dispatches via a
oneshot with a timeout.

### Misc

`KernelState` (`state.rs:16`) — **in-memory only** vars/history/checkpoints (lost
on restart). `execution.rs` — `ExecContext`/`ExecResult` data shims.
`config_seed.rs` — the embedded default bodies (`theme.toml`, `mcp.toml`,
`system.md`) that seed the `/config` host directories only while empty
(`docs/config-namespace.md`). `config_doc.rs` and the `ConfigDocFs` backend it
served are gone — every `/config` tree mounts `LocalBackend` over a real host
directory. `seed_presets.rs`, `seed_scripts.rs`
— idempotent boot-time seeding (presets; the `/config/rc` tree via
`include_dir!`).

---

## The embedded shell + VFS

The old design ran kaish as a separate sandboxed process over a socket. **The code
embeds it.** See [overview](README.md#process--transport-model).

### `EmbeddedKaish` (`src/runtime/embedded_kaish.rs:59`)

Owns one `kaish_kernel::Kernel`, a `SessionContextMap`, a `SessionId`, and the
timeout policy. `with_identity_mode` (`:265`) is the builder: registers the
session→context pair, builds the input filesystem, gets the shared
`FileDocumentCache`, builds `KaijutsuBackend`, clones the `Arc<MountTable>`, wraps
it in a `MountBackend` (writable or read-only), and constructs the kaish kernel
with `/v/docs` and `/v/input` mounted. `execute_with_options` (`:463`) is the
single entry. cwd persists in the kaish kernel and is restored from the DB via the
**backend namespace**, not host-FS `is_dir()` (`restore_cwd_from_db`, `:556`).

Backends: **`KaijutsuBackend`** (routes `/docs/{ctx}/{block}` into the block
store + tool dispatch), **`MountBackend`** (the primary `KernelBackend`; routes
file I/O through the document cache on writable mounts, raw VFS otherwise;
`deny_if_read_only` gates mutations), **`KaijutsuFilesystem`** /
**`InputFilesystem`** (adapt kernel documents and input to the kaish
`Filesystem` trait), **`ReadOnlyFs`** (refuses all mutations), **`SwapFilesystem`**
(read-only view of unflushed file buffers at `/v/swap/<kernel-id>/<real-path>`,
`docs/file-buffers.md`). `SessionContextMap` is a global `DashMap<SessionId, ContextId>`.

### VFS (`src/vfs/`)

`VfsOps` (`ops.rs:29`) — path-based async ops, no inodes; `real_path` returns
`Some` for Local, `None` for Memory. `MountTable` (`mount.rs:57`) impls `VfsOps`,
routes by longest-prefix match, errors on cross-mount rename, and can `freeze()`
(after which mount/unmount are rejected — mounts are fixed at startup).
`LocalBackend` (real FS, canonicalized + root-jailed) and `MemoryBackend`
(in-memory; note it uses a *blocking* `std::sync::RwLock`). Server mount layout
(`create_shared_kernel`, `kaijutsu-server/src/rpc.rs`): read-only `/`, an opaque
`/dev` (browsable but skipped by ambient sweeps), read-write `~/src` and `/tmp`,
the `/config` trees (`/config/rc`, `/config/kernel`, `/config/client`,
`/config/midi`), the ephemeral runtime views `/run/midi`, `/run/audio`,
`/run/roster`, the read-only CAS mount `/v/cas`, and the client-share registry
`/r`; then frozen.

### File cache (`src/file_tools/cache.rs:154`)

`FileDocumentCache` is the bridge that makes shell builtins and MCP file tools
share **one kernel document per real file**. Key = `file_context_id(path)` (UUIDv5
over `"kaijutsu:file:{path}"`) after lexical canonicalization (`path.rs`), so
`foo.rs`, `./foo.rs`, `/abs/foo.rs` all collapse to one key. Cache miss loads via
VFS → creates a `DocKind::File` doc; hits check `disk_mtime > loaded_mtime` and
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

No trait — a closed `enum Provider` (`mod.rs:405`) with exhaustive `match`
dispatch: `Claude` (real), `OpenAi` (real, generic compat core), `DeepSeek` (thin
preset over OpenAi, `reasoning_required: true`), `CodexApp` (connect-only client
for a configured Codex app-server daemon over `ws://`/`wss://` — the provider
never launches Codex itself, see `docs/codex-app-backend.md`), `Mock` (test,
cfg-gated). There is no `Gemini` variant — the earlier stub was removed rather
than finished; add a real provider (or point the OpenAI-compatible core at
Google's OpenAI-shaped endpoint) when one is needed. `LlmRegistry` (`mod.rs:1006`)
holds named providers, default, aliases, per-provider config. Streaming:
`provider.stream(...) → ProviderStream` whose `next_event()` yields
provider-agnostic `StreamEvent`s (Thinking/Text start-delta-end, ToolUse, Done,
Error). Cancellation via `CancellationToken` in a `biased` select. Reasoning
continuity: `ContentBlock::Reasoning { text, signature }` re-emits each thinking
block with its exact provider signature on tool-use turns. Credential resolution
(`config.rs:273`, `resolve_api_key`): `api_key_file` (`~`-expanded, first hit
wins) → the backend's env var; there is deliberately no third source, config
holds no inline key material.

### MCP broker (`src/mcp/`)

See [overview](README.md#tool-dispatch-the-mcp-broker). `McpServerLike`
(`server_like.rs:51`) is the one interface. `Broker` (`broker.rs:122`) holds
instances, bindings, per-instance policies + semaphores, hook tables, a
notification fan-out, and the block store (to emit `Notification` blocks).
`list_visible_tools` (`:1455`) filters by binding then resolves visible names
(unqualified if unique, else `instance__tool`, cleaned to Anthropic's pattern,
sticky once set). `call_tool` (`:1560`): binding check → semaphore → PreCall hooks
→ call raced against timeout+cancel → truncate → PostCall → OnError. External
servers (`servers/external.rs`) wrap `rmcp` over stdio/HTTP and inject identity +
W3C trace into `_meta`; reconnect is manual-only, never automatic on a health
flip to `Down`. `external_registry::reconcile_external_mcp_servers` is the
actual caller that reads `/config/kernel/mcp.toml` and registers/connects
configured servers into the broker — at boot and on `kj mcp reload` — closing
the gap the module doc once named ("the caller `ExternalMcpServer` always
lacked").

### kj surface (`src/kj/`)

`KjDispatcher` (`mod.rs:267`) holds drift router, block store, KernelDb, kernel,
and the semantic index. `dispatch` (`:391`) string-matches `argv[0]`; some verbs
require an active context. `kj_command()` (`:975`) builds a full clap tree **for
schema reflection only** — routing stays in `dispatch` (the single-source caveat is
manual). `require_cap` (`:657`) reads the binding from KernelDb and gates
whichever `Capability` its caller names, distinguishing three outcomes (a DB read
failure, no usable loadout, and an unauthorized-but-readable loadout) rather than
collapsing them into one denial. Escalation-relevant verbs gate on the five
authorities: `drive`→`Capability::Drive`, `fork`→`Fork`, `drift`→`Drift`,
`transport`→`Transport`, lifecycle verbs→`Operator`. `KjCaller` carries a
`privileged` flag stamped at construction (never derived from agent-settable env),
and `rc_depth` capped to prevent runaway recursion.

### Block tools + image (`src/mcp/servers/block.rs`, `src/image/`)

`BlockToolsServer` (`mcp/servers/block.rs`) exposes 8 structural tools
(`block_create`/`append`/`edit`/`splice`/`read`/`search`/`list`/`status`) plus
cross-block `kernel_search` and the content tools `svg_block` (`usvg`-validated,
`block_store.rs`), `abc_block` (parse-validated), `img_block`/`img_block_from_path`
(CAS-backed). `src/block_tools/` itself now holds only the line↔byte/char
translation helpers (`translate.rs`) the server calls into, not the tools
themselves. `image` is an `ImageBackend` trait + `ImageBackendRegistry`
(streaming byte generation); the actual generate-image-block pipeline lives in the
server.

---

## Lifecycle: how fork/new/drift hook in

Context creation writes `KernelDb` rows, then `DriftRouter::register[_fork]`, then
runs the **rc lifecycle scripts** under `/config/rc/<context_type>/<verb>/` (kaish
scripts, sort-key order): `create` on new, `fork` on fork, `drift` on drift,
`tick` on each beat. These set cache breakpoints, tool bindings, and the
hydration marker. On fork, `fork_document_filtered` copies the parent document,
applying `ForkBlockFilter` to drop curated-out blocks — which is why exclude/edit
take effect "at fork."

---

## Smells (not fixed — see [issues](../issues.md))

- **`KernelDb` god-table** — ~14k lines, ~20 tables, every write behind one
  full-DB mutex. The file's own header comment names this a recognized
  "god-table + single-mutex" smell and records the decision **not** to split
  it pre-emptively (`kernel_db.rs:8`) — revisit only once write-contention
  under concurrent contexts is an observed problem, not a theoretical one.
- **Dual kernel identity** — `Kernel::id()` (`kernel.rs:473`) vs the separate
  `KernelState.id` (`state.rs:16`, read at `kernel.rs:1362`).
- **Kernel-facade vs `MountTable`** — some callers go through `Kernel::mount`/
  `Kernel::vfs()`, others (e.g. `FileDocumentCache`) hold their own cloned
  `Arc<MountTable>` directly. Harmless today (one shared table behind every
  clone), but nothing enforces that a future second table couldn't diverge.

No silent fallbacks remain in this crate's core paths: `list_tool_defs_via_broker`
propagates a broker error instead of returning an empty list (`kernel.rs:757`);
`Broker::binding_checked` and `KjDispatcher::require_cap` (`mcp/broker.rs:1404`)
distinguish a DB read failure from a genuine empty binding rather than
collapsing both into deny-all; `apply_additive_migrations` propagates anything
but SQLite's "duplicate column" case (tested, `kernel_db.rs:8257`); block
editing is hashline-addressed (`file_tools/hashline.rs`), not byte-offset;
`new_ephemeral`'s temp dir is cleaned up by a `Drop`-guarded `TempDirGuard`
(`kernel.rs:212`); and `LocalBackend::setattr` actually sets mtime (tested,
`vfs/backends/local.rs:964`).
