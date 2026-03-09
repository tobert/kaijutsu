# kj Implementation Phases

*Living document — tracks what's done, what's next, and what's deferred.*

---

## Phase 1: KernelDb — Data Model & Queries ✅

**Status:** Complete (commit `dbbb113`, 2026-03-08)

SQLite persistence layer for contexts, edges, presets, workspaces.
~32 methods, 20 e2e tests. New types in kaijutsu-types.

**What shipped:**
- `crates/kaijutsu-types/src/enums.rs` — ForkKind, EdgeKind, ConsentMode, ToolFilter
- `crates/kaijutsu-types/src/ids.rs` — WorkspaceId, PresetId
- `crates/kaijutsu-kernel/src/kernel_db.rs` — KernelDb with row types, CRUD, CTE queries
- ConsentMode/ToolFilter moved to kaijutsu-types, re-exported from original locations
- All downstream crates compile clean (server, app, mcp)

**Design notes captured during implementation:**
- `ToolFilter` doesn't support postcard (HashSet is positional-incompatible). JSON TEXT
  for SQLite column, JSON on Cap'n Proto wire. Not a problem — postcard is only used
  for block sync payloads
- ~~`now_millis()` is `pub(crate)`~~ — now `pub` in kaijutsu-types (Phase 2).
  KernelDb still has its own local `now_millis() -> i64` for the signed return type
- Drift staging table intentionally omitted — push writes blocks directly, scratch
  context pattern covers batched staging if needed later

