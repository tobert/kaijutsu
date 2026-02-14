# 11 — Error Swallowing

**Priority:** P1 | **Found by:** Gemini 3 Pro code review

## Problem

Multiple places silently discard errors:

### Serialization `unwrap_or_default`
- `kaijutsu-kernel/src/block_store.rs:495,532,568,606,659,687` — `serde_json::to_vec(&ops).unwrap_or_default()` sends empty bytes to subscribers on failure
- `kaijutsu-client/src/rpc.rs:462` — `call_mcp_tool` sends empty string on serialization failure

### Flush Errors Swallowed
- `kaijutsu-server/src/git_backend.rs:379` — `flush_all` logs warning but returns `Ok(())`. Caller thinks data is safe
- `kaijutsu-kernel/src/file_tools/cache.rs:188` — `flush_dirty` aborts on first error, remaining files not flushed

### Silent Snapshot Corruption
- `kaijutsu-kernel/src/block_store.rs:768` — `load_from_db` catches deserialization errors and creates empty document. User sees blank doc; corrupted data unrecoverable

### JSON Parse Fallbacks
- `kaijutsu-crdt/src/document.rs:233` — `tool_input` JSON parsing uses `.ok()`, converting parse errors to `None`
- `kaijutsu-kernel/src/rhai_engine.rs:172` — Invalid JSON defaults to `Null`

## Impact

Data loss masked as success. Users trust operations that silently failed.

## Fix

1. Replace `unwrap_or_default()` with proper error propagation
2. `flush_all` should collect errors, flush remaining, return all errors
3. `load_from_db` should return `Err` or mark document as corrupted
4. JSON parse failures should preserve raw text, not discard

## Files to Modify

- `crates/kaijutsu-kernel/src/block_store.rs`
- `crates/kaijutsu-client/src/rpc.rs`
- `crates/kaijutsu-server/src/git_backend.rs`
- `crates/kaijutsu-kernel/src/file_tools/cache.rs`
- `crates/kaijutsu-crdt/src/document.rs`
- `crates/kaijutsu-kernel/src/rhai_engine.rs`

## Verification

- Test: serialization failure propagates error (not empty bytes)
- Test: flush with partial failure continues and reports all errors
- Test: corrupted DB returns error, not empty doc

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
