# 01 — Drift Flush Safety

**Priority:** P0 | **Found by:** Gemini 3 Pro code review

## Problem

`DriftFlushEngine` drains the staging queue *before* inserting drift blocks. If `insert_from_snapshot` fails, the drift content is permanently lost.

Same pattern exists in both kernel and server:
- `kaijutsu-kernel/src/drift.rs:770`
- `kaijutsu-server/src/rpc.rs:1632`

## Impact

Any insertion failure (full disk, serialization error, CRDT conflict) silently discards drift content. User sees "flush succeeded" but data is gone.

## Fix

1. Clone or peek the staging queue before draining
2. Attempt insertion for each item
3. Only remove successfully inserted items
4. Re-queue failed items with error logging
5. Return partial success/failure to caller

## Files to Modify

- `crates/kaijutsu-kernel/src/drift.rs` — `DriftFlushEngine::execute`
- `crates/kaijutsu-server/src/rpc.rs` — drift flush RPC handler

## Verification

- Unit test: mock a failing `insert_from_snapshot`, verify items remain in staging queue
- Integration test: flush with mixed success/failure, verify partial delivery

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
