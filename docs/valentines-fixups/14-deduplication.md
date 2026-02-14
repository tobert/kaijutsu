# 14 — Deduplication

**Priority:** P2 | **Found by:** Gemini 3 Pro code review

## Items

### Error Conversion
- `kaijutsu-server/src/git_filesystem.rs:88` and `docs_filesystem.rs:66` — identical `backend_to_io` error conversion
- `kaijutsu-server/src/mount_backend.rs:66` and `git_filesystem.rs:107` — identical `entry_info_to_dir_entry` / `entry_info_to_metadata`
- **Fix:** Extract to shared `server::util` module

### Height Calculation
- `kaijutsu-app/src/cell/systems.rs:1098` vs `cell/components.rs:434` — both compute height from line count with different padding
- **Fix:** Single authoritative function

### String Truncation
- `kaijutsu-app/src/ui/constellation/render.rs:620` and `mini.rs:293` — identical truncation logic
- **Fix:** Extract to `constellation::util`

### Order Calculation
- `kaijutsu-crdt/src/document.rs:296` — `calc_order_index` repeats "get order from root map" three times
- **Fix:** Extract helper

### Provider Type Strings
- `kaijutsu-kernel/src/llm/mod.rs:248` — "anthropic", "gemini" etc hardcoded in multiple places
- **Fix:** Define enum with `Display`/`FromStr`

## Files to Modify

~8 files across server, app, crdt, and kernel crates.

## Verification

- `cargo check` clean
- Behavior unchanged (pure refactor)

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
