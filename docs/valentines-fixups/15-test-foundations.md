# 15 — Test Foundations

**Priority:** P3 | **Found by:** Gemini 3 Pro code review

## Items

### Enable Concurrent CRDT Tests
- `kaijutsu-crdt/src/lib.rs:188` — `test_concurrent_block_insertion` and `test_concurrent_text_editing` are `#[ignore]`. These are the most critical tests for a CRDT library.
- **Fix:** Un-ignore, fix any failures, ensure they run in CI

### Critical Path Coverage Gaps
High-impact missing tests (ordered by risk):
1. Drift flush failure recovery
2. Sync buffer overflow recovery
3. Atlas growth correctness
4. Tiling reconciler (no tests at all)
5. Config/git watcher echo prevention
6. Input dispatch priority (Dialog > Global)
7. Radial layout + cycle detection
8. MCP drift tool round-trip
9. Telemetry sampler rates (cost control)
10. Reconnection backoff timing

## Fix

1. Un-ignore concurrent tests, fix if broken
2. Add at least one test per critical gap (focus on items 1-5)
3. Mark remaining gaps with `TODO(test)` comments

## Files to Modify

- `crates/kaijutsu-crdt/src/lib.rs` — un-ignore tests
- New test files as needed for each gap

## Verification

- `cargo test` passes with concurrent tests enabled
- New tests cover the specified scenarios

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
