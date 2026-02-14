# Kaijutsu Deep Code Review — Gemini Pro Findings

**Date:** 2026-02-14
**Reviewer:** Gemini 3 Pro (via consult_gemini_pro)
**Scope:** ~76 KLOC across 7 crates (15 chunks, all completed)

## Legend

- **P0** — Real bug, data loss or corruption risk
- **P1** — Latent bug, race condition, or security issue
- **P2** — Cleanup, inconsistency, or dead code
- **P3** — Testing gap (no immediate bug, but risky)

---

## Chunk 1: kaijutsu-crdt (~3.3 KLOC)

### P0: Ordering Precision Loss (Fractional Indexing)
- **document.rs:530** — Block order stored as `(f64 * 1_000_000) as i64`, allowing only ~20 subdivisions before collisions. Repeated insertions between blocks will produce duplicate order values.
- **Fix:** Store f64 directly or use string-based fractional indexing.

### P1: BlockId Parsing Vulnerability
- **block.rs:45** — `BlockId::from_key` uses `/` separator without validating that `document_id`/`agent_id` don't contain slashes. An agent_id like `"user/alice"` breaks parsing.
- **Fix:** Validate inputs in `BlockId::new` to reject `/`.

### P2: Silent JSON parse failure
- **document.rs:233** — `tool_input` JSON parsing uses `.ok()`, converting parse errors to `None`. Partial/streaming JSON silently becomes "no input".
- **Fix:** Return raw text when JSON parsing fails.

### P1: `next_seq` collision on snapshot restore
- **document.rs:1074** — Restoring from filtered snapshots (e.g., `fork_at_version`) may reset `next_seq` to stale value, causing `DuplicateBlock`.
- **Fix:** Track `next_seq` independently, not inferred from visible blocks.

### P3: Concurrent tests are `#[ignore]`
- **lib.rs:188** — `test_concurrent_block_insertion` and `test_concurrent_text_editing` are disabled. These are the most critical tests for a CRDT.

### P3: No drift block DAG interaction tests
- Drift blocks have struct tests but no tests for how they interact with `get_ancestors`.

### P2: Redundant ordering logic
- **document.rs:296** — `calc_order_index` repeats "get order from root map" three times. Extract helper.

---

## Chunk 2: kaijutsu-kernel Core (~6.7 KLOC)

### P0: Drift Flush Data Loss
- **drift.rs:770** — `DriftFlushEngine` drains staging queue *before* insertion. If `insert_from_snapshot` fails, drift content is permanently lost.
- **Fix:** Only drain after confirmed insertion, or re-queue on failure.

### P0: Silent Data Loss on Snapshot Corruption
- **block_store.rs:768** — `load_from_db` catches deserialization errors and creates an empty document. User sees blank doc; edits may overwrite the corrupted-but-recoverable data.
- **Fix:** Return error or mark document as "Corrupted/Unreadable".

### P1: Serialization failures swallowed in sync
- **block_store.rs:495,532,568,606,659,687** — `serde_json::to_vec(&ops).unwrap_or_default()` sends empty bytes to subscribers on failure.
- **Fix:** Propagate error instead of broadcasting empty data.

### P1: History entry left pending on panic
- **kernel.rs:274** — History entry added before execution, updated after. If execution panics, entry stays "pending" forever.

### P2: `EngineArgs` ignores `false` booleans
- **tools.rs:232** — `Bool(true)` becomes a flag, `Bool(false)` is silently dropped.

### P2: Hardcoded truncation limits in drift
- **drift.rs:360,422** — 2000 and 8000 byte limits buried in helpers. Should be configurable.

### P3: No tests for drift flush failure scenarios

---

## Chunk 3: Kernel Subsystems — LLM + VFS + MCP Pool (~8.5 KLOC)

### P1: VFS Path Resolution Fails on Symlinked Roots
- **vfs/backends/local.rs:104** — On macOS (`/tmp` -> `/private/tmp`), creating files in a symlinked root fails with `PathEscapesRoot` because uncanonicalized path doesn't match canonical root.
- **Fix:** Canonicalize root at construction time.

### P0: Broken Tool Use Streaming
- **llm/stream.rs:422** — `RigStreamAdapter` emits `ToolUse` immediately on first `ToolCall` event (often with empty args) and ignores all `ToolCallDelta` events. Most providers stream tool calls incrementally.
- **Fix:** Buffer tool call, apply deltas, emit only when complete.

