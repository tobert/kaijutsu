# LWW Critical TODO: Per-Field Header Merge

## Problem

`BlockContent::merge_header()` uses whole-header Last-Writer-Wins (LWW):
when a remote header has a higher `updated_at` (Lamport timestamp), **all**
mutable fields are replaced atomically. This means concurrent mutations to
different fields will silently drop the loser's change.

## Affected Fields

All fields updated in `merge_header()`:
- `status` (Running → Done / Error)
- `collapsed` (user toggle)
- `ephemeral` (LLM hydration filter)
- `compacted` (summary flag)
- `tool_kind`, `exit_code`, `is_error` (tool lifecycle)

## Race Scenario

```
Store A (Lamport=5):  set_ephemeral(block, true)   → updated_at=6
Store B (Lamport=5):  set_status(block, Done)       → updated_at=6

Merge A→B: same timestamp, agent_id tiebreak → one wins entirely
Result: loser's field change is silently dropped
```

## Impact

- **ephemeral + status race**: An LLM stream finishes (`status=Done`) while
  another peer marks the block ephemeral. The loser's change is dropped.
- **collapsed + status race**: User collapses a block while the LLM finishes
  streaming it. One change is lost.

In practice, most of these races are unlikely because the fields change at
different lifecycle stages. The highest-risk scenario is `ephemeral` being
set concurrently with `status` completion on streaming blocks.

## Regression Baseline

`test_lww_race_ephemeral_overwritten_by_status` in `block_store.rs` documents
the current behavior. If per-field LWW is implemented, that test's final
assertion will fail (by design) to signal the test needs updating.

## Proposed Fix: Per-Field LWW

Replace the single `updated_at` with per-field timestamps:

```rust
struct BlockHeader {
    // ... existing fields ...
    status_at: u64,
    collapsed_at: u64,
    ephemeral_at: u64,
    compacted_at: u64,
    tool_meta_at: u64,  // covers tool_kind, exit_code, is_error
}
```

Each `set_*` method ticks the Lamport clock and writes to the field-specific
timestamp. `merge_header` compares per-field, accepting the highest timestamp
for each independently.

## Wire Protocol Impact

- `BlockHeader` gains 5 new `u64` fields (40 bytes per header on wire)
- Cap'n Proto schema change needed
- Backward compatible: old peers send `updated_at` only; new peers can fall
  back to whole-header merge when per-field timestamps are all 0

## Design Session

This is a follow-up design task. The current whole-header LWW is documented
and tested as a known limitation.
