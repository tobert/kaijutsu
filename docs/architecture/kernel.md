# The kernel

*Deep-dive companion to [README.md](README.md). Covers `kaijutsu-kernel` — the
instrument's body. Source symbols identify implementation owners; numeric
line references in older sections may have moved.*

The kernel owns context data, model interactions, the VFS, and tools. It does
**not** run an LLM turn (the server does); it supplies everything a turn needs.

---

## `Kernel` (`src/kernel.rs:41`)

`Kernel` owns the shared `BlockStore`, `KernelDb` handle, VFS mount table,
and `FileDocumentCache`. Constructors build these services together; callers
use `blocks()`, `kernel_db()`, `vfs()`, and `file_cache()` to reach the same
instances. `new` and `with_flows` share initialization.

Other owners include the model registry, broker, peer registry, drift router,
CAS, timeout and consent policies, timelines, and audio registries. Four flow
buses carry block, turn, editor, and ledger events. `ShellOperationRegistry`
owns durable operation receipts and the per-context kaish job managers.

The server still owns the headless turn and approval-resume drivers. The
planned move into kernel runtime ownership is separate from shared shell
construction and settlement; see `docs/kaish-integration.md`. `Kernel`
implements `VfsOps` by forwarding to its mount table.

---

## Subsystems

### Persistence — `KernelDb` (`src/kernel_db.rs:1824`)

SQLite (WAL, `rusqlite`), one `Connection` behind `Arc<Mutex<KernelDb>>`. Full
schema laid down on open (`SCHEMA`, `:500`); migrations are "bump = wipe" except
additive `ALTER TABLE` guards (`apply_additive_migrations`, `:2012`) that
propagate a genuine failure instead of swallowing it — only SQLite's
"duplicate column" case is treated as already-migrated. Selected tables:

| Table(s) | Purpose |
|---|---|
| `kernel` | singleton identity |
| `workspaces`, `workspace_paths` | named path collections |
| `presets`, `preset_args` | model/filter patches (full/window/spawn) |
| `documents` | document registry |
| `contexts`, `context_edges` | per-conversation metadata + DAG edges (fork/drift provenance) |
| `oplog`, `doc_snapshots` | append-only op journal + compaction checkpoints |
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

Topic-partitioned publish/subscribe with NATS-style `*`/`>` wildcards. The
kernel owns block, turn, editor, and ledger buses. Block mutations publish
complete acceptance groups; ordered subscribers receive them without loss or
are terminated for recovery. Timing directives use a separate lane.

Compose input is a `Draft` block and travels on the block feed. There is no
input-document bus. `TextAppended` and `TextReplaced` carry the kernel's text
classification; clients do not decode storage operations. See
`docs/change-feed.md` for grouping, ordering, and recovery.

### Peers — `PeerRegistry` (`src/peers.rs:115`)

Named RPC callbacks (the Bevy app, external MCP). Attach with a `nick` and an
`mpsc::Sender<InvokeRequest>`; re-attach replaces. `invoke_peer` dispatches via a
oneshot with a timeout.

### Misc

`execution.rs` — `ExecContext`/`ExecResult` data shims. Shell cwd and exported
variables live in `context_shell` and `context_env`; each invocation gets its
own kaish scope. `Kernel::id()` supplies kernel identity, and the kernel name
has its own lock.
`config_seed.rs` — the embedded default bodies (`theme.toml`, `mcp.toml`,
`system.md`) that seed the `/config` host directories only while empty
(`docs/config-namespace.md`). `config_doc.rs` and the `ConfigDocFs` backend it
served are gone — every `/config` tree mounts `LocalBackend` over a real host
directory. `seed_presets.rs`, `seed_scripts.rs`
— idempotent boot-time seeding (presets; the `/config/rc` tree via
`include_dir!`).

---

## The embedded shell + VFS

Kaish runs in-process against the kernel VFS and shared file cache. Rc is a
separate lifecycle consumer of that interpreter. The complete caller migration
and intended ownership are in [Kaish integration and rc lifecycle](../kaish-integration.md).
The description here is the current implementation.

### Construction and execution

