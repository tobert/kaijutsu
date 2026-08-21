# The app

*Deep-dive companion to [README.md](README.md). Covers `kaijutsu-app` — the
Bevy GUI and its render pipeline. Code is truth; this page was re-derived
2026-08-21 against the current tree. The plugin list, `Screen` enum, module
map, and render pipeline below are read from source, not carried over from
the pre-conversation-surface version of this page — see each section for
where. One thing below is still asserted rather than verified: the "Smells"
section beyond the four bullets marked checked.*

`kaijutsu-app` connects to a remote kernel over SSH (via `kaijutsu-client`),
maintains a multi-context document mirror, and renders conversation blocks
through the conversation surface (`view::surface`) — an entity-free,
GPU-buffer renderer, not a Bevy UI node per block.

**Bevy 0.19.** `crates/kaijutsu-app/Cargo.toml`: `bevy = { version = "0.19",
features = ["default", "bevy_ui_debug"] }`. `Cargo.lock:878`: `name = "bevy"`,
`version = "0.19.0"`. Check `Cargo.lock` yourself before trusting a version
claim anywhere else — see CLAUDE.md "Bevy Quick Reference".

---

## Bevy architecture

One always-on `Camera3d` (`main.rs` `setup_camera`) with `Hdr` and additive
thresholded `Bloom::OLD_SCHOOL` (threshold 1.0, low-frequency boost pulled
down from the preset — a tight rim glow for the well's HDR "bling", not a
fog), clearing to the theme background. No `Camera2d` anywhere in the app;
conversation UI renders after tonemapping so its colors are unaffected by
bloom.

**`Screen` state machine** (`ui/screen.rs`) — five variants, not two:
`Conversation` (default), `Editor`, `Room`, `Diff`, `Fsn`. There is no
`Screen::TimeWell`: the time well is a station reached through
`Screen::Room`'s `RoomState::zoomed` (a camera pose + a resource write, not a
screen transition) — same as the patch bay. Only the FSN landscape earns its
own `Screen` variant among the room's dive targets, because it is an
unbounded world rather than bounded room furniture (`ui/screen.rs`'s
`Screen::Fsn` doc, quoting `docs/scenes/shell.md`).

**Plugin build order** (`main.rs`, `.add_plugins` calls in file order):
`DefaultPlugins → MeshPicking → BrpExtras → KjText → Input → Cell →
ConversationSurface → BlockRender → Peers → ShaderFx → Actor → ShareDial →
Dj → MidiIn → MidiPresence → MidiExchange → AppScreen → Screen → Commands →
Tiling → TilingReconciler → Dock → RosterFeed → QuickContext → Drift →
PeerRoster → Room → PatchBay → TimeWell → Tracker → Fsn → Editor → DiffView →
Timeline → Tweening`. Two ordering invariants called out in comments there:
`ConversationSurfacePlugin` must follow `CellPlugin` (its `Update` sets
anchor `.after()` two systems `CellPlugin` registers); `RoomPlugin` must
precede every zoomable station's plugin (`well_keyboard`/`patch_bay_keyboard`
clear `RoomState::zoomed` on Escape, and if the station ran first in the same
tick `room_keyboard` would see the already-cleared flag and double-fire its
own Escape branch — found live via BRP).

**Frame order:**
`InputPhase{SyncContext → Dispatch → Handle}` (`input/mod.rs:132` — no
`Cleanup` phase) →
`CellPhase{Input → Sync → Spawn → Buffer → Layout}` (`cell/plugin.rs:21`) →
`SurfaceSet{Content → Shape → Measure → Window}` (`view/surface/mod.rs`),
chained `.after()` `CellPhase::Spawn`'s `sync_conversation_geometry` and
`view::scroll::smooth_scroll` — not a member of `CellPhase` itself; `Buffer`
and `Layout` now carry only overlay/shell-dock bookkeeping, no
conversation-content systems → `PostUpdate` render-scene builds (RTT resize,
taffy layout) → render-world `ExtractSchedule` → `Render`.