### P1: MCP Resource Cache Race
- **mcp_pool.rs:673** — `on_resource_updated` removes cache entry, but a concurrent in-flight fetch can re-insert stale data after invalidation.
- **Fix:** Use generation counters or timestamps.

### P2: Hardcoded elicitation timeout (300s)
- **mcp_pool.rs:775** — Should be configurable.

### P2: Provider type string literals scattered
- **llm/mod.rs:248** — "anthropic", "gemini" etc hardcoded in multiple places. Should be an enum.

---

## Chunk 4: Kernel Tools — Block Tools + File Tools + Git (~6.3 KLOC)

### P0: Non-Atomic "Atomic" Edits
- **block_tools/engines.rs:326** and **file_tools/edit.rs:108** — Batch edits applied sequentially. If op 3 of 5 fails, ops 1-2 remain applied. Breaks atomicity contract.
- **Fix:** Validate all ops before applying, or use CRDT transactions.

### P1: Partial Flush Data Loss
- **file_tools/cache.rs:188** — `flush_dirty` aborts on first VFS write error. Remaining files not flushed.
- **Fix:** Collect errors, continue flushing all files.

### P2: Silent binary content drop in git diff
- **git_ops.rs:247** — Non-UTF8 lines silently dropped. Insert `[Binary content]` placeholder.

### P1: Git commit summarize race
- **git_engine.rs:320** — Diff fetched, LLM called (slow), then commit. Staged content may change during LLM call.

### P1: File cache load race
- **file_tools/cache.rs:88** — Concurrent `create_document` calls can fail if second thread checks before first finishes inserting block.

### P2: Unused export `line_end_byte_offset`
- **block_tools/translate.rs:66** — Public but unused.

### P3: No file size limits in `FileDocumentCache`
- Opening huge files will OOM the kernel.

---

## Chunk 5: Kernel Remaining — DB + Config + Agents (~3.9 KLOC)

### P1: Config Watcher Echo Loop
- **config_backend.rs:670** — Time-based flushing map (200ms) is unreliable. FS events may arrive late, causing echo loop.
- **Fix:** Use content hash comparison.

### P1: Rhai `insert_block` swallows JSON errors
- **rhai_engine.rs:172** — Invalid JSON defaults to `Null`, may create malformed blocks.

### P2: Conversation DB `save_block` could panic in transaction
- **conversation_db.rs:209** — If serialization panics inside `DELETE + re-INSERT` loop, transaction may be left open.

### P2: Legacy `DocumentKind` aliases
- **db.rs:36** — `output`, `system`, `user_message` are legacy aliases. Clean up if not needed for migration.

### P3: No config watcher debounce tests
### P3: No Rhai memory limit tests

---

## Chunk 6: kaijutsu-client (~4.6 KLOC)

### P0: SSH Channels Dropped on Connect
- **lib.rs:38** — `connect_ssh` returns only the `rpc` channel stream; `control` and `events` channels are dropped, sending `SSH_MSG_CHANNEL_CLOSE`. Server may terminate connection.
- **Fix:** Retain all channels in `RpcClient`.

### P0: Sync Buffer Overflow Causes Permanent Desync
- **sync.rs:139** — When pending ops exceed 200, oldest are dropped. Client transitions to "Synced" but is permanently missing those blocks.
- **Fix:** Trigger full re-sync if buffer overflows.

### P1: RPC Actor Concurrent PushOps Ordering
- **actor.rs:565** — Each command spawned as separate `spawn_local` task. PushOps could arrive at server out of order if one yields.
- **Fix:** Process PushOps serially.

### P2: `unwrap_or_default` on JSON serialization
- **rpc.rs:462** — `call_mcp_tool` sends empty string on serialization failure.

### P2: Unused SSH channels struct fields
- **ssh.rs:118** — `control` and `events` populated but never read.

### P3: No reconnection backoff tests
### P3: No sync buffer overflow recovery tests

---

## Chunk 7: kaijutsu-server RPC (~6 KLOC)

### P0: Drift Flush Data Loss (Server Side)
- **rpc.rs:1632** — Same drain-before-insert pattern as kernel. Duplicate of Chunk 2 finding.

