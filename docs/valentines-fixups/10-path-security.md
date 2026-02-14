# 10 — Path Security

**Priority:** P1 | **Found by:** Gemini 3 Pro code review

## Problem

### BlockId Slash Injection
`kaijutsu-crdt/src/block.rs:45` — `BlockId::from_key` uses `/` separator without validating inputs. An agent_id like `"user/alice"` breaks parsing and could forge block ownership.

### Git Path Traversal
`kaijutsu-server/src/git_backend.rs:262` — `..` components not sanitized in `resolve_path`. Potential directory traversal outside repo root.

### VFS Symlink Escape
`kaijutsu-kernel/src/vfs/backends/local.rs:104` — On systems where root is symlinked (macOS `/tmp` → `/private/tmp`), path resolution fails with `PathEscapesRoot` because uncanonicalized path doesn't match canonical root.

## Impact

- Slash injection: broken block parsing, potential ID spoofing
- Path traversal: read/write files outside intended directory
- Symlink: VFS operations fail on macOS

## Fix

1. **Validate** `BlockId::new` — reject `/` in document_id and agent_id
2. **Reject `..`** components in git backend `resolve_path` (or canonicalize + check prefix)
3. **Canonicalize root** at `LocalBackend` construction time

## Files to Modify

- `crates/kaijutsu-crdt/src/block.rs` — `BlockId::new` validation
- `crates/kaijutsu-server/src/git_backend.rs` — `resolve_path`
- `crates/kaijutsu-kernel/src/vfs/backends/local.rs` — root canonicalization

## Verification

- Test: `BlockId::new("a/b", "c", 1)` returns error
- Test: path with `../` rejected by git backend
- Test: symlinked root resolves correctly

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
