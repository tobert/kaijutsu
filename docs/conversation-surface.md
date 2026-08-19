# The conversation surface

*2026-08-16 — direction agreed (Amy + Fable, gemini-pro deliberation concurring).
Status: shipped. All slices (0 through 5, decapitation) landed 2026-08-18 —
the conversation surface (`view::surface`) is the sole conversation renderer;
the legacy Bevy-UI per-block-cell path and its `ConversationRenderPath` flag
are deleted. See `docs/devlog.md` for the arc; the "Migration slices" section
below is the original plan and no longer describes live branches.*

## The goal, stated as an invariant

**Scrolling changes one number and touches nothing else.** A wheel event mutates
a scroll offset; the next frame draws visible glyphs at `y − offset`. No layout
engine, no widget-tree mutation, no text shaping, no entity churn on that path —
ever. Frame cost is O(visible pixels), independent of document size and block
size. That is what Ghostty/Kitty-class terminals do, and it is the standard the
conversation view is held to ("input-to-photon in a frame or two").

The corollary for big blocks: **freezes become structurally impossible, not
tuned away.** Shaping and rasterization never run synchronously on the scroll
or render path.

## Why now

The conversation has been through its transformations and come out stable:
block text is a plain `String` (the CRDT is gone, 2026-08-16), streaming is
100% append, the kernel is the sole sequencer, and the app follows a change
feed. The rendering stack was already crawling toward this design — an
entity-free height model (`ConversationGeometry`) instead of trusting layout,
Parley+MSDF instead of Bevy `Text`, shaders drawing over the text. The rewrite
finishes the thought by evicting Bevy UI from the middle of it.

What the middle costs today (survey, 2026-08-16): the conversation is a Bevy UI
flex column of per-block cells, virtualized by `Display::None` + spacer nodes;
scrolling flows wheel → 20px quantum → eased target → `ScrollPosition` → taffy;
band churn re-fires `replace_children` most scroll frames; ~5 full-document
walks run per frame ungated; entering the band shapes and rasterizes a block's
MSDF texture synchronously (the freeze); per-block RTT textures mean a giant
block is a giant texture (memory, max-texture-size). Follow-mode streaming
renders at the 10 Hz reactive idle rate. None of these are tuning problems;
they are the architecture.

## Target architecture: direct-surface virtualization

The conversation viewport stays a node in the tiling layout. Its **content**
stops being a widget tree and becomes a single custom render pass:

- **One shared MSDF glyph atlas**, not per-block RTT textures. Blocks reduce to
  cached runs of glyph instances (atlas UVs + positions). The GPU draws every
  visible glyph in one instanced pass. Per-block textures are deleted — with
  them go the memory bloat, the max-texture-size ceiling on giant blocks, and
  the synchronous raster freeze.
- **Scroll offset is a draw-time translation** (uniform in the vertex shader).
  Event → offset → same-frame draw. Sub-pixel offsets are allowed all the way
  down; the vertex shader snaps final coordinates to *physical* pixels so text
  doesn't shimmer ("glyph swimming" — see failure modes).
- **`ConversationGeometry` survives, repurposed.** The prefix-sum height model
  stops driving entity lifecycle (spawn/despawn bands, `plan_block_band`) and
  becomes a pure spatial index: at extraction time, an O(log n) query answers
  "which blocks intersect `[offset, offset + viewport]`", and only those blocks'
  pre-shaped glyph buffers are pushed to the GPU. Virtualization becomes
  culling, not ECS surgery.
- **Shaping is off the hot path, and append-aware.** Shaped layouts are cached
  per (block, version). The historical backlog shapes off-thread in the task
  pool; the live streamed tail shapes incrementally (append-only means only the
  last line re-shapes) — small enough to stay synchronous so the tail never
  pops in late. Giant blocks shape lazily by line-chunk; if Parley needs
  whole-string context (bidi), an internal max-chunk layer bounds it anyway.
- **Borders, pulses, selection, overlays** become instanced SDF quad primitives
  in the same pass (or an adjacent one), sharing the scroll uniform — pixel
  lockstep with the text for free. This is a better home for the existing
  over-text shaders than compositing across UI nodes.