### P1: Agentic Loop No Backoff
- **rpc.rs:2220** — `process_llm_stream` loops up to 20 iterations with no delay between tool calls. Stuck model burns tokens rapidly.

### P1: Race in `join_context`
- **rpc.rs:1070** — Check-then-create document pattern. Two concurrent joins see "not exists", both try create.

### P2: Massive rpc.rs file (~2700 lines)
- Split into `rpc/world.rs`, `rpc/kernel.rs`, `rpc/vfs.rs`.

### P2: Hardcoded "default" context name
- **rpc.rs:430** — Define constant.

### P2: Duplicated tool registration boilerplate
- **rpc.rs:76** — Could use macro or registry pattern.

### P3: No agentic loop iteration limit tests
### P3: No VFS error -> Cap'n Proto exception mapping tests

---

## Chunk 8: Server Backends + Auth (~6 KLOC)

### P1: Git Backend Flush Swallows Errors
- **git_backend.rs:379** — `flush_all` logs warning on failure but returns `Ok(())`. Caller thinks data is safe.

### P1: Path Traversal in `resolve_path`
- **git_backend.rs:262** — `..` components not sanitized. Potential directory traversal.
- **Fix:** Reject `..` components or canonicalize before splitting.

### P1: SSH Auth Timing Attack
- **ssh.rs:335** — DB lookup timing difference can enumerate valid keys. Verify `russh` `auth_rejection_time` covers this.

### P1: Git Watcher Echo Loop
- **git_backend.rs:446** — Same pattern as config watcher. Flush triggers watcher, watcher writes back to CRDT.

### P2: Duplicated `backend_to_io` error conversion
- **git_filesystem.rs:88** and **docs_filesystem.rs:66** — Extract to shared module.

### P2: Duplicated `entry_info_to_dir_entry` / `entry_info_to_metadata`
- **mount_backend.rs:66** and **git_filesystem.rs:107** — Extract to shared module.

### P3: No git backend watcher integration tests
### P3: No SSH auth persistence/migration tests

---

## Chunk 9: App — Input + Connection + Dashboard (~4 KLOC)

### P1: Dashboard List Race Condition
- **dashboard/mod.rs:266** — Detached async tasks for kernel/context list. Rapid switching can cause stale data to overwrite fresh data. `KernelAttached` lacks generation ID.

### P2: Input Sequence Timeout Edge Case
- **input/dispatch.rs:56** — `is_expired` uses `>` not `>=`. Unlikely to matter in practice.

### P1: FocusArea Desync on Screen Transition
- **input/systems.rs:32** — `sync_focus_from_screen` may reset focus during typing if screen state is re-set to same value.

### P2: `InputContext` may be redundant with `FocusArea`
- **input/context.rs:54** — 1:1 mapping in most cases.

### P3: No input dispatch priority tests (Dialog vs Global)
### P3: No reconnect backoff tests

---

## Chunk 10: App — Cell System (~5 KLOC)

### P1: Scroll Jitter
- **cell/systems.rs:1145** — `visible_height` updated after `smooth_scroll` runs, causing 1-frame scroll jitter on resize/layout change.

### P0: Text Shaping Performance Bottleneck
- **cell/systems.rs:1072** — `visual_line_count` (HarfBuzz text shaping) called every frame for every block when any layout change occurs. Long conversations will frame-drop on every streamed token.
- **Fix:** Cache per-block height, only re-measure on content or width change.

### P1: Cursor Position Desync
- **cell/systems.rs:1367** — `compute_cursor_position` re-derives row/col from raw string instead of shaped buffer layout. Can mismatch visual text.

### P2: Duplicate height calculation logic
- **cell/systems.rs:1098** vs **cell/components.rs:434** — Both compute height from line count with different padding.

### P3: No scroll clamping tests (content shrink while scrolled)
### P3: No layout generation invalidation tests

---

## Chunk 11: App — Tiling WM + Layout + UI State (~6 KLOC)

### P1: Resize Logic Flaw
- **ui/tiling.rs:739** — Resize clamps individual ratios before normalization. If one ratio hits min (0.1), normalization pushes it below the minimum.
- **Fix:** Calculate max delta respecting both panes' bounds before applying.

