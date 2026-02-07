# MSDF Glyph Spacing — Almost But Not Quite

## Status: Close but some letter pairs still touch

Fonts look much better overall. Cell boundary fade is the right structural approach.
Some letter pairs still visually merge — likely pairs with very tight sidebearings
like "oo", letters in "model", etc.

## What We Did

### 1. Cell Boundary Fade (the structural fix)

Added `cell_x` vertex attribute — normalized position within each glyph's advance
cell (0=pen position, 1=right advance boundary). The fragment shader suppresses
alpha outside this range via smoothstep.

**Files:**
- `buffer.rs` — `advance_width` field on `PositionedGlyph`, from `glyph.w`
- `pipeline.rs` — `cell_x` on `MsdfVertex`, computed as `(-anchor_x / adv, (quad_width - anchor_x) / adv)`. Vertex layout 28→32 bytes
- `msdf_text.wgsl` — `cell_x` on VertexInput/Output, cell_mask in fragment shader

**Current fade:** `smoothstep(-0.03, 0.03, cell_x) * smoothstep(1.03, 0.97, cell_x)`

This starts fading 3% inside the cell boundary (~0.4px at 13.2px advance).

### 2. Late Discard Threshold

Raised from 0.01 → 0.05. Kills very faint alpha that passes stem darkening + gamma.

### 3. Parameter Tuning (reverted — cell fade handles this)

Tried stem_darkening 0.15→0.08, horz_scale 1.1→1.0. Made NO visible difference
because the issue is structural (AA zone wider than sidebearing), not parametric.
Restored to original values since the cell fade is the actual fix.

## Root Cause Analysis

The AA transition zone in the shader is ~4.5× wider than tight monospace sidebearings:

| Metric | Value |
|--------|-------|
| AA zone width (doffset) | 0.091 SDF units (0.18 total) |
| Sidebearing for 'm' | ~0.04 SDF units |
| Alpha at advance boundary ('m', stem_darkening=0.15) | **64%** |
| Alpha at advance boundary ('m', stem_darkening=0.00) | **21%** |

No shader parameter reduces boundary alpha below ~20% for tight glyphs. The fix
MUST be structural — either layout-level spacing or shader-level boundary clamping.

## What's Still Happening

The cell fade at 0.97→1.03 clips ~0.4px on each side. But for glyphs like 'o' whose
curves extend very close to the advance boundary, even 0.4px isn't enough. The SDF
produces significant alpha (20-40%) within the last 3% of the cell.

## Next Steps to Try

### A. More aggressive fade (easy, might clip visible content)
```wgsl
// Start fade at 95% of cell instead of 97%
smoothstep(-0.05, 0.05, cell_x) * smoothstep(1.05, 0.95, cell_x)
```
This clips ~0.66px per side. Risk: visible notching on 'm', 'w', 'o' right edges.

### B. Advance-width-scaled fade (better)
Instead of a fixed 3% fade, compute the fade width from the actual px_range:
```wgsl
let fade_px = 0.5 / screen_px_range(in.uv); // AA zone in SDF units
let fade_cell = fade_px * N; // convert to cell units using advance width
```
This would adapt the fade to the actual AA zone width at the current font size.

### C. Letter spacing compensation (layout-level)
Add ~1px of extra spacing in cosmic-text metrics or as a post-layout offset.
Problem: cumulative — 60 chars × 1px = 60px wider lines, breaks wrapping.

### D. Size-dependent stem darkening
Scale darkening inversely with font size: strong at 12px, zero at 22px+.
The cell fade then only needs to handle the base AA (21% vs 64%).

### E. Narrower MSDF range
Reduce from 4.0 to 2.0-3.0. Doesn't change AA zone width in screen pixels
(it's determined by fwidth), but reduces atlas padding and quad overlap.
Probably not helpful since the issue is within the cell, not in the padding.

## Key Insight

The cell_x approach is architecturally right — it bridges cosmic-text's
metric-based layout and the MSDF renderer's field-based rendering. The remaining
issue is tuning HOW AGGRESSIVELY to clip at the boundary without creating
visible artifacts on wide characters.
