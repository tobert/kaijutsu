# Codebase renovation review — 2026-10-14

Reviewed at `040f9b21`, across all eight workspace domains (~416k lines of Rust
in 29 crates). No implementation changed during this pass. Each finding
distinguishes what was **Observed** in source from the **Consequence** inferred
from control flow and concurrency, names any **Unknown**, specifies the
**Renovation**, and states the **First test** to write before changing behavior.

---

## Executive summary and renovation order

The September 16 architecture debt items for `ConversationSession` lifetime,
`RuntimePool` ownership, `/v/input` removal, and `EmbeddedKaish::for_context`
caller migration have landed. The next wave of reliability and clarity hazards
clusters around five structural themes:

1. **Silent fallbacks and asymmetric guards that corrupt or poison state**
   (`FileDocumentCache::flush_one` defaulting a failed read to `""` and
   truncating the host file; `BlockDocument` tombstoned-block mutators panicking
   inside `BlockStore::accept` and permanently poisoning the context;
   `KernelDb::get_context_binding` using `.ok()` instead of `.optional()?`;
   SFTP `posix-rename` unlinking the destination before validating the source;
   `SemanticIndex::new` deleting all metadata and synthesis rows when an HNSW
   load fails).
2. **Pulse and real-time path violations** (`materialize_committed` holding
   `timeline.lock()` on the beat thread across `AbcToMidiDeriver`, CAS `fsync`,
   and `KernelDb` SQLite transactions; `BeatScheduler::fire_due` passing
   `(scheduled, scheduled)` to `ClockSource::next_fire`; `Transport::Starving`
   never resuming when external MIDI clock returns; multi-voice ABC tunes
   serializing voices end-to-end instead of playing from tick 0).
3. **Turn, approval-resume, and hydration concurrency windows** (`approval_resume`
   awaiting approved commands inline in one task and blocking all other asks;
   `prompt::submit` + `hydrate_messages` + `offer_input` racing during `submit`
   rc execution; `llm/hydrate.rs` dropping sibling model `ToolResult`s when a
   user shell result interleaves; `shadow.rs` dropping `CREATE.lock()` before
   `run_create_lifecycle().await`).
4. **Client/server subscription and connection lifecycle bugs** (`kaijutsu-app`
   spawning a replacement global `RpcActor` SSH connection on every cache-miss
   context switch; `BlockEventsForwarder` permanently latching `closed = true`
   when zero broadcast receivers exist; `RpcActor::reject_not_ready` rejecting
   disconnected `SubscribeContext` / `UnsubscribeContext` / `WatchContexts`;
   `run_context_feed` waking on every unrelated context's events;
   `spawn_editor_reconciler` holding an `Arc<SharedKernelState>` cycle).
5. **Divided ownership across surfaces and policy tables** (`is_readonly_kj`
   diverging from `Classify::classify` on `--out`; `BlockToolsServer` and
   `BuiltinTasksServer` bypassing context scoping and `ensure_writable_context`;
   `DriftRouter` and `KernelDb` diverging on `try_write()`; `EditorCore`
   stripping trailing newlines on paste / `:r` / `:s`; triplicated ask and
   outbox state across `kaijutsu-tui` and `kaijutsu-app`).

### Recommended renovation waves (non-overlapping file sets)

- **Wave 1 — Data-loss, panic, and corruption guards (bounded, high-leverage fixes)**
  - **1A (`file_tools/cache.rs`, `runtime/mount_backend.rs`)**: Fix `flush_one`
    0-byte truncation on read error; treat `disk_attr.is_none()` as stale for
    clean cached files; check `pin_count` before evicting on reload error;
    follow symlinks for `flush_one_guarded` generation checks.
  - **1B (`blocks/block_store.rs`, `blocks/content.rs`, `block_store.rs`, `order_repair.rs`)**:
    Filter `!b.is_deleted()` in all five remaining `BlockDocument` mutators;
    fix `order_midpoint` zero-suffix ordering and `agent_order_suffix`; make
    `order_repair::plan` emit `Rekey` only when sorting changes block order;
    fix `BlockStore::live_status` caching on unloaded contexts.
  - **1C (`kernel_db.rs`, `drift.rs`)**: Change `.ok()` to `.optional()?` in
    `get_context_binding`; gate `migrate_context_model_rollover` in
    `kernel_migrations` and honor configured `backends`; clear `in_flight` in
    `DriftRouter::replay_dead_letter` and deduplicate `rehydrate_from_block_log`.
  - **1D (`server/src/sftp.rs`, `vfs/mount.rs`, `vfs/backends/share.rs`, `server/src/ssh.rs`)**:
    Remove destructive pre-unlink in SFTP `POSIX_RENAME`; normalize `.` and `..`
    in `MountTable` and `validate_rw_mounts`; skip `generations_for` on symlinks
    in `ShareRegistry::getattr`.

- **Wave 2 — Shared pulse, Hyoushigi, and audio/MIDI correctness**
  - **2A (`server/src/beat.rs`, `server/src/clock.rs`, `kernel/src/hyoushigi/model.rs`)**:
    Pass `(effective_scheduled, now)` to `ClockSource::next_fire` and fix
    `SystemClock::next_fire`; resume `Transport::Starving` in
    `apply_clock_estimate`; disarm `CancelOnDrop` on normal `ModelOutput`
    delivery; advance `failure_water` past unanchorable events in
    `drain_track_failures`.
  - **2B (`abc/src/midi.rs`, `audio-runtime/src/dj/{midi,audio,core}.rs`, `audio-runtime/src/observer.rs`)**:
    Reset `writer.current_tick = 0` per voice in `build_single_track_writer`;
    scale `abc_to_timed_events` by `tempo.beat_unit`; resolve `CuePayload::Cas`
    in `dj/midi.rs`; preserve `dropped_cues` telemetry across `DjCore::settle`;
    make the 9 WAV unit tests hermetic.
  - **2C (`kernel/src/hyoushigi/mod.rs`, `hyoushigi/src/engine.rs`, `server/src/beat.rs`)**:
    Read crossed cell bytes from `Timeline` memory in `publish_render_cues`;
    move write-barrier CAS `fsync` and `KernelDb` writes off the beat thread;
    compact committed history in `Timeline`.

- **Wave 3 — Runtime, turn execution, approval resume, and council gate**
  - **3A (`runtime/approval_resume.rs`, `runtime/llm_stream.rs`, `runtime/prompt.rs`, `runtime/turn_request.rs`, `runtime/turn_state.rs`, `mcp/servers/shell.rs`)**:
    Spawn approved ask execution onto reserved `RuntimeSlot`s instead of
    blocking `run_delivery` inline; unify background waiting-receipt creation in
    `llm_stream.rs`; fix `leave_to_worker()` completion notice and cache eviction;
    use a batch token instead of `interrupt.hard()` on tool persistence failure;
    close the `prompt::submit` / `hydrate_messages` / `offer_input` race; box
    `run_llm_stream` on the turn stack.
  - **3B (`approval-ledger/src/{ask,schema,council,rules,claim}.rs`, `kernel/src/council/{shadow,gate,sync}.rs`, `kernel/src/approval_identity.rs`)**:
    Hold an async lock across `judge_shadow` + `run_create_lifecycle` and
    separate pre-decision shadow priming from post-decision outcome recording;
    preserve statement order and multiplicity in `find_redeemable` and
    `asks_about_submission`; index `council_programs(program_decision_id)` and
    filter program rows in `list_bump_flavors`; pass remaining deadline in
    `DecisionRequest::timeout_ms` and scope `CouncilSync` invalidation per
    server; collapse `approval_identity.rs` onto one synchronous `&KernelDb`
    lock; retire dead `approval_ledger::claim`.

- **Wave 4 — Control plane (`kj`), MCP brokerage, and LLM providers**
  - **4A (`kj/readonly.rs`, `kj/block.rs`, `kj/cas.rs`, `kj/context.rs`, `kj/drift.rs`, `kj/drive.rs`, `kj/character.rs`, `kj/fork.rs`)**:
    Unify `is_readonly_kj` with `Classify::classify` and route `--out` through
    VFS + `WorkspaceGuard`; gate `kj context demote|promote|conclude`; route
    `drift_pull`/`merge`/`flush` through `deliver_drift`; stamp `caller.actor_id`
    in `kj drive --prompt`; copy `context_egress` in `context_rotate`; propagate
    `live_lineage_root` errors.
  - **4B (`mcp/broker.rs`, `mcp/context_servers.rs`, `mcp/servers/{block,tasks,hooks_builtin}.rs`, `llm/{hydrate,claude,mk,codex,mod}.rs`)**:
    Fix `Broker::list_visible_tools` empty-binding cache poisoning and persist
    sticky `name_map` resolutions; clean up resource subscriptions in
    `clear_binding`; enforce `tool_ctx.context_id` and `ensure_writable_context`
    in `BlockToolsServer` and `BuiltinTasksServer`; fix `hydrate.rs` interleaved
    user shell result flushing; replace `claude::Client::new` `.expect()` with
    `LlmError::AuthError`; hold one endpoint `Slot` across `mk` render+generate.

