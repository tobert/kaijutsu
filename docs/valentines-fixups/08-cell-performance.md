# 08 — Cell Performance (Text Shaping)

**Priority:** P0 | **Found by:** Gemini 3 Pro code review

## Problem

`cell/systems.rs:1072` — `visual_line_count` (HarfBuzz text shaping) called every frame for every block when any layout change occurs. Long conversations frame-drop on every streamed token.

## Impact

Conversations with 50+ blocks become visually janky during streaming. Each new token triggers full re-measurement of all blocks.

## Fix

1. **Cache per-block height** — store `(content_hash, width) → line_count` mapping
2. **Only re-measure** when block content changes (new `content_version`) or container width changes
3. Use `layout_gen` change detection to skip entirely when nothing changed

## Files to Modify

- `crates/kaijutsu-app/src/cell/systems.rs` — `visual_line_count` calls
- `crates/kaijutsu-app/src/cell/components.rs` — add cache component/resource

## Verification

- Profile before/after with 100+ block conversation
- Verify streamed tokens don't cause full re-layout
- Existing visual tests still pass

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
