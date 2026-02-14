# 13 — Dead Code Cleanup

**Priority:** P2 | **Found by:** Gemini 3 Pro code review

## Items

### Unused Exports / Functions
- `kaijutsu-kernel/src/block_tools/translate.rs:66` — `line_end_byte_offset` public but unused
- `kaijutsu-app/src/text/msdf/buffer.rs` — `advance_width`, `subpixel_offset` in `PositionedGlyph` unused; `MsdfTextBuffer::new` unused
- `kaijutsu-app/src/text/msdf/generator.rs` — `cap_height_em`, `ascent_em` unused

### Unused Enum Variants
- `kaijutsu-app/src/ui/tiling.rs` — `WritingDirection::VerticalRl`, `PaneContent::Editor`, `PaneContent::Shell`, `PaneContent::Text`, `Edge::East`/`West`
- `kaijutsu-app/src/nine_slice.rs` — `CornerPosition`, `EdgePosition` enums

### Stale / Commented Code
- `kaijutsu-app/src/main.rs:122` — `// setup_input_layer,`
- `kaijutsu-app/src/ui/constellation/model_picker.rs:50` — unnecessary `#[allow(dead_code)]` on `ModelPickerDialog`

### Legacy Aliases
- `kaijutsu-kernel/src/db.rs:36` — `DocumentKind` legacy aliases (`output`, `system`, `user_message`)

### Never-Read Messages
- `kaijutsu-app/src/ui/layout.rs:196` — `LayoutSwitched` message defined but never read

## Fix

Remove dead code. For enum variants marked `#[allow(dead_code)]` with plan-phase comments, verify if the plan is still active before removing.

## Files to Modify

See items above — roughly 10 files.

## Verification

- `cargo check` with no new warnings
- `cargo clippy` clean on modified files

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
