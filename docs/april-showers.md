# April Showers: Kaijutsu Server & Kernel Tech Debt Review
**Date:** April 4, 2026
**Status:** Research & Strategy

This document outlines critical architectural weaknesses, "difficult fruit," and technical debt identified during a deep-dive review of the `kaijutsu-server` and `kaijutsu-kernel` crates.

---

## 1. Critical Persistence Gap (Durability Risk)

The system previously relied on "voluntary" snapshots for persistence. If the server crashed, all user edits since the last LLM turn were lost.

### Analysis
- **File:** `crates/kaijutsu-server/src/rpc.rs`
- **Line:** 6826
- **Observation:** This is the *only* place in the core RPC/Tool loop where `save_snapshot` is called.

```rust
// crates/kaijutsu-server/src/rpc.rs:6826
// Save final state after streaming completes
if let Err(e) = documents.save_snapshot(context_id) {
    log::warn!("Failed to save snapshot for cell {}: {}", context_id, e);
}
```

- **Vulnerability:** Tools like `block_edit`, `block_create`, and `block_append` (in `crates/kaijutsu-kernel/src/block_tools/engines.rs`) mutate the `BlockStore` in memory but **never** call `save_snapshot`. 

### Resolution (April 6, 2026)

**Partially resolved.** Corrections and fixes applied:

- `block_create` (`insert_block_as`) already called `auto_save` — the original analysis was inaccurate for this method.
- `block_append`/`block_edit` intentionally skip `auto_save` for streaming performance. This is by design — callers flush explicitly when a turn completes. A test (`test_block_append_persists_to_db`) now validates this contract.
- **5 metadata mutations** previously had no `auto_save` and no comment explaining why: `set_ephemeral`, `set_excluded`, `set_content_type`, `set_output`, `set_tool_use_id`. All now call `auto_save`. These are low-frequency operations where the persistence cost is negligible.
- **3 fork methods** (`fork_document`, `fork_document_at_version`, `fork_document_filtered`) inserted DB metadata but never snapshotted content. All now call `auto_save(new_id)` after insertion.

### Further Resolution (April 7, 2026)

- **`merge_ops` now calls `auto_save`.** This is in the active push_ops RPC path (client mutation → push_ops → merge_ops). Without it, server-side persistence of CRDT-synced content was entirely missing. Test: `test_merge_ops_persists_to_db`.

### Remaining
- A "dirty flag + background flusher" pattern is still worth considering for crash resilience during streaming, but the explicit flush contract is workable for now.

---

## 2. Multi-user Isolation Bugs (State Contamination)

The server creates a single shared `Kernel` for all SSH connections, but several "global" trackers within that kernel were not session-aware.

### Analysis
- **File:** `crates/kaijutsu-server/src/rpc.rs`
- **Line:** 137, 1003
- **Observation:** `create_shared_kernel` initialized a single `CurrentContext` tracker.

```rust
// BEFORE (shared across ALL connections):
let current = Arc::new(RwLock::new(None));
```

- **The Bug:** When User A runs `kj context switch foo`, it updates this shared `Arc`. User B, in a completely different SSH session, will suddenly find their "current" context has changed.
- **Label Collisions:** `DriftRouter` (in `drift.rs`) uses a global `label_to_id` map. Two users cannot both have a context named "notes" without clobbering each other.

### Resolution (April 6, 2026)

**Resolved.**

- **Session isolation:** `CurrentContext` (`Arc<RwLock<Option<ContextId>>>`) replaced with `SessionContextMap` (`Arc<RwLock<HashMap<SessionId, ContextId>>>`). Each SSH session's `ToolContext` carries its `SessionId`, so context switches are scoped per-session. The map is stored on `SharedKernelState` and cleaned up when `run_rpc` returns (SSH disconnect).
- **Label uniqueness:** `DriftRouter::register()` now returns `Result` and rejects duplicate labels via `check_label_available()`. Same guard on `register_fork()` and `rename()`. `resolve_prefix()` in `ids.rs` also detects ambiguity on exact label matches (previously returned the first hit without checking for duplicates). New error variant: `DriftError::LabelInUse`.
- **cwd persistence:** `KjBuiltin` now saves the outgoing context's cwd to `context_shell` before switching, and `EmbeddedKaish::with_identity_and_db` restores persisted cwd on session creation. Previously, `cd` changes were lost on context switch or reconnect.

