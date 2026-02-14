# 07 — Constellation Safety

**Priority:** P0 + P1 | **Found by:** Gemini 3 Pro code review

## Problem

### Infinite Recursion (P0)
`ui/constellation/mod.rs:327` — `count_tree_descendants` has no cycle detection. Malformed server data with a parentage cycle causes stack overflow crash.

### Mini Click Doesn't Switch Context (P1)
`ui/constellation/mini.rs:252` — `handle_mini_render_click` updates visual focus but does NOT send `ContextSwitchRequested`. Click appears to work but doesn't actually switch context.

## Impact

- Cycle in context tree: client crash (stack overflow)
- Mini click: user thinks they switched context but didn't — edits go to wrong context

## Fix

1. **Add `visited: HashSet<ContextId>`** to `count_tree_descendants` (or depth limit)
2. **Remove duplicate mini click handler** — let `mod.rs` handle all clicks, or add `ContextSwitchRequested` to mini handler

## Files to Modify

- `crates/kaijutsu-app/src/ui/constellation/mod.rs` — cycle detection
- `crates/kaijutsu-app/src/ui/constellation/mini.rs` — click handler

## Verification

- Unit test: construct cycle in context tree, verify no crash + returns finite count
- Manual test: click mini constellation node, verify context actually switches

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
