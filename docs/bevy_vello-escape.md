# bevy_vello Escape

*Retiring the bevy_vello fork in favor of direct vello + parley integration.*

We maintain six patches on a fork of bevy_vello (`tobert/bevy_vello`, branch
`fix/ui-node-padding`, consumed as a path dep from `crates/kaijutsu-app`).
Upstream has gone quiet. Rather than upstream the patches into a dormant
project, we evaluated what bevy_vello actually does for us — and found that
almost everything load-bearing is either already direct vello, or code we
patched so heavily it's effectively ours.

The forcing function is Bevy version bumps: when Bevy 0.19 lands and upstream
doesn't port promptly, we'd be doing the render-API migration inside a foreign
6,200-line codebase, ~1,900 lines of which (lottie, picking, diagnostics) we
never use. Better to own ~2,000 lines shaped for exactly what we need.

---

## Decision

Drop bevy_vello. Depend on `vello`, `vello_svg`, `kurbo`, `peniko`, and
`parley` directly. Vello's role narrows to an **offscreen rasterizer**:
build a `Scene`, `render_to_texture`, present the texture on a mesh or
`ImageNode`. This is the pattern `view/block_render.rs` already uses in
production.

The decision is coupled to (and confirmed by) the constellation/UI rewrite
direction: the card stack is heading toward a 3D game scene with a mesh per
card, not a zoomable 2D canvas. bevy_vello's UI compositing pass composites
in screen space only — it cannot put content on a mesh in a 3D scene. Every
surface vello touches in that world is a texture regardless, so the
compositing layer (the expensive part of any vello integration) has zero
customers and never gets written.

---

## What bevy_vello does for us today (June 2026 inventory)

**Already direct vello — migration is import swaps.** The bulk of our
rendering builds raw `vello::Scene`s: ABC notation (`text/abc.rs`),
sparklines (`text/sparkline.rs`), block rendering (`view/block_render.rs`),
form chrome (`ui/form/scene.rs`), fieldset borders, constellation
connectors. Block rendering drives the GPU itself — it locks bevy_vello's
shared `VelloRenderer` resource and calls `render_to_texture` in the Render
schedule (`block_render.rs:970-1040`). The MSDF text pipeline
(`text/msdf/`) is our own wgpu pipeline end to end.

**The text stack — the real dependency.** `VelloFont::layout()` (parley
shaping, font assets, variable-font axes) feeds both text paths: MSDF
extraction via `text/msdf/layout_bridge.rs` and scene-rendered rich text
via `text/rich.rs`. ~15 call sites. One shaping source for both paths is
what keeps metrics consistent — that property must survive the migration,
and does, because parley is a standalone crate. (The layout cache patch
turned out to sit on the UI-render path, not on `layout()` — see phase 3's
scope correction below; it retires in phase 4, not 3.)

**The UI integration — thin wrappers on a pass we won't need.** ~32
`UiVelloScene` and ~16 `UiVelloText` spawn sites across forms,
constellation 2D, dock, legend, tree. The components are trivial; the
~1,700 lines of extract/compose/render-graph machinery behind them is the
part we're declining to own. These sites retire as the UI rewrite eats
each subsystem.

**Marginal or dead.** SVG: two call sites (`text/rich.rs:320,322`);
`vello_svg`/`usvg` work as direct deps. Picking: feature enabled, zero
usage (constellation picking is `MeshPickingPlugin` on meshes). Lottie:
not enabled.

**The fork's six patches** (vs upstream v0.13 / Bevy 0.18 / vello 0.7,
~470 lines): parley layout cache (301 lines, perf), content-box/border-box
origin fixes (×3, UI padding correctness), `OverflowWrap` exposure,
`load_svg_from_str_with_options` for custom fontdb. Five of six live in
the text/UI-padding path — we weren't patching edges, we were patching the
core because our usage outran upstream. The fixes fold into our own code
natively; the fork archives.

---

## What we lose, and why that's acceptable

All losses are variations of one thing: per-frame, resolution-independent
vector compositing.

- **Smooth arbitrary zoom over vector content.** A texture-cached ABC
  staff or SVG scaled up samples a fixed-res texture. This is the one
  future feature foreclosed cheaply by the compositing pass — and it's been
  explicitly declined: the card stack will not have arbitrary zoom.
- **Per-frame animated vector graphics** (morphing paths, lottie-style
  motion) become a texture re-render per element per frame. Fine for one
  or two elements, not a screen of them. Nothing shipped or planned needs
  this.
- **Arbitrary linework in UI space.** Bevy UI has no beziers. In a 3D
  scene, connectors want to be geometry anyway — decide mesh polylines vs
  a shared overlay texture as part of the constellation rewrite, not after.

