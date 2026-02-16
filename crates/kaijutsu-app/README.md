# kaijutsu-app

Bevy 0.18 GUI client for Kaijutsu.

```bash
cargo run -p kaijutsu-app
```

## Input System

Input is focus-based, not modal. `FocusArea` is the sole authority for keyboard
routing — `dispatch_input` reads raw keyboard/mouse/gamepad and emits
`ActionFired` / `TextInputReceived`. `InputMap` holds all bindings and is
BRP-mutable at runtime.

| FocusArea | Purpose |
|-----------|---------|
| Compose | Text input — shell vs chat auto-detected from prefix (`:` or `` ` ``) |
| Constellation | Context navigation (hjkl, Enter to switch, Tab to toggle) |
| Dialog | Modal dialogs |

## Tiling Window Manager

Panes are managed by a `TilingTree` resource containing recursive `TileNode`
values. The reconciler builds Bevy flex entities from the tree. Alt+hjkl for
focus, Alt+v/s to split, Alt+q to close.

## Constellation

Full-screen radial tree graph for navigating contexts, toggled by Tab.
`ConstellationVisible(bool)` toggles display, `ConstellationCamera` handles pan
and zoom with smooth interpolation. Nodes show context name and model badge.

## Text Rendering

All text uses Multi-channel Signed Distance Field (MSDF) rendering for
resolution-independent crisp edges at any scale.

### Techniques

| Technique | Purpose | Source |
|-----------|---------|--------|
| **MTSDF** | Multi-channel SDF with true SDF in alpha for corner correction | [Chlumsky/msdfgen][msdfgen] |
| **Shader hinting** | Gradient-based stroke detection for direction-aware AA | [astiopin/webgl_fonts][webgl-fonts] |
| **Stem darkening** | Thickens thin strokes at small sizes (FreeType-style) | [FreeType documentation][freetype-stem] |
| **TAA jitter** | Halton sequence sub-pixel offsets for temporal super-resolution | Bevy's TAA implementation |

### Quality Parameters

Font rendering quality is tunable via `~/.config/kaijutsu/theme.rhai` with hot-reload:

```rhai
// Core quality (high impact)
let font_stem_darkening = 0.15;  // 0.0-0.5, thickens thin strokes
let font_hint_amount = 0.8;      // 0.0-1.0, stroke direction sharpening
let font_taa_enabled = true;     // temporal anti-aliasing

// Fine-tuning
let font_horz_scale = 1.1;       // vertical stroke AA width
let font_vert_scale = 0.6;       // horizontal stroke AA width
let font_text_bias = 0.5;        // SDF threshold (thickness)
```

### Fonts

We request [Noto fonts][noto] by name (`"Noto Sans"`, `"Noto Sans Mono"`) so the
system font database provides fallback for CJK, emoji, and symbols. Essential
variants are bundled in `assets/fonts/` as fallback.

[msdfgen]: https://github.com/Chlumsky/msdfgen
[webgl-fonts]: https://github.com/astiopin/webgl_fonts
[freetype-stem]: https://freetype.org/freetype2/docs/reference/ft2-properties.html#no-stem-darkening
[noto]: https://fonts.google.com/noto