- **Rich content** (ABC→staff SVG, diffs' special treatments, images) rides as
  textured quads positioned in the same coordinate space. Rich blocks are the
  minority; they don't get to reimpose a layout engine on the majority.

What stays untouched: the block store, the change feed, `ContextMirror`,
kernel/wire — this is entirely an app-side render rewrite. What is deleted at
the end: `UiRttTexture` and per-block RTT, `plan_block_band` /
`spawn_block_cells` band lifecycle, `replace_children` reordering, taffy
involvement in conversation content, `ScrollPosition` on the container, and the
render-mode flip-flopping that made smoothing constants change mid-gesture.

## Scroll feel doctrine

- Wheel/trackpad deltas apply **unquantized** (the 20px quantum was a dead
  zone; slice 0 removes it).
- Easing, if kept at all, is frame-rate independent: `1 − exp(−k·dt)`. Direct
  1:1 tracking during a gesture is acceptable and terminal-like; a short
  settle for keyboard jumps is fine.
- **No momentum/inertia physics.** The do-not-build fence in issues.md stands.
- The render loop runs Continuous whenever the offset is moving **or** content
  is streaming while followed; reactive idle is for actually-idle.
- Follow mode is sticky: leaving the tail is an explicit user act and so is
  returning; new content never re-latches follow by itself.

## Migration slices

0. **Defect relief on the existing stack** (branch `scroll-relief`, in flight):
   quantum removal, exponential ease, follow-streaming Continuous gate,
   logical-rounding removal, follow-yank fix, log demotion. Makes today
   livable; nothing here is thrown away by the rewrite (the doctrine above is
   implemented first here).
1. **Atlas + pipeline foundation, shadow mode.** Build the shared atlas manager
   and the custom render pass (glyph instances + SDF quads). Render *one*
   block's text through it, overlaid on the live app, scroll uniform wired.
   Validates MSDF clarity, physical-pixel snapping, and 1-frame latency without
   touching the existing view. Ship behind a debug toggle.
2. **Geometry hand-off.** Wire `ConversationGeometry` to the extraction phase;
   draw all visible plain-text blocks through the new pass; existing UI cells
   for those blocks stop rendering (kept dormant behind the flag).
3. **Rich content + chrome.** Borders/headers/pulse/selection as SDF
   primitives; ABC/SVG/diff blocks as textured quads. Input hit-testing (block
   focus, x/za targets) reads geometry, not UI nodes.
4. **Decapitation.** Delete the UI-node path: band lifecycle, RTT, spacers,
   container scrolling. `ConversationGeometry` + the surface are the view.

Each slice ships with the app usable; the flag flips per-slice, not big-bang.

## Failure modes to watch (gemini deliberation, kept verbatim in spirit)

- **Glyph swimming**: continuous float offsets + GPU translation shimmer unless
  the final coordinates snap to physical device pixels (scale-factor aware).
- **Anchor jumps on height correction**: when async shaping replaces an
  estimated height with a real one, prefix sums shift; deep-scrolled views
  jump violently unless a compensation step adjusts `scroll_offset` by the
  delta in the same frame, keeping the top visible block pinned.
- **Pipelined-render latency**: Bevy runs Update and Render concurrently;
  extract the scroll state as late as possible or the "same-frame" claim
  quietly becomes next-frame.
- **HiDPI seams**: the geometry model is logical, the snap is physical; every
  conversion goes through one helper (`view/ui_rtt::logical_size` lineage) or
  it will be wrong on exactly one machine.

## Error rendering policy (companion decision, same day)

Errors live **in the log + a badge** — no pinned surface, no toast layer.
There never was a pinned region: the half-screen "tool error" wall was an
unbounded-height Error block revealed top-anchored by follow mode. Policy
(branch `error-render`, in flight): Error blocks default to a collapsed stub —
category, **provenance** ("what is this connected to": resolve `parent_id` to
the tool call / interrupted turn and say so; unresolvable parents are labeled
orphans, which is also information), first lines of detail, expandable via the
normal collapse toggle. The dock badge counts each failure once (a failed tool
currently mints two `Status::Error` blocks — ToolResult + child Error block)
and gains a jump-to-latest-error action. Kernel-side truth is untouched:
error blocks stay whole in the document and hydrate to the LLM as before
(docs/memory.md doctrine — the operator and the model can always read them).
