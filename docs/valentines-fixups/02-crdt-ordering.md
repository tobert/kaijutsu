# 02 — CRDT Ordering Precision Loss

**Priority:** P0 | **Found by:** Gemini 3 Pro code review

## Problem

Block order stored as `(f64 * 1_000_000) as i64` in `document.rs:530`. After ~20 repeated insertions between the same two blocks, fractional values collide, producing duplicate order keys.

## Impact

Long conversations with many interleaved blocks (common in agent loops) will eventually produce misordered blocks. Affects display order and DAG traversal.

## Fix

Options (pick one):
1. **Store f64 directly** in the order map — simplest, but float comparison is fragile
2. **String-based fractional indexing** (e.g., Figma's approach) — robust, no precision loss
3. **Increase multiplier** to `1e12` — buys ~40 subdivisions, kicks can down road

Recommended: option 2 for correctness, or option 3 as quick fix with a TODO.

## Files to Modify

- `crates/kaijutsu-crdt/src/document.rs` — `calc_order_index`, order storage

## Verification

- Unit test: insert 100 blocks between same two siblings, verify all have unique order values
- Existing tests should continue to pass

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
