# 12 — MCP Hardening

**Priority:** P1 | **Found by:** Gemini 3 Pro code review

## Problem

### Hook Listener Quadratic Bandwidth
`kaijutsu-mcp/hook_listener.rs:268` — `push_ops` re-sends all un-acked ops on every call. Rapid hook events cause O(N²) bandwidth.

### Background Task Leak
`kaijutsu-mcp/lib.rs:245` — Background event listener has no shutdown signal. Multiple `KaijutsuMcp` instances leak tasks.

### O(N) Block Lookup
`kaijutsu-mcp/helpers.rs:46` — `find_block` iterates all documents and all blocks. Gets worse as kernel grows.

## Impact

- Bandwidth: MCP becomes unusable under rapid edits
- Task leak: memory/CPU grows over time
- O(N) lookup: slow response times for block operations

## Fix

1. **Track optimistic frontier** — only send ops after last acked position
2. **Store `AbortHandle`** for background task, cancel on drop
3. **Maintain `HashMap<BlockId, DocumentId>`** index for O(1) lookup

## Files to Modify

- `crates/kaijutsu-mcp/src/hook_listener.rs`
- `crates/kaijutsu-mcp/src/lib.rs`
- `crates/kaijutsu-mcp/src/helpers.rs`

## Verification

- Test: rapid ops don't cause quadratic retransmission
- Test: dropping `KaijutsuMcp` cancels background tasks
- Benchmark: block lookup stays constant time as document count grows

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