### P1: Stale PaneMarker After MRU Assignment
- **ui/tiling_reconciler.rs:825** — `assign_mru_to_empty_panes` updates `TilingTree` and `PaneSavedState` but NOT the `PaneMarker` component. Systems querying `PaneMarker` see stale data.

### P1: Stale Materials on Theme Change
- **ui/materials.rs:30** — `setup_material_cache` runs only at `Startup`. Theme reload doesn't update cached materials.
- **Fix:** Add system watching `Res<Theme>` changes to update material assets.

### P2: Dual Layout Systems
- `layout.rs` (RON-based for Dashboard) and `tiling.rs` (Rust-struct for Conversation) duplicate reconciliation logic. Consider unifying.

### P2: Dead code in tiling.rs
- `WritingDirection::VerticalRl`, `PaneContent::Editor`, `PaneContent::Shell`, `PaneContent::Text`, `Edge::East`/`West` — all unused.

### P2: `LayoutSwitched` message defined but never read
- **ui/layout.rs:196**

### P2: `ViewStack` / `AppScreen` / `View` overlap
- Three partially-overlapping concepts for screen state.

### P2: Compose state loss risk on tree rebuild
- **ui/tiling_reconciler.rs:164** — Fragile if PaneIds ever change during split operations.

### P1: Blocking `hostname::get()` in theme loader
- **ui/theme_loader.rs:50** — Can block during frame update. Cache at startup.

### P3: No tiling reconciler tests at all
### P3: No layout reconciler tests

---

## Chunk 12: App — Constellation (~2.9 KLOC)

### P0: Infinite Recursion / Stack Overflow
- **ui/constellation/mod.rs:327** — `count_tree_descendants` has no cycle detection. Malformed server data with parentage cycle crashes client.
- **Fix:** Add `visited: HashSet` or depth limit.

### P1: Incomplete Mini Click Handler
- **ui/constellation/mini.rs:252** — `handle_mini_render_click` updates focus but does NOT send `ContextSwitchRequested`. Click appears to work visually but doesn't switch context.
- **Fix:** Remove duplicate handler; let `mod.rs` handle clicks.

### P1: Ancestry Line Flicker
- **ui/constellation/render.rs:460** — If `DriftState` temporarily returns empty/incomplete data during reconnect, valid ancestry lines are despawned.

### P2: Fragile Child Indexing in Mini Render
- **ui/constellation/mini.rs:218** — Relies on implicit child order (1st=dot, skip 2nd, 3rd=label). Use marker components.

### P2: Hardcoded `FocusArea::Constellation` on Dialog Close
- **ui/constellation/create_dialog.rs:435** — Should restore previous focus.

### P2: Duplicate truncation logic
- `render.rs:620` and `mini.rs:293` — Identical string truncation. Extract to shared util.

### P2: Unnecessary `#[allow(dead_code)]` on `ModelPickerDialog`
- **model_picker.rs:50** — It is used.

### P3: No radial layout unit tests
### P3: No cycle detection tests

---

## Chunk 13: App — MSDF Text Rendering (~5.2 KLOC)

### P0: Atlas Corruption on Growth
- **text/msdf/atlas.rs:227** — `grow` method re-packs existing regions into new positions via `rect_packer`, but copies pixel data to OLD positions. Packer state completely desynchronized from texture content. Future insertions overwrite existing glyphs.
- **Fix:** Don't support dynamic growth with `rect_packer`. Initialize large (2048x2048) or implement custom shelf-packer.

### P1: Double Extraction of UI Text
- **text/msdf/pipeline.rs:863** — `MsdfUiText` entities extracted via `ui_query` AND `cell_query`. The `ui_query` extraction has empty glyphs (masked by loop being empty), but wastes bandwidth/memory.
- **Fix:** Exclude `With<MsdfText>` from `ui_query`.

### P1: Black Screen on First Frame
- **text/msdf/pipeline.rs:1003** — `MsdfTextTaaNode` returns `Ok(())` if TAA resources missing (first frame/resize). The blit doesn't happen, screen stays black.
- **Fix:** Add fallback blit path when TAA resources unavailable.

### P2: Dead code in buffer.rs
- `advance_width`, `subpixel_offset` in `PositionedGlyph` unused.
- `MsdfTextBuffer::new` unused (only `new_with_width` used).

### P2: Dead code in generator.rs
- `cap_height_em`, `ascent_em` unused.