**Power management:** `WinitSettings` in `main.rs`: focused mode reactive at
100 ms (10 Hz idle), unfocused mode reactive-low-power at 500 ms (2 Hz
background).

---

## Module map

Groupings below follow `view/mod.rs`'s own module doc, which is current and
worth reading directly for more detail than fits here.

- **`cell/`** — a migration shim, not real content: `cell/mod.rs` re-exports
  everything from `view/` so old `crate::cell::X` import paths still resolve.
  Real content: `cell/plugin.rs` (`CellPlugin`, `CellPhase` ordering, resource
  init, BRP methods) and `cell/block_border.rs` (`compute_border_style` and
  friends — per-block border rules, called by both the legacy path and
  `view::surface::chrome`).
- **`view/`** — component types and rendering systems for every full-viewport
  scene, not just the conversation:
  - **Conversation block pipeline** — `lifecycle` (main-cell + focused-pane
    bookkeeping), **`surface`** (the conversation renderer — see below),
    `block_render` (the shared MSDF/SVG render plumbing every *other* text
    surface draws through), `format` (pure formatting helpers), `geometry`
    (row layout model), `sync`/`submit`/`document`/`editor`/`role_divider`
    (server sync, prompt submission, the `CellEditor` buffer, role-divider
    layout math).
  - **Chat-adjacent surfaces** — `overlay` (input overlay), `shell_dock`
    (Ctrl+Z shell row), `scroll`.
  - **Room-level scenes reached from the shell** — `room` (station carousel),
    `time_well`, `patch_bay`, `fsn`, `tracker` — sharing `scene_geometry`'s
    datums (octagon shell, W-wall patch-wheel mount).
  - **Styling** — `scene_palette` (the `[scene]` theme.toml lane).
  - **Render plumbing** — `ui_rtt` (the generic render-to-texture primitive +
    HiDPI-aware sizing helpers, no vello). Bevy Remote Protocol inspector glue
    lives in `kaish::brp_methods`, not here.
  - `components` — the component/resource types shared across all of the
    above (`CellEditor`, `BlockCell`, `FocusTarget`, `InputOverlay`,
    `GlobalErrorQueue`, …).
- **`input/`** — focus-based action dispatch, the one legal reader of raw
  input (see "Input" below). `FocusArea` (`Compose|Conversation|Dialog`,
  `input/focus.rs:35`) is the single source of truth for what keys do;
  `ActiveSurface` (`Chat|Shell`, `input/focus.rs:73`) disambiguates within
  Compose. `InputMap` bindings, `Action` enum, `VimMachineResource` for modal
  compose, `InterruptState` (Ctrl+C).
- **`connection/`** — SSH attach + event polling. `bootstrap.rs` spawns a
  single-threaded tokio `LocalSet` thread (for `!Send` capnp) running the
  `ActorHandle`. `actor_plugin.rs` owns the live handle + reactive connection
  state, polls server events/status/results each frame, and persists the
  current context via `set_last_context`. Also here: `drift.rs` (context list
  + staged-queue polling), `peers.rs` (attached-peer roster), `roster.rs`
  (`/run/roster/index` feed), `share_dial.rs` (`/r` client shares).
- **`peers/`** — peer registry + `invoke_peer` transport
  (`PeerInvocationChannel`; dispatches `"switch_context"`/`"active_context"`).
- **`kaish/`** — in-process kaish syntax validation (lexer/parser, no
  subprocess) for red-tinting bad shell input, plus the BRP inspector methods
  (`kaish/brp_methods.rs`).
- **`text/`** — shaping + rendering on one Parley source. `VelloFont` (family
  name; glyphs in the shared Parley collection), `ShapingFonts`/`TextMetrics`,
  the MSDF path (`MsdfAtlas` 1024², async SDF gen, `MsdfBlockRenderer`),
  `RichContent`/`RichContentKind` (Markdown/Sparkline/Svg/Abc/Output/Image),
  ABC→scene, sparkline→scene, markdown brushes, ANSI span handling.
