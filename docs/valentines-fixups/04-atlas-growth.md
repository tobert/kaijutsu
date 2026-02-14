# 04 — Atlas Growth Corruption

**Priority:** P0 | **Found by:** Gemini 3 Pro code review

## Problem

`text/msdf/atlas.rs:227` — `grow` method re-packs existing regions into new positions via `rect_packer`, but copies pixel data to the OLD positions. Packer state is completely desynchronized from texture content. Future insertions overwrite existing glyphs.

## Impact

After atlas grows (triggered by many unique glyphs), text rendering shows corrupted/wrong glyphs. Affects any session that uses enough unique characters to trigger growth.

## Fix

Options:
1. **Initialize large atlas** (2048x2048) and never grow — wastes VRAM but simple
2. **Copy to new positions after re-pack** — fix the grow method to use new positions for pixel copy
3. **Custom shelf-packer** that guarantees stable positions — no re-packing needed on growth

Recommended: option 2 (fix the existing code) with option 1 as fallback if too complex.

## Files to Modify

- `crates/kaijutsu-app/src/text/msdf/atlas.rs` — `grow` method

## Verification

- Unit test: insert glyphs, trigger growth, verify all glyphs still at correct positions
- Visual test: render text with many unique characters, verify no corruption after growth

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
