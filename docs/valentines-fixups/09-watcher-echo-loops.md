# 09 — Watcher Echo Loops

**Priority:** P1 | **Found by:** Gemini 3 Pro code review

## Problem

### Config Watcher Echo
`config_backend.rs:670` — Time-based debounce (200ms) is unreliable. FS events may arrive late, causing: write config → FS event → re-read → re-write → loop.

### Git Watcher Echo
`git_backend.rs:446` — Same pattern. CRDT flush writes files, git watcher detects changes, writes back to CRDT, triggering another flush.

## Impact

CPU spin on config/git changes. In worst case, infinite loop of writes.

## Fix

Use **content hash comparison** instead of time-based debounce:
1. Before writing, compute hash of new content
2. Store hash of last-written content
3. On FS event, compare file hash against last-written hash — skip if same

## Files to Modify

- `crates/kaijutsu-kernel/src/config_backend.rs` — config watcher
- `crates/kaijutsu-server/src/git_backend.rs` — git watcher

## Verification

- Test: write config, verify no echo re-write
- Test: flush git, verify watcher doesn't re-ingest

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
