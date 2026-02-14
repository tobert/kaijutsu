# Valentine's Fixups 💘

Comprehensive code review findings from **Gemini 3 Pro** (2026-02-14), organized into actionable task files.

**Review scope:** ~76 KLOC across 7 crates — 10 P0s, 24 P1s, 25+ P2s, 20+ P3s.

See [kaijutsu-review-findings.md](kaijutsu-review-findings.md) for raw findings.

## Task Checklist

### P0 — Data Loss / Crashes
- [ ] [01 — Drift flush safety](01-drift-flush-safety.md) — drain-before-insert loses data
- [ ] [02 — CRDT ordering](02-crdt-ordering.md) — fractional index precision loss
- [ ] [03 — Tool streaming](03-tool-streaming.md) — ToolCallDelta ignored in RigStreamAdapter
- [ ] [04 — Atlas growth](04-atlas-growth.md) — MSDF packer/texture desync
- [ ] [05 — Sync resilience](05-sync-resilience.md) — SSH channel drop + sync overflow + PushOps ordering
- [ ] [06 — Batch edit atomicity](06-batch-edit-atomicity.md) — non-atomic "atomic" edits
- [ ] [07 — Constellation safety](07-constellation-safety.md) — infinite recursion + mini click bug
- [ ] [08 — Cell performance](08-cell-performance.md) — text shaping O(N) per frame

### P1 — Latent Bugs / Security
- [ ] [09 — Watcher echo loops](09-watcher-echo-loops.md) — config + git watcher echo prevention
- [ ] [10 — Path security](10-path-security.md) — BlockId slash injection + git traversal + VFS symlinks
- [ ] [11 — Error swallowing](11-error-swallowing.md) — serialization unwrap_or_default + flush errors + JSON fallbacks
- [ ] [12 — MCP hardening](12-mcp-hardening.md) — hook listener bandwidth + task leak + O(N) lookup

### P2 — Cleanup
- [ ] [13 — Dead code cleanup](13-dead-code-cleanup.md) — unused variants, stale imports, commented code
- [ ] [14 — Deduplication](14-deduplication.md) — duplicated logic across modules

### P3 — Testing
- [ ] [15 — Test foundations](15-test-foundations.md) — enable concurrent CRDT tests + critical path coverage

### P1+P2 — UI Polish
- [ ] [16 — UI polish](16-ui-polish.md) — scroll jitter, cursor desync, theme reload, focus bugs

## Git Strategy

One commit per task minimum. Commit messages credit Gemini:

```
fix: prevent drift flush data loss on insertion failure

Drain staging queue only after confirmed insertion, re-queue on failure.
Found by Gemini 3 Pro code review.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>
Co-Authored-By: Gemini 3 Pro <noreply@google.com>
```