### Further Resolution (April 7, 2026)

- **`context_env` and `init_script` now applied on session creation.** Added `EmbeddedKaish::apply_context_config()` which reads env vars and init_script from KernelDb and applies them via the kaish kernel. Called at both EmbeddedKaish creation sites in rpc.rs. Tests: `test_context_env_applied_on_creation`, `test_init_script_applied_on_creation`.

### Remaining
- Label scoping is global (first-come-first-served), not per-principal. Two different users cannot independently use the same label. This is acceptable for now but may need principal-scoped labels if multi-tenancy grows.

---

## 3. Coarse-Grained Locking (Performance Bottlenecks)

Heavy use of `Mutex` and `RwLock` on large structures inhibits concurrency.

### Analysis
- **McpServerPool:** `crates/kaijutsu-kernel/src/mcp_pool.rs`
  - Uses `Arc<Mutex<ConnectedServer>>` per server. MCP supports concurrent requests, but this lock serializes them.
- **KernelDb:** `crates/kaijutsu-kernel/src/block_store.rs`
  - Wrapped in `Arc<parking_lot::Mutex<KernelDb>>`. Every RPC call (listing contexts, getting kernel info) must wait for this single lock.

### Recommendation
- Investigate `rmcp` concurrency to remove the per-server `Mutex`.
- Migrate `KernelDb` to a connection pool (e.g., `r2d2` or `sqlx`) to allow concurrent reads.

---

## 4. "God Object" Bloat

### Analysis
- **rpc.rs:** ~7,901 lines. Handles World, Kernel, VFS, Agent, and LLM logic. It is the definition of a "God Object."
- **kernel_db.rs:** ~3,616 lines. Mixes schema management, context logic, and workspace management.
- **block_store.rs:** ~2,833 lines (Kernel) + ~2,500 lines (CRDT).

### Recommendation
Surgically decompose `rpc.rs` into `rpc/kernel.rs`, `rpc/world.rs`, etc. Move logic from `rpc.rs` into domain-specific services in `kaijutsu-kernel`.

---

## 5. Resiliency & Workarounds

### MCP Reconnection

**Resolved (April 7, 2026).** `McpServerPool::reconnect()` drops the dead connection, re-registers using stored config, and replaces the pool entry. `call_tool()` now catches `ServiceError` and retries once after reconnection. Cooldown of 5s per server prevents reconnect storms. `#[allow(dead_code)]` removed from `config` field.

### Placeholder RPCs

**`complete()` resolved (April 7, 2026).** Wired to `RhaiEngine::complete()` — returns completions for 40+ rhai scripting functions.

**`interrupt()` and `subscribe_output()` remain stubs.** These require an async execute redesign: `execute()` currently blocks until command completion, so there's no way for the client to get an `execId` to interrupt or to stream output. Planned fix: make `execute()` non-blocking (return execId immediately), implement `subscribe_output()` for streaming, then `interrupt(execId)` becomes meaningful.

---

## Summary of Action Items
1. ~~**Harden Persistence:** Trigger auto-saves on block mutations.~~ Largely resolved (April 6). `merge_ops` auto_save added (April 7). Streaming paths (append/edit) use explicit flush by design.
2. ~~**Session Isolation:** Scope `CurrentContext` and `DriftRouter` labels.~~ Resolved (April 6). Per-session context map, label uniqueness enforcement, cwd persistence on switch.
3. **Refactor rpc.rs:** Begin breaking down the 8k-line file. (Deferred — not causing development friction yet.)
4. **Pool Database:** Move away from a single `Mutex` for `KernelDb`. (Deferred — not a bottleneck yet.)
5. ~~**Restore env/init_script on session creation.**~~ Resolved (April 7). `EmbeddedKaish::apply_context_config()` called after creation.
6. ~~**MCP Reconnection.**~~ Resolved (April 7). `reconnect()` + retry-on-ServiceError in `call_tool()`.
7. ~~**`complete()` RPC.**~~ Resolved (April 7). Wired to RhaiEngine.
8. **Async execute redesign:** Make `execute()` non-blocking, implement `subscribe_output()` and `interrupt(execId)`. (Planned — largest remaining item.)