### P2: Verbose debug geometry code in pipeline.rs
- Lines 30-136 could use quad-pushing helper.

### P3: No atlas growth tests (would catch the corruption bug)
### P3: No UI text rendering tests

---

## Chunk 14+15: App Remaining + MCP + Telemetry (~6.8 KLOC)

### P1: MCP Hook Listener Quadratic Bandwidth
- **kaijutsu-mcp/hook_listener.rs:268** — `push_ops` re-sends all un-acked ops on every call. Rapid hook events cause O(N²) bandwidth.
- **Fix:** Track "optimistic frontier" or `last_pushed_frontier`.

### P1: Background Task Leak in MCP
- **kaijutsu-mcp/lib.rs:245** — Background event listener has no shutdown signal. Multiple `KaijutsuMcp` instances leak tasks.
- **Fix:** Store `AbortHandle`, cancel on drop.

### P1: O(N) Block Lookup in MCP Helpers
- **kaijutsu-mcp/helpers.rs:46** — `find_block` iterates all documents and all blocks. Gets worse as kernel grows.
- **Fix:** Maintain `HashMap<BlockId, DocumentId>` index.

### P2: Inconsistent MCP Tool Error Returns
- Some tools return raw text, others return JSON `{"success": true}`, others return `"Error: ..."`. Standardize on JSON.

### P2: `KAIJUTSU_MCP_TOOLS` list may desync
- **kaijutsu-mcp/hook_types.rs:160** — Manual list must match actual tool impls. Missing entry causes hook loops.

### P2: Dead code in nine_slice.rs
- `CornerPosition`, `EdgePosition` enums defined but unused.

### P2: Commented-out code in main.rs
- **kaijutsu-app/main.rs:122** — `// setup_input_layer,` — remove.

### P3: No MCP drift tool tests
### P3: No telemetry sampler unit tests (critical for cost control)

---

## Summary by Priority

### P0 — Real Bugs / Data Loss (10)
1. Drift flush data loss (kernel + server) — drain before insert
2. Snapshot corruption → silent empty doc
3. SSH channels dropped on connect
4. Sync buffer overflow → permanent desync
5. Non-atomic batch edits
6. Broken tool use streaming (ToolCallDelta ignored)
7. CRDT ordering precision loss (20 subdivisions max)
8. Text shaping performance bottleneck (O(N) per frame)
9. Atlas corruption on growth (packer/texture desync)
10. Constellation infinite recursion on cycle

### P1 — Latent Bugs / Races (24)
1. BlockId parsing vulnerability (slash in IDs)
2. next_seq collision on snapshot restore
3. Serialization failures swallowed in sync
4. VFS path resolution fails on symlinks
5. MCP resource cache race
6. Partial flush data loss
7. Git commit summarize race
8. File cache load race
9. Config watcher echo loop
10. Git watcher echo loop
11. Git backend flush swallows errors
12. Path traversal in git_backend resolve_path
13. SSH auth timing attack
14. RPC actor PushOps ordering
15. Dashboard list race condition
16. Scroll jitter / cursor desync
17. Resize logic flaw (ratios drift below min)
18. Stale PaneMarker after MRU assignment
19. Stale materials on theme change
20. Mini constellation click doesn't switch context
21. Ancestry line flicker on reconnect
22. MCP hook listener quadratic bandwidth
23. MCP background task leak
24. MCP O(N) block lookup

### P2 — Cleanup (25+)
- Hardcoded constants (truncation limits, timeouts, "default")
- Duplicated code (error converters, height calc, path logic, truncation)
- Unused exports, legacy aliases, dead enum variants
- Missing enum for provider types
- Large files needing split (rpc.rs)
- Dual layout systems (layout.rs vs tiling.rs)
- ViewStack/AppScreen/View overlap
- Inconsistent MCP tool error returns
- Dead code in buffer.rs, generator.rs, nine_slice.rs
- Commented-out code in main.rs

### P3 — Testing Gaps (20+)
- Concurrent CRDT tests disabled
- No drift failure tests
- No sync recovery tests
- No reconnect tests
- No input priority tests
- No watcher echo tests
- No file size limit enforcement
- No tiling/layout reconciler tests
- No radial layout or cycle detection tests
- No atlas growth tests
- No MCP drift tool tests
- No telemetry sampler tests