- **Wave 5 — Client core, server feeds, editor, and interactive surfaces**
  - **5A (`client/src/{actor,subscriptions,document_store}.rs`, `server/src/{context_feed,rpc}.rs`)**:
    Accept `SubscribeContext`, `UnsubscribeContext`, and `WatchContexts` while
    disconnected; emit `FeedEvent::Resubscribed` after `subscribe_context`
    succeeds and call `mirror.begin_rehydrate()`; remove permanent `closed`
    latch on `BlockEventsForwarder`; filter `run_context_feed` at ingress; break
    the `spawn_editor_reconciler` `Arc<SharedKernelState>` cycle; replace
    `DriftRouter` `try_write()` with `.write()`.
  - **5B (`editor/src/lib.rs`, `kernel/src/editor.rs`, `app/src/{view,input,ui,connection}/`, `tui/src/`, `mcp/src/`, `acp/src/`)**:
    Preserve trailing newlines across `EditorCore::new`, `insert_at`,
    `apply_remote_text`, and `apply_substitution`; stop respawning `RpcActor` on
    cache-miss context switches in `kaijutsu-app`; preserve per-context block
    collapse state; scope input/scroll handlers to active `Screen` and overlays;
    fix `ShellDock` cursor/selection geometry; honor `timeout_ms` and retain a
    persistent broadcast receiver in `kaijutsu-mcp::wait_for_notification`.

---

## Section 1: Kernel Durable State, BlockStore & Drift

### 1.1 Deterministic panic and permanent context poisoning when mutating tombstoned blocks
- **Observed**: In `crates/kaijutsu-kernel/src/blocks/block_store.rs`,
  `get_block_header` (`186-188`), `get_block_snapshot` (`192-194`), and 15 block
  mutators filter out soft-deleted blocks via `.filter(|b| !b.is_deleted())`.
  Five mutators omit that filter: `delete_block` (`985-993`),
  `set_content_type` (`1155-1163`), `set_exit_code` (`1236-1244`),
  `set_ephemeral` (`1246-1254`), and `set_excluded` (`1257-1265`). In
  `crates/kaijutsu-kernel/src/block_store.rs`, `set_ephemeral` (`2058-2063`),
  `set_excluded` (`2088-2093`), `set_content_type` (`2184-2189`), and
  `set_exit_code` (`2290-2295`) run inside `self.accept(context_id, |entry| ...)`,
  which sets `entry.poisoned = true` (`1075`), mutates the tombstoned block, and
  then calls `entry.doc.get_block_header(block_id).expect(...)` — which panics
  because `get_block_header` filters `!b.is_deleted()`.
- **Consequence**: Mutating `excluded`, `ephemeral`, `content_type`, or
  `exit_code` on a soft-deleted block panics while `entry.poisoned` is `true`.
  Because `BlockStore::get` (`764`) and `get_mut` (`783`) assert
  `!entry.poisoned`, every subsequent read or write to that context panics until
  server restart. Calling `delete_block` (`2840-2856`) on an already-deleted
  block also bumps the version, journals a duplicate op, and emits a duplicate
  `BlockFlow::Deleted`.
- **Renovation**: Add `.filter(|b| !b.is_deleted())` to all five mutators (or
  route all 20 mutators through one `live_block_mut` helper).
- **First test**: In `crates/kaijutsu-kernel/src/block_store.rs`, insert a
  block, call `delete_block`, then call `set_excluded`, `set_ephemeral`,
  `set_content_type`, `set_exit_code`, and `delete_block` on the deleted ID;
  assert each returns `Err(BlockStoreError::Block(BlockDocumentError::BlockNotFound(_)))`
  and `store.get(ctx)` succeeds.

### 1.2 `order_repair::plan` emits `Rekey` even when sorting by tick does not change order
- **Observed**: `order_repair::plan` (`crates/kaijutsu-kernel/src/order_repair.rs:59-88`)
  partitions blocks into contiguous runs of `is_turn_block`. Whenever a run
  contains a `result` ID in `damaged` (`70`), `plan` clones `run` into `order`,
  calls `order.sort_by_key(|b| b.tick)` (`79`), and unconditionally pushes a
  `Rekey` (`80-86`) without checking whether sorting changed the order of IDs in
  `run`.
- **Consequence**: When a misordered `(result, call)` pair has
  `result.tick <= call.tick` or is separated by a non-turn block (`Role::User`,
  `BlockKind::Drift`, `BlockKind::Notification`, or tickless block), sorting by
  tick leaves `order` identical to `run`, yet `repair()` (`136-155`) still mints
  new keys, journals `rekey_blocks`, and reports the same `rekeys` and
  `remaining` count on every `--apply` (explaining `docs/issues.md:722`).
- **Renovation**: Skip emitting a `Rekey` when
  `order.iter().map(|b| b.id).eq(run.iter().map(|b| b.id))`.
- **First test**: In `crates/kaijutsu-kernel/src/order_repair.rs`, construct a
  document with a `ToolResult` before its `ToolCall` at the same `Tick`, run
  `repair(&store, ctx)`, and assert `report.rekeys.is_empty()`,
  `report.remaining.len() == 1`, and `store.version(ctx)` is unchanged.

### 1.3 Fractional-index zero-suffix invariant violation in `order_midpoint` and `agent_order_suffix`
- **Observed**: In `crates/kaijutsu-kernel/src/blocks/content.rs:69-92`,
  `order_midpoint(a, b)` maps missing bytes past `a.len()` to `a_val = 0`
  (`line 80`), which is also the index of `'0'` (`base62_index(b'0') == 0`).
  Meanwhile, `BlockDocument::agent_order_suffix`
  (`crates/kaijutsu-kernel/src/blocks/block_store.rs:381-390`) maps principal
  bytes through `BASE62[(byte as usize) % 62]`, which can emit `'0'`.
- **Consequence**: When `a` is a prefix of `b` followed by `'0'` (e.g.,
  `a = "V"`, `b = "V0"`), `order_midpoint("V", "V0")` returns `"V0V"`, which
  violates `a < mid < b` (`"V0V" > "V0"`).
- **Renovation**: Map `agent_order_suffix` into `BASE62[1..62]` (`1 + byte % 61`)
  so suffixes never end in `'0'`, and handle `'0'`-tailed suffixes in
  `order_midpoint`.
- **First test**: Assert `let mid = order_midpoint("V", "V0"); assert!("V" < mid.as_str() && mid.as_str() < "V0");`.

### 1.4 `DriftRouter::replay_dead_letter` preserves `in_flight = true` and `rehydrate_from_block_log` duplicates replayed items
- **Observed**: `DriftRouter::drain_dead_letter` (`crates/kaijutsu-kernel/src/drift.rs:1139-1149`)
  sets `item.in_flight = true`. `replay_dead_letter(id)` (`1197-1206`) moves the
  item back to `self.staging` and resets `retry_count = 0` without resetting
  `in_flight = false` or `cancelled = false`. In `mark_consumed` (`831-832`),
  `self.persisted_blocks.remove(&staged_id)` runs before
  `p.blocks.set_status(p.queue_ctx, &block_id, Status::Done)`, and
  `rehydrate_from_block_log` (`873-897`) does not deduplicate `Status::Pending`
  blocks by `payload.drift.id`.
- **Consequence**: Replaying an in-flight dead letter leaves `in_flight = true`
  in `self.staging`, where `drain()` (`1008`) skips it forever. If `set_status`
  fails during `mark_consumed`, two `Status::Pending` blocks with the same
  `drift.id` survive in `drift-queue` and both load on restart.
- **Renovation**: Refuse or clear `in_flight` in `replay_dead_letter`, retain
  `persisted_blocks` if `set_status` errors, and deduplicate by `drift.id` in
  `rehydrate_from_block_log`.
- **First test**: Call `drain_dead_letter()`, then `replay_dead_letter(id)`, and
  assert `drain(None)` delivers the replayed item; write two `Pending` blocks
  with the same `drift.id` into `queue_ctx` and assert `rehydrate_from_block_log`
  produces one queue entry.

### 1.5 `BlockStore::live_status` caches `Status::Pending` for unloaded contexts
- **Observed**: `BlockStore::live_status` (`crates/kaijutsu-kernel/src/block_store.rs:2925-2941`)
  inserts `Status::Pending` into `self.live_status` when `self.get(context_id)`
  is `None`, and neither `load_from_db` (`3226`), `load_one_from_db` (`3343`),
  nor `publish_fork` (`1184`) invalidates or updates `self.live_status`.
