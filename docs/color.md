# Color — one identity, two lanes

**The vibe target is synthwave** (Amy, 2026-07-12): deep indigo-violet grounds,
electric violet + gold two-tone, neon that *earns* its dark ground by blooming.
The reference feel is the time well's dived interior — HDR-forward, confident —
not the muted octagon it replaced. "Muted" was never chosen; it was the
accidental sum of ~30 per-site brightness constants that no module owned. This
doc + the theme system make color a *decision* again.

This is the app-wide color contract: the 3D scenes (room, well, patch bay,
tracker…) and the 2D conversation view speak **one identity** through **two
rendering lanes** that must never be confused.

## The two lanes

| | Scene lane | UI lane |
|---|---|---|
| What | 3D materials (room, well, patch bay, glyphs, rays) | Conversation blocks, dock, overlays, MSDF text fx |
| Color space | **linear** RGB (`LinearRgba`, raw `Vec3`/`Vec4` uniforms) | **sRGB** (`Color::srgb`, `.to_srgba()` into shader uniforms) |
| Pipeline | HDR camera → bloom (threshold 1.0) → tonemap | rendered post-tonemap; values are display-referred |
| Brightness | the tier ladder below; >1.0 blooms | 0..1 only; "glow" is a shader effect, not HDR |
| Source of truth | `[scene]` in `theme.toml` → `ScenePalette` resource | flat keys in `theme.toml` → `Theme` resource |

Rules that keep the lanes honest:

- A scene material takes **linear** values; a UI shader takes **sRGB**. Never
  pass one where the other is expected — nothing in the type system catches it.
- The bloom threshold sits at **1.0** and is load-bearing: values ≤ 1.0 are
  structure, values > 1.0 glow. **Sustained** HDR means *live activity* (a
  playing track, chatter, a beat); decoration may only *visit* HDR — traveling
  crests, brief gem glints — capped by the `crest` tier.
- Hues are defined once as **identity hues** (brightest channel ≈ 1.0) and
  placed on a brightness tier by multiplication. Don't bake brightness into a
  hue.

## One source of truth: `theme.toml`

The theme is **CRDT-owned** (`/etc/config/theme.toml`, seeded from
`assets/defaults/theme.toml` — see `docs/config-crdt-ownership.md`) and reaches
the app over RPC on connect. It carries:

- **Flat keys + `[ansi]`** → `ui/theme.rs::Theme` — every conversation-view,
  dock, overlay, and text-effect color. This system predates the pass and is
  kept as-is; the pass re-skins its *values*.
- **`[scene]`** (added by this pass) → `view/scene_palette.rs::ScenePalette` —
  the 3D identity hues (hex sRGB in the file, linearized on parse), the
  brightness **tier ladder**, and the live-signal **gains**.
- **`[scene.post]`** → the camera's post chain: bloom intensity/boost and the
  tonemapper by name. Applied **live** on theme change (the one part of the
  scene lane that hot-applies), so `kj config set … theme.toml` is a live
  color-management console.

Apply semantics: UI colors and `[scene.post]` apply immediately on
`ThemeReceived`; `[scene]` hues/tiers are read at **spawn time**, so a running
room re-skins on the next room entry. (Materials are built once at spawn — this
is a documented trade, not an accident.)

`view/palette.rs` keeps two jobs only: the **compiled-in defaults** that
`ScenePalette::default()` mirrors (so the app renders correctly before the
kernel answers), and cross-module **geometry contracts** (`WALL_APOTHEM`,
`STATION_W_*`) that were never color. Scene modules must not define private
color/brightness constants anymore — new color goes through `ScenePalette`.

## The tier ladder

LDR structure tiers (multiply an identity hue):

| Tier | Default | Meaning |
|---|---|---|
| `surface` | ~0.01–0.02 (lift table) | near-black working surfaces; silhouettes, not brightness |
| `etch` | 0.28 | engraved detail — guide rings, ticks |
| `marker` | 0.42 | station markers at rest |
| `trim` | 0.50 | gold architectural trim (table rims, pylon caps) |
| `hardware` | 0.55 | brass sockets/pegs/jacks |

Decoration glow (breathing/traveling, from the trace-glow discipline):

| Tier | Default | Meaning |
|---|---|---|
| `trough_wiring` | 0.55 | floor traces at rest |
| `trough_subtle` | 0.75 | the calmest breathers (inscribed ring, pads ~0.65) |
| `crest` | 1.25 | the ceiling ANY decoration crest may touch (>1.0 = soft halo) |

Invariant (tested): `trough × crest < 1.0` — decoration never *sustains* HDR.

Live-signal gains (allowed to sustain HDR because they ARE the activity tell):

| Gain | Default | Element |
|---|---|---|
| `pulse` | 6.0 | patch-bay traffic packet |
| `chord_selected` | 3.4 | selected chord idle |
| glyph `HDR_SCALE` | 3.0 (wgsl const) | terrace glyph emissive |
| card rim gains | 1.6–3.5 (wgsl consts) | well card status rims |

wgsl-const gains stay consts for now (uniform-izing every shader wasn't worth
it this pass) but they are **members of this ladder** — tune them against it,
and if one needs live tuning, promote it to a `ScenePalette` gain and a
material uniform.

## Post chain

One shared HDR `Camera3d` (`main.rs::setup_camera`): bloom `intensity 0.12 /
low_frequency_boost 0.25 / threshold 1.0 / OLD_SCHOOL`, tonemapper
**TonyMcMapface**, no exposure/grading components. `[scene.post]` overrides
intensity/boost/tonemapper; threshold stays 1.0 — it's the HDR-tell boundary,
not a style knob. Tonemapper A/B is live: change the value, `kj config set`,
watch the room. (BRP `world_mutate_components` on the camera works too.)

## How to

- **Add a colored element**: decide its lane. Scene → pick an identity hue from
  `ScenePalette` (add one if genuinely new) × a tier; live signals get a gain.
  UI → add a `theme.toml` key + `Theme` field; no literals in view code.
- **Re-skin the app**: edit `theme.toml` (`kj config set /etc/config/theme.toml
  --content "$(cat file)"`), UI + post apply live, re-enter the room for scene
  hues. The shipped default skin lives in `assets/defaults/theme.toml`
  (seeded once — a fresh kernel picks it up; a live one needs `kj config set`).
- **Check a color at a pixel**: BRP screenshot + eyedropper; remember the
  screenshot is post-tonemap sRGB.

## History

- Tokyo Night was the UI default from the rhai-theme era through 2026-07;
  the rhai system itself was deleted in Phase 6 (`5dbca599`), replaced by the
  CRDT theme. The synthwave re-skin (this pass) replaced Tokyo Night as the
  shipped default; the Tokyo Night values live on in git history.
- The conversation view was already fully theme-tokened before this pass —
  the pass unified *identity*, it didn't have to invent UI plumbing.