- **`shaders/`** — `BlockFxMaterial` (a `UiMaterial`, the legacy per-block
  path's texture + glow + fx_params + text glow + cursor beam + SDF border +
  label gaps + selection highlight) plus the 3D-scene materials (well cards,
  track rays, trace glow, chords, terrace rings). The conversation surface
  does not use `BlockFxMaterial` — its chrome is a WGSL pass of its own (see
  below).
- **`ui/`** — chrome: `DockState` (north/south docks as single
  VelloUiScene+texture entities), `TilingTree`/`TilingReconciler` WM,
  `screen.rs` (`Screen` state machine, above), `theme.rs` (from
  `theme.toml`), timeline scrubbing, debug overlay, `quick_context.rs` (the
  Ctrl+A prefix's peek overlay).
- **`commands/`**, **`config/`** — keyboard shortcuts (`:` commands go
  server-side via kaish); config loading with accumulated startup errors.

---

## The conversation surface (`view/surface/`)

The conversation's sole renderer. Full design rationale and history:
`docs/conversation-surface.md` (status: shipped, all slices landed
2026-08-18 — the legacy per-block-cell Bevy-UI column is deleted, along with
its `ConversationRenderPath` flag). This section is the module map;
`view/surface/mod.rs`'s own doc comment is more complete and should be read
directly for schedule-placement reasoning.

**The invariant:** scrolling changes one number and touches nothing else. No
Bevy UI node, no per-block render-to-texture, no taffy layout per block — a
block scrolling into view costs a binary search and a buffer re-upload, not
an entity spawn. This also removes the legacy path's biggest structural
smell: a per-block RTT capped a giant block's texture at 8192 px (see
"Smells" below); the surface's RTT is one pane-sized target, never
document-sized, so that failure mode cannot recur here.

**Four stages, chained in `SurfaceSet`** (`content → chrome → chunk/shape_cache
→ window`, run every frame `.after()` `sync_conversation_geometry` and
`smooth_scroll` — see "Frame order" above for why Shape must run after the
scroll ease, not before):

1. **Content** (`content.rs`) — `BlockContentCache`, keyed by `BlockId` not
   entity: formats each block's text, detects rich content (markdown, diff,
   ABC tune, sparkline, SVG, image) once and caches the parsed result behind
   an `Arc` alongside the text. **`chrome.rs`** runs in the same set:
   `BlockChromeCache` calls `cell::block_border`'s pure
   `compute_border_style` over the content cache — same rules as the legacy
   path, no ECS.
2. **Shape** (`chunk.rs` + `shape_cache.rs`) — a block is split into chunks
   (runs of complete hard lines — chunk boundaries only ever land after a
   `\n`) and each chunk is shaped once into `Arc`'d glyph runs
   (`ShapedBlockCache`). The re-shape decision is one equality check on
   `ShapeKey` (content version, wrap width, collapsed, indent, metrics
   epoch); nothing outside that key — scroll offset, atlas version — is a
   reshape reason. Rows near the viewport shape synchronously; the rest go to
   `AsyncComputeTaskPool` nearest-row-first. The cache is bounded
   (`SHAPE_CACHE_MAX_GLYPHS`, LRU beyond `DESPAWN_MARGIN_SCREENS`); in-window
   and streaming blocks are never evicted. A streaming block re-shapes only
   its open tail chunk — complete chunks freeze and are reused by `Arc`
   pointer. A theme-only recolor rewrites glyph colors through
   `Arc::make_mut` without reshaping, for the span-free majority; spanned
   (markdown) blocks can't take that path and reshape instead. This
   bookkeeping — key-equality reshape gating, LRU eviction under budget,
   frozen-chunk reuse, recolor-without-reshape, generation tracking for cache
   invalidation — is pinned by tests added 2026-08-21 in
   `view/surface/shape_cache.rs` (`#[cfg(test)] mod tests`, ~1800 lines);
   read them for the exact guarantees rather than trusting this paragraph's
   summary.
3. **Measure** (`shape_cache.rs::apply_shaped_measurements`) — feeds shaped
   heights back into `ConversationGeometry` and anchor-compensates the scroll
   offset so a late-landing async shape doesn't visibly shift the viewport.
