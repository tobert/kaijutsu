# bevy_vello Escape

*Retiring the bevy_vello fork in favor of direct vello + parley integration.*

> **STATUS: COMPLETE (2026-06-13).** `bevy_vello` is no longer a dependency of
> `kaijutsu-app`. All phases shipped; the fork is archived. This document is the
> historical design record — it is intentionally the one place the `bevy_vello`
> name survives in the tree.

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
     deleted `form/` and `constellation/` outright, removing roughly half the
     phase-4 `UiVello*` sites by deletion rather than conversion. Audit of the
     *kept* conversation view found only **two live render sites** —
     `ui/dock.rs` and one `UiVelloText` in `ui/tiling_reconciler.rs:537`; every
     other `UiVello*` grep hit is a doc comment. The remaining `bevy_vello`
     coupling is then just: the `VelloPlugin`/`VelloView` pair, the chrome
     `FontHandles` (`VelloFont`-typed), and the phase-3c SVG-via-`bevy_vello`
     calls.

   - **Generic primitive (decided 2026-06-13).** Rather than convert each site
     to a bespoke scene-to-texture copy of `block_render`, we unify. A diff of
     `block_render`'s three internal systems showed the **render** path is
     byte-for-byte generic and **extract** is generic-minus-the-MSDF-filter;
     only **scene-building** and **sizing** are genuinely per-consumer. So the
     scene→`ImageNode`-texture pattern is extracted into
     `view/vello_ui_texture.rs`: `VelloUiScene` + `VelloUiTexture` +
     `extract_vello_scenes` + `render_vello_scenes` + `VelloUiTexturePlugin`,
     plus a pure, unit-tested `vello_texture_dims()` sizing helper and
     `create_vello_texture()`. Consumers own their scene-build + resize systems;
     `version > last_rendered` is the sole re-rasterize signal (`0` = skip, so
     MSDF cells just stay `0`). End state: dock, role-group borders, block
     cells, overlay text, and shell-dock text all ride one extract/render.

     Migration sequence (each compiles + verifies independently):
     1. **[DONE]** Primitive module + tests.
     2. **[DONE — pending live verify]** `ui/dock.rs` → first consumer; dropped
        its `UiVelloScene` import. Added a width-change rebuild trigger (the
        old `UiVelloScene` composite masked stale-width reflow; the texture path
        would stretch it).
     3. **[DONE — pending live verify]** Migrated `block_render`'s
        role-group-border branch onto the primitive: `build_role_group_scenes`
        writes `VelloUiScene`; borders spawn with `VelloUiScene`/`VelloUiTexture`
        (`lifecycle.rs`); the role branch left `resize_block_textures` for a
        dedicated `resize_role_group_textures` (shared `vello_texture_dims`).
        They now ride `extract_vello_scenes`/`render_vello_scenes`; the block
        extract/render still serves cells until step 4.
     4. **[DONE — `BlockTexture` deleted]** Split `BlockScene`: render fields
        (`scene`/`built_*`) → `VelloUiScene`; bookkeeping (`content_version`/
        `last_built_version`/`scene_version`/`text`/`color`) stays. Swapped
        `BlockTexture`→`VelloUiTexture` across cells + `view/overlay.rs` +
        `view/shell_dock.rs` (also read by the MSDF extract + material binding);
        `shaders/mod.rs` reads `built_*` off `VelloUiScene`. Deleted
        `block_render`'s `extract_block_scenes`/`render_block_textures`/
        `create_block_texture`; cells now ride the generic extract/render, with
        `render_msdf_block_textures` ordered `.after(render_vello_scenes)`.
        **MSDF gate:** `scene_version` (bumped every rebuild) stays the monotonic
        base for `msdf_glyphs.version`; `VelloUiScene.version` is set
        `= scene_version` ONLY on the Vello path, so MSDF cells leave it
        untouched and the generic extract skips their empty scene (the MSDF pass
        clears the texture itself via `has_vello_content == false`) — exactly the
        old `render_method != Msdf` skip.

        Verified live: block-cell MSDF text (+rainbow), role border, chat overlay
        (+breathing-border material), shell dock (+accent-border material), both
        docks. **Not yet exercised:** a Vello-content *cell* (ABC/SVG/sparkline,
        `has_vello_content == true`) where `render_vello_scenes` rasterizes the
        scene and MSDF composites labels on top — needs a real conversation with
        rich content. Logic unchanged vs. old path (same composite, same
        ordering); only the texture component was renamed.

   `BlockScene` is now misnamed (holds no scene) — rename to `BlockContent` is a
   tracked follow-up (docs/issues.md).

   - **[DONE] Chrome teardown + dep removal.** The last `UiVelloText`
     (`tiling_reconciler` unfocused-pane summary) became a native Bevy `Text`
     node — the one surface using Bevy's text pipeline, a pragmatic choice for a
     dimmed secondary label. Phase-3c SVG moved to direct `vello_svg`/`usvg`
     (`text/rich.rs`, `SvgFontDb`). Chrome `FontHandles` deleted (`ShapingFonts`
     is now the sole font resource); `vello_style`/`VelloFontAxes` chrome helpers
     removed. `VelloPlugin` and the `VelloView` camera marker removed. The
     `bevy_vello` path-dep is **gone from `Cargo.toml`**; `cargo tree` confirms
     it's no longer in the graph, and `vello_svg 0.9` dedups onto `vello 0.7`.
     All in-tree `bevy_vello` mentions cleaned from code/other-docs; the name
     survives only here (the design record) by intent.

Outcome: kaijutsu owns the full vector path — offscreen `vello::Renderer`
(`vello_rasterizer`), the parley shaping core (`text/shaping`), and the generic
`Scene`→`ImageNode`-texture primitive (`vello_ui_texture`) shared by block cells,
role borders, docks, and the MSDF overlay/shell-dock surfaces. No big-bang swap;
each phase shipped and was verified on its own.
