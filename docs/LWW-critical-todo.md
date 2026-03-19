# Per-Field LWW Header Merge — Implemented

## Status: Done

Per-field LWW merge for `BlockHeader` is implemented. Each mutable field group
has its own Lamport timestamp. Concurrent mutations to independent fields are
both preserved.

## Design

### Field Groups (5 timestamps)

| Timestamp | Fields | Setter |
|-----------|--------|--------|
| `status_at` | `status` | `set_status()` |
| `collapsed_at` | `collapsed` | `set_collapsed()` |
| `ephemeral_at` | `ephemeral` | `set_ephemeral()` |
| `compacted_at` | `compacted` | (set via merge/direct) |
| `tool_meta_at` | `tool_kind`, `exit_code`, `is_error` | (set at block creation) |

`updated_at` = `max(all per-field timestamps)` for clock advancement.

### Tiebreaker

When timestamps are equal, the **greater value** wins (both peers compute the
same result independently). `Status` discriminant order: `Pending < Running <
Done < Error` — Error wins on tie, which is correct.

### Wire Format

Per-field timestamps are CRDT-internal (postcard serialization in `SyncPayload`).
Not in the Cap'n Proto schema — no RPC changes needed. Clean break from old
format (no backward-compat fallback; persisted state must be wiped).

## Test Coverage

- `test_lww_race_ephemeral_overwritten_by_status` — both concurrent changes preserved
- `test_per_field_lww_independent_merge` — three fields, different timestamps, all preserved
- `test_per_field_lww_same_field_higher_ts_wins` — higher timestamp wins on same field
- `test_per_field_lww_tiebreaker_convergence` — equal timestamps, value-based tiebreak converges
