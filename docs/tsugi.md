# 次 (tsugi) — MSDF Next Steps

Phases 0-5 complete. Text renders via MSDF shader pipeline with
hinting, stem darkening, directional AA, gamma correction, rainbow.
Per-block text glow via 9-tap sampling in block_fx.wgsl.
Vello handles SVG/sparkline/ABC/borders only.

## Phase 5: Per-Block Bloom (Text Glow) ✓

**Implemented: Option A — extend block_fx.wgsl**

The existing `BlockFxMaterial` post-process shader already runs per-pixel
on every visible block. Add a multi-tap text glow mode that samples the
block texture with offset UVs to approximate gaussian blur behind text.

### What to do

In `assets/shaders/block_fx.wgsl`:
- Add a `text_glow_radius` uniform (0 = disabled, >0 = pixel spread)
- Add a `text_glow_color` uniform (RGBA, usually matches border color)
- Sample the block texture at 8-12 offset positions (tent/disc kernel)
- Accumulate alpha from those samples → glow mask
- Blend glow color behind text: `glow_rgb * glow_alpha * (1 - tex.a)`
- Existing border glow (`sd_rounded_box`) stays unchanged

In `src/shaders/block_fx_material.rs`:
- Add `text_glow_radius: f32` and `text_glow_color: Vec4` to `BlockFxMaterial`
- Wire from `BlockBorderStyle` or theme

In `src/shaders/mod.rs` (`sync_block_fx`):
- Map theme glow params to the new material fields

### Tradeoffs

- 9-tap tent filter gives ~3px soft glow. Good enough for subtle halo.
- No extra textures, no extra render passes, no texture management.
- Quality ceiling: can't do wide diffuse bloom (would need 25+ taps).
- If quality insufficient, upgrade to Option B (dedicated bloom pass
  with half-res ping-pong textures, ported from old `msdf_bloom.wgsl`).

### Verification

- Toggle glow on/off via theme param, verify halo appears behind text only
- Check glow doesn't wash out text at high intensity
- Compare with border glow — they should complement, not fight

## Phase 6: TAA Temporal Super-Resolution

**Status: deferred — significant complexity for per-block architecture**

The old MSDF engine had TAA (Halton jitter + YCoCg variance clipping)
that accumulated 8 sub-pixel samples over time for temporal
super-resolution. This produced extremely smooth edges at small sizes.

### What it requires

Per-block history textures (ping-pong double buffer per visible block):
- Each block needs 2 extra textures at full resolution
- Ping-pong: read from A, blend into B, swap next frame
- Must detect texture resize and reset accumulation
- Must handle block visibility changes (reset on show, free on hide)

Per-frame jitter in the MSDF vertex shader:
- Halton(2,3) sequence, 8 samples, [-0.5, 0.5] pixel offset
- Applied to vertex positions before NDC conversion
- Frame counter per block (not global — blocks update independently)

TAA blend pass (per-block, after MSDF render):
- Read current MSDF render + history texture
- Convert to YCoCg for variance clipping (prevents ghosting)
- AABB clamp history to neighborhood of current frame
- Weighted blend: initial_weight for fresh samples, final_weight for converged
- Write to the other history texture (ping-pong)

Blit pass:
- Copy final history texture to the block's display texture

### Why it's deferred

- N visible blocks × 2 history textures = 10-30 extra GPU textures
- N × 1 TAA blend pass + N × 1 blit pass = 10-30 extra render passes/frame
- Texture lifecycle management (create/resize/free) is complex
- The current MSDF quality with hinting + stem darkening + directional AA
  is already a large improvement over Vello. TAA is diminishing returns
  until we're tuning subpixel quality at 12-14px sizes.

### Source material

All the old TAA code exists in the pre-vello worktree:
- `~/src/wt/pre-vello/.../msdf/pipeline.rs` — `MsdfTextTaaNode`, `MsdfTextTaaResources`, Halton sequence
- `~/src/wt/pre-vello/assets/shaders/msdf_text_taa.wgsl` — YCoCg variance clipping, blend weights

### When to revisit

- When glyph spacing is tuned and text looks good at 20px
- When someone notices subpixel shimmer at 12-14px sizes
- When we want to do 3D text compositing (TAA is essential for that)

## Contributing factors to track

- Glyph spacing is slightly tight — anchor math may need per-font tuning
- The 1-frame blank flash on texture resize (GpuImage upload latency)
  self-heals but is technically visible. Could be fixed with a
  "pending texture" guard that delays the resize until GpuImage is ready.
- The crash on large contexts (173 blocks) was from Vello's
  "paint too large" warning on 16384px tall textures — not MSDF-related
  but worth investigating separately.