- **Consequence**: Calling `live_status(ctx)` before `ctx` is loaded pins
  `Status::Pending` in `self.live_status` even after `ctx` loads with a
  different tail status.
- **Renovation**: Do not cache `live_status` when `self.get(context_id)` is
  `None`, and update `live_status` on document load and fork publication.
- **First test**: Persist a context whose last block is `Status::Error`, call
  `live_status(ctx)` on a cold `BlockStore`, then call `load_one_from_db(ctx)`
  and assert `live_status(ctx) == Status::Error`.

### 1.6 `KernelDb::get_context_binding` swallows SQLite errors as `Ok(None)` and `migrate_context_model_rollover` clobbers custom backends on every `open()`
- **Observed**:
  1. `KernelDb::get_context_binding` (`crates/kaijutsu-kernel/src/kernel_db.rs:5705-5719`)
     terminates its `SELECT ... FROM context_bindings` query with `.ok()`
     instead of `.optional()?`.
  2. `apply_additive_migrations` (`2832`) runs `migrate_context_model_rollover`
     (`3169-3215`) on every `KernelDb::open()` without a `kernel_migrations`
     entry, overwriting any context whose `provider` is not in
     `["anthropic", "deepseek", "ollama", "gpt"]` to `"deepseek"` /
     `"deepseek-flash"`.
- **Consequence**: A SQLite error on `context_bindings` silently returns
  `Ok(None)` (causing `insert_forked_context` at `4143` to fork a child with no
  loadout and `Broker::binding_checked` to see no binding). Any context pinned
  to a custom backend (`codex`, `openrouter`, etc.) is overwritten to DeepSeek on
  restart.
- **Renovation**: Replace `.ok()` with `.optional()?` at `kernel_db.rs:5719`,
  and record `migrate_context_model_rollover` in `kernel_migrations` (while
  preserving providers present in `backends`).
- **First test**: Drop `context_bindings` in a test DB and assert
  `get_context_binding` returns `Err(_)`; configure a `"codex"` backend, set a
  context's model to `"codex"` / `"gpt-5"`, reopen `KernelDb`, and assert the
  context retains `"codex"`.

### 1.7 Additional Section 1 cleanups
- **Unify `load_from_db` and `load_one_from_db`** (`block_store.rs:3024-3345`):
  Share one hydration helper that checks oplog sequence continuity (`entry_seq == expected`)
  and skips calling `calc_order_key` (`blocks/block_store.rs:977`) when
  `snapshot.order_key.is_some()`.
- **Map unique label conflicts in `KernelDb::promote_context`**
  (`kernel_db.rs:4625-4645`): Wrap the `UPDATE` with `map_unique_violation` so
  resurrecting an archived context whose label was reused returns
  `KernelDbError::LabelConflict`.
- **Batch-lock per subscriber in `FlowBus::publish_batch`**
  (`flows.rs:893-937, 1166-1174`): Push all messages in an atomic group under
  one `slot.state.lock()` acquisition and notify once.

---

## Section 2: Kernel Runtime, Turn Execution, Kaish & Shell Settlement

### 2.1 `FileDocumentCache::flush_one` defaults failed snapshot reads to `""`, truncating the on-disk file to 0 bytes
- **Observed**: In `crates/kaijutsu-kernel/src/file_tools/cache.rs:807-824`,
  `flush_one` reads the buffer content via
  `self.block_store.block_snapshots(ctx_id).ok().and_then(|snaps| snaps.iter().find(|s| s.id == block_id).map(|s| s.content.clone())).unwrap_or_default()`,
  writes `content.as_bytes()` to `self.vfs.write_all(vfs_path, ...)`, and calls
  `self.settle_clean(...)` to delete the `dirty_file_buffers` recovery row.
- **Consequence**: Any error in `block_snapshots(ctx_id)` or a missing block
  causes `flush_one` to overwrite the host file with 0 bytes and erase the dirty
  recovery marker.
- **Renovation**: Propagate `block_snapshots` errors and missing `block_id` as
  `Err(FlushError::Backend(...))` before touching VFS or clearing the swap
  marker.
- **First test**: In `file_tools::cache::tests`, mark a cached file dirty,
  delete its backing document in `block_store`, call `flush_one(path)`, and
  assert `Err(FlushError::Backend(_))` with the on-disk file and
  `dirty_file_buffers` row untouched.

### 2.2 Clean cached files deleted or renamed on disk remain in `FileDocumentCache` and serve ghost content
- **Observed**: In `FileDocumentCache::try_get_or_load`
  (`crates/kaijutsu-kernel/src/file_tools/cache.rs:373-397`), when `vfs.getattr`
  returns `Err(VfsError::NotFound)` on a deleted file, `disk_attr` is `None`, so
  `stale` (`matches!((disk_generation, loaded_generation), (Some(d), Some(l)) if d > l)`)
  and `symlink` are both `false`. For a clean entry (`dirty == false`),
  `if !stale && !symlink` returns `Ok((cid, bid))` without calling
  `reload_block_from_disk`. In addition, `MountBackend::remove`
  (`runtime/mount_backend.rs:597-613`), `remove_recursive` (`719-740`), and
  `rename` (`619-625`) do not invalidate `file_cache`, and
  `FileDocumentCache::exists` (`cache.rs:316-322`) returns `true` for clean
  cache entries without checking disk.
- **Consequence**: Reading a file once and then removing it with `rm` in kaish
  or on the host leaves `cat` and MCP `read` serving the deleted file's content,
  and causes `WriteMode::CreateNew` to fail with `AlreadyExists`.
- **Renovation**: Treat `disk_attr.is_none()` as `stale = true` in
  `try_get_or_load`, check `self.vfs.exists` for clean entries in
  `FileDocumentCache::exists`, and guard cache eviction in `try_get_or_load`
  (`cache.rs:404, 411`) on `entry.pin_count == 0`. In `flush_one_guarded`
  (`cache.rs:990-991`), follow symlinks when comparing disk generation.
- **First test**: In `mount_backend.rs`, `read` a file, `remove` it, and assert
  a second `read` returns `Err(BackendError::NotFound)` and `WriteMode::CreateNew`
  succeeds.

### 2.3 Single-task `approval_resume::run_delivery` awaits approved command execution inline
- **Observed**: `approval_resume::start`
  (`crates/kaijutsu-kernel/src/runtime/approval_resume.rs:851`) spawns one task
  running `run_delivery`. Inside `for answer in answers` (`912-1235`), it awaits
  `act_on_executable_answer(...).await` inline (`1058-1067`), which awaits
  `command::run_into_blocks(...).await` (`765-779`) with
  `CommandJobOutput::Settled` and without reserving a separate `RuntimeSlot`.
- **Consequence**:
  1. Any long-running approved command in one context blocks delivery of all
     other ask answers, refusal seeds, held-ask releases, and completion notices
     across every context in the kernel.
  2. Approved background commands (`HoldUntil::Started`) release the waiting
     model turn at job start (`753`), but keep `run_delivery` blocked until the
     command finishes and do not stream output (`CommandJobOutput::Settled`).
  3. If an approved command triggers a nested ask, `run_delivery` deadlocks
     against itself.
- **Renovation**: Reserve a `RuntimeSlot` before claiming/redeeming an allowed
  ask and spawn command execution onto that slot (using
  `CommandJobOutput::LiveExecution` when `HoldUntil::Started`).
- **First test**: In `approval_resume.rs`, hold Context A's approved command on
  a barrier, answer Context B's ask, and assert Context B's ask settles while
  Context A's command is still running.

### 2.4 Race between `ShellServer` publishing a background `Waiting` receipt and `llm_stream` registering `HoldUntil::Started`
- **Observed**: For `run_in_background: true`, `ShellServer::call_tool`
  (`crates/kaijutsu-kernel/src/mcp/servers/shell.rs:323-342`) calls
  `tool_command::create_operation`, which calls `db.release_approval_pair(ask)`
  and `announce_ledger_change` before returning to `llm_stream.rs:1783` where
  `held_asks().hold(ask, HoldUntil::Started)` is registered (`1801`).
- **Consequence**: If the ask is answered before `llm_stream.rs:1801` runs,
  `approval_resume` sees `held == None` and processes it as an unheld turn pair
  without releasing `held_asks`, leaving `hold.wait()` at `llm_stream.rs:1811`
  hanging until interrupted.
- **Renovation**: Have `ShellServer::call_tool` return `Err(McpError::refused_gate(...))`
  for background gate asks just as it does for foreground asks, and let
  `llm_stream.rs` own `create_operation` + `held_asks().hold` +
  `release_approval_pair` in one place.
- **First test**: Race a background `shell_write` gate denial against turn hold
  registration and assert the turn receives the denial without hanging.

