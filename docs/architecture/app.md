# The app

*Deep-dive companion to [README.md](README.md). Covers `kaijutsu-app` — the Bevy
0.18 GUI and its render pipeline. Code is truth; verified 2026-06-16.*

`kaijutsu-app` connects to a remote kernel over SSH (via `kaijutsu-client`),
maintains a multi-context CRDT mirror, and renders conversation blocks as
individual GPU textures using a **kaijutsu-owned vello rasterizer** (the
`bevy_vello` dependency was removed).

---

## Bevy architecture

One always-on `Camera3d` (HDR + `Bloom::NATURAL`) clears to the theme
background — there is no `Camera2d` anywhere in the app. One `Screen` state
machine: `Conversation` (default) and `TimeWell` (`ui/screen.rs:24`). Plugins build in
order: `KjText → Input → Cell → VelloRasterizer → VelloUiTexture → BlockRender →
Peers → ShaderFx → Actor → AppScreen → Screen → Commands → Tiling →
TilingReconciler → Dock → Drift → TimeWell → Timeline → Tweening`. Frame order:
`InputPhase{SyncContext→Dispatch→Handle→Cleanup}` →
`CellPhase{Input→Sync→Spawn→Buffer→Layout}` → PostUpdate render-scene builds →
render-world `Extract → Render`. Power management: reactive 10 Hz idle / 2 Hz
background (`main.rs:181`).

---

## Module map

- **`cell/`** — a migration shim: component types now live in `view/`; `cell/mod.rs`
  re-exports them so old `crate::cell::X` paths compile. Real content:
  `cell/plugin.rs` (`CellPlugin`, phase ordering, resource init, BRP methods) and
  `cell/block_border.rs` (`BlockBorderStyle` and friends — per-block border data).
- **`view/`** — the functional heart of the conversation screen:
  - `components.rs` — all ECS components/resources (`CellEditor` = per-block DTE
    store + cursor, `BlockCell`, `BlockCellContainer` = `IndexMap<BlockId,Entity>`,
    `FocusTarget`, `InputOverlay`, `GlobalErrorQueue`, …).
  - `document.rs` — `DocumentCache` (`HashMap<ContextId, CachedDocument>`, LRU cap
    8) wrapping `SyncedDocument` + optional `SyncedInput`.
  - `block_render.rs` — `BlockScene` (now misnamed: holds version/text/color, *not*
    a scene), `build_block_scenes` (dispatch per `RichContentKind`),
    `resize_block_textures`, and the MSDF extract/render systems.
  - `vello_rasterizer.rs` / `vello_ui_texture.rs` — the shared `vello::Renderer`
    (`Arc<Mutex>`) and the `VelloUiScene`/`VelloUiTexture` Scene→texture primitive.
  - `lifecycle.rs` — spawns block-cell bundles (`BlockCell, BlockScene,
    VelloUiScene, VelloUiTexture, MsdfBlockGlyphs, ImageNode,
    MaterialNode<BlockFxMaterial>`).
  - `render.rs`, `sync.rs`, `scroll.rs`, `submit.rs`, `overlay.rs` (floating chat),
    `shell_dock.rs` (Ctrl+Z shell row), `cursor.rs`, `fieldset.rs` (role-group
    rules), `format.rs`, `brp_methods.rs`.
  - `view/time_well/` — the full-viewport 3D context browser
    (`Screen::TimeWell`), now a carousel of four per-idle-age-band
    magic-circle rings receding into a shared throat glow: `card.rs` (pure
    `ContextInfo → CardData` mapping + idle-age band assignment via
    `kaijutsu_viz::layout::assign_idle_band` over `HotNow`/`ThisWeek`/
    `ThirtyDays`/`Horizon`, plus the per-band ring geometry), `scene.rs`
    (`TimeWellState`, camera/cards, ring-centric `(focused_ring, ring_pos)`
    keyboard nav — spin-to-gate on Left/Right, camera dolly on Up/Down),
    `sync.rs` (keyed-join reconcile), `text.rs` (per-card vello scene),
    `hud.rs` (edge HUD), `activity.rs` (kernel-activity ring pulse), `panel.rs`
    (shared in-scene MSDF panel primitive).
- **`input/`** — focus-based action dispatch. `FocusArea` (`Compose|Conversation|
  Dialog`, `focus.rs:71`) is the single source of truth for what keys do;
  `ActiveSurface` (`Chat|Shell`, `focus.rs:116`) disambiguates within Compose
  (Ctrl+Z toggles). `InputMap` bindings, `Action` enum, `VimMachineResource` for
  modal compose, `InterruptState` (Ctrl+C).
- **`connection/`** — SSH attach + event polling. `bootstrap.rs` spawns a
  single-threaded tokio `LocalSet` thread (for `!Send` capnp) running the
  `ActorHandle`. `actor_plugin.rs` owns the live handle + reactive connection
  state, polls server events/status/results each frame, and persists the current
  context to kernel KV.