**TODOs from this phase (carry forward):**
- ~~`DriftRouter.created_at` uses seconds~~ — resolved in Phase 2
- `auth_db.rs` uses `unixepoch()` (seconds) — migrate to millis (future)
- `docs/kj-schema.md` DEFAULT timestamps still show `unixepoch()` — update to millis
- `docs/kj-schema.md` — add `archived_at` to workspace table (schema has it, docs don't)

---

## Phase 2: DriftRouter ↔ KernelDb Integration ✅

**Status:** Complete (commits `e190486`, `486fa71`, 2026-03-09)

KernelDb is now the durable backing store for DriftRouter. Labels, model
assignments, fork lineage, and tool filters survive server restart.

**What shipped:**
- Renamed context-level `parent_id` → `forked_from` across all crates (block-level
  `parent_id` unchanged — that's DAG parentage, different concept)
- `ContextHandle` gained `created_by: PrincipalId`; `register()` / `register_fork()`
  take 4 args
- `now_epoch()` (seconds) deleted from drift.rs → `kaijutsu_types::now_millis()` (now `pub`)
- `KernelDb` on `SharedKernelState` behind `std::sync::Mutex` (not tokio — all ops sync)
- Recovery rewrite: KernelDb primary source, BlockStore discovery as fallback for
  documents not yet in DB
- Write-through on 6 mutation sites: `create_context`, `join_context`,
  `fork_from_version`, `fork_filtered`, `configure_llm`, `set_context_tool_filter`
- Fork paths insert structural edges via `insert_edge()`
- `update_tool_filter()` added to KernelDb
- `map_unique_violation()` now distinguishes FK (→ Validation) from UNIQUE (→ LabelConflict)
  via SQLite extended error codes
- `roundtrip_create_and_recover` + `fk_violation_is_validation_error` tests added
- 510 tests pass across kernel/types/server

**Design decisions:**
- KernelDb lives on SharedKernelState, not DriftRouter — rpc.rs does write-through
- Lock ordering: always `kernel_db` → `drift()`. Never reversed
- `ContextHandle` does NOT gain `system_prompt`/`consent_mode`/`workspace_id`/`preset_id`
  — those stay DB-only, queried on demand (Phase 3+)
- Cap'n Proto wire field stays `parentId` — cosmetic rename deferred

**TODOs resolved:**
- ~~`DriftRouter.created_at` uses seconds~~ → now millis
- ~~`now_millis()` visibility~~ → now `pub` in kaijutsu-types

**TODOs carried forward:**
- `auth_db.rs` uses `unixepoch()` (seconds) — migrate to millis (future)
- `docs/kj-schema.md` DEFAULT timestamps still show `unixepoch()` — update to millis
- Cap'n Proto wire field rename `parentId` → `forkedFrom` (cosmetic, low priority)

---

## Phase 3: `kj` Builtin + `context_shell()` MCP Tool ✅

**Status:** Complete (commit `9ad45b4`, 2026-03-09)

Unified command interface across three modalities: kaish builtin, MCP tool,
and (future) standalone CLI. Single `KjDispatcher` in kaijutsu-kernel dispatches
all subcommands. `KjBuiltin` in kaijutsu-server bridges kaish's `Tool` trait.
`context_shell` MCP tool routes through `shell_execute` RPC.

**What shipped:**

Kernel crate (`kj/` module — 8 files, 36 unit tests):
- `mod.rs` — `KjDispatcher`, `KjCaller`, `KjResult` (Ok/Err/Switch), dispatch routing
- `refs.rs` — Context reference parsing (`.`, `.parent` chains, labels, hex prefixes)
- `format.rs` — Text table/tree formatting for context lists, info, drift queue
- `context.rs` — `list [--tree]`, `info [<ctx>]`, `switch <ctx>`, `create <label> [--parent <ctx>]`
- `fork.rs` — Full fork with `--name` and `--prompt`, deep-copies BlockStore document
- `drift.rs` — `push <dst> <content>`, `flush`, `queue`, `cancel <id>`
- `preset.rs` — Read-only `list` and `show <label>`
- `workspace.rs` — Read-only `list` and `show <label>`

Server crate:
- `kj_builtin.rs` — kaish `Tool` impl, bridges positional args to `KjDispatcher`
- `EmbeddedKaish::with_identity` gains `configure_tools` callback (9th param) —
  passes `SharedContextId` so KjBuiltin can track context switches
- `SharedKernelState.kernel_db` promoted to `Arc<Mutex<>>` (shared w/ dispatcher)
- `SharedKernelState.kj_dispatcher: Arc<KjDispatcher>` — created in `create_shared_kernel`
- KjBuiltin registered at all 3 kaish creation sites in rpc.rs

MCP crate:
- `context_shell` tool — same polling pattern as `shell`, distinct entry point
- `ContextShellRequest` model in models.rs

**Design decisions:**
- `KjResult::Switch(ContextId, String)` — context switch is a distinct variant so
  `KjBuiltin` handles the `SharedContextId` update, keeping `KjDispatcher` pure
- `KjDispatcher` holds `SharedDriftRouter`, `SharedBlockStore`, `Arc<Mutex<KernelDb>>`,
  `KernelId` — same state that rpc.rs uses for write-through
- All kj commands are testable against in-memory state (no server, no kaish)
- `context_shell` vs `shell` MCP tools: same `shell_execute` RPC path, different
  discoverability/description. Both dispatch through EmbeddedKaish → KjBuiltin

**Phase 3b (resolved in Phase 4):**
- ~~`kj drift pull <src> [prompt]`~~ — stub added (requires LLM integration)
- ~~`kj drift merge [ctx]`~~ — stub added (requires LLM integration)
- ~~`kj drift history [ctx]`~~ — implemented (reads drift provenance edges)

---

## Phase 4: Complete kj Command Library ✅

**Status:** Complete (2026-03-09)

Implemented the remaining 18 subcommands plus latch integration, bringing the
kj module from 12 to 30 subcommands across 5 groups. 69 unit tests (up from 36).

**What shipped:**

Infrastructure (4A):
- `kj/parse.rs` (NEW) — shared arg parsing (`extract_named_arg`, `strip_named_arg`,
  `has_flag`, `extract_all_named_args`, `parse_model_spec`, `parse_tool_filter_spec`)
- `KjResult::Latch { command, target, message }` variant for destructive ops
- `KjCaller.confirmed: bool` field — set when `--confirm <nonce>` validates
- KjBuiltin latch bridge: extracts `--confirm`, verifies via `ctx.verify_nonce()`,
  converts `Latch` to kaish exit code 2 via `ctx.latch_result()`
- 5 new KernelDb methods: `delete_structural_edge`, `delete_context`,
  `contexts_using_preset`, `contexts_using_workspace`, `find_context_by_label`

Non-destructive commands (4B):
- `kj context set <ctx> [--model p/m] [--system-prompt] [--tool-filter] [--consent]` —
  write-through to KernelDb + DriftRouter
- `kj context log [<ctx>]` — fork lineage via `db.fork_lineage()` CTE
- `kj context move <ctx> <new-parent>` — reparent via structural edge delete + insert
- `kj fork --shallow [--depth N]` — uses `ForkBlockFilter { max_blocks, exclude_compacted }`
- `kj fork --preset <label>` — applies preset settings after fork
- `kj fork --as <template> --name <n>` — subtree fork (copies tree shape, empty docs)
- `kj preset save <label> [--model] [--system-prompt] [--tool-filter] [--consent] [--desc]`
- `kj workspace create <label> [--desc] [--path ...]`
- `kj workspace add <label> <path> [--mount m]`
- `kj workspace bind <label> [ctx]`
- `kj drift history [ctx]` — shows outgoing/incoming drift edges with timestamps

Latched destructive commands (4C):
- `kj context archive <ctx>` — soft-delete target + recursive children via `subtree_snapshot`
- `kj context remove <ctx>` — hard DELETE (CASCADE), BlockStore delete, DriftRouter unregister
- `kj context retag <label> <ctx>` — move label between contexts (clears old, sets new)
- `kj preset remove <label>` — delete (FK SET NULL on contexts)
- `kj workspace remove <label>` — archive (soft delete)

LLM stubs (4D):
- `kj fork --compact` → "not yet implemented (requires LLM)"
- `kj drift pull <src> [prompt]` → "not yet implemented (requires LLM)"
- `kj drift merge [ctx]` → "not yet implemented (requires LLM)"

**Design decisions:**
- `forked_from` is immutable provenance. `context move` changes structural edges only
- Latch is kernel-level concept (`KjResult::Latch`), nonce is kaish-level (`ctx.latch_result()`)
- No DriftRouter changes for archive — archived contexts stay registered, filtered by
  `list_active_contexts`. For `context remove`, `unregister()` removes from router
- Subtree fork creates empty documents — copies tree shape and settings, not conversation history
- `std::sync::MutexGuard<KernelDb>` is NOT Send — all async kj methods drop the DB lock
  before any `.await` point to keep futures Send

---

## Phase 5: App UI Integration

- Constellation reads context metadata from KernelDb (model, provider, label, archived)
- Fork form uses presets (load preset list, apply on fork)
- Context info panel shows workspace, preset provenance, drift history
- Tree view (`kj context list --tree`) in constellation

---

## Phase 6: Tab Completion

- Context labels (with prefix resolution)
- Preset labels
- Workspace labels
- Tag syntax (`opusplan:` then hex prefix suggestions)
- Integrated into kaish's completion system

---

## Deferred / Open Questions

- **Cross-kernel drift** — schema has `kernel_id` everywhere for future use
- **Compact quality** — what makes a good compaction summary? Preset-level setting?
- ~~**Retag safety**~~ — resolved in Phase 4: `kj context retag` is latch-gated
- **Workspace auto-mounts** — how workspace paths translate to VFS mounts at context join time
- **kj CLI binary** — standalone `kj` command for headless scripting (thin adapter over kernel)
- **Scratch/self context** — a default per-user context for dumping things (like DM-ing yourself on Slack). Could serve as staging area for drift: push to scratch, review, then push to target. Just a context with a well-known label (e.g. `scratch` or `notes`) — no special schema support needed, emerges from existing primitives
- **drift_staging table** — removed from Phase 1 schema. Push writes blocks directly. If batched staging is needed later, scratch context pattern covers it
- ~~**`now_millis()` visibility**~~ — resolved in Phase 2: now `pub` in kaijutsu-types