### 2.5 Soft-interrupt `leave_to_worker()` during approved command execution drops completion notice and `ConversationCache::evict`
- **Observed**: `act_on_executable_answer`
  (`crates/kaijutsu-kernel/src/runtime/approval_resume.rs:509, 538, 783-802`)
  snapshots `let held = kernel.held_asks().holder(&answer.request_id)` before
  running `run_into_blocks`. If `reads_result(&held)` was `true` at claim time,
  it suppresses the completion notice (`538`) and, after `run_into_blocks`
  finishes, returns `ExecAction::Settled` (`787`) before
  `conversations().evict(context_id)` (`801`).
- **Consequence**: If the holding turn soft-interrupts with `keep_held_asks: true`
  and calls `hold.leave_to_worker()` (`llm_stream.rs:1868-1874`) while the
  command is running, the finished command neither emits a completion notice nor
  evicts `ConversationCache`.
- **Renovation**: Determine whether the holding turn actually received the
  result from the return value of `kernel.held_asks().release(&answer.request_id)`
  *after* `run_into_blocks` completes, and always evict `ConversationCache` when
  settling an existing pair in place.
- **First test**: Start an approved foreground command held by a turn, trigger
  `leave_to_worker()` while the command is mid-run, let the command finish, and
  assert the completion notice is queued and the conversation session is marked
  for reset.

### 2.6 Live-input / turn-start hydration races and `interrupt.hard()` on tool block persistence failure
- **Observed**:
  1. `prompt::submit` (`runtime/prompt.rs:33, 48-55, 68`) acquires a `TurnLease`
     via `turns().begin(context)` and inserts user block `B` into `documents`
     before `queue_startup` (`runtime/turn_request.rs:134-155`) runs
     `rc::run(VERB_SUBMIT)` and calls `offer_input`. Meanwhile, an active Turn 1
     starting in `run_llm_stream` (`runtime/llm_stream.rs:2368, 2460`) calls
     `hydrate_messages` (folding all blocks in `documents`, including `B`) while
     initializing `last_block_id = after_block_id`.
  2. When persisting a tool block fails in `run_llm_stream`
     (`llm_stream.rs:3349-3353`), it calls `interrupt.hard()`, which sets
     `stopped = true` in `process_llm_stream` (`2193-2197`) and skips
     `wake_undelivered_input`.
  3. `spawn_admitted_turn` (`llm_stream.rs:711`) calls `spawn_runtime_task` from
     inside `queue_startup`'s pool thread, hitting `RuntimeSlotInner::Local`
     (`worker.rs:344`) and dropping the `ADMISSION_CAPACITY` `OwnedPermit` before
     streaming begins, while pinning `run_llm_stream` on the stack without
     `Box::pin`.
- **Consequence**: Submitting a prompt while another turn is starting can fold
  the new block before its `submit` rc runs, insert Turn 1's reply blocks before
  `B`, or spawn a duplicate turn after `close_ingress`; a tool persistence error
  drops queued `LiveInput`; and the streaming turn runs without holding its
  admission permit and with a large stack frame.
- **Renovation**: Advance `last_block_id` to the last hydrated block and avoid
  folding un-offered future user submissions in `hydrate_messages`; acquire a
  new `TurnLease` only when `offer_input` does not join an active turn; use a
  batch-scoped cancellation token instead of `interrupt.hard()` on persistence
  failure; await `Box::pin(process_llm_stream(...))` directly inside
  `queue_startup`'s `RuntimeSlot` task.
- **First test**: Submit input `B` while Turn 1 is between `open_ingress` and
  `hydrate_messages` with a slow `submit` rc script on `B`; assert `B` is not
  sent to the provider before its `submit` rc finishes and is delivered exactly
  once.

---

## Section 3: Approval Ledger, Council Gate & Identity

### 3.1 Shadow context creation race and redundant double-`PUT` per call
- **Observed**: In `crates/kaijutsu-kernel/src/council/shadow.rs:99-123, 448-485`,
  `judge_shadow` locks `CREATE.lock()`, inserts the `judge` child context row,
  and drops `CREATE.lock()` before `record_call` awaits
  `run_create_lifecycle(kernel, judge.shadow)` (`102`). `record_call` then
  appends the `Role::User` block and calls `spawn_prime` (`122`), and
  `record_outcome` (`357-365`) appends the `Role::Model` outcome block and calls
  `spawn_judge`, which calls `prime_shadow` a second time (`272`) before calling
  `decide_traced` (`279`).
- **Consequence**: Concurrent calls on a seat's first turn can see the `judge`
  row before `run_create_lifecycle` writes the stance block, causing
  `project_shadow` (`projection.rs:78-87`) to fail with `"the shadow has no stance"`
  or place the stance block after the first `Role::User` block. Every judged
  call also triggers two full-context `PUT`s, and the judge evaluates the shadow
  with the gate's own outcome turn already appended.
- **Unknown**: Whether the megakernel's `judge` prompt expects the gate's
  `Role::Model` outcome turn to be present when scoring the call.
- **Renovation**: Guard shadow creation + `run_create_lifecycle` with an async
  mutex so callers wait until the stance block is committed, and eliminate the
  redundant `PUT`.
- **First test**: In `council/shadow.rs`, run two concurrent `record_call`
  futures on a fresh seat and assert the shadow's first block is always
  `(Role::System, ContentType::Text)`.

### 3.2 `find_redeemable` and `asks_about_submission` collapse statement order and multiplicity into a `BTreeSet`
- **Observed**: `approval_ledger::ask::find_redeemable`
  (`crates/approval-ledger/src/ask.rs:930-980`) and `asks_about_submission`
  (`crates/kaijutsu-kernel/src/kj/gate.rs:527-543`) convert statement digests
  into a `BTreeSet` instead of comparing ordered slices (`ORDER BY stmt_seq ASC`).
- **Consequence**: When `exec_source` is `None`, an allowed ask for statements
  `[A, B]` will match and redeem `[B, A]` or `[A, B, B]`.
- **Renovation**: Compare ordered `Vec<&str>` against `ORDER BY stmt_seq ASC`.
- **First test**: In `approval-ledger/src/ask.rs`, allow an ask with statements
  `["a", "b"]` and assert `find_redeemable` returns `Ok(None)` for `["b", "a"]`
  and `["a", "b", "b"]`.

### 3.3 Missing index on `council_programs(program_decision_id)` and unfiltered program rows in `list_bump_flavors`
- **Observed**: `council_programs` (`crates/approval-ledger/src/schema.rs:978-991`)
  has no index on `program_decision_id`. `list_council_submissions`
  (`council.rs:422-435`) runs `WHERE NOT EXISTS (SELECT 1 FROM council_programs p WHERE p.program_decision_id = d.decision_id)`,
  while `list_bump_flavors` (`council.rs:364-379`) omits that `NOT EXISTS`
  filter.
- **Consequence**: Every bump-streak check full-scans `council_programs` per
  candidate `council_decisions` row, and `list_bump_flavors` can include program
  sub-decisions if their `submission_digest` collides.
- **Renovation**: Add `idx_council_programs_program` on
  `council_programs(program_decision_id) WHERE program_decision_id IS NOT NULL`
  and add the `NOT EXISTS` filter to `list_bump_flavors`.
- **First test**: Insert a shell decision and a linked program decision with the
  same `submission_digest` and assert `list_bump_flavors` returns only the shell
  decision's flavor.

### 3.4 Council deadline accounting, cross-server cache invalidation, and `refuse_bump` rule-check ordering
- **Observed**:
  1. `decision_request` (`crates/kaijutsu-kernel/src/council/gate.rs:297-307`)
     sends `timeout_ms: Some(council.deadline_ms)` (the full unadjusted budget)
     after `prepare_within` has already consumed part of `deadline`.
  2. `CouncilSync::invalidate` and `invalidate_spec`
     (`crates/kaijutsu-kernel/src/council/sync.rs:440-449`) evict entries across
     all servers rather than `&council.server`, and `CouncilSync::failed`
     (`473-478`) wipes cached `identities` and `specs` on any context `PUT`
     error.
  3. In `run_gate_recorded` (`crates/kaijutsu-kernel/src/kj/gate.rs:620-624`),
     `refuse_bump` runs before `run_gate_once` re-checks whether a standing rule
     or redeemable ask arrived while the council HTTP call was in flight.
- **Renovation**: Compute `timeout_ms` from remaining `deadline` in `ask_within`;
  scope `CouncilSync` invalidation to `&council.server` and keep `identities` /
  `specs` on context-only `PUT` errors; re-check policy and redeemable asks
  before `refuse_bump`.
- **First test**: In `council/sync.rs`, prepare two mock servers, trigger a 404
  on `mock_a`, and assert `mock_b`'s held context and spec are not re-uploaded.

