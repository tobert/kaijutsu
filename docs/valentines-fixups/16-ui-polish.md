# 16 — UI Polish

**Priority:** P1 + P2 | **Found by:** Gemini 3 Pro code review

## Items

### P1: Scroll Jitter
- `cell/systems.rs:1145` — `visible_height` updated after `smooth_scroll` runs, causing 1-frame jitter on resize/layout change
- **Fix:** Update `visible_height` before scroll calculation

### P1: Cursor Position Desync
- `cell/systems.rs:1367` — `compute_cursor_position` re-derives row/col from raw string instead of shaped buffer layout, can mismatch visual text
- **Fix:** Derive cursor position from the same shaped buffer used for rendering

### P1: Stale Materials on Theme Change
- `ui/materials.rs:30` — `setup_material_cache` runs only at `Startup`. Theme reload doesn't update cached materials
- **Fix:** Add system watching `Res<Theme>` changes to update material assets

### P1: Resize Logic Flaw
- `ui/tiling.rs:739` — Resize clamps individual ratios before normalization, allowing drift below minimum
- **Fix:** Calculate max delta respecting both panes' bounds before applying

### P1: Stale PaneMarker After MRU Assignment
- `ui/tiling_reconciler.rs:825` — `assign_mru_to_empty_panes` updates tree and saved state but NOT `PaneMarker` component
- **Fix:** Update `PaneMarker` component in same function

### P1: FocusArea Desync on Screen Transition
- `input/systems.rs:32` — `sync_focus_from_screen` may reset focus during typing if screen state is re-set to same value
- **Fix:** Only update FocusArea when screen actually changes (compare previous value)

### P1: Blocking `hostname::get()` in Theme Loader
- `ui/theme_loader.rs:50` — Can block during frame update
- **Fix:** Cache hostname at startup

### P1: Ancestry Line Flicker
- `ui/constellation/render.rs:460` — Empty DriftState during reconnect despawns valid lines
- **Fix:** Don't despawn lines when DriftState is empty/stale

### P2: Hardcoded FocusArea::Constellation on Dialog Close
- `ui/constellation/create_dialog.rs:435` — Should restore previous focus
- **Fix:** Save previous FocusArea before dialog open, restore on close

### P2: Fragile Child Indexing in Mini Render
- `ui/constellation/mini.rs:218` — Relies on implicit child order
- **Fix:** Use marker components instead of index

## Files to Modify

~10 files in `kaijutsu-app/src/`

## Verification

- Manual testing for scroll, cursor, theme reload
- Verify constellation doesn't flicker on reconnect
- Verify resize doesn't drift below minimums

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