What we keep: vello upstream improvements (sparse strips, hybrid CPU/GPU)
arrive via the direct dependency; inline SVG and ABC stay first-class (the
product commitment); a future canvas/ink surface works as render-to-texture,
which is how most canvas apps composite anyway.

The honest framing: bevy_vello was an option on "vector-native UI
everywhere," paid for in patches, exercised only for chrome that Bevy UI
now does natively (rounded corners, borders) plus our materials.

---

## Texture-on-mesh consequences (card stack)

Known costs of the 3D direction, none new:

- **Resolution budgeting replaces zoom.** Card textures match expected
  screen-space size. Reading mode (card promoted close to camera)
  re-renders block content at higher resolution on promotion — a discrete,
  debounced event, same machinery as re-render-on-content-change.
- **Mipmaps on block textures.** Cards receding in perspective shimmer
  without them.
- **MSDF escape hatch.** Baked into a texture, MSDF's scale-independence
  is spent at bake time. If reading-mode text quality disappoints, render
  MSDF as live quads in the 3D scene — the atlas and shaping pipeline
  already support it; that's a renderer change, not an architecture change.

---

## Migration phases (each shippable alone)

1. **[DONE — `1d40a31`] Direct deps + import swap.** Added `vello`,
   `parley`, `kurbo`, `peniko` as direct dependencies (version-locked to the
   fork's pins so Cargo dedups to the same crate instances — the import swap
   is type-safe before the renderer is owned); swapped `bevy_vello::vello::*`
   / `bevy_vello::parley::*` across the raw-scene sites. Dropped `picking`.
2. **[DONE — `f5cb8b7`] Own the renderer resource.** `VelloRasterizerPlugin`
   (`view/vello_rasterizer.rs`) creates a kaijutsu-owned `vello::Renderer` in
   the render world — settings in `build()`, renderer in `finish()` once
   `RenderDevice` exists, with the CPU safe-mode fallback. `render_block_textures`
   pulls our `VelloRasterizer`/`VelloRasterizerSettings`. Two `vello::Renderer`
   instances coexist with bevy_vello's until phase 4.
3. **[DONE — `8896f16`] Own the text shaping stack.** Ported the parley
   shaping core into `text/shaping/`: the `VelloFont` asset, `layout()`, the
   style/axes/alignment types, the `.ttf` loader, the shared font context.
   `VelloFont::layout()` is the one shaping source for both text paths (MSDF +
   rich scene). **Scope correction vs the original plan:** the layout cache
   and content-box/padding patches were *not* folded in — they live on the
   UI-render path (`render_with_layout`, `systems.rs`), which `layout()` never
   touches, so they are **phase-4** concerns. Also dropped the vestigial
   `VelloFont.bytes` (glyphs live in the shared collection) and
   `VelloTextStyle.font` (the font is the `layout` receiver) — neither is read
   on the layout-only path. **FontHandles/ShapingFonts split:** the shared
   `FontHandles` was the coupling point — our path needs our `VelloFont`, the
   UI chrome needs bevy_vello's. `FontHandles` stays bevy_vello-typed for the
   chrome; a new `ShapingFonts` resource carries our handles. Same .ttf files
   load twice during the transition; collapses at phase 4.
   - **3c (deferred follow-up):** the 2 `text/rich.rs` SVG calls + the
     `usvg::fontdb` access (`resources.rs`/`plugin.rs`) still go through
     `bevy_vello::integrations::svg`; move to direct `vello_svg`/`usvg`.
     Small, separable.
4. **Retire UI chrome sites.** The `UiVelloScene` (~50) / `UiVelloText` (~38)
   spawns + `VelloPlugin` + the `VelloView` camera marker convert as the
   constellation/forms/dock rewrite lands each subsystem — Bevy UI + materials,
   or scene-to-texture where vector content is real. The layout-cache and
   content-box/padding patches retire here with the UI-render path. When the
   last `UiVello*` goes: merge `ShapingFonts` back into `FontHandles`, delete
   the `bevy_vello` dependency, archive the fork.
   - **[Partial — `24c73b3`]** The app-reset demolition (docs/kernel-kv.md §2)
     deleted `form/` and `constellation/` outright, removing **roughly half**
     the phase-4 `UiVello*` sites by deletion rather than conversion. Remaining
     sites live in the *kept* conversation view and need real conversion (not
     deletion): `text/` (rich text + components), `view/render.rs`,
     `cell/block_border.rs`, `ui/dock.rs`, `ui/tiling_reconciler.rs`. The
     `VelloView` camera marker and the `bevy_vello` dep stay until those land.

Phases 1–3 are done and were independent of the UI rewrite; phase 4 rides the
rewrite's schedule. No big-bang swap.