### 3.5 Fragmented `KernelDb` locks in `approval_identity.rs`, dead `approval_ledger::claim`, and non-transactional `rules::revoke`
- **Observed**:
  - `resolve_context_review`, `resolve_context_review_for`, and
    `resolve_review_assignment` (`crates/kaijutsu-kernel/src/approval_identity.rs:38-155`)
    are `pub async fn` with zero `.await` points and lock `self.kernel_db().lock()`
    up to four separate times per call.
  - `approval_ledger::claim` (`crates/approval-ledger/src/claim.rs`) has zero
    production callers, yet `kj/ledger.rs` help text (`288-358`) still describes
    claiming, and `cancel`/`escalate` (`decide.rs:343, 373`) reject `claimed`
    rows.
  - `rules::revoke` (`crates/approval-ledger/src/rules.rs:242-266`) reads outside
    a write transaction and ends with `debug_assert_eq!(rows, 1)`.
- **Renovation**: Collapse `approval_identity.rs` onto a single synchronous
  `&KernelDb` lock guard; retire `claim.rs` and update `kj/ledger.rs` help text;
  wrap `rules::revoke` in `BEGIN IMMEDIATE` without `debug_assert_eq!(rows, 1)`.
- **First test**: Call `rules::revoke` twice on the same rule ID inside tests
  and assert both return `Ok(())`.

---

## Section 4: Control Plane (`kj` Verbs), MCP Brokerage & LLM Providers

### 4.1 `kj block cat|original --out` and `kj cas get --out` bypass read-only shell policy, gate policy, capabilities, and VFS
- **Observed**: `Classify for BlockCommand` (`crates/kaijutsu-kernel/src/kj/block.rs:1930-1932`)
  classifies `out: Some(_)` as `Effect::Write`, but `is_readonly_kj`
  (`crates/kaijutsu-kernel/src/kj/readonly.rs:285-287`) uses a separate tuple
  match that ignores `--out` / `-o`. Furthermore, `block_cat` (`kj/block.rs:1046`),
  `block_original` (`1173`), and `cas_get` (`kj/cas.rs:171`) write directly to
  the host filesystem with `std::fs::write` without checking write capabilities
  or `WorkspaceGuard`.
- **Consequence**: A read-only `shell` call can overwrite arbitrary host files
  via `kj block cat <id> --out /path` or `kj cas get <hash> --out /path` without
  triggering `ShellPolicy::ReadOnly`, gate asks, or VFS mount guards.
- **Renovation**: Replace the hand-rolled tuple table in `is_readonly_kj` with
  `KjCli::try_parse_from` + `cli.classify() == Effect::Read`, require write
  capability for `--out`, and write through `vfs().write_all` +
  `WorkspaceGuard::check_write`.
- **First test**: Assert `validate_readonly("kj block cat abc --out /tmp/x").is_err()`
  and `validate_readonly("kj cas get deadbeef --out /tmp/x").is_err()`.

### 4.2 `kj context demote`, `promote`, and `conclude` lack capability and confirmation checks
- **Observed**: In `dispatch_context` (`crates/kaijutsu-kernel/src/kj/context.rs:692-703`),
  `Archive` requires `Capability::Operator` and `--confirm` (`Effect::Destroy`),
  whereas `Demote` (which archives a context when called repeatedly, `2681-2711`),
  `Promote` (which resurrects archived contexts, `2716-2735`), and `Conclude`
  check no capability and are classified as `Effect::Write`.
- **Consequence**: Any worker seat can archive any context in the house via
  repeated `kj context demote <id>` or resurrect archived contexts via
  `kj context promote <id>`.
- **Unknown**: Whether worker seats are intended to conclude/pause their own
  seat or child seats in their lineage without `Capability::Operator`.
- **Renovation**: Require `Capability::Operator` (or lineage-root/reviewer
  authority) on `Demote`, `Promote`, and `Conclude`, and require `--confirm`
  when `demote` transitions a context to `Archived`.
- **First test**: From a worker seat without `Capability::Operator`, run
  `kj context promote <archived>` and `kj context demote <live>` (to archive)
  and assert refusal.

### 4.3 `kj drift merge` skips `drift_target_refusal` and `drift pull|merge|flush` wake turns before running `drift` rc scripts
- **Observed**: `deliver_drift` (`crates/kaijutsu-kernel/src/kj/drift.rs:193-269`,
  used by immediate `drift_push`) runs `Lifecycle::Drift` rc (`241-251`) before
  `offer_drift` (`258`). `drift_pull` (`554-570`), `drift_merge` (`691-717`), and
  `drift_flush` (`855-881`) duplicate delivery inline with `offer_drift` before
  `Lifecycle::Drift` rc, and `drift_merge` (`612-756`) never calls
  `drift_target_refusal(target_id)`.
- **Consequence**: `kj drift merge` can inject drift blocks into archived or
  concluded contexts, and `pull`/`merge`/`flush` wake running turns before
  `drift` rc scripts execute.
- **Renovation**: Call `drift_target_refusal` at the start of `drift_merge` and
  route `drift_pull`, `drift_merge`, and `drift_flush` through `deliver_drift`.
- **First test**: Archive a target context, run `kj drift merge <src> into <archived>`,
  and assert refusal before any LLM distillation call runs.

### 4.4 `kj drive --prompt` stamps `caller.principal_id` instead of `caller.actor_id`, and `context_rotate` drops `context_egress`
- **Observed**:
  1. `kj/drive.rs:153-163, 178` passes `caller.principal_id` (requester) instead
     of `caller.actor_id` (performer) to `insert_block_as` and `TurnRequest::new`.
  2. `context_rotate` (`kj/context.rs:2428-2573`) copies `ContextRow`,
     `context_env`, and `context_shell`, but does not copy `context_egress` rows
     from the predecessor.
  3. `kj/context.rs:519, 621, 2000-2001, 2096-2097` use
     `matches!(db.lineage_root(target_id), Ok(root) if root == ...)`, turning DB
     or lineage-cycle errors into permission refusals (`docs/issues.md:466`).
- **Renovation**: Stamp `caller.actor_id` in `kj/drive.rs`; copy `context_egress`
  in `context_rotate`; replace `db.lineage_root` with `db.live_lineage_root` and
  propagate `Err`.
- **First test**: Drive a worker context from a connection whose `actor_id` is
  `banto` (`!= principal_id`) and assert the seed block's `principal_id` is
  `banto`.

### 4.5 `Broker::list_visible_tools` poisons `self.bindings` on DB error and drops sticky `name_map` updates
- **Observed**: `Broker::list_visible_tools`
  (`crates/kaijutsu-kernel/src/mcp/broker.rs:1555-1619`) calls
  `self.binding(&context_id).await.unwrap_or_default()` and unconditionally
  writes `binding` into `self.bindings` (`1619`) without calling
  `self.persist_binding(&context_id, &binding)`. Also, `clear_binding`
  (`1505-1511`) does not remove `context_id` from `self.subscriptions` or
  `self.resource_parents`.
- **Consequence**: A transient DB fault during `list_visible_tools` caches an
  empty deny-all binding in `self.bindings` until restart; sticky `name_map`
  resolutions computed during `list_visible_tools` are never persisted to
  SQLite; and archived contexts continue receiving MCP `ResourceUpdated` blocks.
- **Renovation**: Use `self.binding_checked(&context_id).await?` in
  `list_visible_tools`, persist `binding` when `apply_resolutions` modifies
  `name_map`, and purge `subscriptions` and `resource_parents` in
  `clear_binding`.
- **First test**: Resolve a colliding tool name via `list_visible_tools`, clear
  the in-memory `bindings` map, reload from `KernelDb`, and assert the sticky
  `name_map` entry was persisted.

### 4.6 Cross-context mutation and whole-kernel document listing in `BlockToolsServer` and `BuiltinTasksServer`
- **Observed**: `BlockToolsServer::find_block`
  (`crates/kaijutsu-kernel/src/mcp/servers/block.rs:957-967`) and
  `BuiltinTasksServer::find_task`
  (`crates/kaijutsu-kernel/src/mcp/servers/tasks.rs:418-438`) extract
  `block_id.context_id` from the caller-supplied `BlockId` without checking
  `context_id == tool_ctx.context_id` or `ensure_writable_context`. `block_list`
  (`mcp/servers/block.rs:612-654`) iterates over `self.documents.list_ids()`
  (every document in the kernel) while ignoring `BlockListParams.path_prefix`
  and `depth`. And `img_block_from_path` (`879-902`) reads `self.vfs.read_all`
  without resolving relative paths against `tool_ctx.cwd` or calling
  `WorkspaceGuard::check_read`.
- **Consequence**: Any seat with `builtin.block` or `builtin.tasks` can edit
  blocks or tasks in any other context (including archived contexts) and read
  arbitrary VFS paths via `img_block_from_path`.