---

## Methodology

### Review Process

This review was conducted on 2026-02-14 using **Gemini 3 Pro** via the `consult_gemini_pro` MCP tool, orchestrated by **Claude Opus 4.6** via Claude Code.

### Chunking Strategy

The ~76 KLOC codebase was divided into 15 review chunks, ordered by dependency (foundations first):

| Chunk | Crate/Module | ~KLOC | Focus |
|-------|-------------|-------|-------|
| 1 | kaijutsu-crdt | 3.3 | CRDT primitives, document, DAG |
| 2 | kaijutsu-kernel core | 6.7 | Kernel, block store, drift, flows |
| 3 | kaijutsu-kernel subsystems | 8.5 | LLM, VFS, MCP pool |
| 4 | kaijutsu-kernel tools | 6.3 | Block tools, file tools, git |
| 5 | kaijutsu-kernel remaining | 3.9 | DB, config, agents, Rhai |
| 6 | kaijutsu-client | 4.6 | RPC client, ActorHandle, sync |
| 7 | kaijutsu-server RPC | 6.0 | 88 Cap'n Proto methods |
| 8 | kaijutsu-server backends | 6.0 | Git backend, kaish, auth, SSH |
| 9 | kaijutsu-app input/connection | 4.0 | Input dispatch, dashboard |
| 10 | kaijutsu-app cells | 5.0 | Cell rendering, block layout |
| 11 | kaijutsu-app tiling/UI | 6.0 | Tiling WM, theme, layout |
| 12 | kaijutsu-app constellation | 2.9 | Context navigation graph |
| 13 | kaijutsu-app MSDF text | 5.2 | Text rendering pipeline |
| 14+15 | kaijutsu-app remaining + MCP + telemetry | 6.8 | Shaders, MCP server, OTel |

### Per-Chunk Prompt

Each chunk was reviewed with the same prompt template:

```
Deep code review of [module description]. Look for:
- **Dead code**: unused functions, unreachable branches, stale imports
- **Latent bugs**: off-by-one, unwrap on fallible paths, race conditions, missing error handling
- **Inconsistencies**: naming conventions, patterns used differently in similar code
- **Cleanup opportunities**: duplicated logic, overly complex functions, missing abstractions
- **Testing gaps**: untested critical paths, missing edge case coverage
- **Test suggestions**: specific tests worth adding, ranked by risk/impact

Be thorough and specific. Reference file:line_number for every finding.
```

Files were passed via `file_paths` parameter, giving Gemini Pro direct access to read source code.

### Execution Details

- **15 Gemini Pro calls** total (hit 1M tokens/min rate limit after 10 parallel calls, waited ~60s, completed remaining 4)
- **Wall clock time**: ~3 minutes for review phase
- **Token usage**: ~1M input tokens (source code), ~50K output tokens (findings)
- Findings were manually triaged by Claude Opus 4.6 into P0/P1/P2/P3 severity levels
- No `semantic_search` cross-referencing was needed — Gemini Pro findings were specific enough with file:line references

### Triage Criteria

| Priority | Criteria |
|----------|---------|
| **P0** | Real bug: data loss, corruption, crash, or severe performance regression |
| **P1** | Latent bug: race condition, security issue, or logic error that requires specific conditions to trigger |
| **P2** | Cleanup: dead code, inconsistency, duplication, or architectural concern |
| **P3** | Testing gap: no immediate bug, but missing coverage for critical paths |

### False Positive Assessment

Some Gemini findings may be false positives or already mitigated:
- **SSH auth timing attack** — `russh` has `auth_rejection_time` set to 1s; needs verification that it covers all paths
- **PushOps ordering** — Cap'n Proto RPC may guarantee ordering within a single connection; needs verification
- **VFS symlink issue** — May only affect macOS, Linux `/tmp` is usually not symlinked
- **Text shaping bottleneck** — May be mitigated by `layout_gen` check; needs profiling to confirm

### Next Steps

1. **Verify P0 findings** against actual code (Gemini sometimes hallucinates line numbers)
2. **Fix confirmed P0s** in focused commits
3. **Add highest-priority tests** for CRDT concurrency, drift flush, sync recovery
4. **Batch P2 cleanup** into a separate PR
5. **Run `cargo clippy`** as final pass
