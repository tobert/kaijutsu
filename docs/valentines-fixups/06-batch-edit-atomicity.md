# 06 — Batch Edit Atomicity

**Priority:** P0 | **Found by:** Gemini 3 Pro code review

## Problem

Batch edits in `block_tools/engines.rs:326` and `file_tools/edit.rs:108` are applied sequentially. If operation 3 of 5 fails, operations 1-2 remain applied. This breaks the atomicity contract that batch edits either all succeed or all fail.

## Impact

Partial edits leave documents in inconsistent state. Particularly dangerous for multi-block refactoring operations.

## Fix

1. **Validate all operations** before applying any (check offsets, content existence)
2. If validation passes, apply all (CRDT ops are individually safe)
3. If any validation fails, return error without applying any
4. Alternative: CRDT transaction support (heavier, future work)

## Files to Modify

- `crates/kaijutsu-kernel/src/block_tools/engines.rs` — batch edit engine
- `crates/kaijutsu-kernel/src/file_tools/edit.rs` — file edit operations

## Verification

- Unit test: batch with intentionally failing op, verify no ops applied
- Unit test: valid batch, verify all ops applied

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