- **Renovation**: Enforce `block_id.context_id == tool_ctx.context_id` and
  `ensure_writable_context` in `BlockToolsServer` and `BuiltinTasksServer`,
  scope `block_list` to `tool_ctx.context_id`, and check `WorkspaceGuard::check_read`
  in `img_block_from_path`.
- **First test**: Attempt `block_edit` and `task_update` from Context A on a
  block in Context B (and on an archived context) and assert rejection.

### 4.7 Hydration drops sibling model `ToolResult`s on interleaved user shell result, `claude::Client::new` panics on non-ASCII API key, and `mk` drops its endpoint `Slot` between `render` and `generate`
- **Observed**:
  1. In `crates/kaijutsu-kernel/src/llm/hydrate.rs:299-312`, when a user shell
     `ToolResult` (`user_shell_pending`) arrives while a multi-tool assistant
     turn has partial `ToolResult`s queued in `self.tool_results`, `self.flush_all()`
     flushes only the first subset of model `ToolResult`s before pushing
     `Message::user("[User ran ...]")`. `into_messages()` (`474-478`) then drops
     the remaining model `ToolResult`s as orphaned and synthesizes fake
     `"Tool execution was interrupted"` errors.
  2. `claude::Client::new` (`crates/kaijutsu-kernel/src/llm/claude/mod.rs:105`)
     calls `HeaderValue::from_str(&api_key).expect("Anthropic API key must be ASCII")`.
  3. `MkClient` (`crates/kaijutsu-kernel/src/llm/mk/mod.rs:154, 169, 185, 210-243, 342`)
     acquires and drops three separate endpoint `Slot`s across `model_info`,
     `render`, and `open_generate`, and slices `&done.context[..done.context.len().min(16)]`
     without `floor_char_boundary(16)`.
- **Renovation**: Buffer user-shell `[User ran ...]` messages in `hydrate.rs`
  until the active assistant `ToolResult` batch finishes flushing; return
  `Err(LlmError::AuthError(...))` instead of `.expect(...)` in
  `claude::Client::new`; hold one `Slot` across `prepare_generate` through
  `open_generate` and use `floor_char_boundary(16)` in `llm/mk/mod.rs`.
- **First test**: In `llm/hydrate.rs`, hydrate an `Assistant` message with two
  `ToolCall`s (`t1`, `t2`), an interleaved `User` `ToolCall` (`u1`), and results
  `ToolResult(t1)`, `ToolResult(u1)`, `ToolResult(t2)`; assert both `t1` and
  `t2` results survive in the hydrated messages.

---

## Section 5: Server, Wire Protocol, VFS & Client Core

### 5.1 Disconnected `subscribe_context`, premature `FeedEvent::Resubscribed`, and permanent `BlockEventsForwarder::closed` latch in `kaijutsu-client`
- **Observed**:
  1. `RpcActor::reject_not_ready` (`crates/kaijutsu-client/src/actor.rs:2681-2717, 3064-3121`)
     rejects `SubscribeContext`, `UnsubscribeContext`, and `WatchContexts` when
     the actor is in `Connecting` or `Cooldown`, contradicting the doc comments
     at `actor.rs:1305, 2289-2292, 2772-2774`.
  2. `RpcActor::issue_context_feed` (`actor.rs:2906-2952`) sends
     `FeedEvent::Resubscribed` *before* `kernel.subscribe_context` succeeds, and
     `DocumentEntry::drain_feed` (`crates/kaijutsu-client/src/document_store.rs:110-120`)
     replaces `self.mirror = ContextMirror::new(...)` instead of calling
     `self.mirror.begin_rehydrate()`.
  3. `BlockEventsForwarder::forward` (`crates/kaijutsu-client/src/subscriptions.rs:328-347`)
     permanently latches `self.closed = true` the first time `self.event_tx.send(event)`
     sees zero broadcast receivers (while `spawn_actor` at `actor.rs:4224, 4254`
     drops the initial `_` receiver and eagerly connects).
- **Consequence**: Subscribing during initial connect fails with `NotReady`; a
  failed `subscribe_context` after `Resubscribed` leaves the client view frozen;
  and any server event arriving before `handle.subscribe_events()` is called
  permanently deafens the broadcast channel for the life of the connection.
- **Renovation**: Handle `SubscribeContext`, `UnsubscribeContext`, and
  `WatchContexts` in `reject_not_ready`; emit `FeedEvent::Resubscribed` only
  after `subscribe_context` succeeds and call `mirror.begin_rehydrate()`; make
  `BlockEventsForwarder::closed` a warn-once latch rather than dropping future
  sends.
- **First test**: Send an event through `BlockEventsForwarder` with 0 active
  receivers, then call `event_tx.subscribe()`, send a second event, and assert
  the subscriber receives it.

### 5.2 Server `run_context_feed` wakes on all contexts' events, and `spawn_editor_reconciler` holds an `Arc<SharedKernelState>` reference cycle
- **Observed**:
  1. `run_context_feed` (`crates/kaijutsu-server/src/context_feed.rs:80-175`)
     pushes events from *all* contexts into `batch` in `append_message` (`154-158`)
     and only filters by `flow.context_id() == Some(context_id)` inside
     `deliver()` (`172`) after a 4ms `FEED_BATCH_WINDOW` timer. Its inner
     `complete < batch.len()` loop (`100-108`) awaits `sub.recv_event()` without
     a deadline or size cap.
  2. `spawn_editor_reconciler` (`crates/kaijutsu-server/src/rpc.rs:423-474`,
     called at `ssh.rs:1487`) owns an `Arc<ServerRegistry>` ->
     `Arc<SharedKernelState>` while looping on `appended_sub.recv()` /
     `replaced_sub.recv()` without selecting on `shutdown.cancelled()`.
- **Consequence**: Every block mutation on any context wakes every open
  `run_context_feed` task and starts a 4ms timer; and `SharedKernelState::drop`
  (`rpc.rs:374-400`) can never run while `spawn_editor_reconciler` is alive.
- **Renovation**: Filter by `context_id` and `carries(&flow)` inside
  `append_message` before opening the 4ms batch window; pass `Weak<Kernel>` +
  `CancellationToken` into `spawn_editor_reconciler` and select on
  `shutdown.cancelled()`.
- **First test**: Spawn `spawn_editor_reconciler`, drop the outer
  `Arc<SharedKernelState>`, and assert `shutdown.is_cancelled()` becomes `true`
  and the reconciler thread exits.

### 5.3 `DriftRouter` / `KernelDb` desynchronization via `try_write()` and wrong reviewer helper in `set_context_cwd`
- **Observed**:
  1. `set_context_state`, `conclude`, `rename_context`, `promote_context`,
     `demote_context`, and `archive_context`
     (`crates/kaijutsu-server/src/rpc.rs:6330-7008`) use `drift().try_write()`
     and either fail with `"drift router busy"` or log a warning and return
     `Ok(())` after mutating `KernelDb`.
  2. `set_context_cwd` (`rpc.rs:7853`) calls `context_reviewer(kernel, context_id)`
     instead of `context_reviewer_for(kernel, context_id, principal)`.
  3. `cherry_pick_block` (`rpc.rs:4789`) uses `insert_block_as` instead of
     `insert_from_snapshot_as`, dropping tool, file, notification, resource, and
     style metadata.
- **Renovation**: Use synchronous `drift().write()` after committing to
  `KernelDb`; call `context_reviewer_for` in `set_context_cwd`; use
  `insert_from_snapshot_as` in `cherry_pick_block`.
- **First test**: Call `rename_context` while another thread briefly holds
  `drift().read()` and assert `DriftRouter` reflects the new label.

### 5.4 Destructive SFTP `POSIX_RENAME`, `MountTable` `..` traversal, and `/r` symlink `getattr` failure
- **Observed**:
  1. `SftpSession::extended` for `POSIX_RENAME`
     (`crates/kaijutsu-server/src/sftp.rs:795-805`) runs
     `if self.vfs.exists(&to).await { self.vfs.unlink(&to).await?; }` before
     `self.vfs.rename(&from, &to).await`.
  2. `MountTable::normalize_mount_path`, `owner_of`, and `resolve_real_path_sync`
     (`crates/kaijutsu-kernel/src/vfs/mount.rs:330-356, 742-877`) and
     `ssh::validate_rw_mounts` (`crates/kaijutsu-server/src/ssh.rs:164-218`) do
     not lexically normalize `.` and `..` before matching mount prefixes.
  3. `ShareRegistry::getattr` (`crates/kaijutsu-kernel/src/vfs/backends/share.rs:233-242`)
     calls `generations_for` after `lstat`, which calls `resolve_follow` on the
     symlink target (`share_server.rs:527`).
