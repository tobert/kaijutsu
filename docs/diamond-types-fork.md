# Diamond Types Fork

**Date:** 2025-01-18 (updated 2026-01-23)
**Status:** ✅ Integrated and working

## Overview

We forked [diamond-types](https://github.com/josephg/diamond-types) to get a
unified CRDT foundation for Kaijutsu. The fork is at:

**https://github.com/tobert/diamond-types** (branch: `feat/maps-and-uuids`)

## Why We Forked

1. **`more_types` branch has Map/Set/Register** but wasn't merged to master
2. **cola had no tests** — not production-ready
3. **Unified CRDT = simpler architecture** — one library for everything

## Current Architecture

```
Document (Map CRDT)
├── blocks (OR-Set of block IDs)
├── order:<id> → i64 (fractional indices for ordering)
└── block:<id> (Map)
    ├── parent_id, role, status, kind (LWW via Map)
    ├── content (Text CRDT)
    ├── tool_input (Text CRDT, streamable)
    └── collapsed, author, created_at, etc.
```

## What's Implemented

### From `more_types` branch:
- [x] `CRDTKind`: Map, Register, Set, Text
- [x] `Primitive`: Nil, Bool, I64, Str
- [x] `CreateValue`: Primitive or NewCRDT (nested CRDTs)
- [x] `OpContents`: RegisterSet, MapSet, Set, Text
- [x] `MVRegister`: Multi-value register for conflict handling
- [x] `Branch` with overlay for state management

### New in our fork:
- [x] Serde derives on public types (feature: "serde")
- [x] OR-Set integration into OpLog
- [x] LWW Map module
- [x] Concurrent operation stress tests
- [x] 189 tests passing (up from 87)

### Deferred:
- [ ] UUID-based client IDs (still using numeric AgentId)
- [ ] Native List CRDT (using fractional indices workaround)
- [ ] v3 encoding format changes

## Resolution: List/Sequence CRDT

We chose **Option A: fractional indices in Map** (`order:<id> → i64`).

This is a proven approach used by many CRDT systems. Works well for our use
case. Native List CRDT can be added later if needed.

## Success Criteria

- [x] Single crate handles both block ordering AND text content
- [x] All 87+ tests from more_types still pass (now 189 tests)
- [x] New tests for nested CRDT operations
- [x] Clean serde serialization for Cap'n Proto bridge
- [x] **Drop cola dependency entirely** ✅
- [x] Convergence tests pass

## Dependency

```toml
# In Cargo.toml
diamond-types = {
  git = "https://github.com/tobert/diamond-types",
  branch = "feat/maps-and-uuids",
  features = ["serde"]
}
```

## References

- [diamond-types upstream](https://github.com/josephg/diamond-types)
- [Joseph's blog on CRDTs](https://josephg.com/blog/crdts-go-brrr/)
- [INTERNALS.md](https://github.com/josephg/diamond-types/blob/master/INTERNALS.md)

---

## Changelog

**2026-01-23**
- Updated to reflect actual implementation status
- All success criteria now checked off
- cola completely removed from codebase
- 189 tests passing including concurrent stress tests
- Documented fractional index approach for ordering
