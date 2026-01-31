# 会術 Kaijutsu

*"The Art of Meeting"*

Kaijutsu is an agentic interface and kernel that offers a crdt-all-the-things
approach to collaborative editing with multiple models and users participating
in real time. The 会術 ui is built on Bevy 0.18 with custom MSDF text rendering.
The kernel relies on [a fork of diamond-types][dt-fork] that completes and
extends map and register types. We will upstream that when we have a moment.

[dt-fork]: https://github.com/tobert/diamond-types/tree/feat/maps-and-uuids

## Status

This is a friends & family release. MIT license so if you wanna fork and try
it, cool, but I (Amy Tobey) haven't put much effort into making it work on any
other machine yet.

If CRDTs excite you and cargo build isn't scary, this might be for you. If you
don't know what that is, please come back later and we'll explain why it's cool
and show you a demo.

-Amy

## Forked Dependencies

We maintain forks of several dependencies with fixes or extensions we need. These will be upstreamed once proven out:

| Fork | Branch | Why |
|------|--------|-----|
| [diamond-types](https://github.com/tobert/diamond-types) | `feat/maps-and-uuids` | Completes Map/Set/Register types |
| [glyphon](https://github.com/tobert/glyphon) | `bevy-0.18-compat` | cosmic-text 0.16 for Bevy 0.18 |
| [bevy_brp](https://github.com/tobert/bevy_brp) | `fix/send-keys-populate-text-field` | send_keys populates text field correctly |
| [anthropic-api](https://github.com/tobert/anthropic-api) | `add-tooluse-to-request-content-block` | ToolUse in request content blocks |

## Text Rendering

Kaijutsu uses Multi-channel Signed Distance Field (MSDF) rendering for all text,
providing resolution-independent rendering with crisp edges at any scale.

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