- **Consequence**: SFTP self-rename (`a.txt` -> `a.txt`) or renaming a missing
  source over an existing destination deletes `to`;
  `table.is_writable(Path::new("/tmp/../etc/passwd"))` returns `true` and
  `resolve_real_path_sync(Path::new("/tmp/../../etc"))` escapes the mount root;
  and `getattr` on a dangling or out-of-jail symlink in `/r/<client>` fails with
  `NotFound` or `PermissionDenied` instead of returning `FileType::Symlink`.
- **Renovation**: Delete the pre-unlink in SFTP `POSIX_RENAME`; lexically
  normalize `.` and `..` in `MountTable::normalize_mount_path` and
  `validate_rw_mounts`; skip `generations_for` when `attr.is_symlink()` in
  `ShareRegistry::getattr`.
- **First test**: Run SFTP `posix-rename` from `a.txt` to `a.txt` (and from
  `missing.txt` to `a.txt`) and assert `a.txt` survives both calls.

---

## Section 6: Beat Scheduler, Hyoushigi, Audio & Musical Timebase

### 6.1 Blocking CAS `fsync`, synchronous ABC derivation, and `KernelDb` SQLite locks on the beat thread
- **Observed**: `BeatScheduler::process_track`
  (`crates/kaijutsu-server/src/beat.rs:1665, 1676, 1797`) calls
  `materialize_track` and `publish_render_cues` synchronously on the
  single-threaded beat scheduler. `materialize_committed`
  (`crates/kaijutsu-kernel/src/hyoushigi/mod.rs:662-822`) holds `timeline.lock()`
  across `AbcToMidiDeriver::derive`, two `FileStore::store` calls (each running
  `file.sync_all()` at `crates/kaijutsu-cas/src/staging.rs:119-123`), and
  `KernelDb` SQLite writes (`761, 794`). `publish_render_cues` (`beat.rs:1797`)
  then reads each crossed cell back from disk via `cas.retrieve(&cref.hash)`, and
  `transport_env` (`beat.rs:2007`) clones and sorts every block in
  `score_context` on every tick. Meanwhile, `Timeline` (`crates/kaijutsu-hyoushigi/src/engine.rs:190-196`)
  never evicts committed cells or `cas` bytes from RAM.
- **Consequence**: Disk `fsync` latency or `KernelDb` lock contention stalls the
  shared pulse across all tracks and blocks concurrent producer submissions on
  `timeline.lock()`, while `Timeline` grows without bound in RAM.
- **Renovation**: Serve crossed cell bytes from `Timeline` RAM in
  `publish_render_cues`, offload durable CAS/SQLite materialization from the
  beat thread after releasing `timeline.lock()`, and compact committed cells in
  `Timeline` past the fallback window.
- **First test**: Inject a 200ms delay into block/CAS materialization and assert
  `BeatScheduler` continues firing subsequent beats within jitter tolerance.

### 6.2 `BeatScheduler::fire_due` passes `(scheduled, scheduled)` to `ClockSource::next_fire` and `Transport::Starving` never resumes
- **Observed**:
  1. `BeatScheduler::fire_due` (`crates/kaijutsu-server/src/beat.rs:1341`) calls
     `tr.clock.next_fire(scheduled, scheduled)` instead of
     `tr.clock.next_fire(effective_scheduled, now)` because
     `SystemClock::next_fire(_last, now)` (`crates/kaijutsu-server/src/clock.rs:88-90`)
     computes `now + self.period` instead of `last + self.period`.
  2. When a track enters `Transport::Starving` (`beat.rs:1367-1371`),
     `apply_clock_estimate` (`beat.rs:947-1007`) updates `ModeledClock` but
     never transitions `Transport::Starving` back to `Transport::Playing` or
     pushes a `ScheduledBeat` onto `self.heap`.
- **Consequence**: `ModeledClock::next_fire` evaluates `starving(scheduled)` on
  a stale instant and can schedule beats in the past (`< now`), and a track that
  starves during a 2s external MIDI clock pause stays wedged forever even after
  clock estimates resume.
- **Renovation**: Change `SystemClock::next_fire(last, _now)` to return
  `Some(last + self.period)`, pass `(effective_scheduled, now)` in `fire_due`,
  and resume `Transport::Starving` -> `Transport::Playing` (pushing to
  `self.heap`) in `apply_clock_estimate`.
- **First test**: Starve a `ModeledClock` track past `STARVE_AFTER` so
  `transport == Transport::Starving`, then feed a fresh `ClockEstimate` to
  `apply_clock_estimate` and assert `transport == Transport::Playing` with a
  scheduled beat on `heap`.

### 6.3 Multi-voice ABC tunes play sequentially instead of simultaneously, non-quarter `Q:` beat units are ignored, and `CuePayload::Cas` ABC cues are dropped
- **Observed**:
  1. In `build_single_track_writer` (`crates/kaijutsu-abc/src/midi.rs:273`,
     called by `midi::events`), `writer.current_tick` is never reset to `0` at
     the start of each voice in `for voice in &tune.voices`.
  2. `abc_to_timed_events` (`crates/kaijutsu-audio-runtime/src/dj/midi.rs:234-235`)
     computes `secs_per_tick = 60.0 / bpm / ticks_per_beat`, ignoring
     `tempo.beat_unit` `(num, den)`.
  3. `dj/midi.rs:281-285` logs a warning and drops any MIDI cue with
     `CuePayload::Cas(hash)` (emitted for any score > 4 KiB at `beat.rs:1788`),
     and `dj/audio.rs:197` routes `audio/midi` cues to `rodio`.
- **Consequence**: Multi-voice ABC scores on the live ALSA sink play Voice 2
  after Voice 1 ends; scores with `Q:1/2=120` or `Q:3/8=120` play at the wrong
  tempo; and scores > 4 KiB are silently dropped at playback time.
- **Renovation**: Reset `writer.current_tick = 0` per voice in
  `build_single_track_writer`; scale `secs_per_tick` by `tempo.beat_unit` in
  `abc_to_timed_events`; load `CuePayload::Cas` from `PrefetchState::cached_path`
  in `dj/midi.rs` and exclude `audio/midi` in `dj/audio.rs`.
- **First test**: Parse a two-voice ABC tune (`V:1\nC4|\nV:2\nE4|`), call
  `kaijutsu_abc::midi::events`, and assert both `NoteOn` events have `tick == 0`.

### 6.4 `CancelOnDrop` fires after normal `ModelOutput` delivery, `DjCore::settle` drops `cue_dropped` telemetry, and `drain_track_failures` wedges on missing anchors
- **Observed**:
  1. `ModelOutput::resolve` (`crates/kaijutsu-kernel/src/hyoushigi/model.rs:68-78`)
     never disarms `CancelOnDrop` when `receiver.await` succeeds (`docs/issues.md:691`).
  2. In `dj/thread.rs:693-710`, `due_clicks` runs before `due_cues` and calls
     `self.settle(now)` (`dj/core.rs:483-520`), which clears `self.pending_cues`
     and discards `ClockFallback.dropped_cues` before `due_cues` can report
     `kaijutsu.dj.cue_dropped`.
  3. `drain_track_failures` (`crates/kaijutsu-server/src/beat.rs:1951-1961`)
     executes `break;` without advancing `failure_water` if `anchor` is `None`.
  4. Nine tests in `crates/kaijutsu-audio-runtime/src/{audio_sched.rs,dj/audio.rs}`
     hardcode `/home/atobey/src/pawlsa-mcp/pawlsa-test.wav` and `return;` when
     absent.
- **Renovation**: Disarm `CancelOnDrop` on `Ok(bytes)`; accumulate `dropped_cues`
  on `DjCore` across `settle` calls until drained by `due_cues`; fall back to
  `score_ctx` or advance `failure_water` in `drain_track_failures`; synthesize
  an in-memory WAV fixture in `audio-runtime` tests.
- **First test**: Schedule a future cue in `DjCore`, advance `now` past
  `FREE_RUN_MAX`, call `due_clicks` then `due_cues`, and assert `due_cues`
  reports `dropped == 1` with `DropReason::FreeRunExpired`.

---

## Section 7: Interactive Surfaces, Editor & Bridges

### 7.1 `EditorCore` strips trailing newlines on `new`, `insert_at`, `apply_remote_text`, and `apply_substitution`
- **Observed**: `EditorCore::text()` (`crates/kaijutsu-editor/src/lib.rs:277-279`)
  always strips one trailing `\n`, while modalkit's `EditBuffer::from_str` and
  `set_text` only append a trailing `\n` when the input does not already end
  with `\n`. `EditorCore::new` (`170`), `insert_at` (`239-249`),
  `apply_remote_text` (`321-349`), and `apply_substitution` (`611-623`) pass
  strings directly to `from_str` / `set_text` without appending the structural
  `\n`. In `insert_at`, `diff_op(&before, &after)` is computed before `set_text`
  eats the trailing `\n`.