- **`peers/`** — peer registry + `invoke_peer` transport (`PeerInvocationChannel`;
  dispatches `"switch_context"`/`"active_context"`).
- **`kaish/`** — in-process kaish syntax validation (lexer/parser, no subprocess)
  for red-tinting bad shell input.
- **`text/`** — shaping + rendering on one Parley source. `VelloFont` (family name;
  glyphs in the shared Parley collection), `ShapingFonts`/`TextMetrics`, the MSDF
  path (`MsdfAtlas` 1024², async SDF gen, `MsdfBlockRenderer`), `RichContent` +
  `RichContentKind` (Markdown/Sparkline/Svg/Abc/Output/Image), ABC→scene,
  sparkline→scene, markdown brushes.
- **`shaders/`** — `BlockFxMaterial` (a `UiMaterial`): texture + glow + fx_params
  (breathe/pulse/chase) + text glow + cursor beam + SDF border + label gaps +
  selection highlight. `sync_block_fx` maps border style + theme → uniforms.
- **`ui/`** — chrome: `DockState` (north/south docks as single
  VelloUiScene+texture entities), `TilingTree`/`TilingReconciler` WM,
  `DriftState`, timeline scrubbing, `Theme` (from `theme.toml`), debug overlay.
- **`commands/`**, **`config/`** — keyboard shortcuts (`:` commands now go
  server-side via kaish); config loading with accumulated startup errors.

---

## Render pipeline (two passes, one rasterizer)

Block cells render in two passes sharing one `vello::Renderer`:

**Pass 1 — vello rasterization.** PostUpdate `build_block_scenes` inspects each
cell's `RichContentKind`: `Svg` (via `vello_svg`), `Abc` (via
`kaijutsu_abc::engrave`), and `Sparkline` append to `VelloUiScene.scene` and set
`render_method = Vello`; `Markdown`/`Output`/plain run Parley → `collect_msdf_glyphs`
and set `render_method = Msdf` (leaving `ui_scene.version == 0` so vello extract
skips them); `Image` draws a placeholder. Border/label glyphs always go to MSDF.
`resize_block_textures` computes physical dims and reallocates the `Image` (hard
cap 8192 px). The render world extracts dirty scenes (version-gated) and
`render_vello_scenes` rasterizes to texture.

**Pass 2 — MSDF compositing**, *after* pass 1: `render_msdf_block_textures`
composites glyphs on top of vello blocks (border already present) or clears first
for MSDF blocks.

Role-group borders and docks use plain `VelloUiScene + VelloUiTexture + ImageNode`
(no material). Block cells carry **both** `ImageNode` (so Bevy prepares the
`GpuImage`) and `MaterialNode<BlockFxMaterial>` (samples the same handle for
post-fx) — the ordering gotcha noted in memory; `resize_block_textures` updates the
handle atomically.

---

## Connection to the kernel

`main.rs` parses `--host/--port/--insecure`. `ActorPlugin` spawns the bootstrap
thread and a `SpawnActor` command; on `ActorReady`, an `IoTaskPool` task waits for
`Connected`, calls `whoami` (→ identity), `attach_peer(nick="kaijutsu-app")`
(funnels invocations to `PeerInvocationChannel`), then `list_contexts` + reads
kernel KV `<client-id>.current_context` to restore the last-viewed context.
Context switches travel `ContextSwitchRequested → handle_context_switch` (join on
cache miss). Ongoing block events drain from `subscribe_events` each frame and
route by `context_id` into the matching `SyncedDocument`. `persist_current_context`
writes the active id back to kernel KV on change.

---

## Smells (not fixed — see [issues](../issues.md))

- **`BlockScene` misnomer** — no longer holds a scene; rename to `BlockContent` is
  a tracked follow-up (`block_render.rs:51`).
- **`cell/` shim** — transition artifact; module boundary is meaningless until
  old import paths are cleaned up.
- **Triple Chat/Shell discriminator** — `FocusArea` + `ActiveSurface` +
  `InputOverlay.mode` (the last unread by the submit path). A three-variant
  `FocusArea::Compose(ActiveSurface)` would collapse it.
- **77 `#[allow(dead_code)]`** suppressors for future-phase API (error-block UI,
  syntax highlight, inline editing, …) — inhibits dead-code discovery; prefer
  `#[cfg(feature)]`.
- **Tall-block single-texture cap** (8192 px, halved at 2× HiDPI) — large tool
  output gets lossy Y-compression; the fix is tiled rendering.
- **Image blocks are placeholders** — full CAS-read + decode pipeline is a
  follow-up.
- **Connection race** — `IdentityReceived` patches `connected = true` to work
  around a one-frame deferred-resource race (`actor_plugin.rs:634`).