`runtime/embedded_kaish.rs::EmbeddedKaish` owns one kaish kernel, invocation
session/context tracking, and timeout policy. Its common constructor wires
`MountBackend`, `/v/docs`, `/v/swap`, host execution policy, output limits,
and the context's shared JobManager. Foreground and background execution
methods both propagate tracing. Background work can outlive the shell that
started it.

`kj/context_shell.rs` supplies the shared context-shell factory used by
interactive commands, model shells, rc, hooks, and editor reads. Each invocation
gets a fresh session map, explicit identity, registered Kaijutsu builtins, and
restored durable environment/cwd. Restoration checks the backend namespace.
Read-only shells refuse filesystem mutation and host subprocess execution by
construction. Output profile and rc authority are separate choices.

Rc, hook, and editor callers interpret their own results. Interactive commands
and approval resume share server `shell_run.rs`; MCP shell completion has a
separate path. Cwd/export write-back helpers still live in server `rpc.rs`.
These are the ownership gaps the migration must close.

`spawn_kaish_thread` in kernel `lib.rs` reserves a 16 MiB stack for dedicated
threads that can enter kaish. Server `main.rs` configures the Tokio worker
stack too. The requirement applies to any command that can re-enter rc through
`kj`; the helper itself does not construct a shell or runtime.

### Backends and builtin adapters

`KaijutsuBackend` connects block access and tool dispatch to kernel state.
`MountBackend` routes host-mounted file access through the kernel file cache
where required. `KaijutsuFilesystem` implements `/v/docs`; `ReadOnlyFs`
restricts mutations; `SwapFilesystem` exposes dirty buffers read-only at
`/v/swap/<kernel-id>/<real-path>`. `KjBuiltin`, editor/job builtins, and the
configured curl tool implement kaish's tool interface.

A materialized shell uses an isolated `SessionContextMap`. A connection keeps
its own map; the transport records a shell's context switch there when needed.

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

`kj/lifecycle.rs` resolves `/config/rc/<context_type>/<verb>/`, snapshots
selected bodies before execution, and runs them in lexical filename order.
Wired verbs are `create`, `fork`, `attach`, `drift`, `tick`, `rotate`, and
`submit`. It supplies lifecycle facts, enforces recursion limits, records runs,
and emits error/trace blocks. A failed script marks the run failed while later
scripts continue.

Currently `.md` entries author durable system-text blocks and `.kai` entries
execute through the shared factory with rc authority and internal output
limits. Ordinary hooks and editor commands do not receive rc authority.
Replacing the `.md` handler with explicit scripts is planned;
see `docs/kaish-integration.md`, "Rc Markdown: explicit instruction authoring".

Prompt composition and hydration rules live in `docs/prompts.md`. Fork copies
and filters the parent's document before hydrating a new conversation; changing
an rc source affects later lifecycle runs rather than existing instructions.

---

## Smells (not fixed — see [issues](../issues.md))

- **`KernelDb` god-table** — ~14k lines, ~20 tables, every write behind one
  full-DB mutex. The file's own header comment names this a recognized
  "god-table + single-mutex" smell and records the decision **not** to split
  it pre-emptively (`kernel_db.rs:8`) — revisit only once write-contention
  under concurrent contexts is an observed problem, not a theoretical one.
- **Kernel-facade vs `MountTable`** — some callers go through `Kernel::mount`/
  `Kernel::vfs()`, others (e.g. `FileDocumentCache`) hold their own cloned
  `Arc<MountTable>` directly. Harmless today (one shared table behind every
  clone), but nothing enforces that a future second table couldn't diverge.

Examples of explicit failure handling: `list_tool_defs_via_broker`
propagates a broker error instead of returning an empty list (`kernel.rs:757`);
`Broker::binding_checked` and `KjDispatcher::require_cap` (`mcp/broker.rs:1404`)
distinguish a DB read failure from a genuine empty binding rather than
collapsing both into deny-all; `apply_additive_migrations` propagates anything
but SQLite's "duplicate column" case (tested, `kernel_db.rs:8257`); block
editing is hashline-addressed (`file_tools/hashline.rs`), not byte-offset;
`new_ephemeral`'s temp dir is cleaned up by a `Drop`-guarded `TempDirGuard`
(`kernel.rs:212`); and `LocalBackend::setattr` actually sets mtime (tested,
`vfs/backends/local.rs:964`).
