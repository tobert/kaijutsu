# MSDF Stale Glyph on Empty Overlay

## Symptom

Type "aaa" in compose, backspace 3x. Buffer is correctly empty (Enter does nothing)
but the last "a" glyph remains visible. Cursor moves to position 0 correctly.

## Why it surfaced now

The mode-indicator-to-dock change made `display_text()` return `""` when the overlay
is empty. Previously it returned `"NORMAL │ "` which always produced glyph runs that
overwrote stale glyphs.

## Where to look

- `crates/kaijutsu-app/src/view/overlay.rs` `build_overlay_glyphs` (~line 310):
  when `text_changed` is true and display is empty, `collect_msdf_glyphs` returns
  an empty vec and `msdf_glyphs.glyphs` is set to `[]` with version bumped. The
  MSDF shader should respect the empty glyph list but apparently doesn't clear
  the previous frame's texture content.

- Check whether the MSDF render pass skips draw when glyph count is 0 but doesn't
  clear the region, or if there's a stale texture/atlas tile being sampled.

- Also check `shell_dock.rs` — same pattern, same potential issue.

## Possible fixes

1. MSDF shader: clear the region when glyph list is empty (proper fix).
2. `ContentSize` / node height: collapse to 0 when empty so nothing is sampled.
3. Workaround: `display_text()` returns `" "` when empty (masks the bug).