- **Consequence**: Loading a draft ending in `\n` drops the newline; pasting or
  running `:s` on a buffer ending with a blank line eats the blank line and
  desynchronizes `EditorCore` from the kernel block, triggering false
  `peer_wrote` reconciliation and requiring the two-write
  `supplied_terminator_at_tail` workaround in `crates/kaijutsu-kernel/src/editor.rs:942-1000`.
- **Renovation**: Always append one structural `\n` before calling
  `EditBuffer::from_str` / `set_text` inside `EditorCore`, then retire the
  two-write `supplied_terminator_at_tail` workaround in `kernel/src/editor.rs`.
- **First test**: Round-trip `["", "a", "a\n", "a\n\n"]` through `EditorCore::new`,
  `insert_at`, `apply_remote_text`, and `apply_substitution`, asserting
  `core.text()` and the emitted `EditOp` preserve trailing newlines identically.

### 7.2 `kaijutsu-app` spawns a replacement global `RpcActor` SSH connection on every cache-miss context switch and wipes block collapse state
- **Observed**:
  1. In `handle_context_switch` (`crates/kaijutsu-app/src/view/sync.rs:393-408`),
     whenever `ctx_id` is not in `doc_cache`, it sends
     `BootstrapCommand::SpawnActor` with a new random UUID instance, which
     replaces the global `RpcActor` resource (`actor_plugin.rs:1311`), bumps
     `RpcActor.generation`, clears `SessionPrincipal`, detaches `ComposeBuffers`
     (`input/compose/systems.rs:346-359`), resets `LedgerMirror`
     (`connection/ledger.rs:970-977`), drops the `EditorOutbox` writer, and
     terminates all existing context feeds in `DocumentCache`.
  2. `sync_main_cell_to_conversation` (`view/sync.rs:312-324`) rebuilds the
     single `MainCell`'s `RenderBlockStore` from the outgoing context's
     `prev_collapsed` map, losing all user collapse/expand choices across context
     switches.
- **Renovation**: Use the existing `RpcActor` handle in `handle_context_switch`
  to join, fetch, and subscribe to an uncached context (reserving `SpawnActor`
  for initial connect and dead-actor reconnect), and persist `collapsed_blocks`
  per context in `CachedContext` (`view/document.rs:20-52`).
- **First test**: Switch `MainCell` from Context A (with a collapsed `Thinking`
  block) to an uncached Context B and back to A; assert `RpcActor.generation`
  does not increment and the `Thinking` block in A remains collapsed.

### 7.3 `kaijutsu-app` input/scroll handlers leak across screens and split panes render duplicate active-context surfaces
- **Observed**:
  1. `in_conversation` and `scroll_context_active`
     (`crates/kaijutsu-app/src/input/focus.rs:13-20`) do not check
     `in_state(Screen::Conversation)` or overlay state (`AskSheet`,
     `LedgerRibbon`), and `handle_prompt_prefill` (`input/compose/systems.rs:456-469`)
     runs on `Screen::Room`, `Screen::Editor`, and `Screen::Diff` (opening a
     `ShellDock` that `derive_contexts` at `input/context.rs:182-207` refuses to
     route keys into).
  2. `screen_revealing_switched_context` (`view/sync.rs:36-41`) forces
     `Screen::Editor` and `Screen::Diff` back to `Screen::Conversation` on any
     context switch.
  3. `extract_conversation_surfaces` (`view/surface/extract.rs:450-526`) draws
     the single `main_cell` geometry into every split pane's
     `ConversationSurface` underneath `UnfocusedPaneSummary`.
  4. `build_shell_dock_glyphs` (`view/shell_dock.rs:271-307`) ignores
     `kind_changed` and `selection_changed`, leaving the shell dock cursor
     fixed as a `Block` with no visual selection highlights.
- **Renovation**: Gate conversation/compose/scroll conditions on
  `Screen::Conversation` and closed modal overlays; restrict
  `screen_revealing_switched_context` to `Screen::Room`; suppress
  `ConversationSurface` extraction on unfocused split panes; wire cursor kind
  and selection geometry in `build_shell_dock_glyphs`.
- **First test**: In `kaijutsu-app`, assert `MouseWheel` and `Ctrl+A g` while on
  `Screen::Diff` neither mutate `ConversationScrollState` nor open `ShellDock`.

### 7.4 `kaijutsu-mcp` ignores `wait_for_notification.timeout_ms` and drops between-call hook events; `kaijutsu-acp` resolves `prompt()` before the turn runs
- **Observed**:
  1. `KaijutsuMcp::wait_for_notification` (`crates/kaijutsu-mcp/src/lib.rs:421-457`)
     ignores `params.timeout_ms` (`let _ = params;`) and only subscribes to
     `self.hook_tx` inside the tool call (`429`), dropping any hook event that
     arrived between calls.
  2. `KaijutsuAgent::prompt` (`crates/kaijutsu-acp/src/bridge.rs:416-434`)
     returns `Ok(PromptResponse::new(StopReason::EndTurn))` immediately after
     `actor.prompt` spawns the turn, before the model emits any tokens.
- **Renovation**: Wrap `rx.recv()` in `tokio::time::timeout` and hold a
  persistent receiver on `KaijutsuMcp`; await the turn's terminal `TurnEvent` in
  `KaijutsuAgent::prompt` before returning `PromptResponse`.
- **First test**: Send a `HookEvent` before calling
  `wait_for_notification(WaitForNotificationParams { timeout_ms: 50 })` and
  assert the queued event is returned immediately, and a subsequent call with no
  event times out after 50ms.

---

## Section 8: Semantic Index, Telemetry & Peer Crates

### 8.1 `SemanticIndex::new` wipes all metadata and persisted synthesis rows if HNSW graph load fails
- **Observed**: In `crates/kaijutsu-index/src/index.rs:83-90`, if
  `load_hnsw_with_dist` fails on existing `index.hnsw.{graph,data}` files,
  `HnswIndex::new` logs a warning and returns an empty `HnswIndex`
  (`graph_point_count() == 0`). Immediately afterward, `SemanticIndex::new`
  (`crates/kaijutsu-index/src/lib.rs:226-229`) sees `graph_count != meta_count`
  and runs `this.rebuild()` (`402-455`), which sees `Err(_)` from
  `hnsw.get_embedding(*slot)` for every row in `index_entries` and calls
  `meta.remove(*ctx_id)` (`429`) — deleting every row in `index_entries` and
  every persisted row in `synthesis`, `synthesis_keywords`, and
  `synthesis_top_blocks` (`metadata.rs:245-262`).
  In addition, when `rebuild()` replaces `*hnsw = new_index` (`lib.rs:441`), the
  `Box::leak(hnsw_io_box)` allocation from `HnswIndex::new` (`index.rs:50`) is
  never reclaimed.
- **Consequence**: A corrupt or truncated HNSW graph file causes startup
  `rebuild()` to silently delete all persisted synthesis gists/keywords in
  `index_meta.db` even though synthesis rows do not depend on HNSW graph slots.
- **Renovation**: In `SemanticIndex::rebuild`, delete only the `index_entries`
  row (not `delete_synthesis_rows`) when repairing an orphaned HNSW slot, and
  store `HnswIo` in an owned/droppable wrapper once the borrowing `Hnsw` is
  replaced.
- **First test**: In `kaijutsu-index`, store a synthesis result and an index
  entry for `ctx`, corrupt `index.hnsw.graph`, reopen `SemanticIndex`, and
  assert `idx.synthesis_cache().get_any(ctx)` is preserved.

### 8.2 ` index_watcher` inner debounce loop does not distinguish stream closure from timeout, and `MetadataStore` silently skips corrupt `context_id` blobs
- **Observed**:
  1. `spawn_index_watcher` (`crates/kaijutsu-index/src/watcher.rs:64-70`) matches
     `Ok(None)` (stream closed) with the same `_ => break` arm as `Err(_)`
     (1s debounce timeout), so after indexing the batch it polls
     `events.recv().await` a second time after `None`.
  2. `MetadataStore::get_context_id` (`crates/kaijutsu-index/src/metadata.rs:157-162`),
     `load_all_synthesis` (`348-350`), `all_slots` (`433`), and
     `all_context_ids` (`459`) silently ignore rows where `context_id.len() != 16`,
     and `SemanticIndex::len()` (`lib.rs:565-568`) swallows DB errors with
     `meta.count().unwrap_or(0)`.
- **Renovation**: Track `stream_closed = true` on `Ok(None)` in
  `spawn_index_watcher` so it saves and exits cleanly after flushing `pending`;
  return `Err(IndexError::Database("corrupt context_id blob".into()))` when
  `bytes.len() != 16`.
- **First test**: Feed a `StatusReceiver` that yields one terminal event followed
  immediately by `None` (and panics if polled after `None`), and assert
  `spawn_index_watcher` indexes the event, saves the HNSW index, and exits cleanly.
