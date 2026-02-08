# MSDF Text Rendering: Design Realignment

A clear-eyed review of our MSDF pipeline, what the state of the art actually does,
where our implementation diverges, and what to do about it.

## 1. MSDF Primer

**Multi-channel Signed Distance Fields** (Chlumsky, 2015) encode the distance from
each texel to the nearest glyph edge. Unlike single-channel SDF (Valve, 2007), MSDF
uses R/G/B channels to encode distance to different edge segments, preserving sharp
corners. Our variant is **MTSDF** — the alpha channel stores the true (single-channel)
SDF for corner correction.

### The canonical shader

From [Chlumsky's msdfgen README](https://github.com/Chlumsky/msdfgen):

```glsl
float sd = median(texture(msdf, texCoord).rgb);
float screenPxDistance = screenPxRange() * (sd - 0.5);
float opacity = clamp(screenPxDistance + 0.5, 0.0, 1.0);
color = mix(bgColor, fgColor, opacity);
```

Key properties:
- `sd = 0.5` is the glyph edge (inside > 0.5, outside < 0.5)
- `screenPxRange()` adapts AA width to screen-space glyph size
- The opacity transition is exactly 1 screen pixel wide
- Outside the glyph shape, `sd` drops below 0.5 rapidly → opacity → 0

### Blending

Chlumsky explicitly recommends **premultiplied alpha** for compositing glyphs
over arbitrary backgrounds
([discussion](https://github.com/Chlumsky/msdf-atlas-gen/discussions/68)):

> The background color isn't meant to be the actual background color you will see
> on the screen but simply the value the shader will output for empty pixels, and
> it is assumed some sort of blending will take place. So, it would generally be
> something with 0 alpha like `vec4(0.0)`. If you have problems with an "outline"
> that shouldn't be there, look into blending in alpha-premultiplied color space.

Blend state: `src = ONE, dst = ONE_MINUS_SRC_ALPHA`.

### What the industry does about overlapping quads

**Nobody clips at the advance boundary.** Every production MSDF renderer reviewed:

| Implementation | Overlap handling |
|---|---|
| Chlumsky's reference shader | Premultiplied alpha, no clipping |
| Metal MSDF (Sihao Lu, 2025) | Premultiplied alpha blend, no depth test |
| BabylonJS MSDF | Standard alpha blend |
| Godot `BaseMaterial3D` MSDF mode | `no_depth_test = false` (optional, off by default for text) |
| Red Blob Games SDF fonts | Premultiplied alpha; for effects (shadow), combines SDFs via `MAX` blend equation |

The reason: the SDF evaluation itself handles overlap. Outside the glyph shape,
`sd < 0.5`, and the opacity falls to 0 within ~1px. Two overlapping quads both
producing 0 alpha in the overlap zone blend correctly: `0 + 0 * (1-0) = 0`.

## 2. Our Pipeline — Data Flow

```
┌─────────────────┐    ┌──────────────┐    ┌──────────────────┐
│ Generator       │    │ Atlas        │    │ Buffer           │
│                 │    │              │    │                  │
│ font → msdfgen  │───▶│ rect-pack    │    │ cosmic-text      │
│ → MTSDF bitmap  │    │ glyph regions│    │ → PositionedGlyph│
│ → anchor (em)   │    │ UV coords    │    │ → advance_width  │
│ → padding       │    │              │    │ → x, y position  │
└─────────────────┘    └──────┬───────┘    └────────┬─────────┘
                              │                     │
                    ┌─────────▼─────────────────────▼───────┐
                    │ Pipeline (prepare_msdf_texts)          │
                    │                                        │
                    │ For each glyph:                        │
                    │   msdf_scale = font_size / 32          │
                    │   quad_size = region_size * msdf_scale  │
                    │   anchor_px = anchor_em * font_size     │
                    │   pen_position = text.left + glyph.x    │
                    │   quad_left = pen - anchor_px           │
                    │   → NDC vertices + UV + color + cell_x  │
                    └────────────────┬───────────────────────┘
                                     │
                    ┌────────────────▼───────────────────────┐
                    │ Shader (msdf_text.wgsl)                 │
                    │                                         │
                    │ vertex: NDC → clip_position (+ TAA jit) │
                    │ fragment: sample atlas → median → alpha  │
                    │   + hinting (gradient-based)             │
                    │   + stem darkening (size-adaptive)       │
                    │   + cell_x clipping  ← PROBLEM          │
                    │   + premultiplied blend                  │
                    └─────────────────────────────────────────┘
```

## 3. Coordinate Systems

| System | Origin | Y direction | Units | Where used |
|---|---|---|---|---|
| Font units | Glyph origin (baseline, pen) | Up | design units (e.g., 1000/em) | msdfgen input |
| MSDF bitmap | Bottom-left (msdfgen convention) | Up | pixels at `px_per_em` (32) | Generator output |
| Em units | Glyph origin | Up | fraction of 1em | Anchor storage in `AtlasRegion` |
| Layout pixels | Text area top-left | Down | screen pixels | cosmic-text positions, `glyph.x`/`.y` |
| Screen pixels | Screen top-left | Down | physical pixels | `px_x`, `px_y` in pipeline |
| NDC | Center | Up | [-1, 1] | GPU vertex positions |
| UV | Top-left of atlas region | Down (V flipped) | [0, 1] | Texture sampling |
| **cell_x** | Pen position | Right | [0, 1] within advance | **Our invention** |

### The key transforms

**Generator → Pipeline (scaling)**:
```
msdf_scale = font_size / px_per_em     // e.g., 22/32 = 0.6875
quad_width = atlas_region.width * msdf_scale
anchor_px = anchor_em * font_size
```

**Pipeline → NDC**:
```
px_x = text.left + (glyph.x - anchor_px) * text.scale
x_ndc = px_x * 2.0 / resolution_x - 1.0
```

**cell_x computation**:
```
cell_x_left  = -anchor_px / advance_width
cell_x_right = (quad_width - anchor_px) / advance_width
// GPU interpolates linearly between vertices
// cell_x = 0 at pen position, cell_x = 1 at pen + advance
```

## 4. The Overlapping Quads Situation

For a glyph rendered at 22px with our parameters:

| Parameter | Value |
|---|---|
| `px_per_em` | 32 |
| `msdf_range` | 4.0 |
| `padding` | `ceil(4.0 * 2.0)` = 8px at 32px/em |
| `msdf_scale` | 22/32 = 0.6875 |
| Padding at render size | 8 * 0.6875 = **5.5px per side** |
| Monospace advance ('m') | ~13.2px |
| Quad width (typical 'm') | ink_width + 11px padding ≈ **24px** |
| **Overlap between adjacent quads** | **~11px** (almost the full advance!) |

This is by design — the SDF padding must be large enough for the distance field
to transition smoothly from inside (1.0) to outside (0.0). The padding zone is
where the SDF evaluates to values between 0 and 0.5, producing opacity 0 in the
shader. **The overlap zone should be invisible.**

```
         ◀────── quad_width ≈ 24px ──────▶
         ┌──────────────────────────────────┐
         │░░░░░▓▓▓▓▓▓▓▓▓▓▓▓▓▓░░░░░░░░░░░░░│  glyph 0 (red)
         │ pad │◀── ink ──▶│  pad          │
         └─────┼───────────┼───────────────┘
               │           │
    pen₀       │         pen₁ (= pen₀ + advance)
               │           │
         ┌─────┼───────────┼───────────────────┐
         │░░░░░░░░░░░░░▓▓▓▓▓▓▓▓▓▓▓▓▓▓░░░░░░░░│  glyph 1 (blue)
         │     pad     │◀── ink ──▶│  pad      │
         └─────────────┼───────────┼───────────┘
                       │           │
                     pen₁        pen₂

    ░ = SDF padding (opacity → 0)
    ▓ = ink (opacity → 1)
    ◀════ overlap zone ≈ 11px ════▶
```

In the overlap zone, BOTH quads render fragments. But:
- Glyph 0's SDF in the overlap zone: `sd < 0.5` → `opacity ≈ 0`
- Glyph 1's SDF in the overlap zone (left of its ink): `sd < 0.5` → `opacity ≈ 0`
- Premultiplied blend of two near-zero-alpha fragments = correct result

**The overlap is harmless as long as the SDF evaluates correctly.**

## 5. Where We Went Wrong: cell_x

We invented a `cell_x` vertex attribute that normalizes each fragment's position
within the glyph's advance cell (0 = pen position, 1 = pen + advance). The shader
uses this to clip and fade fragments near the advance boundary.

### Why it seemed necessary

During testing with alternating red/blue colored 'm' glyphs, we observed color
bleeding across the advance boundary. This appeared to indicate that one glyph's
visible area was invading the neighbor's cell.

### Why it's actually wrong

1. **Glyphs legitimately extend past the advance boundary.** Font designers use
   negative sidebearings, overshooting strokes, and tight spacing. The advance
   width is where the *next pen position* goes, not a clipping boundary. Clipping
   at the advance boundary cuts legitimate ink.

2. **Every other text renderer has inter-glyph AA blending.** When two differently-
   colored characters share a boundary pixel, both contribute partial alpha. This
   is correct Porter-Duff compositing — the same thing FreeType, DirectWrite, and
   CoreText do. The test was detecting *normal AA behavior* and calling it a bug.

3. **cell_x fights the SDF.** The SDF already encodes exactly where the glyph edge
   is. Adding a second clipping mechanism at the advance boundary creates two
   competing notions of "where the glyph ends." They don't agree because the glyph
   edge and the advance boundary are different things.

4. **Sub-pixel alignment mismatch.** cell_x is computed from the pen position
   (which can be at any sub-pixel offset) and interpolated across the quad. The
   GPU rasterizer evaluates fragments at pixel centers. The mapping between pixel
   center and cell_x value is approximate, leading to clipping at slightly wrong
   positions — the exact issue we were debugging.

### The evidence

The "color bleed" test alternates red and blue on adjacent 'm' glyphs and checks
for the wrong color at 1px past the advance boundary. This scenario:
- Never occurs in real single-color text
- Occurs in syntax-highlighted code, but the ~1px color mixing at boundaries is
  identical to what every other text engine produces
- Is the natural result of AA compositing, not a rendering bug

## 6. The Correct Approach

### Remove cell_x entirely

Delete from: vertex struct, vertex shader output, pipeline cell_x computation,
shader clipping/fading logic, and the cell_x vertex attribute layout.

The fragment shader becomes simpler:

```wgsl
// Evaluate MSDF with hinting
let text_alpha = msdf_alpha_hinted(in.uv, uniforms.text_bias, in.importance);

// Apply text color
var text_color = in.color.rgb;
if uniforms.rainbow != 0u {
    text_color = rainbow_color(in.screen_pos.x, uniforms.time);
}

// Blend with premultiplied alpha
output = blend_over_premultiplied(output, text_color, text_alpha * in.color.a);

// Discard invisible fragments for performance
if output.a < 0.01 {
    discard;
}

return output;
```

### Keep premultiplied alpha blending

Our blend state is already correct: `src = ONE, dst = ONE_MINUS_SRC_ALPHA`.
This is exactly what Chlumsky recommends.

### No depth testing

Already removed. Correct. MSDF text rendering does not need depth testing.
Overlapping quads are handled entirely by the SDF evaluation + alpha blending.

### Keep the good stuff

Our hinting, stem darkening, importance weighting, and TAA are genuine innovations
on top of the canonical MSDF approach. They should stay. But they operate on the
SDF evaluation, not on clipping — they affect *how thick* strokes appear, not
*where they're clipped*.

## 7. Techniques We're Using — Primer

### 7.1 MTSDF (Multi-channel True SDF)

Standard MSDF stores distance-to-edge in R, G, B channels. The **median** of the
three reconstructs the signed distance. MTSDF adds a fourth channel (alpha) with
the true single-channel SDF. The shader takes `min(median(R,G,B), A)` to correct
corner artifacts where the multi-channel median fails.

**Our implementation**: `generator.rs` calls `shape.generate_mtsdf()`. The shader
samples all four channels and uses `min(median, alpha)`.

### 7.2 Screen Pixel Range (adaptive AA)

`screenPxRange()` measures how many screen pixels fit in the SDF's distance range.
This adapts AA width to zoom level — zoomed in text has wider AA, zoomed out has
sharper. Uses `fwidth(uv)` to measure screen-space UV rate of change.

**Our implementation**: `screen_px_range()` in the shader. We multiply by 2.0
in `msdf_alpha_at()` for sharper transitions — this is a tuning choice, not a bug.

### 7.3 Gradient-Based Hinting (astiopin/webgl_fonts)

Samples the SDF at center, north, and east texels to compute the gradient
(perpendicular to the stroke edge). The gradient direction reveals whether a
stroke is horizontal (crossbar of 'H') or vertical (stem of 'l'). Different
AA widths are applied:

- **Horizontal strokes** (vertical gradient): sharper AA (`vert_scale ≈ 0.6`)
- **Vertical strokes** (horizontal gradient): wider AA (`horz_scale ≈ 1.1`)

This mimics TrueType hinting's focus on horizontal stems without requiring
actual hint bytecode.

**Our implementation**: `msdf_alpha_hinted()` in the shader. Tuning parameters
are loaded from `theme.rhai` for hot-reload.

### 7.4 Stem Darkening (FreeType-style)

At small font sizes, thin strokes (like 'i', 'l', '1') appear too light because
the AA transition occupies a significant fraction of the stroke width. FreeType
compensates by shifting the SDF threshold inward (lowering bias), making strokes
thicker.

The darkening is inversely proportional to font size (via `1/px_range`):
- At 12px: noticeable thickening
- At 24px: minimal effect
- At 48px+: effectively zero

**Our implementation**: `darkening = stem_darkening * clamp(1.0/px_range, 0, 0.5)`
applied to `effective_bias` in `msdf_alpha_hinted()`.

### 7.5 Semantic Weighting (importance)

A per-glyph `importance` value (0.0–1.0) modulates stroke weight:
- 0.0 = thin/faded (inactive context, distant from cursor)
- 0.5 = normal weight
- 1.0 = bold/emphasized (cursor proximity, selection, agent activity)

This shifts the SDF bias: lower importance thins strokes, higher importance
thickens them.

**Our implementation**: Per-vertex `importance` attribute, applied as
`weight_adjust = mix(0.02, -0.015, importance)` in the shader.

### 7.6 Temporal Anti-Aliasing (TAA)

8-sample Halton(2,3) sequence applied as sub-pixel jitter to vertex positions.
History textures accumulate samples via exponential moving average. Provides
temporal super-resolution — effectively 8x AA without 8x fragment cost.

**Our implementation**: `MsdfTextTaaState` manages frame index and jitter.
Vertex shader applies jitter in NDC space. Separate TAA resolve pass blends
current frame with history.

### 7.7 Pixel Alignment (CPU-side)

Baselines snapped to integer pixels. X-height grid fitting applied when font
metrics are available. Monospace fonts optionally get horizontal pixel snapping.

**Our implementation**: `MsdfTextBuffer::visual_line_count()` applies baseline
snapping. Optional `snap_x` in `update_positioned_glyphs()`.

### 7.8 Premultiplied Alpha Compositing

Colors are pre-multiplied by alpha before blending: `premul = (R*a, G*a, B*a, a)`.
Blend equation: `dst' = src + dst * (1 - src_alpha)`. This correctly handles
overlapping semi-transparent layers without double-counting.

**Our implementation**: `blend_over_premultiplied()` in the shader applies
Porter-Duff "over" compositing. Pipeline blend state is `src=ONE, dst=ONE_MINUS_SRC_ALPHA`.

## 8. Action Items

### Remove
- [x] `cell_x` vertex attribute (pipeline.rs vertex struct + layout)
- [x] `cell_x` computation in `prepare_msdf_texts` (pipeline.rs)
- [x] `cell_x` in vertex/fragment shader structs (msdf_text.wgsl)
- [x] Cell boundary clipping + fade logic in fragment shader
- [x] Late discard comment about "depth buffer" (depth testing already removed)
- [x] Depth testing removal is done — verify no stale imports remain

### Adjust
- [x] Color overlap test: change from "bleed = bug" to "verify energy conservation"
  (total alpha at any pixel ≤ 1.0, no brightening artifacts)
- [x] Consider a softer test: verify that at 2px past boundary, wrong-color
  contribution is < 15% (allows natural AA blending at the boundary pixel)

### Keep
- [x] Premultiplied alpha blend state
- [x] MTSDF corner correction
- [x] Gradient-based hinting
- [x] Stem darkening
- [x] Importance weighting
- [x] TAA jitter
- [x] Pixel alignment

### Investigate (future)
- [ ] Whether stem darkening at small sizes expands ink past advance boundaries
  more than desired — may need per-glyph clamping of darkening amount
- [ ] Red Blob Games' `MAX` blend equation approach for glow/shadow effects
  that need to combine SDFs before rendering (currently our glow uses
  per-glyph blend, which may double-blend in overlap zones)

## References

- [Chlumsky, msdfgen](https://github.com/Chlumsky/msdfgen) — canonical MSDF implementation + shader reference
- [Chlumsky thesis](https://github.com/Chlumsky/msdfgen/files/3050967/thesis.pdf) — full MSDF theory
- [Chlumsky on premultiplied alpha](https://github.com/Chlumsky/msdf-atlas-gen/discussions/68) — use premultiplied, not foreground/background mix
- [MSDF Fragment Shader Antialiasing](https://www.fractolog.com/2025/01/msdf-fragment-shader-antialiasing/) — excellent screenPxRange() derivation
- [Red Blob Games: SDF combining](https://www.redblobgames.com/blog/2024-09-27-sdf-combining-distance-fields/) — MIN/MAX for SDF union/intersection
- [Red Blob Games: SDF Fonts](https://www.redblobgames.com/blog/2024-03-21-sdf-fonts/) — practical SDF rendering guide
- [osor.io: Crispy Text on GPU](https://osor.io/text) — runtime vector rasterization alternative
- [Metal MSDF (Sihao Lu)](https://medium.com/@sihaolu/performant-crisp-text-rendering-in-metal-with-multi-channel-signed-distance-field-msdf-9acd634d0052) — production Metal pipeline
- [Valve SDF paper (2007)](https://steamcdn-a.akamaihd.net/apps/valve/2007/SIGGRAPH2007_AlphaTestedMagnification.pdf) — original SDF for real-time text
- [astiopin/webgl_fonts](https://github.com/nicebyte/astiopin-webgl-fonts) — gradient-based hinting technique