4. **Window** (`window.rs` + `chrome.rs::build_chrome_instances`) — decides
   the slack window (viewport + one screen on each side) the surface holds
   buffers for; only rebuilds when the offset leaves that band or a discrete
   `WindowKey` token moves (`geometry_epoch`, `atlas_version`, `metrics_epoch`,
   `theme_epoch`, `shaped_generation`, `viewport` size). Inside the band, a
   scroll is a uniform write and nothing here runs.

**Rich content** (`rich.rs`) rides the same spine: ABC, diff bands, and
sparklines become geometry (not glyphs) built from the same shared
constructors the legacy renderer used, so a diff's word washes can't
disagree between paths; SVG rasterizes to a cached CPU raster the surface
draws as a textured quad. Images remain a placeholder rectangle plus caption
— same as the legacy path (`content.rs`'s `image_placeholder_label`; full
CAS-read + decode is unbuilt, tracked in `docs/issues.md`).

**Labels** (`labels.rs`) — border labels ("TOOL CALL kaish", "thinking") and
the inclusion checkbox are glyphs, not quads; drawn from the same
`compute_border_style` output `chrome.rs` draws the box from, so a label can
never disagree with its own border.

**Target** (`target.rs`) — one viewport-sized RTT per `ConversationSurface`,
composited by a plain `ImageNode` (opaque: the surface clears to the theme
background so premultiplied/straight alpha can't disagree at the seam).
Resized from its own `ComputedNode` after `bevy::ui::UiSystems::Layout`, the
same pattern the dock uses.

**Extraction** (`extract.rs`, render world) — reads the scroll offset in
`ExtractSchedule`, the one moment both worlds are synchronized, so an
in-flight scroll never renders a frame stale. Instance buffers are keyed by
`buffer_version` + atlas version and rebuilt only when one moves; ordinary
scrolling reaches the GPU as a 64-byte uniform write. Chrome draws through
its own WGSL pass (`assets/shaders/surface_chrome.wgsl`) — an instance buffer
of SDF quads per pane, replacing the legacy path's one-`BlockFxMaterial`-per-block
approach — sharing `BlockRenderPlugin`'s `GpuTextureLimits` and MSDF atlas
rather than owning either.

---

## Render pipeline: two paths, not one

The conversation surface (above) is the primary path. `view::block_render`
still serves four other text surfaces that were not part of the
conversation-surface rewrite: the compose overlay, the shell dock, the
editor, and the diff viewer (`view/block_render.rs`'s module doc names all
four). Both paths share MSDF glyphs (`text::msdf`) and the vello `Renderer`
for shaping, but they composite differently.

**Legacy per-surface path** (editor, dock, diff view, overlay) — two passes
sharing one `vello::Renderer`:

- **Pass 1 — vello rasterization.** Each consumer inspects its cell's
  `RichContentKind`: `Svg`/`Abc`/`Sparkline` append to `VelloUiScene.scene`
  and set `render_method = Vello`; `Markdown`/`Output`/plain shape via Parley
  → MSDF glyphs (`render_method = Msdf`, leaving `ui_scene.version == 0` so
  vello extract skips them); `Image` draws a placeholder. Border/label
  glyphs always go to MSDF. `resize_block_textures` reallocates the `Image`
  per block, capped at 8192 px (`FALLBACK_MAX_TEXTURE_DIM` /
  `VELLO_MAX_TEXTURE_DIM`, `view/block_render.rs:271,279`) — a tall block
  past that cap gets lossy Y-compression on these four surfaces (see
  "Smells").
- **Pass 2 — MSDF compositing**, after pass 1: `render_msdf_block_textures`
  composites glyphs on top of vello content (border already present) or
  clears first for MSDF-only blocks.

Role-group borders and docks use plain `VelloUiScene + VelloUiTexture +
ImageNode` (no material). Block cells on this path carry both `ImageNode`
(so Bevy prepares the `GpuImage`) and `MaterialNode<BlockFxMaterial>` (samples
the same handle for post-fx) — an ordering gotcha the two-component split
exists to work around.

---

## Input

All keyboard/gamepad/mouse input flows through the central action table:
`input/dispatch.rs:57`'s `dispatch_input` is documented as "the ONE system
that reads raw input" and is the sole `MessageReader<KeyboardInput>` in the
crate. Never read `ButtonInput`/`KeyboardInput` directly in a view or scene —
add an `InputContext` + bindings to the table instead. The vi editor is the
one sanctioned raw reader (an explicit keyboard grab). Canonical:
`docs/input.md`.

## HiDPI units

`ComputedNode` (size/content_box/border/padding) and UI `GlobalTransform` are
**physical** pixels; font sizes, `Val::Px`, and `ScrollPosition` are
**logical**. Never feed a raw `ComputedNode` dimension into layout math —
convert through `view::ui_rtt::logical_size` / `logical_content_size`
(`view/ui_rtt.rs:81,86`; tested at `view/ui_rtt.rs:163,179` — identity at 1x,
undoes the scale otherwise). Invisible at 1x, breaks on any other scale
factor — a live source of bugs.

---

## Connection to the kernel

`main.rs` parses `--host/--port/--insecure`. `ActorPlugin` spawns the
bootstrap thread and a `SpawnActor` command; on `ActorReady`, an
`IoTaskPool` task waits for `Connected`, calls `whoami` (→ identity),
`attach_peer(nick="kaijutsu-app")` (funnels invocations to
`PeerInvocationChannel`), then `list_contexts` + `get_client_view` to restore
the last-viewed context. Context switches travel `ContextSwitchRequested →
handle_context_switch` (join on cache miss). Ongoing block events drain from
`subscribe_events` each frame and route by `context_id` into the matching
`SyncedDocument`. `persist_current_context` writes the active id back via
`set_last_context` on change.

---

## Smells (not fixed — see [issues](../issues.md))

Checked against source 2026-08-21 unless noted:

- **`BlockScene` misnomer** — `view/block_render.rs`'s `BlockScene` doc
  comment now says so itself: "Carries no rasterizable scene of its own …
  The name is historical; rename to `BlockContent` is a tracked follow-up."
  Only used by the legacy path now — the conversation surface keeps the
  equivalent state in `content::BlockContentCache` instead.
- **`cell/` shim** — still exactly a re-export facade
  (`cell/mod.rs`: "All component types and systems now live in
  `crate::view`. This module re-exports them so existing `crate::cell::X`
  imports continue to work.").
- **83 `#[allow(dead_code)]`** suppressors across `kaijutsu-app/src`
  (counted 2026-08-21: `grep -rc '#\[allow(dead_code)\]' src/**/*.rs` summed)
  for future-phase API (error-block UI, syntax highlight, inline editing,
  …) — inhibits dead-code discovery; prefer `#[cfg(feature)]`.
- **Tall-block single-texture cap (8192 px)** — confirmed still live, but
  **only for the legacy path's four consumers** (editor, dock, diff view,
  overlay); the conversation surface's viewport-sized RTT (`target.rs`)
  structurally cannot hit it. Large tool output in the editor/dock/diff/
  overlay still gets lossy Y-compression; tiled rendering is the fix if it's
  ever prioritized there.
- **Image blocks are placeholders** — confirmed still true on both paths
  (legacy `RichContentKind::Image` and surface `content.rs`'s
  `image_placeholder_label`). Full CAS-read + decode pipeline is unbuilt.
- **Not re-verified this pass** — carried over from the prior version of
  this page without a fresh check: "Triple Chat/Shell discriminator"
  (`FocusArea` + `ActiveSurface` + `InputOverlay.mode`; the first two are
  confirmed still present at `input/focus.rs:35,73` and `InputOverlay` at
  `view/components.rs:290`, but whether the submit path still reads
  `InputOverlay.mode` unread-by-anything-else was not re-checked).
- **Connection race — RESOLVED, remove from future revisions of this list.**
  The `IdentityReceived`-patches-`connected` workaround is still in the code
  (`connection/actor_plugin.rs:1340-1352`) but its own comment now says the
  race it worked around is closed at the source
  (`poll_connection_status` seeds from `current_status()` on resubscribe) and
  it is "kept as a harmless belt-and-suspenders backup." Not a live smell.
