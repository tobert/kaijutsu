# 05 — Sync Resilience

**Priority:** P0 | **Found by:** Gemini 3 Pro code review

## Problem

Three related sync issues:

### SSH Channels Dropped on Connect
`kaijutsu-client/src/lib.rs:38` — `connect_ssh` returns only the `rpc` channel; `control` and `events` channels are dropped, sending `SSH_MSG_CHANNEL_CLOSE`. Server may terminate connection.

### Sync Buffer Overflow → Permanent Desync
`kaijutsu-client/src/sync.rs:139` — When pending ops exceed 200, oldest are dropped. Client transitions to "Synced" but is permanently missing blocks.

### PushOps Ordering
`kaijutsu-client/src/actor.rs:565` — Each command spawned as separate `spawn_local` task. PushOps could arrive out of order if one yields.

## Impact

- Dropped channels: connection instability, especially under load
- Buffer overflow: silent data loss, user sees incomplete conversation
- PushOps ordering: CRDT ops applied out of order, potential corruption

## Fix

1. **Retain SSH channels** in `RpcClient` struct (even if unused now, prevents close)
2. **Trigger full re-sync** when buffer overflows instead of dropping ops
3. **Serialize PushOps** through a single task or channel to guarantee ordering

## Files to Modify

- `crates/kaijutsu-client/src/lib.rs` — retain channels
- `crates/kaijutsu-client/src/ssh.rs` — update struct if needed
- `crates/kaijutsu-client/src/sync.rs` — overflow handling
- `crates/kaijutsu-client/src/actor.rs` — PushOps serialization

## Verification

- Test: verify SSH channels alive after connect
- Test: overflow buffer, verify re-sync triggered (not silent drop)
- Test: rapid PushOps arrive in order

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
